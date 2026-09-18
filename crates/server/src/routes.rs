use std::sync::Arc;

use axum::{
    extract::{Path, Request, State},
    http::{header, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use iiif::{parse_identifier, IiifError, ImageRequest, ProjectionId};
use moka::future::Cache;
use serde_json::json;
use sha1::{Digest, Sha1};
use tiling::{TileEngine, TileError};
use tokio::sync::Semaphore;

use crate::auth::{auth_middleware, AuthConfig};
use crate::origin::resolve_base_url;

/// Default cap on concurrent expensive tile RENDERS (cache-miss `spawn_blocking` computes), independent
/// of both the HTTP-layer concurrency limit and tokio's much larger blocking-pool size. See
/// `AppState::render_semaphore` for the rationale.
pub const DEFAULT_RENDER_PERMITS: usize = 16;

/// The name `AppState::new` files its single engine under.
///
/// Never appears in a URL: a single-image server serves that image at the root, so the name is an
/// internal handle. It is also what the HMAC message binds for root-alias requests, which is why it
/// is a constant rather than a literal repeated in two modules.
pub const ROOT_ALIAS_NAME: &str = "default";

/// Tiles are treated as immutable for a given (proj, region, size, format) tuple — the
/// documented assumption is that the SOURCE data backing a served image does not change during
/// its deployment lifetime (a new dataset gets a new identifier/deployment, not an in-place
/// mutation). Under that assumption a tile can be cached hard by clients/CDNs; one year is the
/// conventional "effectively forever, but not literally infinite" TTL for immutable HTTP
/// resources (matches e.g. how hashed static assets are usually served).
const TILE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// info.json changes far more readily than tile bytes (e.g. if `base_url`/forwarded-origin
/// differs per request), so it gets a much shorter TTL than tiles while still being cacheable
/// for a short window (an OpenSeadragon page load fetches it once and then many tiles).
const INFO_CACHE_CONTROL: &str = "public, max-age=300";

/// Default byte budget for the tile cache: 512 MiB of encoded JPEG bytes. Bounded via the
/// weigher below rather than an entry count, since tile sizes vary a lot with output dimensions
/// and JPEG quality — an entry-count cap would let a flood of large tiles blow well past any
/// sane memory budget while a byte-count cap can't.
const DEFAULT_CACHE_BYTES: u64 = 512 * 1024 * 1024;

/// Cache key for a rendered tile, derived ONLY from the client-facing request: the raw IIIF path
/// segments `{proj}/{region}/{size}/{quality_dot_format}` (rotation is always `0`, the only
/// value `ImageRequest::parse` accepts, so it's omitted as a constant dimension). These are the
/// exact strings from the URL, not the parsed `Region`/`Size` (which hold `f64` and so aren't
/// `Hash`/`Eq`) — using the raw strings is both simpler and already canonical: two requests with
/// the same path segments are the same request by definition, and IIIF clients (OpenSeadragon
/// included) always request a given tile via the same canonical path.
///
/// CRITICAL: this key must NOT include the internal pyramid level `plan()` picks to service the
/// request. The level is entirely a function of `(region, size)` (see `ZarrTileEngine::plan`),
/// so folding it into the key would be redundant at best — and at worst would fragment the cache
/// into multiple entries for what is, from the client's perspective, one identical tile.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TileKey {
    /// The image is part of the key for the same reason rotation is: two images answer the same
    /// `{proj}/{region}/{size}` and are different pictures. Omitting it would serve one image's
    /// tile under another's URL, which is the worst kind of cache bug because every response is a
    /// 200.
    image: crate::name::ImageName,
    proj: String,
    region: String,
    size: String,
    /// Part of the key because `rotationBy90s` is supported: `.../0/default.jpg` and
    /// `.../180/default.jpg` are different images from the same region and size, so omitting
    /// rotation here would serve one for the other.
    rotation: String,
    quality_dot_format: String,
}

#[derive(Clone)]
pub struct AppState {
    /// Resolves a name to an image and holds the open ones. Replaces the single engine this
    /// server held before it could serve more than one image.
    pub registry: Arc<crate::registry::ImageRegistry>,
    /// The image ALSO served at the root under the pre-multi-image paths, when the catalogue holds
    /// exactly one.
    ///
    /// A compatibility promise, not a convenience: every URL a single-image deployment publishes
    /// today keeps working. `None` as soon as there is more than one image, because there would be
    /// no honest answer to "which one".
    pub root_alias: Option<crate::name::ImageName>,
    pub base_url: String,
    /// Explicit `--public-base-url`/`ZIV_PUBLIC_BASE_URL` override, if configured. Takes
    /// precedence over forwarded headers over `base_url` — see `crate::origin::resolve_base_url`.
    pub public_base_url: Option<String>,
    pub auth: Option<AuthConfig>,
    tile_cache: Cache<TileKey, Arc<Vec<u8>>>,
    /// Bounds concurrent expensive tile RENDERS (the `spawn_blocking` compute inside a cache
    /// MISS) independent of the HTTP-layer concurrency limit and tokio's blocking-pool size.
    /// Acquired only on a cache miss, inside the `try_get_with` closure — a cache HIT never
    /// touches this semaphore, so hits stay cheap/fast even when every permit is held by
    /// in-flight renders. See `tile_handler`.
    pub render_semaphore: Arc<Semaphore>,
    /// How many permits `render_semaphore` was built with. Needed at shutdown: acquiring ALL of
    /// them is how the server establishes that no render is still running (see
    /// `crate::drain_renders`), and `Semaphore` does not report its original capacity.
    pub render_permits: usize,
    /// Liveness/readiness flag: `false` only in tests that want to simulate an engine that
    /// failed to become ready (e.g. the backing store became unreachable). In production this
    /// is always `true` by construction — `run_server` only builds `AppState` once `open()` on
    /// the engine has already succeeded, so by the time `AppState` exists the engine IS ready.
    ready: bool,
}

impl AppState {
    /// Single-image compatibility constructor.
    ///
    /// Retained because twenty-five call sites across ten files build state from one engine, and
    /// because `ziv serve one.zarr` is still the common case. Wraps the engine in a one-entry
    /// registry under the name `default`, which is invisible: the root alias means every URL
    /// resolves at the paths it always did.
    pub fn new(engine: Arc<dyn TileEngine + Send + Sync>, base_url: String) -> Self {
        Self::with_cache_bytes(engine, base_url, DEFAULT_CACHE_BYTES)
    }

    /// Construct with an explicit byte budget for the tile cache (used by tests to exercise
    /// eviction without allocating hundreds of megabytes).
    pub fn with_cache_bytes(
        engine: Arc<dyn TileEngine + Send + Sync>,
        base_url: String,
        max_cache_bytes: u64,
    ) -> Self {
        let registry = crate::registry::ImageRegistry::from_single_engine(
            crate::name::ImageName::parse(ROOT_ALIAS_NAME)
                .expect("the root alias name is a valid image name"),
            engine,
        );
        Self::from_registry_with_cache_bytes(Arc::new(registry), base_url, max_cache_bytes)
    }

    /// Build state from an already-constructed registry. The production path.
    pub fn from_registry(registry: Arc<crate::registry::ImageRegistry>, base_url: String) -> Self {
        Self::from_registry_with_cache_bytes(registry, base_url, DEFAULT_CACHE_BYTES)
    }

    pub fn from_registry_with_cache_bytes(
        registry: Arc<crate::registry::ImageRegistry>,
        base_url: String,
        max_cache_bytes: u64,
    ) -> Self {
        let tile_cache = Cache::builder()
            .weigher(|_k: &TileKey, v: &Arc<Vec<u8>>| v.len() as u32)
            .max_capacity(max_cache_bytes)
            .build();
        let root_alias = registry.sole_image();
        AppState {
            registry,
            root_alias,
            base_url,
            public_base_url: None,
            auth: None,
            tile_cache,
            render_semaphore: Arc::new(Semaphore::new(DEFAULT_RENDER_PERMITS)),
            render_permits: DEFAULT_RENDER_PERMITS,
            ready: true,
        }
    }

    /// Enable auth on this state (builder-style). `router()` layers `auth_middleware` on the
    /// info.json+tile sub-router only when this is `Some`.
    pub fn with_auth(mut self, auth: AuthConfig) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Set an explicit public base URL (builder-style), e.g. from `--public-base-url` /
    /// `ZIV_PUBLIC_BASE_URL`. Wins over forwarded headers and the bind-address `base_url` — see
    /// `crate::origin::resolve_base_url`.
    pub fn with_public_base_url(mut self, public_base_url: impl Into<String>) -> Self {
        self.public_base_url = Some(public_base_url.into());
        self
    }

    /// Cap concurrent expensive renders at `permits` instead of `DEFAULT_RENDER_PERMITS`
    /// (builder-style; used by tests to exercise the bound with a small number).
    pub fn with_render_permits(mut self, permits: usize) -> Self {
        self.render_semaphore = Arc::new(Semaphore::new(permits));
        self.render_permits = permits;
        self
    }

    /// Construct a NOT-ready state, for tests simulating a store/image that failed to become
    /// reachable after startup (`/readyz` -> 503).
    #[cfg(test)]
    pub fn not_ready(mut self) -> Self {
        self.ready = false;
        self
    }

    /// Readiness: the engine's backing image was successfully opened before this `AppState` was
    /// constructed (see `ready`'s doc), so "the engine is present and initialized" IS the cheap
    /// readiness probe here — there's no separate handle/connection to re-check per request
    /// without doing a real (expensive) read. `/readyz` reports this flag directly.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Let the cache's async eviction/maintenance tasks (which don't run synchronously with
    /// inserts) finish, then report the total weighted size currently held. Test-only
    /// introspection hook for asserting byte-bound eviction actually happened.
    #[cfg(test)]
    async fn cache_weighted_size(&self) -> u64 {
        self.tile_cache.run_pending_tasks().await;
        self.tile_cache.weighted_size()
    }

    /// Insert a tile directly into the cache, bypassing the engine entirely. Test-only hook used
    /// to prove authz runs BEFORE the cache lookup: pre-seed an entry here, then fire a
    /// request that fails auth for that same key and assert it gets 401, not these bytes.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    async fn seed_cache(
        &self,
        image: &crate::name::ImageName,
        proj: &str,
        region: &str,
        size: &str,
        rotation: &str,
        quality_dot_format: &str,
        bytes: Vec<u8>,
    ) {
        self.tile_cache
            .insert(
                Self::test_key(image, proj, region, size, rotation, quality_dot_format),
                Arc::new(bytes),
            )
            .await;
    }

    /// Read a tile straight out of the cache without going through a route. The other half of
    /// `seed_cache`, used to prove the key separates two images.
    #[cfg(test)]
    async fn peek_cache(
        &self,
        image: &crate::name::ImageName,
        proj: &str,
        region: &str,
        size: &str,
        rotation: &str,
        quality_dot_format: &str,
    ) -> Option<Arc<Vec<u8>>> {
        self.tile_cache
            .get(&Self::test_key(
                image,
                proj,
                region,
                size,
                rotation,
                quality_dot_format,
            ))
            .await
    }

    #[cfg(test)]
    fn test_key(
        image: &crate::name::ImageName,
        proj: &str,
        region: &str,
        size: &str,
        rotation: &str,
        quality_dot_format: &str,
    ) -> TileKey {
        TileKey {
            image: image.clone(),
            proj: proj.to_string(),
            region: region.to_string(),
            size: size.to_string(),
            rotation: rotation.to_string(),
            quality_dot_format: quality_dot_format.to_string(),
        }
    }
}

/// Build the full route tree: the info.json + tile routes gated by `auth_middleware` when
/// `state.auth` is configured (an OFF-by-default `Layer`, so an unauthenticated `AppState`
/// behaves exactly as before this module existed), merged with nothing else here — `/viewer/*`
/// lives in a wholly separate router (`viewer::viewer_router`) built and merged in `lib::app`,
/// never passing through this function, so it can never be gated by this layer no matter what
/// `state.auth` is.
///
/// The auth layer is applied to a SUB-ROUTER containing only these two routes, then that
/// sub-router (not the individual routes) is what's returned/merged — `axum::middleware::Layer`s
/// apply to every route already present in the `Router` they're called on, so scoping which
/// routes get gated is done by building them in their own `Router` first and layering there,
/// rather than by layering the top-level router (which would also catch `/viewer/*` once merged).
pub fn router(state: AppState) -> Router {
    // The root routes exist unconditionally, but answer 404 unless the catalogue has exactly one
    // image (see `AppState::root_alias`). Registering them either way keeps one router shape and
    // means a multi-image server returns "no image at the root" rather than a bare routing 404,
    // which is a more useful thing to read in a log.
    let tiles = Router::new()
        .route("/ziv/dimensions.json", get(dimensions_handler))
        .route("/ziv/images.json", get(images_handler))
        .route("/iiif/{proj}", get(base_uri_redirect))
        .route("/iiif/{proj}/info.json", get(info_handler))
        .route(
            "/iiif/{proj}/{region}/{size}/{rotation}/{quality_dot_format}",
            get(tile_handler),
        )
        .route("/i/{*rest}", get(mount_handler))
        .with_state(state.clone());

    let tiles = match &state.auth {
        Some(auth) => tiles.layer(middleware::from_fn_with_state(
            auth.clone(),
            auth_middleware,
        )),
        None => tiles,
    };

    Router::new().merge(tiles)
}

/// Structured JSON error body: `{"error": "<message>"}`, optionally with a `request_id` so an
/// operator can correlate a generic client-facing 500 with the detailed server-side log line.
struct ApiError {
    status: StatusCode,
    message: String,
    request_id: Option<String>,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiError {
            status,
            message: message.into(),
            request_id: None,
        }
    }

    fn with_request_id(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({ "error": self.message });
        if let Some(id) = &self.request_id {
            body["request_id"] = json!(id);
        }
        (
            self.status,
            [(header::CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
            .into_response()
    }
}

impl From<IiifError> for ApiError {
    fn from(e: IiifError) -> Self {
        ApiError::new(StatusCode::BAD_REQUEST, e.to_string())
    }
}

/// Map a `TileError` to an HTTP status. `OutOfRange` is client input that fell outside the
/// image's bounds or requested an absurd output size — that is the caller's fault, so it maps to
/// 400 (with the underlying message, which is safe to show: it's a description of what about
/// THEIR request was rejected, no internal detail). Every other variant (`Zarr`, `Iiif`,
/// `Encode`) reflects a server-side failure to service an otherwise-valid request, so those stay
/// 500 with a GENERIC body — the real error is logged via `tracing::error!` instead of being
/// handed to the client, which could otherwise leak internal paths/store details.
fn tile_error_response(err: TileError, request_id: Option<String>) -> ApiError {
    match err {
        TileError::OutOfRange(msg) => ApiError::new(StatusCode::BAD_REQUEST, msg),
        // An identifier this service does not serve is a 404, not a 400: the request grammar was
        // fine, the resource simply does not exist.
        TileError::UnknownProjection(msg) => ApiError::new(
            StatusCode::NOT_FOUND,
            format!("unknown image identifier: {msg}"),
        ),
        TileError::UnknownLabel(msg) => {
            ApiError::new(StatusCode::NOT_FOUND, format!("unknown label image: {msg}"))
        }
        TileError::Zarr(_) | TileError::Iiif(_) | TileError::Encode(_) => {
            tracing::error!(error = %err, request_id = request_id.as_deref(), "tile render failed");
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
                .with_request_id(request_id)
        }
    }
}

/// A 404 with a message describing why, in the same JSON shape every other error uses.
fn not_found(message: &str, request_id: Option<String>) -> Response {
    ApiError::new(StatusCode::NOT_FOUND, message)
        .with_request_id(request_id)
        .into_response()
}

/// Maps a registry failure onto a status.
///
/// A name that matches no source is a 404: the request grammar was fine, the resource does not
/// exist. A name that resolves but whose store will not open is a 502, because that is an upstream
/// failure rather than a client mistake, and an operator reading logs needs to tell the two apart.
///
/// **Under auth the distinction collapses to 404.** Otherwise an unauthenticated caller could probe
/// catalogue membership by status code, learning which names exist without ever being allowed to
/// see one. The operator keeps the detail: the real reason is logged either way.
fn registry_error_response(
    state: &AppState,
    err: crate::registry::RegistryError,
    request_id: Option<String>,
) -> Response {
    use crate::registry::RegistryError;
    match err {
        RegistryError::NotFound(name) => not_found(&format!("no image named {name}"), request_id),
        RegistryError::Open(name, why) => {
            tracing::error!(image = %name, error = %why, "image failed to open");
            if state.auth.is_some() {
                return not_found(&format!("no image named {name}"), request_id);
            }
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("image {name} could not be opened"),
            )
            .with_request_id(request_id)
            .into_response()
        }
    }
}

fn request_id_of(req: &Request) -> Option<String> {
    req.headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Weak `ETag` derived from the exact cache key (request-identity based, cheap: no bytes need to
/// be hashed). Weak (`W/"..."`) because the underlying bytes for a given key are, per this
/// server's own encoder, deterministic for a fixed input — but a weak tag is the honest claim:
/// it certifies "same requested resource", not "byte-identical response", which is what IIIF/HTTP
/// callers actually need for conditional GET to be useful (avoiding a re-transfer), without
/// requiring us to hash the full tile body on every request just to mint a strong tag.
fn etag_for(parts: &[&str]) -> String {
    let mut hasher = Sha1::new();
    for p in parts {
        hasher.update(p.as_bytes());
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    format!("W/\"{}\"", hex_encode(&digest))
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `true` if the request's `If-None-Match` header contains the given ETag (exact match, or `*`).
/// IIIF/browser conditional GET always sends back exactly the tag the server issued, so a plain
/// containment check (rather than full structured list parsing) is sufficient here.
fn if_none_match_matches(req: &Request, etag: &str) -> bool {
    let Some(header) = req.headers().get(header::IF_NONE_MATCH) else {
        return false;
    };
    let Ok(value) = header.to_str() else {
        return false;
    };
    value == "*" || value.split(',').any(|v| v.trim() == etag)
}

async fn info_handler(
    State(state): State<AppState>,
    Path(proj): Path<String>,
    req: Request,
) -> Response {
    let Some(name) = state.root_alias.clone() else {
        return not_found("this server has no image at the root", request_id_of(&req));
    };
    serve_info(&state, &name, &proj, "", req).await
}

/// `info.json` for one image.
///
/// `id_prefix` is what the mount contributes: `""` for the single-image root alias, `/i/{name}` for
/// a mounted image. It is passed in rather than derived so this function never has to know which
/// route reached it.
async fn serve_info(
    state: &AppState,
    name: &crate::name::ImageName,
    proj: &str,
    id_prefix: &str,
    req: Request,
) -> Response {
    let base_url = resolve_base_url(
        state.public_base_url.as_deref(),
        req.headers(),
        &state.base_url,
    );
    // An unknown identifier must 404 here too, not just on the image routes — otherwise every
    // string on earth advertises itself as a valid IIIF image service.
    if let ProjectionId::Named(named) = parse_identifier(proj) {
        return tile_error_response(TileError::UnknownProjection(named), request_id_of(&req))
            .into_response();
    }
    let engine = match state.registry.get(name).await {
        Ok(engine) => engine,
        Err(e) => return registry_error_response(state, e, request_id_of(&req)),
    };
    let id_base = format!("{base_url}{id_prefix}/iiif/{proj}");
    let info = engine.image_info(&id_base).to_info_json(2);
    let body = serde_json::to_vec(&info).expect("ImageInfo serializes to valid JSON");
    let etag = etag_for(&[&id_base]);

    if if_none_match_matches(&req, &etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::CACHE_CONTROL, INFO_CACHE_CONTROL),
                (header::ETAG, etag.as_str()),
            ],
        )
            .into_response();
    }

    (
        [
            (header::CONTENT_TYPE, info_content_type(&req)),
            (header::CACHE_CONTROL, INFO_CACHE_CONTROL),
            (header::ETAG, etag.as_str()),
        ],
        body,
    )
        .into_response()
}

/// The catalogue, for the viewer's picker.
///
/// Inside the auth gate, unlike the viewer's static assets. Those are inert; this enumerates what
/// exists, so an unauthenticated caller learning the whole catalogue from it would be a leak. The
/// viewer handles the resulting 401 by degrading to a name input, which is the same graceful path
/// it already takes for `/ziv/dimensions.json`.
///
/// `listable: false` is reported honestly rather than as an empty list. A lazy directory source
/// cannot enumerate without walking a filesystem it exists to avoid walking, and `[]` would make
/// the picker say "no images" when it means "type a name".
async fn images_handler(State(state): State<AppState>, req: Request) -> Response {
    let (listable, images) = match state.registry.list() {
        crate::registry::Listing::Enumerable(names) => (
            true,
            names
                .iter()
                .map(|name| {
                    serde_json::json!({
                        "name": name.as_str(),
                        "href": format!("/i/{name}/"),
                    })
                })
                .collect::<Vec<_>>(),
        ),
        crate::registry::Listing::NotListable => (false, Vec::new()),
    };
    let body = serde_json::json!({ "listable": listable, "images": images });
    let body = serde_json::to_vec(&body).expect("the catalogue serializes to valid JSON");

    // Same identity-based weak ETag scheme as the other metadata documents. The catalogue is fixed
    // for the life of the process in every source Plan A ships, so its identity is enough.
    let etag = etag_for(&["ziv-images", &state.base_url, &images.len().to_string()]);
    if if_none_match_matches(&req, &etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::CACHE_CONTROL, INFO_CACHE_CONTROL),
                (header::ETAG, etag.as_str()),
            ],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, INFO_CACHE_CONTROL),
            (header::ETAG, etag.as_str()),
        ],
        body,
    )
        .into_response()
}

/// Serves the t/z/c extents and channel catalogue the built-in viewer builds its controls from.
///
/// A ziv-specific endpoint rather than extra fields on `info.json`. `info.json` is a IIIF Image
/// API document describing a 2D image and ziv advertises `level2` conformance against the official
/// validator; hanging private properties off it would make a viewer feature able to break that
/// claim. This sits under `/ziv/` where nothing in the IIIF grammar can collide with it.
///
/// It lives inside the auth-gated sub-router, so when `ZIV_AUTH_*` is configured this is protected
/// exactly like tiles and `info.json` — it describes the image, and an unauthenticated caller
/// should learn no more from it than from any other image metadata.
async fn dimensions_handler(State(state): State<AppState>, req: Request) -> Response {
    let Some(name) = state.root_alias.clone() else {
        return not_found("this server has no image at the root", request_id_of(&req));
    };
    serve_dimensions(&state, &name, req).await
}

/// The t/z/c extents and channel catalogue for one image.
async fn serve_dimensions(
    state: &AppState,
    name: &crate::name::ImageName,
    req: Request,
) -> Response {
    let engine = match state.registry.get(name).await {
        Ok(engine) => engine,
        Err(e) => return registry_error_response(state, e, request_id_of(&req)),
    };
    let body = serde_json::to_vec(&engine.dimensions().to_json())
        .expect("ImageDimensions serializes to valid JSON");

    // Same identity-based weak ETag scheme as info.json: the response is a pure function of the
    // image the server was started against, which cannot change while the process is running.
    // The image is part of the ETag because two images on one server answer at two URLs with two
    // different documents, and a shared tag would let a conditional GET for one be satisfied by
    // the other.
    let etag = etag_for(&["ziv-dimensions", &state.base_url, name.as_str()]);
    if if_none_match_matches(&req, &etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::CACHE_CONTROL, INFO_CACHE_CONTROL),
                (header::ETAG, etag.as_str()),
            ],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, INFO_CACHE_CONTROL),
            (header::ETAG, etag.as_str()),
        ],
        body,
    )
        .into_response()
}

/// The `Content-Type` for an `info.json` response.
///
/// `jsonldMediaType` is REQUIRED at IIIF level 1 and above: when the client asks for
/// `application/ld+json`, the response must be labelled as such (with the Image API context as
/// the `profile` parameter); otherwise it must be `application/json`. Serving plain
/// `application/json` unconditionally — as ziv previously did — fails that requirement.
fn info_content_type(req: &Request) -> &'static str {
    const LD_JSON: &str =
        r#"application/ld+json;profile="http://iiif.io/api/image/3/context.json""#;
    let accepts_ld = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("application/ld+json"));
    if accepts_ld {
        LD_JSON
    } else {
        "application/json"
    }
}

/// `baseUriRedirect`, REQUIRED at IIIF level 1 and above: a request for the bare image identifier
/// must redirect to its `info.json` rather than 404.
async fn base_uri_redirect(
    State(state): State<AppState>,
    Path(proj): Path<String>,
    req: Request,
) -> Response {
    // Redirecting on a server with no root image would send the client to a 404 by way of a 303,
    // which is a worse answer than the 404 it is really owed.
    if state.root_alias.is_none() {
        return not_found("this server has no image at the root", request_id_of(&req));
    }
    let target = format!("{}/info.json", urlencode_segment(&proj));
    (StatusCode::SEE_OTHER, [(header::LOCATION, target.as_str())]).into_response()
}

/// The same redirect for a mounted image. Absolute rather than relative because the name may
/// contain slashes, so a relative `Location` would resolve against the wrong path segment.
fn mounted_base_uri_redirect(name: &crate::name::ImageName, proj: &str) -> Response {
    let target = format!("/i/{name}/iiif/{}/info.json", urlencode_segment(proj));
    (StatusCode::SEE_OTHER, [(header::LOCATION, target.as_str())]).into_response()
}

/// Every `/i/…` request.
///
/// One wildcard route because the image name is variable-length in the middle of the path, which
/// axum cannot express declaratively. See [`crate::mount`] for the split, and note that the name is
/// validated here rather than in the router: a malformed name is a 404, not a routing failure.
async fn mount_handler(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    req: Request,
) -> Response {
    let request_id = request_id_of(&req);
    let Some(mount) = crate::mount::split_mount(&rest) else {
        return not_found("no image in the request path", request_id);
    };
    let Ok(name) = crate::name::ImageName::parse(mount.name) else {
        return not_found("malformed image name", request_id);
    };
    let id_prefix = format!("/i/{name}");

    match mount.marker {
        crate::mount::Marker::Iiif => {
            let mut segments = mount.tail.split('/').filter(|s| !s.is_empty());
            let Some(proj) = segments.next().map(str::to_string) else {
                return not_found("no projection identifier", request_id);
            };
            let parts: Vec<String> = segments.map(str::to_string).collect();
            match parts.len() {
                0 => mounted_base_uri_redirect(&name, &proj),
                1 if parts[0] == "info.json" => {
                    serve_info(&state, &name, &proj, &id_prefix, req).await
                }
                4 => {
                    let [region, size, rotation, qdf] =
                        <[String; 4]>::try_from(parts).expect("length checked immediately above");
                    serve_tile(&state, &name, proj, region, size, rotation, qdf, req).await
                }
                _ => not_found("not a IIIF image request", request_id),
            }
        }
        crate::mount::Marker::Ziv => match mount.tail {
            "dimensions.json" => serve_dimensions(&state, &name, req).await,
            _ => not_found("no such ziv endpoint", request_id),
        },
        crate::mount::Marker::Viewer => crate::viewer::serve_asset(mount.tail),
    }
}

/// Percent-encodes the characters that would otherwise change how a path segment parses when it
/// is echoed back into a `Location` header. Deliberately minimal: identifiers here have already
/// been matched by the router as a single segment.
fn urlencode_segment(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' => "%2F".to_string(),
            '?' => "%3F".to_string(),
            '#' => "%23".to_string(),
            c => c.to_string(),
        })
        .collect()
}

async fn tile_handler(
    State(state): State<AppState>,
    Path((proj, region, size, rotation, quality_dot_format)): Path<(
        String,
        String,
        String,
        String,
        String,
    )>,
    req: Request,
) -> Response {
    let Some(name) = state.root_alias.clone() else {
        return not_found("this server has no image at the root", request_id_of(&req));
    };
    serve_tile(
        &state,
        &name,
        proj,
        region,
        size,
        rotation,
        quality_dot_format,
        req,
    )
    .await
}

/// One IIIF image request against one image.
#[allow(clippy::too_many_arguments)]
async fn serve_tile(
    state: &AppState,
    name: &crate::name::ImageName,
    proj: String,
    region: String,
    size: String,
    rotation: String,
    quality_dot_format: String,
    req: Request,
) -> Response {
    let request_id = request_id_of(&req);

    let parsed = match ImageRequest::parse(&region, &size, &rotation, &quality_dot_format) {
        Ok(r) => r,
        Err(e) => return ApiError::from(e).into_response(),
    };

    // Resolve the image BEFORE touching the tile cache, so an unknown name is a 404 that never
    // creates a cache entry, and so a 502 from a broken store is not cached as a tile.
    let engine = match state.registry.get(name).await {
        Ok(engine) => engine,
        Err(e) => return registry_error_response(state, e, request_id),
    };

    let key = TileKey {
        image: name.clone(),
        proj: proj.clone(),
        region,
        size,
        rotation,
        quality_dot_format,
    };
    let media_type = parsed.format.media_type();

    let render_semaphore = state.render_semaphore.clone();
    let id = parse_identifier(&proj);
    // Set to `true` only if this closure actually runs (i.e. this request is the one — of
    // possibly several coalesced concurrent callers — that pays for the render). `try_get_with`
    // does not otherwise expose hit/miss, so this flag is the hook `ziv_tile_cache_hits_total` /
    // `ziv_tile_cache_misses_total` below key off.
    let was_miss = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let was_miss_for_compute = was_miss.clone();
    let compute = async move {
        was_miss_for_compute.store(true, std::sync::atomic::Ordering::SeqCst);
        // THE key DoS bound: acquire a render permit BEFORE spawning the expensive blocking
        // compute, and only inside this cache-MISS closure — a cache HIT never reaches this code
        // at all (try_get_with returns the cached value directly), so hits stay cheap even when
        // every render permit is currently held by in-flight misses. This caps concurrent
        // expensive renders independent of both the HTTP concurrency limit (which bounds
        // in-flight REQUESTS, most of which may be cheap cache hits) and tokio's much larger
        // blocking-thread-pool size (which alone would let far too many simultaneous renders each
        // allocate their own decode/resample buffers and OOM the box).
        //
        // Acquired INLINE, in THIS future — not inside the spawned task below — so a request
        // abandoned (client disconnect, `TimeoutLayer`'s 30s firing) while still merely QUEUED
        // for a permit is cancelled the same way it always was: no permit taken, no render
        // started. That matters because the render is otherwise unstoppable (see below), and a
        // burst of abandoned requests must not each still queue and eventually run a full,
        // discarded render — that would both waste work and starve `drain_renders`, which shares
        // this same semaphore, behind a backlog of renders nobody is waiting on.
        let permit = render_semaphore
            .acquire_owned()
            .await
            .map_err(|_| TileError::Encode("render semaphore closed unexpectedly".to_string()))?;
        let spec = tiling::RenderSpec {
            region: parsed.region,
            size: parsed.size,
            rotation: parsed.rotation,
            quality: parsed.quality,
            format: parsed.format,
            jpeg_quality: 85,
        };
        // From here on the permit is ALREADY held — there is no further `.await` before it moves
        // into the spawned task, so it is never in a state where dropping THIS future (`compute`)
        // could release it while the blocking render is running. `moka::try_get_with`'s "several
        // callers coalesce onto one computation" means this future CAN be dropped out from under
        // the render (the whole point of coalescing is that any one caller can go away), and that
        // is also exactly what `tower_http::timeout::TimeoutLayer`'s 30s `DEFAULT_REQUEST_TIMEOUT`
        // does once it fires. The permit itself, held by the SPAWNED task below rather than by
        // this future, is unaffected either way: a `tokio::spawn`ed task keeps running to
        // completion once started even if every `JoinHandle` awaiting it (including this one) is
        // dropped, unlike an inline future, which simply stops being polled — and so never
        // finishes acquiring/holding/releasing the permit correctly — the moment its owner is
        // dropped. So the permit is held for the render's ACTUAL lifetime (until the
        // `spawn_blocking` closure, which cannot itself be cancelled, provably returns),
        // independent of whether this particular caller is still around to see the result. This
        // is the bug behind the shutdown panic a one-hour remote soak found (`docs/operations.md`):
        // `server::drain_renders` trusts the semaphore to mean "no render still running", which
        // is only true if a permit's release is tied to the render's real completion rather than
        // to whichever caller happened to still be watching.
        let render_task = tokio::spawn(async move {
            let _permit = permit;
            match tokio::task::spawn_blocking(move || engine.render(&id, &spec)).await {
                Ok(Ok(jpeg)) => Ok(Arc::new(jpeg)),
                Ok(Err(e)) => Err(e),
                Err(join_err) => Err(TileError::Encode(format!("task failed: {join_err}"))),
            }
        });
        match render_task.await {
            Ok(result) => result,
            Err(join_err) => Err(TileError::Encode(format!("render task failed: {join_err}"))),
        }
    };

    // `try_get_with` coalesces concurrent misses for the same key into a single execution of
    // `compute` (later callers await the same in-flight future instead of recomputing), and
    // caches ONLY the `Ok` result — an `Err` returned from the closure is propagated to every
    // waiter but never stored, so a transient failure never poisons the cache for subsequent
    // (potentially successful) requests.
    let cache_key_for_etag = format!(
        "{}/{}/{}/{}/{}/{}",
        key.image, key.proj, key.region, key.size, key.rotation, key.quality_dot_format
    );
    match state.tile_cache.try_get_with(key, compute).await {
        Ok(jpeg) => {
            if was_miss.load(std::sync::atomic::Ordering::SeqCst) {
                metrics::counter!("ziv_tile_cache_misses_total").increment(1);
            } else {
                metrics::counter!("ziv_tile_cache_hits_total").increment(1);
            }
            let etag = etag_for(&[&cache_key_for_etag]);
            if if_none_match_matches(&req, &etag) {
                return (
                    StatusCode::NOT_MODIFIED,
                    [
                        (header::CACHE_CONTROL, TILE_CACHE_CONTROL),
                        (header::ETAG, etag.as_str()),
                    ],
                )
                    .into_response();
            }
            (
                [
                    (header::CONTENT_TYPE, media_type),
                    (header::CACHE_CONTROL, TILE_CACHE_CONTROL),
                    (header::ETAG, etag.as_str()),
                ],
                (*jpeg).clone(),
            )
                .into_response()
        }
        Err(shared_err) => {
            metrics::counter!("ziv_tile_cache_misses_total").increment(1);
            let err = Arc::try_unwrap(shared_err).unwrap_or_else(|arc| match &*arc {
                TileError::OutOfRange(m) => TileError::OutOfRange(m.clone()),
                TileError::UnknownProjection(m) => TileError::UnknownProjection(m.clone()),
                TileError::UnknownLabel(m) => TileError::UnknownLabel(m.clone()),
                TileError::Encode(m) => TileError::Encode(m.clone()),
                TileError::Iiif(m) => TileError::Iiif(m.clone()),
                TileError::Zarr(e) => TileError::Encode(e.to_string()),
            });
            tile_error_response(err, request_id).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    fn state() -> AppState {
        let img = zarr_core::ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
        AppState::new(
            Arc::new(tiling::ZarrTileEngine::new(img)),
            "http://test".into(),
        )
    }

    /// A `TileEngine` that counts every `tile()` invocation and returns a fixed JPEG, so tests
    /// can assert exactly how many times the (expensive) render path actually ran regardless of
    /// how many HTTP requests were fired at it.
    struct CountingEngine {
        calls: AtomicUsize,
        sleep_millis: u64,
    }

    impl CountingEngine {
        fn new() -> Self {
            CountingEngine {
                calls: AtomicUsize::new(0),
                sleep_millis: 0,
            }
        }

        fn with_sleep(sleep_millis: u64) -> Self {
            CountingEngine {
                calls: AtomicUsize::new(0),
                sleep_millis,
            }
        }
    }

    impl TileEngine for CountingEngine {
        fn image_info(&self, id_base: &str) -> iiif::ImageInfo {
            iiif::ImageInfo {
                id: id_base.to_string(),
                width: 64,
                height: 64,
                tile_size: 512,
                scale_factors: vec![1],
                sizes: vec![(64, 64)],
            }
        }

        fn dimensions(&self) -> tiling::ImageDimensions {
            tiling::ImageDimensions {
                size_t: 1,
                size_z: 1,
                size_c: 1,
                default_t: 0,
                default_z: 0,
                channels: vec![],
                labels: vec![],
                label_open_failures: vec![],
            }
        }

        fn render(
            &self,
            _id: &iiif::ProjectionId,
            _spec: &tiling::RenderSpec,
        ) -> Result<Vec<u8>, TileError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.sleep_millis > 0 {
                std::thread::sleep(std::time::Duration::from_millis(self.sleep_millis));
            }
            Ok(vec![0xFF, 0xD8, 0xFF, 0xD9]) // minimal SOI/EOI JPEG bytes
        }
    }

    /// A `TileEngine` that sleeps inside `tile()` and tracks how many calls were EVER
    /// concurrently in flight at once (the high-water mark), so a test can prove the render
    /// semaphore actually bounds concurrent renders rather than merely existing unused.
    struct MaxConcurrentEngine {
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        calls: AtomicUsize,
        sleep_millis: u64,
    }

    impl MaxConcurrentEngine {
        fn new(sleep_millis: u64) -> Self {
            MaxConcurrentEngine {
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                sleep_millis,
            }
        }
    }

    impl TileEngine for MaxConcurrentEngine {
        fn image_info(&self, id_base: &str) -> iiif::ImageInfo {
            iiif::ImageInfo {
                id: id_base.to_string(),
                width: 64,
                height: 64,
                tile_size: 512,
                scale_factors: vec![1],
                sizes: vec![(64, 64)],
            }
        }

        fn dimensions(&self) -> tiling::ImageDimensions {
            tiling::ImageDimensions {
                size_t: 1,
                size_z: 1,
                size_c: 1,
                default_t: 0,
                default_z: 0,
                channels: vec![],
                labels: vec![],
                label_open_failures: vec![],
            }
        }

        fn render(
            &self,
            _id: &iiif::ProjectionId,
            _spec: &tiling::RenderSpec,
        ) -> Result<Vec<u8>, TileError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let now_in_flight = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight
                .fetch_max(now_in_flight, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(self.sleep_millis));
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(vec![0xFF, 0xD8, 0xFF, 0xD9])
        }
    }

    /// A `TileEngine` whose `tile()` blocks on an explicit gate rather than a fixed sleep, so a
    /// test can deterministically synchronize with "the render has started" and "the render may
    /// finish" without racing wall-clock sleeps against scheduler/CPU load. `start_tx` sends once
    /// `tile()` begins (proving the render is genuinely in flight / holding a permit); `tile()`
    /// then blocks on `proceed_rx` until the test explicitly releases it.
    struct GatedEngine {
        calls: AtomicUsize,
        start_tx: std::sync::mpsc::SyncSender<()>,
        proceed_rx: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl GatedEngine {
        /// Returns `(engine, started_rx, proceed_tx)`: await `started_rx.recv()` to know the
        /// render has begun; send on `proceed_tx` to let it complete.
        fn new() -> (
            Self,
            std::sync::mpsc::Receiver<()>,
            std::sync::mpsc::SyncSender<()>,
        ) {
            let (start_tx, start_rx) = std::sync::mpsc::sync_channel(0);
            let (proceed_tx, proceed_rx) = std::sync::mpsc::sync_channel(0);
            (
                GatedEngine {
                    calls: AtomicUsize::new(0),
                    start_tx,
                    proceed_rx: std::sync::Mutex::new(proceed_rx),
                },
                start_rx,
                proceed_tx,
            )
        }
    }

    impl TileEngine for GatedEngine {
        fn image_info(&self, id_base: &str) -> iiif::ImageInfo {
            iiif::ImageInfo {
                id: id_base.to_string(),
                width: 64,
                height: 64,
                tile_size: 512,
                scale_factors: vec![1],
                sizes: vec![(64, 64)],
            }
        }

        fn dimensions(&self) -> tiling::ImageDimensions {
            tiling::ImageDimensions {
                size_t: 1,
                size_z: 1,
                size_c: 1,
                default_t: 0,
                default_z: 0,
                channels: vec![],
                labels: vec![],
                label_open_failures: vec![],
            }
        }

        fn render(
            &self,
            _id: &iiif::ProjectionId,
            _spec: &tiling::RenderSpec,
        ) -> Result<Vec<u8>, TileError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _ = self.start_tx.send(());
            let _ = self
                .proceed_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv();
            Ok(vec![0xFF, 0xD8, 0xFF, 0xD9])
        }
    }

    /// A `TileEngine` whose `tile()` always fails, to exercise the 500-generic-body /
    /// error-not-cached paths.
    struct FailingEngine {
        calls: AtomicUsize,
    }

    impl TileEngine for FailingEngine {
        fn image_info(&self, id_base: &str) -> iiif::ImageInfo {
            iiif::ImageInfo {
                id: id_base.to_string(),
                width: 64,
                height: 64,
                tile_size: 512,
                scale_factors: vec![1],
                sizes: vec![(64, 64)],
            }
        }

        fn dimensions(&self) -> tiling::ImageDimensions {
            tiling::ImageDimensions {
                size_t: 1,
                size_z: 1,
                size_c: 1,
                default_t: 0,
                default_z: 0,
                channels: vec![],
                labels: vec![],
                label_open_failures: vec![],
            }
        }

        fn render(
            &self,
            _id: &iiif::ProjectionId,
            _spec: &tiling::RenderSpec,
        ) -> Result<Vec<u8>, TileError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(TileError::Encode("boom: internal detail".to_string()))
        }
    }

    #[tokio::test]
    async fn info_json_route_returns_level2() {
        let app = router(state());
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/info.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["profile"], "level2");
        assert_eq!(v["width"], 64);
    }

    #[tokio::test]
    async fn tile_route_returns_jpeg() {
        let app = router(state());
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let ct = res.headers().get(header::CONTENT_TYPE).unwrap();
        assert_eq!(ct, "image/jpeg");
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[0..2], &[0xFF, 0xD8]);
    }

    #[tokio::test]
    async fn bad_rotation_is_400() {
        let app = router(state());
        // 45 is `rotationArbitrary`, an OPTIONAL feature ziv does not implement. (90/180/270 are
        // `rotationBy90s`, required at level 2, and are served.)
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/45/default.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// End-to-end check of the `OutOfRange` -> 400 mapping: a client requesting an absurdly
    /// large output size must get a 4xx (their fault), never a 500 or a panic, even though the
    /// failure is only detected deep inside `engine.tile` (via `plan()`'s output-size guard).
    #[tokio::test]
    async fn oversized_size_is_400_not_500() {
        let app = router(state());
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/999999999999,/0/default.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// Structured error bodies: a bad-request (bad rotation) response must be JSON containing an
    /// "error" field.
    #[tokio::test]
    async fn bad_request_has_json_error_body() {
        let app = router(state());
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/45/default.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["error"].is_string());
    }

    /// Unknown route: axum's own routing 404, not ours to construct, but must not become a 500.
    #[tokio::test]
    async fn unknown_route_is_404() {
        let app = router(state());
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    /// A forced internal error (a fake engine that always returns `TileError::Encode`) must map
    /// to a 500 with a GENERIC body that does NOT leak the internal error string, but DOES
    /// include a request_id for operator correlation.
    #[tokio::test]
    async fn internal_error_is_500_generic_with_request_id() {
        let state = AppState::new(
            Arc::new(FailingEngine {
                calls: AtomicUsize::new(0),
            }),
            "http://test".into(),
        );
        let app = router(state);
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    .header("x-request-id", "test-req-id-123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8_lossy(&bytes);
        assert!(!body_str.contains("boom"));
        assert!(!body_str.contains("internal detail"));
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["request_id"], "test-req-id-123");
    }

    // --- Cache behavior ---

    /// Two identical requests must invoke the underlying engine's `tile()` exactly ONCE — the
    /// second request is served from the cache.
    #[tokio::test]
    async fn identical_requests_compute_once() {
        let counting = Arc::new(CountingEngine::new());
        let state = AppState::new(counting.clone(), "http://test".into());
        let app = router(state);

        for _ in 0..2 {
            let res = app
                .clone()
                .oneshot(
                    HttpRequest::builder()
                        .uri("/iiif/default/full/max/0/default.jpg")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        }

        assert_eq!(counting.calls.load(Ordering::SeqCst), 1);
    }

    /// N concurrent identical (cache-miss) requests must coalesce into a single compute: fire
    /// them all at once against an engine that sleeps briefly before incrementing its counter,
    /// so a naive (non-coalescing) implementation would very likely run the compute more than
    /// once.
    #[tokio::test]
    async fn concurrent_identical_misses_coalesce() {
        let counting = Arc::new(CountingEngine::with_sleep(50));
        let state = AppState::new(counting.clone(), "http://test".into());
        let app = router(state);

        let mut handles = Vec::new();
        for _ in 0..8 {
            let app = app.clone();
            handles.push(tokio::spawn(async move {
                app.oneshot(
                    HttpRequest::builder()
                        .uri("/iiif/default/full/max/0/default.jpg")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            }));
        }
        for h in handles {
            let res = h.await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        }

        assert_eq!(counting.calls.load(Ordering::SeqCst), 1);
    }

    /// Semantically-equal requests (same path segments) share a cache key -> one compute;
    /// genuinely different requests (different size) get distinct keys -> two computes.
    #[tokio::test]
    async fn distinct_requests_do_not_collide_equal_requests_share() {
        let counting = Arc::new(CountingEngine::new());
        let state = AppState::new(counting.clone(), "http://test".into());
        let app = router(state);

        // Two requests for the SAME size: should share one compute.
        for _ in 0..2 {
            app.clone()
                .oneshot(
                    HttpRequest::builder()
                        .uri("/iiif/default/full/32,32/0/default.jpg")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        }
        assert_eq!(counting.calls.load(Ordering::SeqCst), 1);

        // A genuinely different size: must trigger a second, distinct compute.
        app.clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/16,16/0/default.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(counting.calls.load(Ordering::SeqCst), 2);
    }

    /// A `TileError` from the engine must NOT be cached: a request that fails once, then
    /// succeeds (simulated via two different engines targeting the same key indirectly isn't
    /// possible with a fixed engine, so instead: assert repeated failures each re-invoke the
    /// engine, proving the error was never memoized as a cache hit).
    #[tokio::test]
    async fn errors_are_not_cached() {
        let failing = Arc::new(FailingEngine {
            calls: AtomicUsize::new(0),
        });
        let state = AppState::new(failing.clone(), "http://test".into());
        let app = router(state);

        for _ in 0..3 {
            let res = app
                .clone()
                .oneshot(
                    HttpRequest::builder()
                        .uri("/iiif/default/full/max/0/default.jpg")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        }

        // Every request re-ran the engine (none were served from a poisoned cache entry).
        assert_eq!(failing.calls.load(Ordering::SeqCst), 3);
    }

    /// Byte-bound eviction: with a tiny cache budget, inserting enough distinct tiles must
    /// evict earlier entries rather than growing unbounded. `moka`'s eviction is not
    /// synchronous with insertion, so we `run_pending_tasks` (via `cache_weighted_size`) to
    /// force it to settle before asserting the weighted size stayed within budget.
    #[tokio::test]
    async fn byte_bound_eviction_keeps_memory_bounded() {
        let counting = Arc::new(CountingEngine::new());
        // Fixed JPEG the fake engine returns is 4 bytes; budget for ~3 entries' worth.
        let max_bytes = 12;
        let state = AppState::with_cache_bytes(counting.clone(), "http://test".into(), max_bytes);
        let app = router(state.clone());

        // Request 20 DISTINCT sizes (distinct cache keys) so entries actually accumulate.
        for i in 1..=20u32 {
            let uri = format!("/iiif/default/full/{i},{i}/0/default.jpg");
            let res = app
                .clone()
                .oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        }

        // All 20 distinct requests must have actually computed (proves keys didn't collide).
        assert_eq!(counting.calls.load(Ordering::SeqCst), 20);

        // But the cache itself must have evicted down to (approximately) the configured budget
        // rather than holding all 20 entries' worth of weight (80 bytes > 12-byte budget).
        let weighted_size = state.cache_weighted_size().await;
        assert!(
            weighted_size <= max_bytes,
            "cache grew past its byte budget: {weighted_size} > {max_bytes}"
        );
    }

    // --- Auth ---

    fn bearer_state(token: &str) -> AppState {
        state().with_auth(AuthConfig {
            bearer: Some(token.to_string()),
            hmac_secret: None,
        })
    }

    fn hmac_state(secret: &str) -> AppState {
        state().with_auth(AuthConfig {
            bearer: None,
            hmac_secret: Some(secret.to_string()),
        })
    }

    fn far_future_exp() -> u64 {
        // Comfortably beyond any conceivable test run without touching a real clock library.
        9_999_999_999
    }

    #[tokio::test]
    async fn no_auth_configured_requests_succeed_as_before() {
        // Auth-off default: existing (no-auth) AppState must behave exactly as before this
        // module existed.
        let app = router(state());
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn bearer_valid_token_is_200() {
        let app = router(bearer_state("correct-token"));
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    .header(header::AUTHORIZATION, "Bearer correct-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn bearer_wrong_token_is_401_with_www_authenticate() {
        let app = router(bearer_state("correct-token"));
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    .header(header::AUTHORIZATION, "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            res.headers().get(header::WWW_AUTHENTICATE).unwrap(),
            "Bearer"
        );
    }

    #[tokio::test]
    async fn bearer_missing_is_401() {
        let app = router(bearer_state("correct-token"));
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            res.headers().get(header::WWW_AUTHENTICATE).unwrap(),
            "Bearer"
        );
    }

    /// Bearer-only auth (no HMAC configured) must still succeed even if the request happens to
    /// carry an unparseable `exp` query param -- proves the `Query<AuthQuery>` extraction (which
    /// runs unconditionally before the bearer/HMAC branches) tolerates garbage query params
    /// rather than short-circuiting the whole middleware with an extractor-level 400 before the
    /// valid bearer token is ever checked.
    #[tokio::test]
    async fn bearer_only_succeeds_with_garbage_query_params() {
        let app = router(bearer_state("correct-token"));
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg?exp=not-a-number&sig=garbage")
                    .header(header::AUTHORIZATION, "Bearer correct-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn hmac_valid_signature_within_exp_is_200() {
        let secret = "s3cr3t";
        let exp = far_future_exp();
        let sig = crate::auth::sign(secret, ROOT_ALIAS_NAME, "default", "full", "max", exp);
        let uri = format!("/iiif/default/full/max/0/default.jpg?exp={exp}&sig={sig}");
        let app = router(hmac_state(secret));
        let res = app
            .oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn hmac_expired_is_401() {
        let secret = "s3cr3t";
        let exp = 1; // 1970-01-01T00:00:01Z, long expired.
        let sig = crate::auth::sign(secret, ROOT_ALIAS_NAME, "default", "full", "max", exp);
        let uri = format!("/iiif/default/full/max/0/default.jpg?exp={exp}&sig={sig}");
        let app = router(hmac_state(secret));
        let res = app
            .oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn hmac_tampered_signature_is_401() {
        let secret = "s3cr3t";
        let exp = far_future_exp();
        let sig = crate::auth::sign(secret, ROOT_ALIAS_NAME, "default", "full", "max", exp);
        let mut tampered = sig.clone();
        // Flip the last base64url character so the decoded bytes differ from the real signature.
        tampered.pop();
        tampered.push(if sig.ends_with('A') { 'B' } else { 'A' });
        let uri = format!("/iiif/default/full/max/0/default.jpg?exp={exp}&sig={tampered}");
        let app = router(hmac_state(secret));
        let res = app
            .oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// The signature must bind the REGION, not just the URL path shape: a signature minted for
    /// `full` must be rejected when replayed against a different region (`0,0,10,10`), proving
    /// the message includes the region rather than e.g. only the proj+exp.
    #[tokio::test]
    async fn hmac_tampered_region_is_401() {
        let secret = "s3cr3t";
        let exp = far_future_exp();
        let sig = crate::auth::sign(secret, ROOT_ALIAS_NAME, "default", "full", "max", exp);
        let uri = format!("/iiif/default/0,0,10,10/max/0/default.jpg?exp={exp}&sig={sig}");
        let app = router(hmac_state(secret));
        let res = app
            .oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn hmac_missing_exp_is_401() {
        let secret = "s3cr3t";
        let sig = crate::auth::sign(
            secret,
            ROOT_ALIAS_NAME,
            "default",
            "full",
            "max",
            far_future_exp(),
        );
        let uri = format!("/iiif/default/full/max/0/default.jpg?sig={sig}");
        let app = router(hmac_state(secret));
        let res = app
            .oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// Both bearer AND HMAC configured: EITHER a valid bearer OR a valid signature grants
    /// access (documented OR policy, not AND).
    #[tokio::test]
    async fn both_configured_valid_bearer_alone_grants_access() {
        let secret = "s3cr3t";
        let state = state().with_auth(AuthConfig {
            bearer: Some("correct-token".to_string()),
            hmac_secret: Some(secret.to_string()),
        });
        let app = router(state);
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    .header(header::AUTHORIZATION, "Bearer correct-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn both_configured_valid_signature_alone_grants_access() {
        let secret = "s3cr3t";
        let exp = far_future_exp();
        let sig = crate::auth::sign(secret, ROOT_ALIAS_NAME, "default", "full", "max", exp);
        let state = state().with_auth(AuthConfig {
            bearer: Some("correct-token".to_string()),
            hmac_secret: Some(secret.to_string()),
        });
        let app = router(state);
        let uri = format!("/iiif/default/full/max/0/default.jpg?exp={exp}&sig={sig}");
        let res = app
            .oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn both_configured_neither_valid_is_401() {
        let secret = "s3cr3t";
        let state = state().with_auth(AuthConfig {
            bearer: Some("correct-token".to_string()),
            hmac_secret: Some(secret.to_string()),
        });
        let app = router(state);
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// THE ORDERING TEST: pre-seed the cache with a tile entry for a key, then make a request
    /// for that EXACT key that fails auth. It must get 401, not the cached bytes -- this proves
    /// the auth middleware (a `Layer` around the sub-router) runs BEFORE `tile_handler`'s cache
    /// lookup, so a cache hit can never bypass authz. If auth ran after (or the cache were
    /// checked first), this request would get 200 with the seeded bytes instead.
    #[tokio::test]
    async fn ordering_cache_hit_does_not_bypass_auth() {
        let state = bearer_state("correct-token");
        let seeded_bytes = vec![0xAA, 0xBB, 0xCC, 0xDD];
        state
            .seed_cache(
                &crate::name::ImageName::parse(ROOT_ALIAS_NAME).unwrap(),
                "default",
                "full",
                "max",
                "0",
                "default.jpg",
                seeded_bytes.clone(),
            )
            .await;

        let app = router(state);
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    // No Authorization header at all -> fails auth.
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "a pre-seeded cache hit must not be served to a caller that fails auth"
        );
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_ne!(
            bytes.as_ref(),
            seeded_bytes.as_slice(),
            "response body must not be the cached tile bytes"
        );
    }

    /// The viewer stays unauthed even when auth is enabled: `/viewer/*` lives in a wholly
    /// separate router built in `lib::app`, never passing through this module's auth-gated
    /// sub-router.
    #[tokio::test]
    async fn viewer_stays_unauthed_when_auth_enabled() {
        let state = bearer_state("correct-token");
        let app = crate::app(state);
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/viewer/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// info.json (no region/size to sign) must still be gated by bearer when configured.
    #[tokio::test]
    async fn info_json_route_is_gated_by_bearer() {
        let app = router(bearer_state("correct-token"));
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/info.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let app = router(bearer_state("correct-token"));
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/info.json")
                    .header(header::AUTHORIZATION, "Bearer correct-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    // --- HTTP caching headers ---

    #[tokio::test]
    async fn tile_response_has_cache_control_and_etag() {
        let app = router(state());
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().contains_key(header::CACHE_CONTROL));
        assert!(res.headers().contains_key(header::ETAG));
    }

    #[tokio::test]
    async fn tile_conditional_get_with_matching_etag_is_304() {
        let app = router(state());
        let first = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let etag = first
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let second = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    .header(header::IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        let bytes = axum::body::to_bytes(second.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.is_empty(), "304 must have no body");
    }

    #[tokio::test]
    async fn info_json_has_cache_control_and_etag() {
        let app = router(state());
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/info.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().contains_key(header::CACHE_CONTROL));
        assert!(res.headers().contains_key(header::ETAG));
    }

    #[tokio::test]
    async fn info_json_conditional_get_with_matching_etag_is_304() {
        let app = router(state());
        let first = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/info.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let etag = first
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let second = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/info.json")
                    .header(header::IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
    }

    // --- Render semaphore (bounded blocking-render concurrency) ---

    /// With a render semaphore capped at 2, firing 8 concurrent cache-MISS requests (all
    /// distinct keys, so none can be served from the cache) must never let more than 2 renders
    /// run at the same time — proven via `MaxConcurrentEngine`'s high-water mark, not merely by
    /// asserting the total call count.
    #[tokio::test]
    async fn render_semaphore_bounds_concurrent_renders() {
        let engine = Arc::new(MaxConcurrentEngine::new(30));
        let state = AppState::new(engine.clone(), "http://test".into()).with_render_permits(2);
        let app = router(state);

        let mut handles = Vec::new();
        for i in 1..=8u32 {
            let app = app.clone();
            let uri = format!("/iiif/default/full/{i},{i}/0/default.jpg");
            handles.push(tokio::spawn(async move {
                app.oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
            }));
        }
        for h in handles {
            let res = h.await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        }

        assert_eq!(engine.calls.load(Ordering::SeqCst), 8);
        let max_in_flight = engine.max_in_flight.load(Ordering::SeqCst);
        assert!(
            max_in_flight <= 2,
            "render semaphore should have capped concurrent renders at 2, saw {max_in_flight}"
        );
    }

    /// A request cancelled while still QUEUED for a render permit (never having acquired one)
    /// must never reach `render()`, and must leave the semaphore exactly as it was — proving the
    /// permit `compute` acquires is taken INLINE, in the request's own future, rather than inside
    /// the detached task the render itself runs in (see that call site's doc comment). If the
    /// acquire happened inside the detached task instead, this cancellation would be a no-op from
    /// the caller's perspective — the task would already be spawned and would eventually run a
    /// full render for a caller that is no longer there to receive it, which is both wasted work
    /// and (under load, with many such abandoned requests queued single-file behind a small
    /// number of permits) a way to starve `drain_renders`, which waits on this same semaphore.
    #[tokio::test]
    async fn a_request_cancelled_while_queued_for_a_permit_never_renders() {
        let engine = Arc::new(MaxConcurrentEngine::new(0));
        let state = AppState::new(engine.clone(), "http://test".into()).with_render_permits(1);
        // Hold the one permit externally so any new request is stuck waiting to acquire one.
        let held = state.render_semaphore.clone().try_acquire_owned().unwrap();
        let app = router(state);

        let uri = "/iiif/default/full/99,99/0/default.jpg";
        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            app.clone()
                .oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap()),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "expected the request to still be waiting on the held permit, not answered"
        );
        assert_eq!(
            engine.calls.load(Ordering::SeqCst),
            0,
            "a request cancelled while only queued for a permit must never call render()"
        );

        // Releasing the externally-held permit must free it for a brand new request — proving
        // the cancelled one left the semaphore neither over- nor under-counted.
        drop(held);
        let res = app
            .oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
    }

    /// A cache HIT must NOT need to acquire a render permit: with every permit already held by
    /// an in-flight render, a request for an ALREADY-CACHED tile must still complete promptly
    /// rather than blocking behind the semaphore. Uses `GatedEngine` to deterministically
    /// synchronize "the background render has started" (so the single permit is provably held)
    /// before asserting on the hit, rather than racing a fixed sleep against system load.
    ///
    /// Needs a MULTI-THREAD runtime: this test blocks its own task on `started_rx.recv()` (a
    /// plain `std::sync::mpsc` receive) while `warm`/`bg` are separately `tokio::spawn`ed tasks
    /// that must keep making progress concurrently — on a single-thread runtime that blocking
    /// `recv()` would starve the scheduler and deadlock (the spawned task could never be polled
    /// to the point of sending on `start_tx` in the first place).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cache_hit_does_not_consume_render_permit() {
        let (gated, started_rx, proceed_tx) = GatedEngine::new();
        let engine = Arc::new(gated);
        let state = AppState::new(engine.clone(), "http://test".into()).with_render_permits(1);
        let app = router(state);

        // Warm the cache for size=10,10: it will call into `tile()`, which blocks on the gate.
        // Release it immediately (nothing else is contending for the permit yet) and let the
        // warm request finish normally.
        let app_warm = app.clone();
        let warm = tokio::spawn(async move {
            app_warm
                .oneshot(
                    HttpRequest::builder()
                        .uri("/iiif/default/full/10,10/0/default.jpg")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        });
        started_rx.recv().expect("warm render should start");
        proceed_tx.send(()).expect("release the warm render");
        let warm = warm.await.unwrap();
        assert_eq!(warm.status(), StatusCode::OK);
        assert_eq!(engine.calls.load(Ordering::SeqCst), 1);

        // Occupy the single permit with a slow, DISTINCT (uncached) render running in the
        // background, and wait (via the gate, not a sleep) until it has DEFINITELY started —
        // i.e. is provably holding the only render permit.
        let app_bg = app.clone();
        let bg = tokio::spawn(async move {
            app_bg
                .oneshot(
                    HttpRequest::builder()
                        .uri("/iiif/default/full/20,20/0/default.jpg")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        });
        started_rx
            .recv()
            .expect("background render should start and hold the only permit");

        // The cache HIT for size=10,10 must complete quickly, without waiting on the permit
        // held by the background render (which is currently blocked on the gate and will not
        // release it until this test explicitly says so, below) -- a bounded timeout here proves
        // the hit did NOT queue behind the semaphore.
        let hit = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            app.oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/10,10/0/default.jpg")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .expect("cache hit must not block behind the render semaphore")
        .unwrap();
        assert_eq!(hit.status(), StatusCode::OK);
        // Exactly 2 total calls so far: #1 was the warm render (10,10), #2 was the background
        // render (20,20) starting -- `calls` increments the INSTANT `tile()` begins, before it
        // blocks on the gate, so this count already includes the background render regardless of
        // the hit. The property under test is that the hit did NOT trigger a 3RD call.
        assert_eq!(
            engine.calls.load(Ordering::SeqCst),
            2,
            "the cache hit must not have triggered a render (no 3rd call)"
        );

        // Now release the background render and let it finish.
        proceed_tx.send(()).expect("release the background render");
        let bg = bg.await.unwrap();
        assert_eq!(bg.status(), StatusCode::OK);
        assert_eq!(engine.calls.load(Ordering::SeqCst), 2);
    }

    // --- Forwarded-origin ---

    #[tokio::test]
    async fn info_json_id_uses_bind_address_by_default() {
        let app = router(state());
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/info.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["id"], "http://test/iiif/default");
    }

    #[tokio::test]
    async fn info_json_id_honors_forwarded_headers() {
        let app = router(state());
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/info.json")
                    .header("x-forwarded-proto", "https")
                    .header("x-forwarded-host", "example.org")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["id"], "https://example.org/iiif/default");
    }

    #[tokio::test]
    async fn info_json_id_explicit_public_base_url_wins_over_forwarded_headers() {
        let state = state().with_public_base_url("https://configured.example.net");
        let app = router(state);
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/info.json")
                    .header("x-forwarded-proto", "https")
                    .header("x-forwarded-host", "example.org")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["id"], "https://configured.example.net/iiif/default");
    }

    /// The compatibility constructor must produce a registry with exactly one image, so the
    /// single-image root alias engages and today's URLs keep working.
    #[tokio::test]
    async fn a_single_engine_state_exposes_a_root_alias() {
        let img = zarr_core::ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
        let state = AppState::new(
            Arc::new(tiling::ZarrTileEngine::new(img)),
            "http://test".into(),
        );
        assert!(state.root_alias.is_some());
        let name = state.root_alias.clone().unwrap();
        assert!(state.registry.get(&name).await.is_ok());
    }

    /// A registry with two images has no sole image, so no root alias: there would be no honest
    /// answer to "the image at the root".
    #[tokio::test]
    async fn a_two_image_state_has_no_root_alias() {
        use crate::name::ImageName;
        use crate::registry::{ExplicitSource, ImageRegistry};
        let registry = ImageRegistry::builder()
            .source(Box::new(
                ExplicitSource::from_pairs(vec![
                    (
                        ImageName::parse("a").unwrap(),
                        "../../tests/fixtures/sample_v04.ome.zarr".to_string(),
                    ),
                    (
                        ImageName::parse("b").unwrap(),
                        "../../tests/fixtures/sample_labels.ome.zarr".to_string(),
                    ),
                ])
                .unwrap(),
            ))
            .build();
        let state = AppState::from_registry(Arc::new(registry), "http://test".into());
        assert!(state.root_alias.is_none());
    }

    /// Two images must not share a tile-cache entry. Seeded directly so the test proves the KEY
    /// separates them rather than proving two renders happen to differ.
    #[tokio::test]
    async fn the_tile_cache_key_separates_images() {
        use crate::name::ImageName;
        let img = zarr_core::ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
        let state = AppState::new(
            Arc::new(tiling::ZarrTileEngine::new(img)),
            "http://test".into(),
        );
        let a = ImageName::parse("a").unwrap();
        let b = ImageName::parse("b").unwrap();
        state
            .seed_cache(
                &a,
                "default",
                "full",
                "max",
                "0",
                "default.jpg",
                vec![1, 2, 3],
            )
            .await;
        assert_eq!(
            state
                .peek_cache(&a, "default", "full", "max", "0", "default.jpg")
                .await
                .as_deref()
                .map(Vec::as_slice),
            Some(&[1u8, 2, 3][..])
        );
        assert!(
            state
                .peek_cache(&b, "default", "full", "max", "0", "default.jpg")
                .await
                .is_none(),
            "image b must not read image a's cached tile"
        );
    }
}
