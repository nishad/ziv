//! Builds a synthetic OME-Zarr fixture with a declared-huge single level (100000x100000) used
//! ONLY to exercise `ZarrTileEngine::plan()`'s `MAX_READ_PIXELS` bound — `plan()` computes the
//! read window purely from the declared array shape/metadata and never touches chunk data, so
//! no actual chunk files are written (writing 100000x100000 real pixels would be impractical).
//! Any attempt to actually READ a chunk from this fixture would fail (no chunk files exist);
//! the test that uses it only calls `plan()`, which must reject the request with
//! `TileError::OutOfRange` before any chunk read is attempted.
//!
//! Has `omero` metadata (even though it's a single channel with a trivial window) SPECIFICALLY
//! so `ZarrTileEngine::new()`'s auto-stretch path (`image.omero().is_none()`) is skipped — that
//! path is scoped to testing `plan()`'s bound in isolation, not `new()`'s own read guard.
//! (`new()`'s coarsest-level auto-stretch read is now ALSO bounded by `MAX_READ_PIXELS`, with a
//! safe dtype-natural-range fallback when skipped — see `ZarrTileEngine::new` in
//! `crates/tiling/src/engine.rs` and the NO-omero sibling fixture
//! `build_huge_level_no_omero_fixture.rs`, which exercises that guard directly.)
//!
//! Mirrors `zarr-core/tests/build_fixture.rs`. Run: `cargo test -p ziv-tiling --test
//! build_huge_level_fixture`. Builds into a private temp directory and checks it against the
//! committed fixture; set `ZIV_REGENERATE_FIXTURES=1` to update the committed copy.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_huge_level.ome.zarr")
}

const SIZE: u64 = 100_000;

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
        "{{\"zarr_format\":2,\"shape\":[{s},{s}],\"chunks\":[{s},{s}],\"dtype\":\"|u1\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}",
        s = SIZE
    )
}

fn build(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(root.join(".zattrs"), ZATTRS).unwrap();

    // Declared-only: metadata for a 100000x100000 single-chunk level, no chunk data written.
    let l0 = root.join("0");
    fs::create_dir_all(&l0).unwrap();
    fs::write(l0.join(".zarray"), zarray()).unwrap();
}

#[test]
fn builds_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-tiling --test build_huge_level_fixture",
        |root| {
            build(root);
            assert!(root.join(".zgroup").exists());
            assert!(root.join("0/.zarray").exists());
        },
    );
}
