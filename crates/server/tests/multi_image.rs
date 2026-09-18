//! Route-level behaviour of the `/i/{name}/…` mount.
//!
//! Two fixtures with different content stand in for a catalogue: `sample_v04` (2 channels, no
//! labels) and `sample_labels` (2 channels, one label). Every assertion below is about telling them
//! apart through the URL, because "serves many images" means nothing if two names can return the
//! same picture.
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use server::name::ImageName;
use server::registry::{ExplicitSource, ImageRegistry};
use tower::ServiceExt;

fn app() -> axum::Router {
    let registry = ImageRegistry::builder()
        .source(Box::new(
            ExplicitSource::from_pairs(vec![
                (
                    ImageName::parse("plain").unwrap(),
                    "../../tests/fixtures/sample_v04.ome.zarr".to_string(),
                ),
                (
                    ImageName::parse("nested/labelled").unwrap(),
                    "../../tests/fixtures/sample_labels.ome.zarr".to_string(),
                ),
            ])
            .unwrap(),
        ))
        .build();
    server::app(server::AppState::from_registry(
        Arc::new(registry),
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
async fn serves_a_tile_from_a_named_image() {
    let (status, body) = get("/i/plain/iiif/default/full/max/0/default.jpg").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..2], b"\xff\xd8", "a JPEG");
}

/// The reason names may contain unencoded slashes.
#[tokio::test]
async fn serves_a_tile_from_a_nested_name() {
    let (status, _) = get("/i/nested/labelled/iiif/default/full/max/0/default.png").await;
    assert_eq!(status, StatusCode::OK);
}

/// Two names, two pictures. If this passes with identical bodies, the mount is decorative.
#[tokio::test]
async fn two_names_return_two_different_images() {
    let (_, a) = get("/i/plain/iiif/default/full/max/0/default.png").await;
    let (_, b) = get("/i/nested/labelled/iiif/default/full/max/0/default.png").await;
    assert_ne!(a, b);
}

#[tokio::test]
async fn info_json_carries_the_mounted_id() {
    let (status, body) = get("/i/nested/labelled/iiif/default/info.json").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["id"].as_str().unwrap(),
        "http://test/i/nested/labelled/iiif/default"
    );
}

#[tokio::test]
async fn the_base_uri_redirects_to_info_json() {
    let res = app()
        .oneshot(
            Request::builder()
                .uri("/i/plain/iiif/default")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        res.headers().get("location").unwrap(),
        "/i/plain/iiif/default/info.json"
    );
}

#[tokio::test]
async fn an_unknown_image_is_404() {
    let (status, _) = get("/i/nope/iiif/default/full/max/0/default.jpg").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_malformed_name_is_404() {
    for uri in [
        "/i/iiif/default/full/max/0/default.jpg",
        "/i/a/../b/iiif/default/full/max/0/default.jpg",
        "/i/plain/full/max/0/default.jpg",
    ] {
        let (status, _) = get(uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
}

/// A multi-image server has no honest answer to "the image at the root", so it must not pretend to
/// have one.
#[tokio::test]
async fn a_multi_image_server_has_no_root_alias() {
    for uri in [
        "/iiif/default/full/max/0/default.jpg",
        "/iiif/default/info.json",
        "/iiif/default",
        "/ziv/dimensions.json",
    ] {
        let (status, _) = get(uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
}

/// The viewer builds its controls from this, so it has to describe the image the URL names.
#[tokio::test]
async fn dimensions_are_per_image() {
    let (s1, plain) = get("/i/plain/ziv/dimensions.json").await;
    let (s2, labelled) = get("/i/nested/labelled/ziv/dimensions.json").await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));

    let p: serde_json::Value = serde_json::from_slice(&plain).unwrap();
    let l: serde_json::Value = serde_json::from_slice(&labelled).unwrap();
    assert_eq!(p["labels"].as_array().unwrap().len(), 0);
    assert_eq!(l["labels"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn the_viewer_is_mounted_per_image() {
    for uri in ["/i/plain/viewer/", "/i/plain/viewer"] {
        let (status, body) = get(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(
            String::from_utf8_lossy(&body).contains("<title>ziv viewer</title>"),
            "{uri}"
        );
    }
}

/// The mounted viewer must fetch ITS image's endpoints, not the root's, or every image after the
/// first would build controls for the wrong array. The page derives them from its own URL rather
/// than being templated, so what is pinned here is that the derivation exists. That logic lives in
/// `viewer.js` (the page itself only loads it), so this fetches the script, not `index.html`.
#[tokio::test]
async fn the_mounted_viewer_derives_its_own_endpoints() {
    let (_, body) = get("/i/nested/labelled/viewer/viewer.js").await;
    let js = String::from_utf8_lossy(&body);
    assert!(
        js.contains("window.location.pathname"),
        "the viewer must derive its API base from its own URL"
    );
    // Deriving a base and then not using it is the bug this catches: an earlier version declared
    // DIMENSIONS correctly and still fetched the hardcoded root path beside it, so every mounted
    // viewer silently built controls from a 404. Assert the hardcoded paths are GONE, not merely
    // that a derivation exists somewhere in the file.
    for hardcoded in [r#"fetch("/ziv/dimensions.json""#, r#"var IIIF = "/iiif/""#] {
        assert!(
            !js.contains(hardcoded),
            "the viewer still hardcodes {hardcoded}, so a mounted viewer would read the root's \
             endpoints instead of its own"
        );
    }
}

#[tokio::test]
async fn viewer_assets_are_served_under_the_mount() {
    let (status, _) = get("/i/plain/viewer/openseadragon/openseadragon.min.js").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_catalogue_lists_every_image() {
    let (status, body) = get("/ziv/images.json").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["listable"], serde_json::json!(true));
    let names: Vec<&str> = v["images"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["nested/labelled", "plain"]);
    assert_eq!(v["images"][0]["href"], "/i/nested/labelled/");
}

/// Readiness is "the catalogue is built", not "every image is open", because opening is lazy for
/// every source but argv. A load balancer that waited for the latter would wait forever.
#[tokio::test]
async fn readyz_reports_the_catalogue() {
    let (status, body) = get("/readyz").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["images"], serde_json::json!(2));
    assert_eq!(v["open"], serde_json::json!(0));
}

/// The catalogue enumerates what exists, so unlike the viewer's inert static assets it belongs
/// inside the auth gate.
#[tokio::test]
async fn the_catalogue_is_auth_gated() {
    let registry = ImageRegistry::builder()
        .source(Box::new(
            ExplicitSource::from_pairs(vec![(
                ImageName::parse("plain").unwrap(),
                "../../tests/fixtures/sample_v04.ome.zarr".to_string(),
            )])
            .unwrap(),
        ))
        .build();
    let state = server::AppState::from_registry(Arc::new(registry), "http://test".into())
        .with_auth(server::AuthConfig {
            bearer: Some("t".into()),
            hmac_secret: None,
        });

    let unauthed = server::app(state.clone())
        .oneshot(
            Request::builder()
                .uri("/ziv/images.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthed.status(), StatusCode::UNAUTHORIZED);

    let authed = server::app(state)
        .oneshot(
            Request::builder()
                .uri("/ziv/images.json")
                .header("authorization", "Bearer t")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authed.status(), StatusCode::OK);
}
