//! Builds a synthetic OME-Zarr fixture identical in shape to `sample_huge_level.ome.zarr`
//! (declared-only 100000x100000 single level, no chunk data) but WITHOUT `omero` metadata. This
//! is the exact shape that used to trigger `ZarrTileEngine::new()`'s unconditional
//! coarsest-level percentile auto-stretch read (see `build_huge_level_fixture.rs`'s doc comment
//! for the history of that gap) — P2's remote-object-store support made a huge image with no
//! `omero` metadata reachable via `serve <remote-url>`, turning what was once a theoretical gap
//! into a real startup DoS/OOM vector for a full synchronous network read before the server
//! binds.
//!
//! Used by `tiling::engine::tests::new_skips_unbounded_autostretch_read_on_huge_no_omero_level`
//! to prove `ZarrTileEngine::new()` now guards this read with `MAX_READ_PIXELS` and falls back
//! to a safe default window rather than attempting to read 10 billion pixels.
//!
//! Mirrors `build_huge_level_fixture.rs`. Run: `cargo test -p ziv-tiling --test
//! build_huge_level_no_omero_fixture`. Builds into a private temp directory and checks it
//! against the committed fixture; set `ZIV_REGENERATE_FIXTURES=1` to update the committed copy.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/sample_huge_level_no_omero.ome.zarr")
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
  }]
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
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-tiling --test build_huge_level_no_omero_fixture",
        |root| {
            build(root);
            assert!(root.join(".zgroup").exists());
            assert!(root.join("0/.zarray").exists());
        },
    );
}
