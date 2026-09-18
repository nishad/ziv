//! Liveness (`/healthz`) and readiness (`/readyz`) endpoints.
//!
//! Both are UNAUTHED — health checks are fired by load balancers/orchestrators (Kubernetes
//! kubelet, an ALB target-group check, etc.) that carry no application credentials, so gating
//! either behind auth would make the deployment un-probeable. Both are also NOT cached (no
//! `Cache-Control`/`ETag`): a stale cached readiness result is actively harmful (a load balancer
//! acting on a minute-old "ready" during an outage), so responses fall through the tile
//! cache/HTTP-cache-header machinery entirely — these handlers never touch `AppState::tile_cache`
//! and never set `Cache-Control`.
//!
//! - `GET /healthz` — liveness: 200 always, as long as the process can answer HTTP at all. This
//!   does NOT check the backing store; a process that's up but whose store went unreachable is
//!   still "alive" (restarting it wouldn't help) but not "ready" — that distinction is exactly
//!   what `/readyz` is for.
//! - `GET /readyz` — readiness: 200 when the catalogue is built and usable, 503 otherwise, with a
//!   JSON body reporting how many images the catalogue holds and how many are currently open.
//!
//!   Readiness deliberately does NOT mean "every image is open". Opening is lazy for every source
//!   but argv, so a server with a hundred catalogued images is ready the moment it can resolve
//!   them; a probe that waited for all of them to open would wait forever, and on a slow remote
//!   would fail the deployment for no reason. `images` is `null` when a source cannot enumerate
//!   (a lazy directory root), because guessing a number would be worse than saying so. See
//!   `AppState::is_ready` for where the flag lives and how a test can flip it.
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Router};

use crate::routes::AppState;

pub fn health_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    StatusCode::OK
}

async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    let images = match state.registry.list() {
        crate::registry::Listing::Enumerable(names) => serde_json::json!(names.len()),
        // A lazy source cannot say how many images it has without walking a filesystem, and
        // guessing a number here would be worse than saying so.
        crate::registry::Listing::NotListable => serde_json::Value::Null,
    };
    let body = serde_json::json!({
        "ready": state.is_ready(),
        "images": images,
        "open": state.registry.open_count().await,
    });
    let status = if state.is_ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&body).expect("readiness serializes to valid JSON"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
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
    async fn healthz_is_always_200() {
        let app = health_router(state());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn healthz_has_no_cache_headers() {
        let app = health_router(state());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(!res
            .headers()
            .contains_key(axum::http::header::CACHE_CONTROL));
        assert!(!res.headers().contains_key(axum::http::header::ETAG));
    }

    #[tokio::test]
    async fn readyz_is_200_when_ready() {
        let app = health_router(state());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_is_503_when_not_ready() {
        let app = health_router(state().not_ready());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn healthz_is_200_even_when_not_ready() {
        // Liveness is independent of readiness: a not-ready backing store doesn't mean the
        // process itself is dead.
        let app = health_router(state().not_ready());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
