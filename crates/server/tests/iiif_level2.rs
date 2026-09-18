//! Route-level proof of every IIIF Image API 3.0 **level 2** requirement ziv's `info.json`
//! claims by advertising `"profile": "level2"`.
//!
//! These exist because the claim was, for a while, false. Running the official
//! `iiif-validate.py` against a live `ziv serve` (see `docs/conformance.md`) found six required
//! features missing — `png`, `!w,h`, `rotationBy90s`, a real `gray`, the json-ld media type and
//! the base-URI redirect — plus unknown identifiers and unknown qualities both answering 200.
//! The in-repo `iiif::conformance` assertions could not have caught any of them: they check the
//! SHAPE of `info.json`, not whether the server honours what that shape promises.
//!
//! Each test below names the compliance-table feature it pins, so that if the profile ever
//! changes, the set of tests that must change is obvious.
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

fn app() -> axum::Router {
    let img = zarr_core::ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
    server::app(server::AppState::new(
        Arc::new(tiling::ZarrTileEngine::new(img)),
        "http://test".into(),
    ))
}

async fn get(uri: &str) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    get_with(uri, &[]).await
}

async fn get_with(
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let mut builder = Request::builder().uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let res = app()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes, headers)
}

/// Feature `jsonldMediaType` (required at level 1 and 2).
#[tokio::test]
async fn info_json_honours_the_ld_json_media_type() {
    let (status, _, headers) = get("/iiif/default/info.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/json");

    let (status, _, headers) = get_with(
        "/iiif/default/info.json",
        &[("accept", "application/ld+json")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ct = headers["content-type"].to_str().unwrap();
    assert!(
        ct.starts_with("application/ld+json"),
        "must answer ld+json when asked for it, got: {ct}"
    );
    assert!(
        ct.contains("http://iiif.io/api/image/3/context.json"),
        "ld+json response must carry the Image API context as its profile, got: {ct}"
    );
}

/// Feature `baseUriRedirect` (required at level 1 and 2).
#[tokio::test]
async fn bare_identifier_redirects_to_info_json() {
    let (status, _, headers) = get("/iiif/default").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "default/info.json");
}

/// Feature `sizeByConfinedWh` — `!w,h` (required at level 2).
///
/// The fixture is 64x64, so confining to a non-square box must preserve the aspect ratio and fit
/// inside it, rather than distorting to exactly the box like `w,h` does.
#[tokio::test]
async fn confined_size_fits_inside_the_box_preserving_aspect() {
    let (status, body, _) = get("/iiif/default/full/!40,20/0/default.jpg").await;
    assert_eq!(status, StatusCode::OK, "!w,h must be served at level 2");
    let (w, h) = jpeg_dimensions(&body);
    assert_eq!(
        (w, h),
        (20, 20),
        "square source confined to 40x20 fits as 20x20"
    );

    // Contrast: `w,h` distorts to exactly the requested box.
    let (status, body, _) = get("/iiif/default/full/40,20/0/default.jpg").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(jpeg_dimensions(&body), (40, 20));
}

/// Feature `rotationBy90s` (required at level 2); `rotationArbitrary` and mirroring are optional
/// and must still be refused.
#[tokio::test]
async fn quarter_turns_are_served_and_other_angles_refused() {
    for rot in ["0", "90", "180", "270"] {
        let (status, body, headers) =
            get(&format!("/iiif/default/full/max/{rot}/default.jpg")).await;
        assert_eq!(status, StatusCode::OK, "rotation {rot} must be served");
        assert_eq!(headers["content-type"], "image/jpeg");
        assert_eq!(
            &body[0..2],
            &[0xFF, 0xD8],
            "rotation {rot} must return JPEG"
        );
    }
    for rot in ["45", "!90", "1"] {
        let (status, _, _) = get(&format!("/iiif/default/full/max/{rot}/default.jpg")).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "rotation {rot} is not implemented and must be refused"
        );
    }
}

/// A 90-degree turn on a NON-square region must swap the output dimensions — the check that
/// actually distinguishes real rotation from returning the unrotated image.
#[tokio::test]
async fn quarter_turn_swaps_output_dimensions() {
    let (status, body, _) = get("/iiif/default/0,0,40,20/max/0/default.jpg").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(jpeg_dimensions(&body), (40, 20));

    let (status, body, _) = get("/iiif/default/0,0,40,20/max/90/default.jpg").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        jpeg_dimensions(&body),
        (20, 40),
        "a quarter turn must swap width and height"
    );
}

/// Format `png` (required at level 2), served with the right media type and real PNG bytes.
#[tokio::test]
async fn png_format_is_served() {
    let (status, body, headers) = get("/iiif/default/full/max/0/default.png").await;
    assert_eq!(status, StatusCode::OK, "png is required at level 2");
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(
        &body[0..8],
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
    );
}

/// Optional formats stay refused — level 2 requires jpg and png only.
#[tokio::test]
async fn optional_formats_are_refused() {
    for ext in ["webp", "tif", "gif", "pdf", "jp2"] {
        let (status, _, _) = get(&format!("/iiif/default/full/max/0/default.{ext}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{ext} must be refused");
    }
}

/// Qualities `color` and `gray` (both required at level 2), plus optional `bitonal`.
/// `gray` must actually be grey — the earlier bug was accepting the parameter and ignoring it.
#[tokio::test]
async fn gray_quality_returns_a_grey_image() {
    for q in ["default", "color", "gray", "bitonal"] {
        let (status, body, _) = get(&format!("/iiif/default/full/max/0/{q}.png")).await;
        assert_eq!(status, StatusCode::OK, "quality {q} must be served");
        let px = decode_png_rgb(&body);
        if q == "gray" || q == "bitonal" {
            assert!(
                px.chunks_exact(3).all(|p| p[0] == p[1] && p[1] == p[2]),
                "quality {q} must produce equal R=G=B for every pixel"
            );
        }
        if q == "bitonal" {
            assert!(
                px.chunks_exact(3).all(|p| p[0] == 0 || p[0] == 255),
                "bitonal must be pure black or white"
            );
        }
    }
}

/// An unrecognized quality must be refused, not silently rendered as `default`.
#[tokio::test]
async fn unknown_quality_is_400() {
    for q in ["sharpen", "oUusK8", "colour"] {
        let (status, _, _) = get(&format!("/iiif/default/full/max/0/{q}.jpg")).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "quality {q} must be refused"
        );
    }
}

/// An identifier this service does not serve must 404 — on the image route AND on info.json.
/// Previously every string was a valid identifier that rendered the default image.
#[tokio::test]
async fn unknown_identifier_is_404() {
    let (status, _, _) = get("/iiif/not-a-real-identifier/full/max/0/default.jpg").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _, _) = get("/iiif/not-a-real-identifier/info.json").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The identifiers ziv DOES serve keep working.
    let (status, _, _) = get("/iiif/default/info.json").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = get("/iiif/@z=0/full/max/0/default.jpg").await;
    assert_eq!(status, StatusCode::OK);
}

/// Rotation is part of the cache key. Without it, the first-rendered rotation would be served
/// for every other rotation of the same region and size.
#[tokio::test]
async fn rotation_is_part_of_the_cache_key() {
    let app = app();
    let fetch = |uri: &'static str, app: axum::Router| async move {
        let res = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    };
    let unrotated = fetch("/iiif/default/0,0,40,20/max/0/default.jpg", app.clone()).await;
    let rotated = fetch("/iiif/default/0,0,40,20/max/90/default.jpg", app.clone()).await;
    assert_ne!(
        unrotated, rotated,
        "the 90-degree response must not be the cached 0-degree one"
    );
    assert_eq!(jpeg_dimensions(&unrotated), (40, 20));
    assert_eq!(jpeg_dimensions(&rotated), (20, 40));
}

/// Reads width/height out of a JPEG's SOF marker, so the tests can assert on the actual encoded
/// image rather than trusting the request echoed back.
fn jpeg_dimensions(jpeg: &[u8]) -> (u16, u16) {
    let mut i = 2; // skip SOI
    while i + 9 < jpeg.len() {
        assert_eq!(jpeg[i], 0xFF, "expected a JPEG marker at offset {i}");
        let marker = jpeg[i + 1];
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        // SOF0..SOF3 / SOF5..SOF7 / SOF9..SOF11 / SOF13..SOF15 all carry height then width.
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let h = u16::from_be_bytes([jpeg[i + 5], jpeg[i + 6]]);
            let w = u16::from_be_bytes([jpeg[i + 7], jpeg[i + 8]]);
            return (w, h);
        }
        i += 2 + len;
    }
    panic!("no SOF marker found in JPEG");
}

/// Decodes a PNG to raw RGB8, so quality assertions inspect real pixels rather than byte counts.
fn decode_png_rgb(bytes: &[u8]) -> Vec<u8> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    assert_eq!(info.color_type, png::ColorType::Rgb);
    buf.truncate(info.buffer_size());
    buf
}
