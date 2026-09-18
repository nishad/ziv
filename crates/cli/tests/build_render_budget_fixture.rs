//! Builds a synthetic OME-Zarr fixture with a declared-only single level sized to sit ABOVE
//! `iiif::level0::MAX_WHOLE_IMAGE_PIXELS` (64,000,000 px) but comfortably below both
//! `ZarrTileEngine`'s own general `MAX_OUTPUT_PIXELS`/`MAX_READ_PIXELS` bound (16384*16384 px)
//! and `MAX_OUTPUT_DIM`/`MAX_WHOLE_IMAGE_EDGE` (65535 px per edge) — the exact window where `ziv
//! render`'s own whole-image budget check is the thing that refuses the request, rather than the
//! engine's coarser general safety cap firing first with a plainer message.
//!
//! The dimensions (19120x13350) are the design spec's own illustrative example of a
//! full-resolution image the size budget exists for. Like its siblings
//! (`crates/tiling/tests/build_huge_level_fixture.rs` and `..._no_omero_fixture.rs`), this is
//! declared-shape-only: no chunk data is written, because `ziv render`'s budget check (via
//! `ZarrTileEngine::output_size`, which resolves purely from declared array shape/metadata) never
//! reads a chunk before refusing. `omero` metadata is present so `ZarrTileEngine::new()`'s
//! auto-stretch path is skipped and opening this fixture stays instant regardless of its declared
//! size.
//!
//! Run: `cargo test -p ziv --test build_render_budget_fixture`. Builds into a private temp
//! directory and checks it against the committed fixture; set `ZIV_REGENERATE_FIXTURES=1` to
//! update the committed copy.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/sample_over_render_budget.ome.zarr")
}

const WIDTH: u64 = 19_120;
const HEIGHT: u64 = 13_350;

const ZATTRS: &str = r#"{
  "multiscales": [{
    "version": "0.4",
    "axes": [
      {"name":"y","type":"space"}, {"name":"x","type":"space"}
    ],
    "datasets": [
      {"path":"0","coordinateTransformations":[{"type":"scale","scale":[1,1]}]}
    ]
  }],
  "omero": { "channels": [
    {"color":"FFFFFF","window":{"start":0,"end":255},"active":true}
  ]}
}"#;

fn zarray() -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[{HEIGHT},{WIDTH}],\"chunks\":[{HEIGHT},{WIDTH}],\
         \"dtype\":\"|u1\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\
         \"filters\":null,\"dimension_separator\":\".\"}}"
    )
}

fn build(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(root.join(".zattrs"), ZATTRS).unwrap();

    // Declared-only: metadata for a 19120x13350 single-chunk level, no chunk data written.
    let l0 = root.join("0");
    fs::create_dir_all(&l0).unwrap();
    fs::write(l0.join(".zarray"), zarray()).unwrap();
}

#[test]
fn builds_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv --test build_render_budget_fixture",
        |root| {
            build(root);
            assert!(root.join(".zgroup").exists());
            assert!(root.join("0/.zarray").exists());
        },
    );
}
