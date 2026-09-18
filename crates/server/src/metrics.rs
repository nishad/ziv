//! Prometheus `/metrics` endpoint via the `metrics` facade + `metrics-exporter-prometheus`'s
//! `PrometheusHandle::render()`.
//!
//! `PrometheusBuilder::install_recorder()` installs a PROCESS-GLOBAL recorder (the `metrics`
//! crate is a global facade, same model as `log`/`tracing`) and returns a `PrometheusHandle`
//! whose `.render()` produces the Prometheus text-exposition body on demand — there is no
//! separate HTTP listener to manage; `/metrics` here just calls `.render()` inside a normal axum
//! handler. Because the recorder is process-global, `recorder()` is guarded with a
//! `std::sync::OnceLock` so that constructing more than one `AppState` (e.g. across tests in the
//! same process) doesn't panic on a duplicate global-recorder install; every `AppState` shares
//! the one global handle.
//!
//! `/metrics` is UNAUTHED here (consistent with `/healthz`/`/readyz`) — in production this
//! endpoint should be network-restricted (firewalled to the scrape network / not exposed on the
//! public listener) since it can reveal operational detail (request rates, cache hit ratio) that
//! isn't secret but also isn't meant for arbitrary internet callers; ziv itself doesn't implement
//! that restriction, it's a deployment-level concern (e.g. a separate bind address or a proxy
//! rule), noted here rather than silently assumed.
//!
//! ## Counters/gauges/histograms emitted
//! - `ziv_http_requests_total{method,path,status}` — counter, incremented once per completed
//!   HTTP request by [`track_metrics`] (a `axum::middleware::from_fn` wrapping the whole app —
//!   see `lib::app`), labeled by method, route path (the axum route PATTERN, e.g.
//!   `/iiif/{proj}/info.json`, not the raw URI, so per-tile cardinality doesn't blow up the
//!   metric), and response status code.
//! - `ziv_http_request_duration_seconds{method,path}` — histogram of per-request latency,
//!   recorded by the same middleware.
//! - `ziv_tile_cache_hits_total` / `ziv_tile_cache_misses_total` — counters, incremented at the
//!   exact point `tile_handler` consults `AppState`'s tile cache (see `routes::tile_handler`).
//! - `ziv_render_permits_available` — gauge, the render semaphore's currently-available permit
//!   count (`AppState::render_semaphore`), sampled on every `/metrics` scrape.
use axum::{
    extract::{MatchedPath, Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::OnceLock;
use std::time::Instant;

use crate::routes::AppState;

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install (once per process) the global Prometheus recorder and return the handle used to
/// render `/metrics` text. Safe to call repeatedly (e.g. once per test's `AppState`/router
/// construction) — only the first call actually installs the recorder; subsequent calls reuse
/// the same global handle.
pub fn recorder() -> PrometheusHandle {
    HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .expect("failed to install the global Prometheus recorder")
        })
        .clone()
}

pub fn metrics_router(state: AppState) -> Router {
    // Ensure the recorder is installed even if no request has touched a counter yet, so
    // `/metrics` never 500s on a cold-started process.
    recorder();
    Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(state)
}

async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let available_permits = state.render_semaphore.available_permits();
    metrics::gauge!("ziv_render_permits_available").set(available_permits as f64);
    // Sampled at scrape time rather than maintained on every open and eviction. A gauge that only
    // matters when somebody reads it should be computed when somebody reads it, and this keeps the
    // open path free of bookkeeping. Counterpart to `ziv_image_opens_total` and
    // `ziv_image_evictions_total`, which are emitted where the events happen.
    metrics::gauge!("ziv_images_open").set(state.registry.open_count().await as f64);
    recorder().render()
}

/// `axum::middleware::from_fn` handler recording `ziv_http_requests_total` (by method/path/
/// status) and `ziv_http_request_duration_seconds` (by method/path) for EVERY request that
/// reaches it. Applied around the whole app in `lib::app`/`app_with_governance`, so it observes
/// health/metrics/viewer traffic too, not just IIIF routes.
///
/// Uses `MatchedPath` (the axum ROUTE PATTERN, e.g. `/iiif/{proj}/{region}/{size}/{rotation}/
/// {quality_dot_format}`) rather than the raw request URI for the `path` label — using the raw
/// URI would give every distinct tile request its own label value, which is unbounded
/// cardinality and exactly what a Prometheus label must never be. Falls back to `"unmatched"` for
/// a request that didn't match any route (axum's own 404), which is a single fixed label value,
/// not attacker-controlled.
pub async fn track_metrics(req: Request, next: Next) -> Response {
    let method = req.method().to_string();
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());

    let start = Instant::now();
    let response = next.run(req).await;
    let elapsed = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    metrics::counter!(
        "ziv_http_requests_total",
        "method" => method.clone(),
        "path" => path.clone(),
        "status" => status,
    )
    .increment(1);
    metrics::histogram!(
        "ziv_http_request_duration_seconds",
        "method" => method,
        "path" => path,
    )
    .record(elapsed);

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn state() -> AppState {
        let img = zarr_core::ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
        AppState::new(
            Arc::new(tiling::ZarrTileEngine::new(img)),
            "http://test".into(),
        )
    }

    #[tokio::test]
    async fn metrics_endpoint_returns_prometheus_text() {
        let _guard = GAUGE_LOCK.lock().await;
        let app = metrics_router(state());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("ziv_render_permits_available"),
            "expected the render-permits gauge in: {body}"
        );
    }

    /// Serializes tests that read back `ziv_render_permits_available` (or any other gauge on
    /// the process-global Prometheus recorder — see the module doc). The recorder is shared by
    /// every test in this binary that hits `/metrics`, so without this lock a concurrently
    /// running test's own scrape can overwrite a gauge between one test setting it and reading
    /// it back, producing flaky failures. Analogous to `auth::tests::temp_env`'s serialization
    /// of process-global env-var state.
    ///
    /// WARNING for future tests: any new test that sets then reads a gauge value (as opposed to
    /// just asserting a counter/histogram exists) MUST hold `GAUGE_LOCK` for the duration of the
    /// set-then-read, or it will be racy against `metrics_reflects_render_permits_gauge` and any
    /// other lock-holding test.
    static GAUGE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn metrics_reflects_render_permits_gauge() {
        let _guard = GAUGE_LOCK.lock().await;
        let state = state().with_render_permits(3);
        let app = metrics_router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("ziv_render_permits_available 3"),
            "expected gauge value 3 in: {body}"
        );
    }

    /// `track_metrics` (wired into `crate::app` in `lib.rs`) must actually increment
    /// `ziv_http_requests_total` and record `ziv_http_request_duration_seconds` for a real
    /// request, with the axum ROUTE PATTERN (not the raw URI) as the `path` label — proven by
    /// requesting a tile with dynamic path segments and asserting the label is the pattern
    /// (`/iiif/{proj}/{region}/{size}/{rotation}/{quality_dot_format}`), not the literal request
    /// path.
    #[tokio::test]
    async fn requests_increment_http_request_counters_and_histogram() {
        let _guard = GAUGE_LOCK.lock().await;
        let app = crate::app(state());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let metrics_res = metrics_router(state())
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(metrics_res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("ziv_http_requests_total")
                && body.contains(
                    "path=\"/iiif/{proj}/{region}/{size}/{rotation}/{quality_dot_format}\""
                ),
            "expected a labeled request counter for the tile route pattern in: {body}"
        );
        assert!(
            body.contains("ziv_http_request_duration_seconds"),
            "expected the latency histogram in: {body}"
        );
    }
}
