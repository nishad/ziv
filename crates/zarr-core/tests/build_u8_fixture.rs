//! Builds a committed u8-only OME-Zarr 0.4 test fixture (single channel, small), used to
//! exercise the `DType::U8` read arm end-to-end via `ZarrImage::open`/`read_region_f64`.
//! Mirrors `build_fixture.rs` (no Python/zarr dependency, raw zarr v2 uncompressed chunks).
//! Run: `cargo test -p ziv-zarr-core --test build_u8_fixture`. Builds into a private temp
//! directory and checks it against the committed fixture; set `ZIV_REGENERATE_FIXTURES=1` to
//! update the committed copy.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_u8.ome.zarr")
}

// Single channel, single level, 16x16, dtype u8, horizontal gradient (value == x).
const SIZE: u64 = 16;

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
    {"color":"FFFFFF","window":{"start":0,"end":15},"active":true}
  ]}
}"#;

fn zarray() -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[{s},{s}],\"chunks\":[{s},{s}],\"dtype\":\"|u1\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}",
        s = SIZE
    )
}

/// value(y, x) = x (horizontal gradient), one byte per pixel.
fn chunk_bytes() -> Vec<u8> {
    let mut buf = Vec::with_capacity((SIZE * SIZE) as usize);
    for _y in 0..SIZE {
        for x in 0..SIZE {
            buf.push(x as u8);
        }
    }
    buf
}

fn build(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(root.join(".zattrs"), ZATTRS).unwrap();

    let l0 = root.join("0");
    fs::create_dir_all(&l0).unwrap();
    fs::write(l0.join(".zarray"), zarray()).unwrap();
    fs::write(l0.join("0.0"), chunk_bytes()).unwrap();
}

#[test]
fn builds_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-zarr-core --test build_u8_fixture",
        |root| {
            build(root);
            assert!(root.join(".zgroup").exists());
            assert!(root.join("0/.zarray").exists());
            assert_eq!(
                fs::read(root.join("0/0.0")).unwrap().len(),
                (SIZE * SIZE) as usize
            );
        },
    );
}
