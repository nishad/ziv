//! Builds the committed OME-Zarr 0.4 test fixture from source (no Python/zarr).
//! Run: `cargo test -p ziv-zarr-core --test build_fixture`. Builds into a private temp directory
//! and checks it against the committed fixture; set `ZIV_REGENERATE_FIXTURES=1` to update the
//! committed copy.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    // tests/ is at the crate root; the shared fixture dir is at the workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_v04.ome.zarr")
}

fn zarray(shape_yx: u64) -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[1,2,1,{s},{s}],\"chunks\":[1,1,1,32,32],\"dtype\":\"<u2\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}",
        s = shape_yx
    )
}

const ZATTRS: &str = r#"{
  "multiscales": [{
    "version": "0.4",
    "axes": [
      {"name":"t","type":"time"}, {"name":"c","type":"channel"},
      {"name":"z","type":"space"}, {"name":"y","type":"space"}, {"name":"x","type":"space"}
    ],
    "datasets": [
      {"path":"0","coordinateTransformations":[{"type":"scale","scale":[1,1,1,1,1]}]},
      {"path":"1","coordinateTransformations":[{"type":"scale","scale":[1,1,1,2,2]}]}
    ]
  }],
  "omero": { "channels": [
    {"color":"FF0000","window":{"start":0,"end":63},"active":true},
    {"color":"00FF00","window":{"start":0,"end":63},"active":true}
  ]}
}"#;

/// value(channel, global_y, global_x, downsample): ch0 -> x, ch1 -> y, times the downsample factor.
fn value(c: u64, gy: u64, gx: u64, ds: u64) -> u16 {
    (if c == 0 { gx } else { gy } * ds) as u16
}

/// Write one 32x32 uint16 chunk (2048 bytes) at grid (cy, cx) for channel c, downsample ds.
fn chunk_bytes(c: u64, cy: u64, cx: u64, ds: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(2048);
    for y in 0..32u64 {
        for x in 0..32u64 {
            let gy = cy * 32 + y;
            let gx = cx * 32 + x;
            buf.extend_from_slice(&value(c, gy, gx, ds).to_le_bytes());
        }
    }
    assert_eq!(buf.len(), 2048);
    buf
}

fn build(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(root.join(".zattrs"), ZATTRS).unwrap();

    // Level 0: 64x64, grid 1x2x1x2x2 (cy,cx in 0..2). ds = 1.
    let l0 = root.join("0");
    fs::create_dir_all(&l0).unwrap();
    fs::write(l0.join(".zarray"), zarray(64)).unwrap();
    for c in 0..2u64 {
        for cy in 0..2u64 {
            for cx in 0..2u64 {
                let name = format!("0.{c}.0.{cy}.{cx}");
                fs::write(l0.join(name), chunk_bytes(c, cy, cx, 1)).unwrap();
            }
        }
    }

    // Level 1: 32x32, grid 1x2x1x1x1 (single yx chunk). ds = 2 (coarser).
    let l1 = root.join("1");
    fs::create_dir_all(&l1).unwrap();
    fs::write(l1.join(".zarray"), zarray(32)).unwrap();
    for c in 0..2u64 {
        let name = format!("0.{c}.0.0.0");
        fs::write(l1.join(name), chunk_bytes(c, 0, 0, 2)).unwrap();
    }
}

#[test]
fn builds_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-zarr-core --test build_fixture",
        |root| {
            build(root);
            assert!(root.join(".zgroup").exists());
            assert!(root.join("0/.zarray").exists());
            assert_eq!(fs::read(root.join("0/0.0.0.0.0")).unwrap().len(), 2048);
            assert!(root.join("1/0.1.0.0.0").exists());
        },
    );
}
