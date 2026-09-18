//! `/ziv/dimensions.json` — the endpoint the built-in viewer builds its z/t sliders and channel
//! toggles from.
//!
//! This is deliberately NOT part of `info.json`. That document is a IIIF Image API description of
//! a 2D image, and ziv advertises `level2` conformance against the official validator; hanging
//! ziv-specific properties off it would let a viewer feature put that claim at risk. The tests
//! here therefore also assert the separation holds.
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use server::{AppState, AuthConfig};
use tower::ServiceExt;

fn app_for(fixture: &str) -> axum::Router {
    let img = zarr_core::ZarrImage::open(fixture).unwrap();
    server::app(AppState::new(
        Arc::new(tiling::ZarrTileEngine::new(img)),
        "http://test".into(),
    ))
}

async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let res = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

const MULTIDIM: &str = "../../tests/fixtures/sample_multidim.ome.zarr";
const FLAT: &str = "../../tests/fixtures/sample_u8.ome.zarr";
const BROKEN_LABEL: &str = "../../tests/fixtures/sample_broken_label.ome.zarr";

/// The multi-dimensional fixture is 3 x 3 x 5 (t x c x z) with three labelled, coloured channels,
/// so every field has a value that could only come from reading the image.
#[tokio::test]
async fn reports_axis_extents_and_channel_catalogue() {
    let (status, d) = get_json(app_for(MULTIDIM), "/ziv/dimensions.json").await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(d["sizeT"], 3);
    assert_eq!(d["sizeZ"], 5);
    assert_eq!(d["sizeC"], 3);

    // The defaults must be where the `default` projection actually renders, so the controls open
    // on the image already displayed instead of jumping on first interaction. `default_projection`
    // picks the middle z plane.
    assert_eq!(d["defaultT"], 0);
    assert_eq!(d["defaultZ"], 2, "default z is the middle plane of 5");

    let channels = d["channels"].as_array().expect("channels array");
    assert_eq!(channels.len(), 3);
    for (i, (label, color)) in [("Red", "FF0000"), ("Green", "00FF00"), ("Blue", "0000FF")]
        .iter()
        .enumerate()
    {
        assert_eq!(channels[i]["index"], i);
        assert_eq!(channels[i]["label"], *label, "label comes from omero");
        assert_eq!(
            channels[i]["color"], *color,
            "colour is what the channel actually composites with"
        );
        assert_eq!(channels[i]["active"], true);
        assert_eq!(channels[i]["window"]["start"], 0.0);
        assert_eq!(channels[i]["window"]["end"], 255.0);
    }
}

/// A plain 2D single-channel image must report extents of 1, which is what tells the viewer to
/// show no controls at all rather than sliders with one position.
#[tokio::test]
async fn a_flat_image_reports_single_extents() {
    let (status, d) = get_json(app_for(FLAT), "/ziv/dimensions.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(d["sizeT"], 1);
    assert_eq!(d["sizeZ"], 1);
    assert_eq!(d["sizeC"], 1);
    assert_eq!(d["defaultT"], 0);
    assert_eq!(d["defaultZ"], 0);
}

/// A label `labels/.zattrs` declares but that fails to open must not vanish without a trace: it is
/// absent from `labels` (the same as before this fix — an unopenable label is not servable), but
/// present in `labelOpenFailures` with its name and why, as data for any API consumer, not just
/// the exporter's own `--labels` warning (`exporter::plan_views`, tested in
/// `crates/exporter/src/views.rs`). A working sibling label must be unaffected: still listed, and
/// carrying nothing about its broken neighbour.
#[tokio::test]
async fn a_label_that_fails_to_open_is_named_in_label_open_failures_not_silently_dropped() {
    let (status, d) = get_json(app_for(BROKEN_LABEL), "/ziv/dimensions.json").await;
    assert_eq!(status, StatusCode::OK);

    let labels = d["labels"].as_array().expect("labels array");
    assert_eq!(labels.len(), 1, "{labels:?}");
    assert_eq!(labels[0]["name"], "good");

    let failures = d["labelOpenFailures"]
        .as_array()
        .expect("labelOpenFailures array");
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0]["name"], "bad");
    assert!(
        failures[0]["reason"]
            .as_str()
            .unwrap()
            .contains("multiscales"),
        "{:?}",
        failures[0]["reason"]
    );
}

/// Every index the endpoint advertises must actually render. This is the property that keeps the
/// viewer's controls honest: `sizeC` can exceed the RENDERABLE channel catalogue (with no omero
/// metadata the catalogue is capped at three), and a control offering a channel the compositor
/// ignores would silently produce a blank image.
#[tokio::test]
async fn every_advertised_channel_actually_renders() {
    let app = app_for(MULTIDIM);
    let (_, d) = get_json(app.clone(), "/ziv/dimensions.json").await;
    for ch in d["channels"].as_array().unwrap() {
        let idx = ch["index"].as_u64().unwrap();
        let uri = format!("/iiif/%40c%3D{idx}/full/max/0/default.jpg");
        let res = app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "advertised channel {idx} must render"
        );
    }
}

/// The advertised extents must bound what the image routes accept: one past each end is a 400, so
/// a slider built from `sizeZ`/`sizeT` can never request something the server refuses.
#[tokio::test]
async fn advertised_extents_match_what_the_image_routes_accept() {
    let app = app_for(MULTIDIM);
    let (_, d) = get_json(app.clone(), "/ziv/dimensions.json").await;
    let (size_z, size_t) = (d["sizeZ"].as_u64().unwrap(), d["sizeT"].as_u64().unwrap());

    for (ident, expected) in [
        (format!("%40z%3D{}", size_z - 1), StatusCode::OK),
        (format!("%40z%3D{size_z}"), StatusCode::BAD_REQUEST),
        (format!("%40t%3D{}", size_t - 1), StatusCode::OK),
        (format!("%40t%3D{size_t}"), StatusCode::BAD_REQUEST),
    ] {
        let uri = format!("/iiif/{ident}/full/max/0/default.jpg");
        let res = app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), expected, "for {uri}");
    }
}

/// The endpoint describes the image, so it must sit behind the same gate as tiles and info.json.
/// An unauthenticated caller learning the image's shape would be a leak the other routes prevent.
#[tokio::test]
async fn is_gated_by_auth_exactly_like_the_image_routes() {
    let img = zarr_core::ZarrImage::open(MULTIDIM).unwrap();
    let state = AppState::new(
        Arc::new(tiling::ZarrTileEngine::new(img)),
        "http://test".into(),
    )
    .with_auth(AuthConfig {
        bearer: Some("correct-token".into()),
        hmac_secret: None,
    });
    let app = server::app(state);

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ziv/dimensions.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::UNAUTHORIZED,
        "dimensions must not be readable without the credential that tiles require"
    );

    let res = app
        .oneshot(
            Request::builder()
                .uri("/ziv/dimensions.json")
                .header("authorization", "Bearer correct-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

/// info.json must stay a pure IIIF document: the viewer's needs travel on their own endpoint, and
/// this is what would fail if someone later "simplified" by merging them.
#[tokio::test]
async fn info_json_carries_no_ziv_specific_fields() {
    let (_, info) = get_json(app_for(MULTIDIM), "/iiif/default/info.json").await;
    for key in [
        "sizeT", "sizeZ", "sizeC", "channels", "defaultT", "defaultZ",
    ] {
        assert!(
            info.get(key).is_none(),
            "info.json must not carry {key}; it belongs to /ziv/dimensions.json"
        );
    }
    assert_eq!(info["profile"], "level2");
}

/// Conditional GET, matching info.json's behaviour — the response is a pure function of the image
/// the process was started against and cannot change while it runs.
#[tokio::test]
async fn supports_conditional_get() {
    let app = app_for(MULTIDIM);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ziv/dimensions.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let etag = res.headers()["etag"].to_str().unwrap().to_string();

    let res = app
        .oneshot(
            Request::builder()
                .uri("/ziv/dimensions.json")
                .header("if-none-match", &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
}
