//! Builds a synthetic OME-Zarr 0.4 fixture whose axes are NOT in `…,Y,X` order, to lock down
//! that the tiling engine reads Y/X dims via the `AxesModel` rather than positionally
//! (`shape[len-2]`/`shape[len-1]`). Mirrors `zarr-core/tests/build_fixture.rs` (no Python/zarr
//! dependency, raw zarr v2 chunk files). Run: `cargo test -p ziv-tiling --test build_axes_fixture`.
//! Builds into a private temp directory and checks it against the committed fixture; set
//! `ZIV_REGENERATE_FIXTURES=1` to update the committed copy.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    // tests/ is at the crate root; the shared fixture dir is at the workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_axes_cxy.ome.zarr")
}

// Single level, single chunk, axes order [c, x, y] (X before Y, neither positionally last-two
// as Y-then-X). Shape [c=2, x=WIDTH, y=HEIGHT] with WIDTH != HEIGHT so a positional read swaps
// (and mis-sizes) width/height rather than merely transposing same-sized dims.
const WIDTH: u64 = 40; // size along X
const HEIGHT: u64 = 20; // size along Y

const ZATTRS: &str = r#"{
  "multiscales": [{
    "version": "0.4",
    "axes": [
      {"name":"c","type":"channel"}, {"name":"x","type":"space"}, {"name":"y","type":"space"}
    ],
    "datasets": [
      {"path":"0","coordinateTransformations":[{"type":"scale","scale":[1,1,1]}]}
    ]
  }],
  "omero": { "channels": [
    {"color":"FF0000","window":{"start":0,"end":39},"active":true},
    {"color":"00FF00","window":{"start":0,"end":19},"active":true}
  ]}
}"#;

fn zarray() -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[2,{w},{h}],\"chunks\":[1,{w},{h}],\"dtype\":\"<u2\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}",
        w = WIDTH,
        h = HEIGHT
    )
}

/// value(channel, x, y): ch0 -> x, ch1 -> y (element order is C-order over [c,x,y]).
fn chunk_bytes(c: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity((WIDTH * HEIGHT * 2) as usize);
    for x in 0..WIDTH {
        for y in 0..HEIGHT {
            let v: u16 = if c == 0 { x as u16 } else { y as u16 };
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
    for c in 0..2u64 {
        let name = format!("{c}.0.0");
        fs::write(l0.join(name), chunk_bytes(c)).unwrap();
    }
}

#[test]
fn builds_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-tiling --test build_axes_fixture",
        |root| {
            build(root);
            assert!(root.join(".zgroup").exists());
            assert!(root.join("0/.zarray").exists());
            assert_eq!(
                fs::read(root.join("0/0.0.0")).unwrap().len(),
                (WIDTH * HEIGHT * 2) as usize
            );
        },
    );
}
