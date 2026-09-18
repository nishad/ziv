//! IIIF Image API 3.0 conformance (CI-runnable, hermetic layer — see
//! `iiif::conformance::assert_info_json_conforms`'s module doc for what this does and does not
//! replace) run against a REAL `info.json` served by the live axum router (`server::router`) over
//! a real `ZarrTileEngine` reading a committed fixture — not a synthetic `ImageInfo`, so a
//! regression anywhere in the id-construction/serialization path (`routes::info_handler`,
//! `origin::resolve_base_url`, `ImageInfo::to_info_json`) is caught here even if each piece's own
//! unit tests still pass individually.
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use iiif::assert_info_json_conforms;
use server::{router, AppState};
use tiling::ZarrTileEngine;
use tower::ServiceExt;
use zarr_core::ZarrImage;

fn state() -> AppState {
    let img = ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
    AppState::new(Arc::new(ZarrTileEngine::new(img)), "http://test".into())
}

/// The live `/iiif/{proj}/info.json` route (level2, dynamic serving) must conform to the IIIF
/// Image API 3.0 shape requirements.
#[tokio::test]
async fn live_level2_info_json_conforms_to_iiif_3_0() {
    let app = router(state());
    let res = app
        .oneshot(
            Request::builder()
                .uri("/iiif/default/info.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), axum::http::StatusCode::OK);

    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let violations = assert_info_json_conforms(&info);
    assert!(
        violations.is_empty(),
        "live level2 info.json failed IIIF 3.0 conformance: {violations:#?}\ninfo.json: {info:#}"
    );
    assert_eq!(info["profile"], "level2");
}

/// Same check against a dynamic-projection identifier's info.json (`@z=..,c=..`) — the dynamic
/// path constructs `ImageInfo` independently of the `default` path in `ZarrTileEngine::image_info`
/// (both ultimately call the same method, but this guards against a future divergence).
#[tokio::test]
async fn live_dynamic_projection_info_json_conforms_to_iiif_3_0() {
    let app = router(state());
    let res = app
        .oneshot(
            Request::builder()
                .uri("/iiif/@z=0,c=0:ff0000:0-255/info.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), axum::http::StatusCode::OK);

    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let violations = assert_info_json_conforms(&info);
    assert!(
        violations.is_empty(),
        "live dynamic-projection info.json failed IIIF 3.0 conformance: {violations:#?}"
    );
}
