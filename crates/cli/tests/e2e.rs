//! End-to-end smoke test: serves the committed OME-Zarr fixture over a real HTTP listener
//! (ephemeral port, actual TCP + axum::serve) and drives it with `reqwest`, exactly as a real
//! IIIF client would. This is the final proof that `zarr-core`, `tiling`, `iiif`, and `server`
//! are wired together correctly end-to-end, not just unit-tested in isolation.
use std::sync::Arc;

#[tokio::test]
async fn serves_info_and_tile_over_http() {
    let img = zarr_core::ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
    let engine = Arc::new(tiling::ZarrTileEngine::new(img));

    // Bind an ephemeral port and serve in the background.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = server::AppState::new(engine, format!("http://{addr}"));
    let app = server::app(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // info.json
    let base = format!("http://{addr}");
    let info_res = reqwest::get(format!("{base}/iiif/default/info.json"))
        .await
        .unwrap();
    // Observability layers must be present end-to-end over a real HTTP listener, not just in
    // in-process `oneshot` tests.
    assert!(info_res.headers().contains_key("x-request-id"));
    let info: serde_json::Value = info_res.json().await.unwrap();
    assert_eq!(info["profile"], "level2");
    assert_eq!(info["width"], 64);

    // a tile
    let bytes = reqwest::get(format!("{base}/iiif/default/full/max/0/default.jpg"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&bytes[0..2], &[0xFF, 0xD8]);
}

/// Two images from one command line, each reachable under its derived name, over a real listener.
///
/// The names come from `registry::name_from_path`, which strips a trailing `.ome.zarr`, so the two
/// fixtures land at `/i/sample_v04/` and `/i/sample_labels/`.
#[tokio::test]
async fn serves_two_images_from_one_process() {
    let source = server::registry::ExplicitSource::from_pairs(vec![
        (
            server::registry::name_from_path("../../tests/fixtures/sample_v04.ome.zarr").unwrap(),
            "../../tests/fixtures/sample_v04.ome.zarr".to_string(),
        ),
        (
            server::registry::name_from_path("../../tests/fixtures/sample_labels.ome.zarr")
                .unwrap(),
            "../../tests/fixtures/sample_labels.ome.zarr".to_string(),
        ),
    ])
    .unwrap();
    let registry = Arc::new(
        server::registry::ImageRegistry::builder()
            .source(Box::new(source))
            .build(),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = server::AppState::from_registry(registry, format!("http://{addr}"));
    let app = server::app(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}");

    let catalogue: serde_json::Value = reqwest::get(format!("{base}/ziv/images.json"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = catalogue["images"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["sample_labels", "sample_v04"]);

    // Both serve, and they serve different pictures.
    let mut bodies = Vec::new();
    for name in &names {
        let res = reqwest::get(format!(
            "{base}/i/{name}/iiif/default/full/max/0/default.png"
        ))
        .await
        .unwrap();
        assert_eq!(res.status(), 200, "{name}");
        bodies.push(res.bytes().await.unwrap());
    }
    assert_ne!(bodies[0], bodies[1], "two names must be two pictures");
}
