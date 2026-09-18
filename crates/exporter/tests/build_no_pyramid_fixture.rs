//! Builds a committed, single-level ("no pyramid") OME-Zarr fixture bigger than
//! `ZarrTileEngine`'s fixed 512px tile size, with two z-planes so a `--planes` export's plan has
//! more than one view (`plan.views.len() > 1`) and therefore writes a manifest.
//!
//! Every other multi-plane fixture (`sample_multidim.ome.zarr`, 32x32) is small enough that its
//! sole level already fits in one 512px tile, so `enumerate_request_space` finds a whole-image
//! derivative on its own. This is the one committed image whose sole level does not: 600x600, ONE
//! level (no downsampled dataset at all), so at the fixed 512px tile size that sole level is
//! genuinely multi-tile and `enumerate_request_space` reports no `full/...` derivative whatsoever.
//! This image is within the whole-image budget, though, so `manifest::largest_whole_image` never
//! even looks at `enumerate_request_space` for it: it points straight at
//! `full/{maxWidth},{maxHeight}`, guaranteed by `writer::write_tree`'s level 0 contract regardless
//! of what OpenSeadragon's own request space contains.
//! `enumerate::smallest_level_whole_image` is reached by the manifest only when a tree is OVER
//! that budget — see `manifest::largest_whole_image`'s own doc comment.
//!
//! Shape: 2 z-planes, single channel, 600x600 (y, x). Kept small on disk the same way
//! `build_multi_tile_fixture.rs` is: `fill_value: 0` + 300x300 chunks, only ONE of each plane's 4
//! chunks actually written (the rest read back as the fill value, ordinary sparse-chunk Zarr v2
//! semantics).
//!
//! Run: `cargo test -p ziv-exporter --test build_no_pyramid_fixture`. Builds into a private temp
//! directory and checks it against the committed fixture; set `ZIV_REGENERATE_FIXTURES=1` to
//! update the committed copy.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_no_pyramid.ome.zarr")
}

const SIZE_Z: u64 = 2;
const SIZE_YX: u64 = 600;
const CHUNK: u64 = 300;

const ZATTRS: &str = r#"{
  "multiscales": [{
    "version": "0.4",
    "axes": [
      {"name":"z","type":"space"}, {"name":"y","type":"space"}, {"name":"x","type":"space"}
    ],
    "datasets": [
      {"path":"0","coordinateTransformations":[{"type":"scale","scale":[1,1,1]}]}
    ]
  }],
  "omero": { "channels": [
    {"color":"FFFFFF","window":{"start":0,"end":255},"active":true}
  ]}
}"#;

fn zarray() -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[{SIZE_Z},{SIZE_YX},{SIZE_YX}],\"chunks\":[1,{CHUNK},{CHUNK}],\
         \"dtype\":\"|u1\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\
         \"dimension_separator\":\".\"}}"
    )
}

/// value(z) = a flat, z-dependent value so a pixel spot-check can tell planes apart.
fn plane_value(z: u64) -> u8 {
    (40 + 60 * z) as u8
}

fn build(root: &Path) {
    fs::create_dir_all(root.join("0")).unwrap();
    fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(root.join(".zattrs"), ZATTRS).unwrap();
    fs::write(root.join("0/.zarray"), zarray()).unwrap();

    // Only chunk (0,0) of each plane's 2x2 chunk grid is written; the rest fall back to
    // fill_value=0 — the same sparse trick `build_multi_tile_fixture.rs` uses to stay small.
    for z in 0..SIZE_Z {
        let chunk = vec![plane_value(z); (CHUNK * CHUNK) as usize];
        fs::write(root.join(format!("0/{z}.0.0")), chunk).unwrap();
    }
}

#[test]
fn builds_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-exporter --test build_no_pyramid_fixture",
        |root| {
            build(root);
            assert!(root.join(".zgroup").exists());
            assert!(root.join("0/.zarray").exists());
            assert!(root.join("0/0.0.0").exists());
            assert!(root.join("0/1.0.0").exists());
            // A chunk deliberately left as fill_value (not written) — confirms the sparse layout.
            assert!(!root.join("0/0.1.1").exists());
        },
    );
}
