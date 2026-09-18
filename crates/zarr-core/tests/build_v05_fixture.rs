//! Builds a committed OME-Zarr 0.5-LAYOUT test fixture: Zarr V2 on-disk storage (`.zgroup` /
//! `.zattrs` / `.zarray`, uncompressed, dimension_separator ".") — same array format zarrs
//! already reads in the other fixtures — but with OME-Zarr 0.5-shaped attributes
//! (`{"ome": {"version": "0.5", "multiscales": [...]}}` instead of top-level `multiscales`).
//!
//! This is a genuine end-to-end exercise of the 0.5 metadata-parsing path (`parse_multiscale`'s
//! `attributes.ome.multiscales` branch, `zarr-core/src/metadata.rs`) because `zarrs::Group::open`
//! reads V2 `.zattrs` into the SAME `attributes()` map regardless of the JSON shape inside it —
//! the 0.5 vs 0.4 distinction here is entirely about attribute JSON shape, not array storage
//! format, so a V2-stored array with 0.5-shaped attributes is a faithful 0.5 fixture for the
//! metadata-parsing/open() path (P0 previously only covered this via a synthetic in-memory JSON
//! unit test, never through a real `ZarrImage::open()` call).
//!
//! Mirrors `build_fixture.rs`. Run: `cargo test -p ziv-zarr-core --test build_v05_fixture`.
//! Builds into a private temp directory and checks it against the committed fixture; set
//! `ZIV_REGENERATE_FIXTURES=1` to update the committed copy.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_v05.ome.zarr")
}

fn zarray(shape_yx: u64) -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[1,2,1,{s},{s}],\"chunks\":[1,1,1,16,16],\"dtype\":\"<u2\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}",
        s = shape_yx
    )
}

const ZATTRS: &str = r#"{
  "ome": {
    "version": "0.5",
    "multiscales": [{
      "axes": [
        {"name":"t","type":"time"}, {"name":"c","type":"channel"},
        {"name":"z","type":"space"}, {"name":"y","type":"space"}, {"name":"x","type":"space"}
      ],
      "datasets": [
        {"path":"0","coordinateTransformations":[{"type":"scale","scale":[1,1,1,1,1]}]}
      ]
    }],
    "omero": { "channels": [
      {"color":"FF0000","window":{"start":0,"end":31},"active":true},
      {"color":"00FF00","window":{"start":0,"end":31},"active":true}
    ]}
  }
}"#;

/// value(channel, y, x): ch0 -> x, ch1 -> y.
fn value(c: u64, y: u64, x: u64) -> u16 {
    (if c == 0 { x } else { y }) as u16
}

/// Write one 16x16 uint16 chunk (512 bytes) for channel c.
fn chunk_bytes(c: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(512);
    for y in 0..16u64 {
        for x in 0..16u64 {
            buf.extend_from_slice(&value(c, y, x).to_le_bytes());
        }
    }
    assert_eq!(buf.len(), 512);
    buf
}

fn build(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(root.join(".zattrs"), ZATTRS).unwrap();

    // Level 0: 16x16, grid 1x2x1x1x1 (single yx chunk per channel).
    let l0 = root.join("0");
    fs::create_dir_all(&l0).unwrap();
    fs::write(l0.join(".zarray"), zarray(16)).unwrap();
    for c in 0..2u64 {
        let name = format!("0.{c}.0.0.0");
        fs::write(l0.join(name), chunk_bytes(c)).unwrap();
    }
}

#[test]
fn builds_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-zarr-core --test build_v05_fixture",
        |root| {
            build(root);
            assert!(root.join(".zgroup").exists());
            assert!(root.join("0/.zarray").exists());
            assert_eq!(fs::read(root.join("0/0.0.0.0.0")).unwrap().len(), 512);
        },
    );
}
