//! Builds a committed f32 OME-Zarr 0.4 test fixture whose values are NOT in 0..255 (a
//! 0.0..1000.0 gradient), so percentile auto-stretch is genuinely exercised end-to-end rather
//! than degenerating to a no-op window. Mirrors `build_fixture.rs` (no Python/zarr dependency,
//! raw zarr v2 uncompressed chunks, little-endian f32 bytes, dtype "<f4").
//! Run: `cargo test -p ziv-zarr-core --test build_f32_fixture`. Builds into a private temp
//! directory and checks it against the committed fixture; set `ZIV_REGENERATE_FIXTURES=1` to
//! update the committed copy.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_f32.ome.zarr")
}

// Single channel, single level, 16x16, dtype f32, horizontal gradient value = x * (1000/15)
// so it spans [0.0, 1000.0] and is NOT in the 0..255 byte range.
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
  }]
}"#;

fn zarray() -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[{s},{s}],\"chunks\":[{s},{s}],\"dtype\":\"<f4\",\"compressor\":null,\"fill_value\":0.0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}",
        s = SIZE
    )
}

/// value(y, x) = x * (1000.0 / (SIZE-1)) (horizontal gradient spanning 0.0..=1000.0), 4
/// little-endian bytes per pixel.
fn chunk_bytes() -> Vec<u8> {
    let mut buf = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for _y in 0..SIZE {
        for x in 0..SIZE {
            let v: f32 = x as f32 * (1000.0 / (SIZE - 1) as f32);
            buf.extend_from_slice(&v.to_le_bytes());
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
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-zarr-core --test build_f32_fixture",
        |root| {
            build(root);
            assert!(root.join(".zgroup").exists());
            assert!(root.join("0/.zarray").exists());
            assert_eq!(
                fs::read(root.join("0/0.0")).unwrap().len(),
                (SIZE * SIZE * 4) as usize
            );
        },
    );
}
