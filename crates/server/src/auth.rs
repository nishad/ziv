//! Optional auth for the info.json + tile routes: a static bearer token and/or HMAC-signed URLs.
//!
//! Auth is OFF by default (`AuthConfig::from_env()` returns `None` when neither env var is set) —
//! existing no-auth deployments/tests are unaffected. When enabled, this module's
//! [`auth_middleware`] is applied ONLY to the sub-router carrying `/iiif/{proj}/info.json` and
//! the tile route (see `routes::router`); `/viewer/*` and any future `/healthz` are built as a
//! SEPARATE unauthed router merged alongside it, so static assets are never gated.
//!
//! CRITICAL ordering: this middleware is a `tower`/`axum` `Layer` wrapped around the routes, which
//! places it OUTSIDE (i.e. runs BEFORE) the tile cache lookup that happens inside `tile_handler`'s
//! body. A cache hit can therefore never bypass authz — the request is rejected before the handler
//! (and so the cache) is ever reached. See the `ordering_cache_hit_does_not_bypass_auth` test.
//!
//! ## Secrets: environment, never argv
//!
//! The bearer token and HMAC secret are read from environment variables, never from a CLI flag —
//! argv is visible to any other process on the same host via `ps`/`/proc/<pid>/cmdline`, and lands
//! in shell history, both real leak vectors for a long-lived secret. Set:
//! - `ZIV_AUTH_BEARER` — if set (non-empty), bearer auth is enabled with this exact token value.
//! - `ZIV_AUTH_HMAC_SECRET` — if set (non-empty), HMAC-signed-URL auth is enabled with this secret.
//!
//! Either, both, or neither may be set. **Policy when both are configured: access is granted if
//! EITHER a valid bearer OR a valid signature is presented** (logical OR, not AND) — this lets a
//! deployment offer a bearer token for interactive/API clients and short-lived signed URLs for
//! embedding in a viewer page, without forcing every caller to present both. If only one of the
//! two is configured, that one alone gates every request.
//!
//! ## Why the image is in the message
//!
//! One process serves many images, and two of them answer the same `{proj}/{region}/{size}`. If
//! the message did not name the image, a signature minted for a tile of a public image would
//! verify against the identical tile coordinates of a private one, which is a straightforward
//! authorization bypass rather than a cache oddity. The image is therefore the FIRST field, and
//! `path_segments` reads it from both URL shapes the server offers: `/i/{name}/iiif/…` for a
//! mounted image, and `/iiif/…` for the single-image root alias, whose image is filed under
//! `routes::ROOT_ALIAS_NAME`.
//!
//! ## HMAC-signed URL format
//!
//! Query params: `?exp=<unix_seconds>&sig=<base64url(hmac_sha256(secret, message))>`.
//!
//! The signed **message** binds the resource being requested, not just the URL path, so a
//! signature minted for one tile cannot be replayed against a different region/size by editing
//! the path if the path segments happen to overlap in some other encoding. Canonical message
//! (fields joined with `\n`, matching the IIIF path segments exactly as received):
//!
//! ```text
//! "{proj}\n{region}\n{size}\n{exp}"
//! ```
//!
//! e.g. for `GET /iiif/default/256,256,512,512/512,512/0/default.jpg?exp=1750000000&sig=...` the
//! message is `"default\n256,256,512,512\n512,512\n1750000000"`. A client generates a matching URL
//! by computing `base64url(hmac_sha256(secret, message))` with that exact message and appending it
//! as `sig` alongside the `exp` it signed. Verification recomputes the same message from the
//! live request's path segments + the caller-supplied `exp`, recomputes the HMAC, and rejects
//! (401) if: `sig` is missing, `exp` is missing/unparseable, `exp < now` (expired), or the
//! recomputed HMAC doesn't match the supplied `sig` — the match is `subtle`-constant-time, not
//! `==`, so response timing doesn't leak how many leading bytes matched.

use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Auth configuration: bearer token and/or HMAC secret. Both are optional independently; at
/// least one must be `Some` for `AuthConfig` to actually be constructed by `from_env` (an
/// all-`None` config is represented as `AppState`'s `Option<AuthConfig>` being `None`, i.e. auth
/// off, rather than a `Some(AuthConfig { bearer: None, hmac_secret: None })` that would gate
/// everything and admit nothing).
#[derive(Clone)]
pub struct AuthConfig {
    pub bearer: Option<String>,
    pub hmac_secret: Option<String>,
}

impl AuthConfig {
    /// Read auth config from `ZIV_AUTH_BEARER` / `ZIV_AUTH_HMAC_SECRET`. Returns `None` (auth
    /// off) if neither is set (or both are set to an empty string, treated as unset). Secrets are
    /// intentionally read from the environment only — see the module doc for why not argv.
    pub fn from_env() -> Option<Self> {
        let bearer = env::var("ZIV_AUTH_BEARER").ok().filter(|s| !s.is_empty());
        let hmac_secret = env::var("ZIV_AUTH_HMAC_SECRET")
            .ok()
            .filter(|s| !s.is_empty());
        if bearer.is_none() && hmac_secret.is_none() {
            return None;
        }
        Some(AuthConfig {
            bearer,
            hmac_secret,
        })
    }
}

/// Structured 401 body, mirroring `routes::ApiError`'s shape (`{"error": "..."}"`) without
/// depending on that (private) type — auth is a layer, not a handler, so it builds its own
/// minimal response.
fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::WWW_AUTHENTICATE, "Bearer"),
            (header::CONTENT_TYPE, "application/json"),
        ],
        json!({ "error": message }).to_string(),
    )
        .into_response()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Constant-time compare of a caller-supplied bearer token against the configured one.
fn bearer_matches(configured: &str, supplied: &str) -> bool {
    // Constant-time comparisons require equal-length inputs; a length mismatch alone is safe to
    // observe (it doesn't leak byte-level content), so short-circuit on it before the ct-eq call.
    if configured.len() != supplied.len() {
        return false;
    }
    configured.as_bytes().ct_eq(supplied.as_bytes()).into()
}

/// Recompute `HMAC-SHA256(secret, "{image}\n{proj}\n{region}\n{size}\n{exp}")` and
/// constant-time-compare against the caller-supplied base64url signature. See the module doc for
/// the message format and rationale.
fn hmac_matches(
    secret: &str,
    image: &str,
    proj: &str,
    region: &str,
    size: &str,
    exp: u64,
    sig_b64: &str,
) -> bool {
    let Ok(supplied) = URL_SAFE_NO_PAD.decode(sig_b64) else {
        return false;
    };
    let message = format!("{image}\n{proj}\n{region}\n{size}\n{exp}");
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(message.as_bytes());
    let expected = mac.finalize().into_bytes();
    // Lengths differ (HMAC-SHA256 is always 32 bytes) -> never a match, and comparing unequal
    // lengths ct-eq would be a type error (`ct_eq` requires equal-length slices) or a silent
    // truncation bug depending on API, so the length check is a correctness guard, not just an
    // optimization.
    if expected.len() != supplied.len() {
        return false;
    }
    expected.as_slice().ct_eq(&supplied).into()
}

/// `axum::middleware::from_fn_with_state` handler gating the sub-router it's layered on. Checks,
/// in order: bearer (if configured) using the `Authorization: Bearer <token>` header, then HMAC
/// (if configured) using the `?exp=&sig=` query params against the request's own path segments
/// (`proj`, `region`, `size` extracted straight from the URI so the signature binds what's
/// actually being requested). If EITHER configured method succeeds, the request proceeds
/// (`next.run`); otherwise 401.
///
/// Query params are parsed by hand (`raw_query_param`) rather than via axum's typed `Query<T>`
/// extractor: `Query<T>` fails the WHOLE request with an extractor-level 400 the moment any
/// field doesn't parse (e.g. `exp=not-a-number`), which would run BEFORE this function's body
/// even gets to check the bearer header -- breaking bearer-only auth for any request that
/// happens to carry a malformed/unrelated `exp`/`sig` query string. Parsing by hand and treating
/// "missing or unparseable" as `None` keeps the two auth methods independent, matching the
/// documented OR policy.
pub async fn auth_middleware(
    State(auth): State<AuthConfig>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if let Some(configured) = &auth.bearer {
        if let Some(supplied) = extract_bearer(&request) {
            if bearer_matches(configured, &supplied) {
                return next.run(request).await;
            }
        }
    }

    if let Some(secret) = &auth.hmac_secret {
        if let Some((image, proj, region, size)) = path_segments(&request) {
            let exp = raw_query_param(&request, "exp").and_then(|v| v.parse::<u64>().ok());
            let sig = raw_query_param(&request, "sig");
            if let (Some(exp), Some(sig)) = (exp, sig) {
                if exp >= now_unix()
                    && hmac_matches(secret, &image, &proj, &region, &size, exp, &sig)
                {
                    return next.run(request).await;
                }
            }
        }
    }

    unauthorized("unauthorized")
}

fn extract_bearer(request: &Request<Body>) -> Option<String> {
    let header = request.headers().get(header::AUTHORIZATION)?;
    let value = header.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::to_string)
}

/// Look up a single query param by name from the raw URI query string, tolerating any input
/// (malformed percent-encoding, duplicate keys -> first match, missing `=value`) by simply
/// returning `None` rather than erroring — auth query parsing must never itself become a way to
/// reject an otherwise-valid (e.g. bearer-authed) request.
fn raw_query_param(request: &Request<Body>, name: &str) -> Option<String> {
    let query = request.uri().query()?;
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let key = it.next()?;
        if key == name {
            let value = it.next().unwrap_or("");
            return Some(percent_decode(value).unwrap_or_else(|| value.to_string()));
        }
    }
    None
}

/// Minimal percent-decoding for query values (`%XX` -> byte, `+` left as-is since IIIF/HMAC
/// query values here are base64url/digits which never need it). Returns `None` on malformed
/// escapes so the caller falls back to the raw (undecoded) string rather than erroring — base64
/// URL-safe alphabets and decimal digits don't need percent-encoding in practice, so this only
/// matters for defensively handling a client that encoded anyway.
fn percent_decode(s: &str) -> Option<String> {
    if !s.contains('%') {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            let byte = u8::from_str_radix(hex, 16).ok()?;
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Pull `{image}`, `{proj}`, `{region}`, `{size}` straight off the raw URI path — this runs as a
/// `Layer` wrapping the whole sub-router (both info.json and the tile routes), so it can't use
/// axum's `Path<T>` extractor tied to one specific route's param set. info.json requests have no
/// region/size; those callers must use bearer auth (HMAC signing is meaningless without a tile
/// identity to bind), so a path that doesn't match the tile shape simply yields `None` and falls
/// through to the "not authorized via HMAC" branch.
///
/// Two shapes are recognised, because the server serves two. `/i/{name}/iiif/…` is a mounted image
/// and the name comes off the path. `/iiif/…` is the single-image root alias, whose image is filed
/// under `routes::ROOT_ALIAS_NAME`; using that constant here rather than a literal is what keeps a
/// signature minted for the root alias verifiable against the registry's own name for it.
fn path_segments(request: &Request<Body>) -> Option<(String, String, String, String)> {
    let path = request.uri().path();
    let rest = path.trim_start_matches('/');

    let (image, after) = match rest.strip_prefix("i/") {
        Some(mounted) => {
            let mount = crate::mount::split_mount(mounted)?;
            if mount.marker != crate::mount::Marker::Iiif {
                return None;
            }
            (mount.name.to_string(), mount.tail)
        }
        None => (
            crate::routes::ROOT_ALIAS_NAME.to_string(),
            rest.strip_prefix("iiif/")?,
        ),
    };

    let mut segments = after.split('/');
    let proj = segments.next()?;
    let region = segments.next()?;
    let size = segments.next()?;
    let _rotation = segments.next()?;
    let _quality_dot_format = segments.next()?;
    if segments.next().is_some() {
        return None;
    }
    Some((
        image,
        proj.to_string(),
        region.to_string(),
        size.to_string(),
    ))
}

/// Sign a message for tests / documentation purposes (also usable by any future CLI helper
/// that wants to mint signed URLs without duplicating the HMAC construction).
#[cfg(test)]
pub(crate) fn sign(
    secret: &str,
    image: &str,
    proj: &str,
    region: &str,
    size: &str,
    exp: u64,
) -> String {
    let message = format!("{image}\n{proj}\n{region}\n{size}\n{exp}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(message.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;

    /// The image a single-image server's one image is filed under. Spelled the same as the
    /// default PROJECTION, which is why every call site below names it rather than repeating the
    /// literal: the two are different fields that happen to share a word.
    const ROOT_ALIAS: &str = crate::routes::ROOT_ALIAS_NAME;

    #[test]
    fn bearer_matches_identical_tokens() {
        assert!(bearer_matches("secret-token", "secret-token"));
    }

    #[test]
    fn bearer_rejects_wrong_token() {
        assert!(!bearer_matches("secret-token", "wrong-token"));
    }

    #[test]
    fn bearer_rejects_different_length_token() {
        assert!(!bearer_matches("secret-token", "short"));
    }

    #[test]
    fn hmac_sign_then_verify_matches() {
        let sig = sign(
            "s3cr3t",
            ROOT_ALIAS,
            "default",
            "full",
            "max",
            9_999_999_999,
        );
        assert!(hmac_matches(
            "s3cr3t",
            ROOT_ALIAS,
            "default",
            "full",
            "max",
            9_999_999_999,
            &sig
        ));
    }

    #[test]
    fn hmac_rejects_tampered_region() {
        let sig = sign(
            "s3cr3t",
            ROOT_ALIAS,
            "default",
            "full",
            "max",
            9_999_999_999,
        );
        // Same sig, but verifying against a DIFFERENT region -> must fail: the signature binds
        // the region, not just the path shape.
        assert!(!hmac_matches(
            "s3cr3t",
            ROOT_ALIAS,
            "default",
            "0,0,10,10",
            "max",
            9_999_999_999,
            &sig
        ));
    }

    #[test]
    fn hmac_rejects_wrong_secret() {
        let sig = sign(
            "s3cr3t",
            ROOT_ALIAS,
            "default",
            "full",
            "max",
            9_999_999_999,
        );
        assert!(!hmac_matches(
            "wrong-secret",
            ROOT_ALIAS,
            "default",
            "full",
            "max",
            9_999_999_999,
            &sig
        ));
    }

    /// Path segments are extracted from `Uri::path()`, which returns the RAW (still
    /// percent-encoded) path -- a client cannot forge a `%0A` in a region/size segment to inject
    /// a literal newline into the signed message and collide with the `\n` field delimiter, since
    /// `format!` sees the 3-byte escape sequence `%0A`, never an actual newline byte.
    #[test]
    fn path_segments_does_not_decode_percent_escapes() {
        let req = HttpRequest::builder()
            .uri("/iiif/default/full%0Ainjected/max/0/default.jpg")
            .body(Body::empty())
            .unwrap();
        let (_, _, region, _) = path_segments(&req).unwrap();
        assert_eq!(region, "full%0Ainjected");
    }

    #[test]
    fn raw_query_param_finds_value() {
        let req = HttpRequest::builder()
            .uri("/iiif/default/full/max/0/default.jpg?exp=123&sig=abc")
            .body(Body::empty())
            .unwrap();
        assert_eq!(raw_query_param(&req, "exp").as_deref(), Some("123"));
        assert_eq!(raw_query_param(&req, "sig").as_deref(), Some("abc"));
        assert_eq!(raw_query_param(&req, "missing"), None);
    }

    /// A garbage (unparseable-as-u64) `exp` value must be returned as-is by the raw lookup, not
    /// cause an error -- callers decide separately whether it parses; the lookup itself never
    /// fails on malformed content.
    #[test]
    fn raw_query_param_tolerates_garbage_value() {
        let req = HttpRequest::builder()
            .uri("/iiif/default/full/max/0/default.jpg?exp=not-a-number&sig=%zz")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            raw_query_param(&req, "exp").as_deref(),
            Some("not-a-number")
        );
        // malformed percent-escape falls back to the raw string rather than erroring.
        assert_eq!(raw_query_param(&req, "sig").as_deref(), Some("%zz"));
    }

    #[test]
    fn raw_query_param_none_when_no_query_string() {
        let req = HttpRequest::builder()
            .uri("/iiif/default/full/max/0/default.jpg")
            .body(Body::empty())
            .unwrap();
        assert_eq!(raw_query_param(&req, "exp"), None);
    }

    #[test]
    fn from_env_none_when_unset() {
        // Use distinctive names unlikely to be set in any real environment, and clear them
        // first in case an earlier test in this process set them.
        temp_env(
            &[("ZIV_AUTH_BEARER", None), ("ZIV_AUTH_HMAC_SECRET", None)],
            || {
                assert!(AuthConfig::from_env().is_none());
            },
        );
    }

    #[test]
    fn from_env_bearer_only() {
        temp_env(
            &[
                ("ZIV_AUTH_BEARER", Some("tok-123")),
                ("ZIV_AUTH_HMAC_SECRET", None),
            ],
            || {
                let cfg = AuthConfig::from_env().expect("expected auth enabled");
                assert_eq!(cfg.bearer.as_deref(), Some("tok-123"));
                assert!(cfg.hmac_secret.is_none());
            },
        );
    }

    /// Serializes access to the env vars this module reads, since `cargo test` runs tests in
    /// this process concurrently by default and env vars are global mutable state.
    fn temp_env(vars: &[(&str, Option<&str>)], f: impl FnOnce()) {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (k.to_string(), env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(val) => env::set_var(k, val),
                None => env::remove_var(k),
            }
        }
        f();
        for (k, v) in previous {
            match v {
                Some(val) => env::set_var(&k, val),
                None => env::remove_var(&k),
            }
        }
    }

    /// A signature minted for one image must not open another. The message binds the RESOURCE,
    /// which is what this module's doc promises, and an image is part of the resource now.
    #[test]
    fn a_signature_for_one_image_does_not_verify_for_another() {
        let sig = sign("s3cr3t", "alpha", "default", "full", "max", 9_999_999_999);
        assert!(hmac_matches(
            "s3cr3t",
            "alpha",
            "default",
            "full",
            "max",
            9_999_999_999,
            &sig
        ));
        assert!(!hmac_matches(
            "s3cr3t",
            "beta",
            "default",
            "full",
            "max",
            9_999_999_999,
            &sig
        ));
    }

    /// Signed URLs must work at both shapes the server serves: the mount, and the single-image
    /// root alias.
    #[test]
    fn path_segments_reads_both_url_shapes() {
        let mounted = HttpRequest::builder()
            .uri("/i/nested/name/iiif/default/full/max/0/default.jpg")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            path_segments(&mounted),
            Some((
                "nested/name".to_string(),
                "default".to_string(),
                "full".to_string(),
                "max".to_string()
            ))
        );

        let root = HttpRequest::builder()
            .uri("/iiif/default/full/max/0/default.jpg")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            path_segments(&root),
            Some((
                crate::routes::ROOT_ALIAS_NAME.to_string(),
                "default".to_string(),
                "full".to_string(),
                "max".to_string()
            )),
            "the root alias signs under the name the registry gave it"
        );
    }
}
