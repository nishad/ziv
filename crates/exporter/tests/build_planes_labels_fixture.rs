//! Builds `tests/fixtures/sample_planes_labels.ome.zarr`: the only committed fixture with BOTH
//! `sizeZ > 1` AND a label image.
//!
//! `sample_multidim.ome.zarr` has 5 z-planes and no labels; `sample_labels.ome.zarr` has one
//! label and `sizeZ: 1`. A `--planes --labels` export walks plane trees and overlay trees
//! together (every overlay tree lives under `planes/{z}/labels/{i}/`, one per exported plane),
//! and nothing committed before this exercised that combination at all: not the exporter's own
//! conformance sweep, and not the viewer's "move the z slider while an overlay is selected" path,
//! which only ever ran at z=0 for lack of a second plane to move to.
//!
//! Shape: 4 z-planes, one channel, 32x32 (y, x), single level (no pyramid: `sample_multidim`
//! already shows a single level is enough to exercise `--planes` end to end, and this fixture's
//! job is the plane x label interaction, not pyramid resampling). One label image, `cells`, with
//! the SAME z extent as its parent: `crates/tiling/src/engine.rs`'s `label_rgba` clamps a label's
//! z to its own `size_z`, so a label narrower than its parent (as in `sample_labels`) would render
//! the same plane at every z and defeat the point of this fixture.
//!
//! Both the image and the label are flat per z-plane, and deliberately spaced far apart (50 and 1
//! respectively) so a pixel spot-check can name which plane it read from either the intensity or
//! the overlay:
//!
//! ```text
//! image_value(z) = 40 + 50*z   (0, 1, 2, 3 -> 40, 90, 140, 190)
//! label_value(z) = z + 1       (0, 1, 2, 3 -> 1, 2, 3, 4; the default "distinct" overlay
//!                                palette colours every non-zero value differently, so no
//!                                `image-label` colour table is needed here)
//! ```
//!
//! Run: `cargo test -p ziv-exporter --test build_planes_labels_fixture`. Builds into a private
//! temp directory and checks it against the committed fixture; set `ZIV_REGENERATE_FIXTURES=1`
//! to update the committed copy.
use std::fs;
use std::path::{Path, PathBuf};

const SIZE_Z: u64 = 4;
const SIZE_YX: u64 = 32;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_planes_labels.ome.zarr")
}

const IMAGE_ZATTRS: &str = r#"{
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
    {"label":"Grey","color":"FFFFFF","window":{"start":0,"end":255},"active":true}
  ]}
}"#;

const LABEL_ZATTRS: &str = r#"{
  "multiscales": [{
    "version": "0.4",
    "axes": [
      {"name":"t","type":"time"}, {"name":"c","type":"channel"},
      {"name":"z","type":"space"}, {"name":"y","type":"space"}, {"name":"x","type":"space"}
    ],
    "datasets": [
      {"path":"0","coordinateTransformations":[{"type":"scale","scale":[1,1,1,1,1]}]}
    ]
  }]
}"#;

fn zarray() -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[1,1,{SIZE_Z},{SIZE_YX},{SIZE_YX}],\
         \"chunks\":[1,1,1,{SIZE_YX},{SIZE_YX}],\"dtype\":\"|u1\",\"compressor\":null,\
         \"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}"
    )
}

/// The flat value filling image plane `z`, for every pixel.
fn image_value(z: u64) -> u8 {
    (40 + 50 * z) as u8
}

/// The flat value filling label plane `z`, for every pixel. Never 0 (background/transparent), so
/// every plane's overlay is visibly non-empty.
fn label_value(z: u64) -> u8 {
    (z + 1) as u8
}

fn write_group(dir: &Path, zattrs: &str, plane_value: fn(u64) -> u8) {
    fs::create_dir_all(dir.join("0")).unwrap();
    fs::write(dir.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(dir.join(".zattrs"), zattrs).unwrap();
    fs::write(dir.join("0/.zarray"), zarray()).unwrap();
    for z in 0..SIZE_Z {
        let plane = vec![plane_value(z); (SIZE_YX * SIZE_YX) as usize];
        fs::write(dir.join(format!("0/0.0.{z}.0.0")), plane).unwrap();
    }
}

fn build(root: &Path) {
    write_group(root, IMAGE_ZATTRS, image_value);

    let labels = root.join("labels");
    fs::create_dir_all(&labels).unwrap();
    fs::write(labels.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(labels.join(".zattrs"), r#"{"labels": ["cells"]}"#).unwrap();
    write_group(&labels.join("cells"), LABEL_ZATTRS, label_value);
}

#[test]
fn builds_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-exporter --test build_planes_labels_fixture",
        |root| {
            build(root);
            assert!(root.join(".zgroup").exists());
            assert!(root.join("0/.zarray").exists());
            assert!(root.join("0/0.0.3.0.0").exists(), "z=3 must exist");
            assert!(!root.join("0/0.0.4.0.0").exists(), "only 4 z-planes");
            assert!(root.join("labels/.zattrs").exists());
            assert!(root.join("labels/cells/0/.zarray").exists());
            assert!(
                root.join("labels/cells/0/0.0.3.0.0").exists(),
                "the label covers every plane the image has"
            );
            assert_eq!(
                fs::read(root.join("0/0.0.0.0.0")).unwrap(),
                vec![image_value(0); (SIZE_YX * SIZE_YX) as usize]
            );
            assert_eq!(
                fs::read(root.join("labels/cells/0/0.0.2.0.0")).unwrap(),
                vec![label_value(2); (SIZE_YX * SIZE_YX) as usize]
            );
        },
    );
}
