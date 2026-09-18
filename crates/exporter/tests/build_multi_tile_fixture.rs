//! Builds a committed OME-Zarr 0.4 fixture LARGE ENOUGH that its levels actually tile
//! against `ZarrTileEngine`'s fixed 512px tile size — every other committed fixture is
//! <=64x64, so every one of ITS levels fits in a single 512px tile and the writer/DZI
//! end-to-end tests never exercise the multi-tile enumerator branch (coordinate regions,
//! canonical `w,h` sizing, edge clipping, non-`full` regions) against a REAL `TileEngine`,
//! only against synthetic `ImageInfo` values in `enumerate.rs`'s own unit tests.
//!
//! Shape: single channel, 2D (y,x) axes, dtype u8. Two levels:
//! - level 0: 1024x1024 -> a 2x2 tile grid at the 512px tile size (genuinely multi-tile).
//! - level 1: 512x512 -> a 1x1 tile grid (fits exactly at the `<=` boundary: not the
//!   "fits-in-one-tile" branch's strict `<`, but the tiled branch's degenerate single-cell
//!   grid — see `enumerate.rs`'s `getTileUrl` port). Deliberately included so the FIX2
//!   whole-image-derivative boundary condition (`level_w <= tile_w && level_h <= tile_h`)
//!   is exercised end-to-end, not just in the enumerator's pure unit tests.
//!
//! Kept small on disk via `fill_value: 0` + chunks 256x256: level 0 has a 4x4 chunk grid
//! (16 chunks) but only 4 are written (a diagonal-ish sparse set); the rest silently read
//! back as the fill value per ordinary Zarr v2 semantics (no chunk file needed). Level 1
//! has a 2x2 chunk grid (4 chunks), all written (small: 64KB each). Total on-disk chunk
//! payload: 8 * 65536 bytes = 512KiB uncompressed — larger than the other (<=64x64)
//! fixtures but still small, and required to get a real multi-tile pyramid.
//!
//! Run: `cargo test -p ziv-exporter --test build_multi_tile_fixture`. Builds into a private temp
//! directory and checks it against the committed fixture; set `ZIV_REGENERATE_FIXTURES=1` to
//! update the committed copy. Mirrors `crates/zarr-core/tests/build_fixture.rs` /
//! `build_u8_fixture.rs`.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_multi_tile.ome.zarr")
}

const CHUNK: u64 = 256;

const ZATTRS: &str = r#"{
  "multiscales": [{
    "version": "0.4",
    "axes": [
      {"name":"y","type":"space"}, {"name":"x","type":"space"}
    ],
    "datasets": [
      {"path":"0","coordinateTransformations":[{"type":"scale","scale":[1,1]}]},
      {"path":"1","coordinateTransformations":[{"type":"scale","scale":[2,2]}]}
    ]
  }],
  "omero": { "channels": [
    {"color":"FFFFFF","window":{"start":0,"end":255},"active":true}
  ]}
}"#;

fn zarray(shape_yx: u64) -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[{s},{s}],\"chunks\":[{c},{c}],\"dtype\":\"|u1\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}",
        s = shape_yx,
        c = CHUNK
    )
}

/// value(global_y, global_x) = a cheap, position-dependent gradient distinguishable per
/// chunk (not visually interesting — just needs to differ so a pixel spot-check is
/// meaningful): `(gx ^ gy) & 0xFF`.
fn value(gy: u64, gx: u64) -> u8 {
    ((gx ^ gy) & 0xFF) as u8
}

fn chunk_bytes(cy: u64, cx: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity((CHUNK * CHUNK) as usize);
    for y in 0..CHUNK {
        for x in 0..CHUNK {
            let gy = cy * CHUNK + y;
            let gx = cx * CHUNK + x;
            buf.push(value(gy, gx));
        }
    }
    buf
}

fn build(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(root.join(".zattrs"), ZATTRS).unwrap();

    // Level 0: 1024x1024, chunk grid 4x4 (16 chunks @ 256px). Only a sparse diagonal-ish
    // set is written; the rest fall back to fill_value=0 (no file needed).
    let l0 = root.join("0");
    fs::create_dir_all(&l0).unwrap();
    fs::write(l0.join(".zarray"), zarray(1024)).unwrap();
    let written_l0: &[(u64, u64)] = &[(0, 0), (0, 3), (1, 1), (3, 3)];
    for &(cy, cx) in written_l0 {
        let name = format!("{cy}.{cx}");
        fs::write(l0.join(name), chunk_bytes(cy, cx)).unwrap();
    }

    // Level 1: 512x512, chunk grid 2x2 (4 chunks @ 256px) — all written (small).
    let l1 = root.join("1");
    fs::create_dir_all(&l1).unwrap();
    fs::write(l1.join(".zarray"), zarray(512)).unwrap();
    for cy in 0..2u64 {
        for cx in 0..2u64 {
            let name = format!("{cy}.{cx}");
            fs::write(l1.join(name), chunk_bytes(cy, cx)).unwrap();
        }
    }
}

#[test]
fn builds_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-exporter --test build_multi_tile_fixture",
        |root| {
            build(root);
            assert!(root.join(".zgroup").exists());
            assert!(root.join("0/.zarray").exists());
            assert_eq!(
                fs::read(root.join("0/0.0")).unwrap().len(),
                (CHUNK * CHUNK) as usize
            );
            assert!(root.join("1/0.0").exists());
            assert!(root.join("1/1.1").exists());
            // A chunk deliberately left as fill_value (not written) — confirms the sparse layout.
            assert!(!root.join("0/2.2").exists());
        },
    );
}
