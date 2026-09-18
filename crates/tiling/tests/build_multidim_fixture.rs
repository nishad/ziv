//! Builds `tests/fixtures/sample_multidim.ome.zarr`: the only committed fixture with real extent
//! on **t, c AND z** (3 x 3 x 5). Every other fixture is a single plane, so nothing could
//! exercise the z/t sliders or the channel toggles in the built-in viewer, nor the `@z=`/`@t=`/
//! `@c=` projection identifiers those controls drive, against more than one possible answer.
//!
//! Each (t, c, z) plane is a FLAT value, deliberately:
//!
//! ```text
//! value(t, c, z) = 80 + 40*t + 15*z        (identical for every channel and pixel)
//! ```
//!
//! A flat plane makes every assertion exact rather than approximate — a test can name the byte it
//! expects instead of eyeballing a gradient — and the three channels are given pure red, green and
//! blue in `omero` with a 0-255 window, so the composited output of a channel selection is
//! computable by hand:
//!
//! | active channels | composite RGB |
//! |---|---|
//! | 0 (red)         | `(v, 0, 0)` |
//! | 0 + 1           | `(v, v, 0)` (yellow) |
//! | 0 + 1 + 2       | `(v, v, v)` (grey)   |
//!
//! ...where `v` depends only on t and z. So a browser test can sample ONE pixel of the rendered
//! canvas and know, exactly, both which channels are on and which plane is showing — the property
//! that makes an end-to-end viewer test meaningful rather than a screenshot-shaped smoke test.
//!
//! Run: `cargo test -p ziv-tiling --test build_multidim_fixture`. Builds into a private temp
//! directory and checks it against the committed fixture; set `ZIV_REGENERATE_FIXTURES=1` to
//! update the committed copy.
use std::fs;
use std::path::{Path, PathBuf};

const SIZE_T: u64 = 3;
const SIZE_C: u64 = 3;
const SIZE_Z: u64 = 5;
const SIZE_YX: u64 = 32;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_multidim.ome.zarr")
}

/// The single value filling plane (t, z), for every channel and every pixel. Channels differ only
/// by their `omero` colour, so the channel a pixel came from is readable from the output hue.
pub fn plane_value(t: u64, z: u64) -> u8 {
    (80 + 40 * t + 15 * z) as u8
}

const ZATTRS: &str = r#"{
  "multiscales": [{
    "version": "0.4",
    "axes": [
      {"name":"t","type":"time"}, {"name":"c","type":"channel"},
      {"name":"z","type":"space"}, {"name":"y","type":"space"}, {"name":"x","type":"space"}
    ],
    "datasets": [
      {"path":"0","coordinateTransformations":[{"type":"scale","scale":[1,1,1,1,1]}]}
    ]
  }],
  "omero": { "channels": [
    {"label":"Red","color":"FF0000","window":{"start":0,"end":255},"active":true},
    {"label":"Green","color":"00FF00","window":{"start":0,"end":255},"active":true},
    {"label":"Blue","color":"0000FF","window":{"start":0,"end":255},"active":true}
  ]}
}"#;

fn zarray() -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[{SIZE_T},{SIZE_C},{SIZE_Z},{SIZE_YX},{SIZE_YX}],\
         \"chunks\":[1,1,1,{SIZE_YX},{SIZE_YX}],\"dtype\":\"|u1\",\"compressor\":null,\
         \"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}"
    )
}

fn build(root: &Path) -> u64 {
    fs::create_dir_all(root.join("0")).unwrap();
    fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(root.join(".zattrs"), ZATTRS).unwrap();
    fs::write(root.join("0/.zarray"), zarray()).unwrap();

    let mut written = 0;
    for t in 0..SIZE_T {
        for c in 0..SIZE_C {
            for z in 0..SIZE_Z {
                let plane = vec![plane_value(t, z); (SIZE_YX * SIZE_YX) as usize];
                fs::write(root.join(format!("0/{t}.{c}.{z}.0.0")), plane).unwrap();
                written += 1;
            }
        }
    }
    written
}

#[test]
fn builds_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-tiling --test build_multidim_fixture",
        |root| {
            let written = build(root);
            assert_eq!(written, SIZE_T * SIZE_C * SIZE_Z, "one chunk per (t, c, z)");
            assert!(root.join("0/2.2.4.0.0").exists(), "last plane must exist");
        },
    );
}
