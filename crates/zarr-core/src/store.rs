//! Store-spec dispatch: turns a user-facing string (a local filesystem path, or a
//! `s3://`/`gs://`/`az://`/`http(s)://` URL) into the right zarrs storage backend.
//!
//! Two backends exist because `zarrs_object_store`'s `AsyncObjectStore` is ASYNC-ONLY (there is
//! no sync `object_store`-backed zarrs storage type), while the local filesystem path keeps
//! using zarrs' existing SYNC `FilesystemStore` — unchanged from P0/P1, zero behavioral risk to
//! the well-tested local path. `ZarrImage` (`image.rs`) holds an enum over both group/array
//! kinds and dispatches `open`/`read_region_f64` per-variant; the async variant drives its
//! futures with `pollster::block_on` (see the doc comment on that call site in `image.rs` for
//! why `pollster` specifically, vs. a dedicated `tokio::runtime::Runtime` or
//! `Handle::block_on`/`block_in_place`).
use std::sync::Arc;
use std::time::Duration;

use object_store::ObjectStore;
use url::Url;

use crate::error::ZarrError;

mod ssrf;
pub use ssrf::allow_internal_hosts_from_env;

/// A parsed remote store spec: the boxed [`object_store::ObjectStore`] rooted at the
/// bucket/container/host, plus the residual path within it (e.g. for
/// `s3://my-bucket/images/sample.ome.zarr`, the store is rooted at `my-bucket` and the residual
/// path is `images/sample.ome.zarr`) — this mirrors how `AmazonS3Builder::with_url` /
/// `GoogleCloudStorageBuilder::with_url` / `MicrosoftAzureBuilder::with_url` only consume the
/// bucket/container portion of the URL and silently drop any further path segments, so the
/// residual path must be threaded through separately as the zarr group's `path` argument
/// (`Group::async_open(store, path)`), exactly as the local filesystem path today opens the
/// group at `"/"` relative to a `FilesystemStore` rooted at the given directory.
#[derive(Debug)]
pub struct RemoteStoreSpec {
    pub store: Arc<dyn ObjectStore>,
    pub group_path: String,
}

/// Whether a store spec string names a local filesystem path or a remote URL.
#[derive(Debug)]
pub enum StoreSpec {
    /// A local filesystem path (anything that doesn't parse as one of the recognized remote
    /// URL schemes).
    Local(std::path::PathBuf),
    /// A remote object-store URL (`s3://`, `gs://`/`gcs://`, `az://`/`abfs://`/`abfss://`,
    /// `http://`/`https://`).
    Remote(RemoteStoreSpec),
}

/// Environment variables `object_store`'s `from_env()` builders read, per scheme (documented
/// here since `ZarrImage::open` accepts these schemes but doesn't itself name the env vars):
///
/// - **S3** (`s3://`): any `AWS_*` var recognized by `object_store`'s `AmazonS3ConfigKey`,
///   notably `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_REGION`
///   (or `AWS_DEFAULT_REGION`), `AWS_ENDPOINT` (for S3-compatible stores).
/// - **GCS** (`gs://`): any `GOOGLE_*` var (`GOOGLE_SERVICE_ACCOUNT`,
///   `GOOGLE_SERVICE_ACCOUNT_KEY`, `GOOGLE_BUCKET`, ...) plus the `SERVICE_ACCOUNT` path var.
/// - **Azure** (`az://`): any `AZURE_*` var (`AZURE_STORAGE_ACCOUNT_NAME`,
///   `AZURE_STORAGE_ACCOUNT_KEY`, `AZURE_STORAGE_SAS_KEY`, ...) plus `MSI_ENDPOINT`.
/// - **HTTP(S)** (`http://`/`https://`): no credential env vars — a plain HTTP(S) store with
///   `allow_http` enabled so a `http://` (not just `https://`) URL is accepted (e.g. for a local
///   test server or an internal, non-TLS endpoint).
///
/// Parses `spec` into a [`StoreSpec`]. A spec is treated as a remote URL only if it parses as an
/// absolute URL with one of the recognized schemes below; anything else (including a bare path,
/// a relative path, or a Windows drive-letter path that happens to look URL-ish) is treated as a
/// local filesystem path, matching the existing P0/P1 behavior exactly for every spec that used
/// to be accepted (a `PathBuf`).
///
/// For `http(s)://` specs, the host is checked against the SSRF blocklist (see
/// `ssrf::check_host_allowed`'s doc comment for the exact ranges and rationale) unless the
/// `ZIV_ALLOW_INTERNAL_HOSTS` escape-hatch env var is set. Use
/// [`parse_store_spec_with_options`] to control the escape hatch explicitly (e.g. from a CLI
/// flag) instead of relying on the env var.
pub fn parse_store_spec(spec: &str) -> Result<StoreSpec, ZarrError> {
    parse_store_spec_with_options(spec, ssrf::allow_internal_hosts_from_env())
}

/// Same as [`parse_store_spec`], but with the SSRF escape hatch passed explicitly rather than
/// read from `ZIV_ALLOW_INTERNAL_HOSTS` — this is the deterministic entry point (no ambient env
/// read), used directly by callers that already know whether the hatch should be open (e.g. the
/// CLI, which resolves its `--allow-internal-hosts` flag / the env var itself and passes the
/// final bool here explicitly; see `crates/cli/src/main.rs`).
pub fn parse_store_spec_with_options(
    spec: &str,
    allow_internal_hosts: bool,
) -> Result<StoreSpec, ZarrError> {
    if let Ok(url) = Url::parse(spec) {
        match url.scheme() {
            "s3" => {
                return build_remote(
                    &url,
                    object_store::aws::AmazonS3Builder::from_env()
                        .with_retry(default_retry_config())
                        .with_client_options(default_client_options()),
                )
            }
            "gs" | "gcs" => {
                return build_remote(
                    &url,
                    object_store::gcp::GoogleCloudStorageBuilder::from_env()
                        .with_retry(default_retry_config())
                        .with_client_options(default_client_options()),
                );
            }
            "az" | "abfs" | "abfss" => {
                return build_remote(
                    &url,
                    object_store::azure::MicrosoftAzureBuilder::from_env()
                        .with_retry(default_retry_config())
                        .with_client_options(default_client_options()),
                );
            }
            "http" | "https" => return build_http(&url, allow_internal_hosts),
            _ => {} // fall through to local-path handling below
        }
    }
    Ok(StoreSpec::Local(std::path::PathBuf::from(spec)))
}

/// Request/connect timeouts applied to every remote store's HTTP client (S3/GCS/Azure/HTTP
/// alike), so a hung remote read fails loud instead of pinning a `spawn_blocking` thread (and,
/// transitively, the bounded blocking thread pool) forever. Values are deliberately more
/// generous than `object_store`'s own defaults (30s request / 5s connect) is close already, but
/// set explicitly here rather than relying on the library default so the timeout is a documented
/// ziv behavior, not an incidental upstream default that could silently change on an
/// `object_store` upgrade:
/// - **connect timeout: 10s** — long enough for a slow-but-working TCP handshake / TLS setup
///   against a real remote store, short enough that a firewall-dropped connection to a
///   nonexistent/blackholed host doesn't hang the calling `spawn_blocking` thread indefinitely.
/// - **request timeout: 30s** — covers a full request/response cycle for a single zarr chunk
///   (typically well under a second) with generous headroom for a loaded remote store or a slow
///   link, while still bounding worst-case latency for callers (tile requests, health checks).
fn default_client_options() -> object_store::ClientOptions {
    object_store::ClientOptions::new()
        .with_connect_timeout(Duration::from_secs(10))
        .with_timeout(Duration::from_secs(30))
}

/// Bounded retry policy applied to every remote store: at most 3 retries (4 attempts total) with
/// `object_store`'s default exponential backoff (100ms initial, 15s max, factor 2.0) and a 180s
/// overall retry-timeout ceiling (this crate does not need to change those backoff defaults — a
/// LOW retry count is what matters for a synchronous, spawn_blocking-bound read path: retries
/// happen while holding a blocking-pool thread, so unbounded/high retry counts would extend
/// worst-case thread occupancy). Applies uniformly to transient network errors AND (per
/// `object_store`'s own retry classification) retryable HTTP status codes (429/5xx); a genuine
/// SSRF-blocked or 4xx-not-retryable error is never retried.
fn default_retry_config() -> object_store::RetryConfig {
    object_store::RetryConfig {
        max_retries: 3,
        ..Default::default()
    }
}

/// Shared "bucket-style" remote store construction (S3/GCS/Azure): parse the scheme-specific
/// residual path via `object_store::ObjectStoreScheme`, hand the FULL url to the typed builder's
/// `with_url` (which extracts just the bucket/container/account), build, then box.
fn build_remote<B>(url: &Url, builder: B) -> Result<StoreSpec, ZarrError>
where
    B: RemoteBuilder,
{
    let (_, residual) = object_store::ObjectStoreScheme::parse(url)
        .map_err(|e| ZarrError::Open(format!("unrecognized store URL {}: {e}", redact_url(url))))?;
    let store = builder
        .with_url(url.to_string())
        .build_boxed()
        .map_err(|e| {
            ZarrError::Open(format!(
                "failed to build store for {}: {e}",
                redact_url(url)
            ))
        })?;
    Ok(StoreSpec::Remote(RemoteStoreSpec {
        store: Arc::from(store),
        group_path: format!("/{}", residual.as_ref().trim_start_matches('/')),
    }))
}

/// Redacts a URL down to `scheme://host[:port]` for safe interpolation into an error/log
/// string — strips userinfo (`user:pass@`, e.g. a credentialed `https://user:token@host/...`
/// spec) and the query string (pre-signed S3/GCS URLs carry the signature/credential there,
/// e.g. `?X-Amz-Signature=...`), along with the path, none of which are needed to identify
/// *which host* a store-build/connect error came from. Every place in this module (and
/// `ssrf::check_host_allowed`'s callers) that puts a URL into a `ZarrError::Open`/log string
/// must route it through this first, so a credentialed or pre-signed spec never leaks a secret
/// into logs.
fn redact_url(u: &Url) -> String {
    match u.port() {
        Some(port) => format!("{}://{}:{port}", u.scheme(), u.host_str().unwrap_or("?")),
        None => format!("{}://{}", u.scheme(), u.host_str().unwrap_or("?")),
    }
}

/// `http(s)://` is different from the bucket-style schemes: the ENTIRE url (including any path)
/// is the store root (mirrors `zarrs_object_store`'s own doc example, which does
/// `HttpBuilder::new().with_url("http://...").build()` then treats the whole thing as the
/// store's addressing root) — so the zarr group is opened at `"/"`, exactly like the local
/// `FilesystemStore` path today (`FilesystemStore::new(path)` + `Group::open(store, "/")`).
/// `allow_http` is set so a plain (non-TLS) `http://` URL is usable, not just `https://`.
///
/// SSRF guard: unless `allow_internal` is true, the URL's host is resolved and checked against
/// the internal/link-local/metadata blocklist (see `ssrf::check_host_allowed`) BEFORE the store
/// is built — this is the hook point named in the module's plan (store-build time, covering both
/// the tile-read path and the `new()`-startup auto-stretch read, since both share this
/// once-built store).
///
/// Redirect handling: this guard runs ONCE, against the spec's own host — it does NOT re-run
/// against a redirect target. `object_store`'s HTTP client is built on `reqwest`, which by
/// default FOLLOWS 3xx redirects transparently (up to ~10 hops) with no re-check, so without
/// intervention a public URL that 302s to `http://169.254.169.254/...` would sail straight past
/// this check. `object_store::ClientOptions` exposes no redirect-policy control of its own (see
/// `NoRedirectConnector`'s doc comment below) — so redirects are disabled at the transport layer
/// via a custom `HttpConnector` (`with_http_connector`) instead: a 3xx response is then returned
/// to `object_store`'s own retry/response handling as a hard error (`BareRedirect`/`Status`)
/// rather than silently followed, so any redirect — to a blocked host or not — fails loud.
fn build_http(url: &Url, allow_internal: bool) -> Result<StoreSpec, ZarrError> {
    let host = url
        .host_str()
        .ok_or_else(|| ZarrError::Open(format!("http(s) URL {} has no host", redact_url(url))))?;
    let port = url.port_or_known_default().unwrap_or(80);
    ssrf::check_host_allowed(host, port, allow_internal)?;

    let options = default_client_options().with_allow_http(true);
    let store = object_store::http::HttpBuilder::new()
        .with_url(url.to_string())
        .with_client_options(options)
        .with_http_connector(NoRedirectConnector)
        .with_retry(default_retry_config())
        .build()
        .map_err(|e| {
            ZarrError::Open(format!(
                "failed to build http store for {}: {e}",
                redact_url(url)
            ))
        })?;
    Ok(StoreSpec::Remote(RemoteStoreSpec {
        store: Arc::new(store),
        group_path: "/".to_string(),
    }))
}

/// An [`object_store::client::HttpConnector`] that builds its own `reqwest::Client` with HTTP
/// redirects DISABLED (`reqwest::redirect::Policy::none()`), used only for the `http(s)://` store
/// (`build_http`) to close the redirect-based SSRF bypass documented on that function.
///
/// ## Why this exists
///
/// `object_store::ClientOptions` — the normal way to configure the client `HttpBuilder` uses —
/// exposes plenty of `with_*` knobs (timeouts, TLS, proxy, HTTP/1-vs-2, ...) but genuinely NO
/// redirect-policy control (verified against `object_store` 0.13.2's own source:
/// `ClientOptions::client()` in `client/mod.rs` calls `reqwest::ClientBuilder::new()` and never
/// calls `.redirect(..)`, leaving `reqwest`'s default policy — follow up to ~10 hops — in effect;
/// `object_store`'s own `client/retry.rs` test suite (`test_retry` in that file) explicitly
/// exercises a 302 being followed transparently by the underlying client, and only surfaces a
/// hard error once the redirect COUNT is exhausted, confirming redirects are followed beneath
/// `object_store`'s own response handling, not by it). So a `ClientOptions`-only fix is not
/// possible.
///
/// What 0.13 DOES expose is `HttpBuilder::with_http_connector`, a hook for a custom
/// [`object_store::client::HttpConnector`] — a factory for the [`HttpClient`] used to actually
/// perform requests. This type implements that hook: `connect` builds a plain `reqwest::Client`
/// with `.redirect(Policy::none())`, so any 3xx response comes back to `object_store` as-is. Per
/// the retry-handling `object_store` already has (`client/retry.rs`: a redirect status with no
/// `Location` header becomes `RequestError::BareRedirect`, and one WITH a `Location` header
/// becomes `RequestError::Status`), a redirect now fails the request loudly instead of being
/// followed to an unchecked host — closing the SSRF bypass regardless of what the redirect target
/// is (blocked or not; ziv simply never follows ANY http(s) store redirect).
///
/// This intentionally reimplements a minimal slice of what `ClientOptions::client()` would have
/// applied (that method is `pub(crate)` to `object_store`, not reusable from here) — only the
/// settings `default_client_options()`/`build_http` actually set for the http path: connect/
/// request timeouts and `allow_http`. If either of those default-setting call sites changes, this
/// must be kept in sync (there is no way to derive it automatically from a `ClientOptions` value,
/// since that struct's fields are private).
///
/// [`HttpClient`]: object_store::client::HttpClient
#[derive(Debug, Clone, Copy)]
struct NoRedirectConnector;

impl object_store::client::HttpConnector for NoRedirectConnector {
    fn connect(
        &self,
        options: &object_store::ClientOptions,
    ) -> object_store::Result<object_store::client::HttpClient> {
        // Mirrors the connect/request timeouts and `allow_http` (-> `https_only`) that
        // `default_client_options()` / `build_http` set on `options` — see the struct doc comment
        // for why these can't just be read back out of `options` and forwarded generically.
        let mut builder = reqwest::ClientBuilder::new()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none());
        // `ClientOptions::allow_http` isn't publicly readable either; `build_http` always passes
        // `.with_allow_http(true)` (needed for plain-`http://` specs/local test servers), so
        // `https_only` must stay off here to match — options is otherwise unused, kept as a
        // parameter to satisfy the `HttpConnector` trait signature.
        let _ = options;
        builder = builder.https_only(false);
        let client = builder.build().map_err(|e| object_store::Error::Generic {
            store: "HTTP",
            source: Box::new(e),
        })?;
        Ok(object_store::client::HttpClient::new(client))
    }
}

/// Minimal shared surface over the per-scheme `object_store` builders (`AmazonS3Builder`,
/// `GoogleCloudStorageBuilder`, `MicrosoftAzureBuilder`) needed by `build_remote`: set the URL
/// (already primed with credentials via each builder's own `from_env()`) and build into a boxed
/// trait object.
trait RemoteBuilder {
    fn with_url(self, url: String) -> Self;
    fn build_boxed(self) -> Result<Box<dyn ObjectStore>, object_store::Error>;
}

impl RemoteBuilder for object_store::aws::AmazonS3Builder {
    fn with_url(self, url: String) -> Self {
        object_store::aws::AmazonS3Builder::with_url(self, url)
    }
    fn build_boxed(self) -> Result<Box<dyn ObjectStore>, object_store::Error> {
        Ok(Box::new(self.build()?))
    }
}

impl RemoteBuilder for object_store::gcp::GoogleCloudStorageBuilder {
    fn with_url(self, url: String) -> Self {
        object_store::gcp::GoogleCloudStorageBuilder::with_url(self, url)
    }
    fn build_boxed(self) -> Result<Box<dyn ObjectStore>, object_store::Error> {
        Ok(Box::new(self.build()?))
    }
}

impl RemoteBuilder for object_store::azure::MicrosoftAzureBuilder {
    fn with_url(self, url: String) -> Self {
        object_store::azure::MicrosoftAzureBuilder::with_url(self, url)
    }
    fn build_boxed(self) -> Result<Box<dyn ObjectStore>, object_store::Error> {
        Ok(Box::new(self.build()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Guards the two tests below that mutate `AZURE_STORAGE_ACCOUNT_NAME` — `cargo test` runs
    /// tests in the same crate concurrently on separate threads by default, and env vars are
    /// process-global, so without this lock the two tests could interleave and each observe the
    /// other's env state.
    static AZURE_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn plain_path_is_local() {
        match parse_store_spec("/tmp/x.ome.zarr").unwrap() {
            StoreSpec::Local(p) => assert_eq!(p, std::path::PathBuf::from("/tmp/x.ome.zarr")),
            StoreSpec::Remote(_) => panic!("expected Local"),
        }
    }

    #[test]
    fn relative_path_is_local() {
        match parse_store_spec("tests/fixtures/sample_v04.ome.zarr").unwrap() {
            StoreSpec::Local(p) => {
                assert_eq!(
                    p,
                    std::path::PathBuf::from("tests/fixtures/sample_v04.ome.zarr")
                )
            }
            StoreSpec::Remote(_) => panic!("expected Local"),
        }
    }

    #[test]
    fn s3_url_is_remote_with_bucket_relative_group_path() {
        match parse_store_spec("s3://my-bucket/images/sample.ome.zarr").unwrap() {
            StoreSpec::Remote(r) => assert_eq!(r.group_path, "/images/sample.ome.zarr"),
            StoreSpec::Local(_) => panic!("expected Remote"),
        }
    }

    #[test]
    fn gs_url_is_remote() {
        match parse_store_spec("gs://my-bucket/sample.ome.zarr").unwrap() {
            StoreSpec::Remote(r) => assert_eq!(r.group_path, "/sample.ome.zarr"),
            StoreSpec::Local(_) => panic!("expected Remote"),
        }
    }

    /// Azure has no bucket-embeds-account URL form for the plain `az://container/path` syntax
    /// (unlike S3/GCS) — the storage ACCOUNT must come from `AZURE_STORAGE_ACCOUNT_NAME` (one of
    /// the `AZURE_*` env vars `from_env()` reads; see the module doc comment). Set it for the
    /// duration of this test only, matching how a real deployment would configure it. Guarded by
    /// `AZURE_ENV_LOCK` (see its doc comment) since this mutates process-global env state.
    #[test]
    fn az_url_is_remote() {
        let _guard = AZURE_ENV_LOCK.lock().unwrap();
        // SAFETY: exclusive access to this env var is guaranteed by `AZURE_ENV_LOCK`, held for
        // the duration of the mutation + the call that reads it.
        unsafe {
            std::env::set_var("AZURE_STORAGE_ACCOUNT_NAME", "myaccount");
        }
        let result = parse_store_spec("az://my-container/sample.ome.zarr");
        unsafe {
            std::env::remove_var("AZURE_STORAGE_ACCOUNT_NAME");
        }
        match result.unwrap() {
            StoreSpec::Remote(r) => assert_eq!(r.group_path, "/sample.ome.zarr"),
            StoreSpec::Local(_) => panic!("expected Remote"),
        }
    }

    /// Without an account name available (neither in the URL nor `AZURE_STORAGE_ACCOUNT_NAME`),
    /// the plain `az://container/path` form must fail loud with a clear error, not silently
    /// build an unusable store. Guarded by `AZURE_ENV_LOCK` for the same reason as above.
    #[test]
    fn az_url_without_account_name_fails_loud() {
        let _guard = AZURE_ENV_LOCK.lock().unwrap();
        // SAFETY: exclusive access to this env var is guaranteed by `AZURE_ENV_LOCK`.
        unsafe {
            std::env::remove_var("AZURE_STORAGE_ACCOUNT_NAME");
        }
        let err = match parse_store_spec("az://my-container/sample.ome.zarr") {
            Err(e) => e,
            Ok(_) => panic!("expected an error (no account name available)"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Account must be specified"),
            "unexpected error: {msg}"
        );
    }

    /// `localhost` is a loopback host, which the SSRF guard now blocks by default (see
    /// `store::ssrf`) — this test is about the URL-shape dispatch (http URLs are `Remote` with a
    /// root group path), not about SSRF, so it uses the explicit escape hatch to keep testing a
    /// local/loopback URL without asserting on the guard itself (that's `ssrf`'s own tests, plus
    /// the dedicated rejection tests below).
    #[test]
    fn http_url_is_remote_with_root_group_path() {
        match parse_store_spec_with_options("http://localhost:9999/sample.ome.zarr", true).unwrap()
        {
            StoreSpec::Remote(r) => assert_eq!(r.group_path, "/"),
            StoreSpec::Local(_) => panic!("expected Remote"),
        }
    }

    #[test]
    fn https_url_is_remote() {
        match parse_store_spec("https://example.com/data/sample.ome.zarr").unwrap() {
            StoreSpec::Remote(r) => assert_eq!(r.group_path, "/"),
            StoreSpec::Local(_) => panic!("expected Remote"),
        }
    }

    // --- SSRF guard, exercised through the full `parse_store_spec_with_options` entry point
    // --- (Deliverable A). These pass `allow_internal_hosts=false` EXPLICITLY rather than calling
    // --- the ambient-env-reading `parse_store_spec` — `cargo test` runs tests concurrently on
    // --- separate threads by default, and `ZIV_ALLOW_INTERNAL_HOSTS` is process-global env
    // --- state mutated by the escape-hatch tests further down, so a test that implicitly reads
    // --- the env var without a lock would be racy. `parse_store_spec_env_var_escape_hatch_*`
    // --- below is the one test that legitimately goes through the env var, under its own lock.
    // --- See `store::ssrf`'s own unit tests for the underlying IP-range classification.

    #[test]
    fn parse_store_spec_rejects_cloud_metadata_endpoint() {
        let err = parse_store_spec_with_options("http://169.254.169.254/latest/meta-data/", false)
            .unwrap_err();
        assert!(matches!(err, ZarrError::BlockedHost { .. }), "{err}");
    }

    #[test]
    fn parse_store_spec_rejects_loopback_ip() {
        let err =
            parse_store_spec_with_options("http://127.0.0.1/sample.ome.zarr", false).unwrap_err();
        assert!(matches!(err, ZarrError::BlockedHost { .. }), "{err}");
    }

    #[test]
    fn parse_store_spec_rejects_localhost_hostname() {
        let err =
            parse_store_spec_with_options("http://localhost/sample.ome.zarr", false).unwrap_err();
        assert!(matches!(err, ZarrError::BlockedHost { .. }), "{err}");
    }

    #[test]
    fn parse_store_spec_rejects_private_10_range() {
        let err =
            parse_store_spec_with_options("http://10.0.0.1/sample.ome.zarr", false).unwrap_err();
        assert!(matches!(err, ZarrError::BlockedHost { .. }), "{err}");
    }

    #[test]
    fn parse_store_spec_rejects_private_192_168_range() {
        let err =
            parse_store_spec_with_options("http://192.168.1.1/sample.ome.zarr", false).unwrap_err();
        assert!(matches!(err, ZarrError::BlockedHost { .. }), "{err}");
    }

    #[test]
    fn parse_store_spec_rejects_ipv6_loopback() {
        let err = parse_store_spec_with_options("http://[::1]/sample.ome.zarr", false).unwrap_err();
        assert!(matches!(err, ZarrError::BlockedHost { .. }), "{err}");
    }

    #[test]
    fn parse_store_spec_accepts_public_host_without_connecting() {
        // `example.com` is a normal public host — the guard must not block it. This only
        // proves the host-check passes; it doesn't perform a real network connection (building
        // the `object_store::http` client doesn't itself connect).
        assert!(parse_store_spec_with_options("http://example.com/sample.ome.zarr", false).is_ok());
    }

    #[test]
    fn parse_store_spec_accepts_local_filesystem_path_unchanged() {
        // A plain filesystem path must remain unaffected by the SSRF guard entirely (it never
        // reaches the http branch), and needs no explicit-options variant since it can't
        // observe the env var either way.
        match parse_store_spec("/var/data/sample.ome.zarr").unwrap() {
            StoreSpec::Local(p) => {
                assert_eq!(p, std::path::PathBuf::from("/var/data/sample.ome.zarr"))
            }
            StoreSpec::Remote(_) => panic!("expected Local"),
        }
    }

    /// The `--allow-internal-hosts` escape hatch (`parse_store_spec_with_options(..., true)`)
    /// permits every host the default guard blocks, end-to-end through the full store-spec
    /// entry point (not just the underlying `ssrf::check_host_allowed`, already covered in
    /// `ssrf`'s own tests).
    #[test]
    fn parse_store_spec_with_options_escape_hatch_permits_internal_hosts() {
        for spec in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1/sample.ome.zarr",
            "http://localhost/sample.ome.zarr",
            "http://10.0.0.1/sample.ome.zarr",
            "http://192.168.1.1/sample.ome.zarr",
            "http://[::1]/sample.ome.zarr",
        ] {
            let result = parse_store_spec_with_options(spec, true);
            assert!(
                result.is_ok(),
                "expected {spec} to be permitted, got {result:?}"
            );
        }
    }

    /// The `ZIV_ALLOW_INTERNAL_HOSTS` env var achieves the same effect as the explicit
    /// `parse_store_spec_with_options(..., true)` call, through the default `parse_store_spec`
    /// entry point — this is how the CLI's escape hatch actually reaches this layer in
    /// production (see `crates/cli/src/main.rs`). Guarded by `ssrf`'s `ENV_LOCK`-equivalent
    /// pattern via a local mutex since this mutates process-global env state.
    #[test]
    fn parse_store_spec_env_var_escape_hatch_permits_internal_hosts() {
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: exclusive access guaranteed by ENV_LOCK for the duration of mutation + call.
        unsafe {
            std::env::set_var("ZIV_ALLOW_INTERNAL_HOSTS", "1");
        }
        let result = parse_store_spec("http://169.254.169.254/latest/meta-data/");
        unsafe {
            std::env::remove_var("ZIV_ALLOW_INTERNAL_HOSTS");
        }
        assert!(
            result.is_ok(),
            "expected env escape hatch to permit, got {result:?}"
        );
    }

    // --- Timeout/retry config (Deliverable A, point 2) ---
    //
    // `object_store::ClientOptions`/`RetryConfig` don't expose getters for the values a
    // `with_*` builder call set (they're consumed opaquely by `.build()` into the constructed
    // client) — asserting via each struct's own `Debug` output (both derive `Debug`) is the
    // straightforward way to prove the CONFIGURATION is applied, without needing a real network
    // client or a hung-mock-server integration test (which the plan notes would be flaky). This
    // directly covers the plan's "asserting the config is applied is acceptable" approach.

    #[test]
    fn default_client_options_sets_connect_and_request_timeouts() {
        let opts = default_client_options();
        let debug = format!("{opts:?}");
        assert!(
            debug.contains("connect_timeout: Some(Parsed(10s))"),
            "expected a 10s connect_timeout, got: {debug}"
        );
        assert!(
            debug.contains("timeout: Some(Parsed(30s))"),
            "expected a 30s request timeout, got: {debug}"
        );
    }

    #[test]
    fn default_retry_config_bounds_retries_to_three() {
        let retry = default_retry_config();
        assert_eq!(
            retry.max_retries, 3,
            "expected a bounded retry count of 3, got {}",
            retry.max_retries
        );
    }

    /// The http store build path actually applies `default_client_options()`/
    /// `default_retry_config()` to the constructed `HttpBuilder` (not just that the helper
    /// functions produce the right values in isolation, already covered above) — asserted via
    /// the built store's own `Debug` output, which `object_store::http::HttpStore` implements
    /// and which includes the `client_options`/`retry_config` it was built with.
    #[test]
    fn http_store_build_applies_timeouts_and_retry_config() {
        let url = Url::parse("http://example.com/sample.ome.zarr").unwrap();
        let spec = build_http(&url, false).unwrap();
        let StoreSpec::Remote(remote) = spec else {
            panic!("expected Remote");
        };
        let debug = format!("{:?}", remote.store);
        assert!(
            debug.contains("connect_timeout: Some(Parsed(10s))"),
            "expected the built http store to carry the connect timeout, got: {debug}"
        );
        assert!(
            debug.contains("max_retries: 3"),
            "expected the built http store to carry max_retries: 3, got: {debug}"
        );
    }

    // --- redact_url (Deliverable, credential-leak fix) ---

    #[test]
    fn redact_url_strips_userinfo_and_query() {
        let url = Url::parse("https://user:secret@host/path?sig=abc").unwrap();
        let redacted = redact_url(&url);
        assert_eq!(redacted, "https://host");
        assert!(!redacted.contains("secret"), "leaked userinfo: {redacted}");
        assert!(!redacted.contains("sig"), "leaked query: {redacted}");
        assert!(!redacted.contains("abc"), "leaked query value: {redacted}");
        assert!(!redacted.contains("path"), "leaked path: {redacted}");
    }

    #[test]
    fn redact_url_keeps_scheme_host_and_port() {
        let url = Url::parse("http://example.com:8080/x").unwrap();
        assert_eq!(redact_url(&url), "http://example.com:8080");
    }

    #[test]
    fn redact_url_omits_port_when_not_specified() {
        let url = Url::parse("https://example.com/x?token=deadbeef").unwrap();
        assert_eq!(redact_url(&url), "https://example.com");
    }

    /// `build_http`'s own wrapping error text must route through `redact_url` rather than the
    /// URL's raw `Display`/`to_string()`, exercised via the SSRF-block path (network-free,
    /// deterministic): a credentialed, pre-signed-looking URL pointed at a blocked host still
    /// reaches `redact_url`-wrapped code (`check_host_allowed` itself is called with just the bare
    /// `host` — see `error.rs`'s `BlockedHost` — but this proves the surrounding `build_http` call
    /// as a whole, with a credentialed URL as input, never surfaces the credential anywhere in the
    /// resulting error's `Display`).
    ///
    /// NOTE on scope: this covers ziv's OWN `format!`-constructed error text. `object_store`'s
    /// bucket-style builders (`AmazonS3Builder`/`GoogleCloudStorageBuilder`/
    /// `MicrosoftAzureBuilder`) can ALSO embed the raw, unredacted URL (credentials included) in
    /// THEIR OWN error `Display` (e.g. `Error::UrlNotRecognised { url }` in `object_store`'s aws/
    /// gcp/azure builder modules — verified against `object_store` 0.13.2's source) when
    /// `with_url` rejects the URL shape; `object_store::http::HttpBuilder::build()` has the same
    /// pattern for `Error::UnableToParseUrl` (unreachable in ziv's flow, since the URL is already
    /// successfully parsed before it reaches any builder). That inner error text is produced
    /// entirely inside the `object_store` crate, not something ziv's `redact_url` can intercept
    /// without dropping the underlying error's diagnostic detail altogether — a real, documented
    /// residual gap for the rare case a bucket-style builder rejects the URL shape itself, not
    /// something FIX 2 can close from outside `object_store`.
    #[test]
    fn build_http_error_does_not_leak_credentials_from_url() {
        let credentialed = Url::parse("http://user:supersecret@127.0.0.1/x?sig=topsecret").unwrap();
        let err = build_http(&credentialed, false).unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("supersecret") && !msg.contains("topsecret"),
            "build_http's error text leaked a credential: {msg}"
        );
    }

    // --- redirect-based SSRF bypass (Deliverable, FIX 1) ---
    //
    // `object_store`'s HTTP client (reqwest) follows 3xx redirects by default with no re-check
    // against the SSRF blocklist. `build_http` closes this by installing `NoRedirectConnector`
    // (redirects disabled at the transport layer via `reqwest::redirect::Policy::none()`), so a
    // redirect response comes back to `object_store`'s own response handling as a hard error
    // rather than being silently followed. This is exercised here against a real (if minimal)
    // HTTP/1.1 server speaking on loopback, since the redirect-following behavior lives in the
    // actual reqwest/hyper transport, not in anything mockable at the `ClientOptions`/`Url` level.
    mod redirect_guard {
        use super::*;
        use object_store::ObjectStoreExt as _;
        use std::io::{Read, Write};
        use std::net::TcpListener;

        /// Spawns a minimal single-request HTTP/1.1 server on loopback that ALWAYS answers with
        /// a 302 redirecting to `location`, regardless of the request path. Returns the server's
        /// `http://127.0.0.1:<port>/` base URL. The server thread serves exactly one connection
        /// then exits — sufficient for a single `store.get(...)` call, and avoids needing any
        /// async runtime/mock-server dependency for what is otherwise a one-shot fixed response.
        fn spawn_redirect_server(location: &str) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
            let addr = listener.local_addr().unwrap();
            let location = location.to_string();
            std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    // Drain (a bounded amount of) the request so the client isn't left hanging on
                    // a half-written request when we respond; we don't need to parse it.
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf);
                    let body = "redirecting";
                    let response = format!(
                        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
            format!("http://127.0.0.1:{}/", addr.port())
        }

        /// A public-looking URL that 302-redirects to the cloud metadata endpoint must NOT be
        /// followed — the read must fail loud (any error is acceptable; a follow-and-succeed
        /// would be the bypass this test guards against), and the metadata endpoint's IP must not
        /// appear as if it were successfully fetched.
        ///
        /// `#[tokio::test]` (not `pollster::block_on`, unlike the rest of this crate's sync-facing
        /// tests) because `reqwest`'s async client performs its actual I/O on a tokio reactor —
        /// `pollster::block_on` drives a future to completion without providing one, which is fine
        /// for ziv's own futures (no I/O of their own) but panics ("there is no reactor running")
        /// once a real network call happens inside them, as it does here.
        #[tokio::test]
        async fn redirect_to_blocked_host_is_not_followed() {
            let base = spawn_redirect_server("http://169.254.169.254/latest/meta-data/");
            let url = Url::parse(&base).unwrap();
            // The server's own host (127.0.0.1) is loopback and would itself be blocked by the
            // SSRF guard, so this test goes through `build_http` directly with
            // `allow_internal=true` (equivalent to `--allow-internal-hosts`, simulating an
            // operator who trusts the ORIGIN host but whose origin turns out to be compromised /
            // misconfigured to redirect elsewhere) — the point under test is that the CLIENT
            // itself refuses to follow the redirect, independent of the origin-host SSRF check.
            let spec = build_http(&url, true).expect("building the store itself must succeed");
            let StoreSpec::Remote(remote) = spec else {
                panic!("expected Remote");
            };
            let path = object_store::path::Path::from("zarr.json");
            let result = remote.store.get(&path).await;
            assert!(
                result.is_err(),
                "expected the redirect to fail loud instead of being followed to \
                 169.254.169.254, got: {result:?}"
            );
        }

        /// Sanity check on the harness itself: a redirect to an ordinary (non-blocked-looking)
        /// location is *also* not followed — proving the fix is "redirects are refused,
        /// period", not something that happens to key off the target looking suspicious. See
        /// `redirect_to_blocked_host_is_not_followed` for why this is `#[tokio::test]`.
        #[tokio::test]
        async fn redirect_to_any_host_is_not_followed() {
            let base = spawn_redirect_server("http://example.com/somewhere-else");
            let url = Url::parse(&base).unwrap();
            let spec = build_http(&url, true).unwrap();
            let StoreSpec::Remote(remote) = spec else {
                panic!("expected Remote");
            };
            let path = object_store::path::Path::from("zarr.json");
            let result = remote.store.get(&path).await;
            assert!(
                result.is_err(),
                "expected ANY redirect to be refused rather than followed, got: {result:?}"
            );
        }
    }
}
