//! Route-level behaviour of the `@label=` and `@overlay=` identifier components.
//!
//! Fixture: `tests/fixtures/sample_labels.ome.zarr` — a 64x64 image with a `nuclei` label whose
//! four quadrants are valued 0/1/2/3. The rendering itself is proven in `tiling::engine`; what is
//! pinned here is the part only the HTTP layer decides: which status code each kind of bad
//! request gets, and that two label identifiers are not the same cache entry.
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

fn app() -> axum::Router {
    let img = zarr_core::ZarrImage::open("../../tests/fixtures/sample_labels.ome.zarr").unwrap();
    server::app(server::AppState::new(
        Arc::new(tiling::ZarrTileEngine::new(img)),
        "http://test".into(),
    ))
}

async fn get(uri: &str) -> (StatusCode, Vec<u8>) {
    let res = app()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

#[tokio::test]
async fn a_label_identifier_renders() {
    let (status, body) = get("/iiif/@label=nuclei/full/max/0/default.png").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..8], b"\x89PNG\r\n\x1a\n");
}

/// The palette is part of the identifier, so both spellings must be servable.
#[tokio::test]
async fn both_palettes_are_servable_and_differ() {
    let (s1, distinct) = get("/iiif/@label=nuclei/full/max/0/default.png").await;
    let (s2, table) = get("/iiif/@label=nuclei:table/full/max/0/default.png").await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
    assert_ne!(
        distinct, table,
        "the two palettes must not collide in the tile cache"
    );
}

/// A label the image does not carry is a 404: the grammar was fine, the resource is not there.
/// This is the same reasoning as an unknown projection identifier.
#[tokio::test]
async fn an_unknown_label_is_404() {
    let (status, body) = get("/iiif/@label=mitochondria/full/max/0/default.png").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        String::from_utf8_lossy(&body).contains("mitochondria"),
        "the error should name what was not found"
    );
}

/// A palette name that does not exist is a 400, not a 404 and not a silent fallback: the
/// identifier named a real label, but asked for it in a way this service does not define.
#[tokio::test]
async fn an_unknown_palette_is_400() {
    let (status, body) = get("/iiif/@label=nuclei:rainbow/full/max/0/default.png").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("rainbow"));
}

/// The overlay spelling draws the mask over the image, and is a different picture from either
/// half — a distinct URL that must return a distinct body.
#[tokio::test]
async fn overlay_is_its_own_render() {
    let (s1, image) = get("/iiif/default/full/max/0/default.png").await;
    let (s2, label) = get("/iiif/@label=nuclei:table/full/max/0/default.png").await;
    let (s3, over) = get("/iiif/@overlay=nuclei:table/full/max/0/default.png").await;
    assert_eq!(
        (s1, s2, s3),
        (StatusCode::OK, StatusCode::OK, StatusCode::OK)
    );
    assert_ne!(over, image);
    assert_ne!(over, label);
}

/// An opacity of 0 draws nothing, which must be the image itself rather than an error or a black
/// frame. It is also the clearest available proof that the overlay's base really is the ordinary
/// image render.
#[tokio::test]
async fn an_overlay_at_zero_opacity_is_the_plain_image() {
    let (_, image) = get("/iiif/default/full/max/0/default.png").await;
    let (status, over) = get("/iiif/@overlay=nuclei:table:0/full/max/0/default.png").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(over, image);
}

/// Out of range is a 400, not a clamp. A request that silently became a different request is
/// indistinguishable from the feature not working.
#[tokio::test]
async fn an_out_of_range_opacity_is_400() {
    for spec in ["@overlay=nuclei:table:2", "@overlay=nuclei:table:-1"] {
        let (status, body) = get(&format!("/iiif/{spec}/full/max/0/default.png")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{spec}");
        assert!(String::from_utf8_lossy(&body).contains("opacity"), "{spec}");
    }
}

/// An unparseable opacity is not a 400: the identifier parser never fails, so `:banana` simply
/// leaves the opacity unset and the renderer uses its default. Documented rather than accidental.
#[tokio::test]
async fn an_unparseable_opacity_falls_back_to_the_default() {
    let (s1, banana) = get("/iiif/@overlay=nuclei:table:banana/full/max/0/default.png").await;
    let (s2, plain) = get("/iiif/@overlay=nuclei:table/full/max/0/default.png").await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
    assert_eq!(banana, plain);
}

/// `label=` composes with the rest of the identifier grammar rather than replacing it.
#[tokio::test]
async fn a_label_composes_with_the_plane_selectors() {
    let (status, _) = get("/iiif/@label=nuclei,z=0,t=0/full/max/0/default.png").await;
    assert_eq!(status, StatusCode::OK);
}

/// Region, size, rotation and format still apply to a label render — it is an ordinary IIIF
/// request that happens to draw a different array.
#[tokio::test]
async fn the_iiif_request_parameters_still_apply() {
    let (status, body) = get("/iiif/@label=nuclei/0,0,32,32/16,/90/gray.png").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..8], b"\x89PNG\r\n\x1a\n");
}

/// `info.json` describes the image, and a label is rendered through the image's own coordinate
/// space, so the document must be identical whichever identifier asks for it. Anything else would
/// tell a viewer to request tiles the other identifier cannot serve.
#[tokio::test]
async fn info_json_is_the_same_document_for_a_label_identifier() {
    let (s1, image) = get("/iiif/default/info.json").await;
    let (s2, label) = get("/iiif/@label=nuclei/info.json").await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
    let strip = |b: Vec<u8>| {
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        let mut m = v.as_object().unwrap().clone();
        m.remove("id");
        m
    };
    assert_eq!(strip(image), strip(label));
}
