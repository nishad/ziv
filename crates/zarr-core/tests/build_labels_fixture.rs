//! Builds the committed OME-Zarr 0.4 fixture that carries a `labels/` group.
//! Run: `cargo test -p ziv-zarr-core --test build_labels_fixture`. Idempotent: rewrites the tree.
//!
//! Two properties of real label images are deliberately baked in, because both are easy to get
//! wrong and neither shows up in a fixture where labels merely mirror their parent:
//!
//! 1. **The label pyramid is independent.** The image has two levels; `nuclei` has three. A level
//!    index chosen for one is meaningless for the other. (The IDR sample this was modelled on has
//!    three and four.)
//! 2. **Label values are identifiers, not intensities.** The label plane is four flat quadrants
//!    valued 0/1/2/3, so any resample that averages neighbouring values produces a value that is
//!    not in the image at all, and a test can say so exactly.
//! 3. **A label has one channel; its parent has two.** The image is a red horizontal ramp plus a
//!    blue vertical ramp, so a test can change the channel selection and see the overlay's BASE
//!    change under an unchanged mask — which is the property that separates an overlay from a
//!    picture with a mask painted on it.
//!
//! Value 0 has no colour-table entry, mirroring the convention that 0 is background.
//!
//! Run: `cargo test -p ziv-zarr-core --test build_labels_fixture`. Builds into a private temp
//! directory and checks it against the committed fixture; set `ZIV_REGENERATE_FIXTURES=1` to
//! update the committed copy.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_labels.ome.zarr")
}

fn zarray(shape_yx: u64, channels: u64, dtype: &str) -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[1,{channels},1,{s},{s}],\"chunks\":[1,1,1,32,32],\"dtype\":\"{dtype}\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}",
        s = shape_yx
    )
}

/// `datasets` entries for `count` levels, each halving y/x.
fn multiscales(count: usize) -> String {
    let datasets: Vec<String> = (0..count)
        .map(|l| {
            let s = 1u64 << l;
            format!(
                "{{\"path\":\"{l}\",\"coordinateTransformations\":[{{\"type\":\"scale\",\"scale\":[1,1,1,{s},{s}]}}]}}"
            )
        })
        .collect();
    format!(
        r#""multiscales": [{{
    "version": "0.4",
    "axes": [
      {{"name":"t","type":"time"}}, {{"name":"c","type":"channel"}},
      {{"name":"z","type":"space"}}, {{"name":"y","type":"space"}}, {{"name":"x","type":"space"}}
    ],
    "datasets": [{}]
  }}]"#,
        datasets.join(",")
    )
}

fn image_zattrs() -> String {
    format!(
        r#"{{
  {},
  "omero": {{ "channels": [
    {{"label":"Across","color":"FF0000","window":{{"start":0,"end":255}},"active":true}},
    {{"label":"Down","color":"0000FF","window":{{"start":0,"end":255}},"active":true}}
  ]}}
}}"#,
        multiscales(2)
    )
}

/// The label group's own attributes: a multiscale image PLUS the `image-label` colour table.
///
/// Values 1, 2 and 3 are coloured; 3 is deliberately semi-transparent so an alpha-aware render
/// path has something to prove. Value 0 is absent, which is what "background" looks like on disk.
fn label_zattrs() -> String {
    format!(
        r#"{{
  {},
  "image-label": {{
    "version": "0.4",
    "colors": [
      {{"label-value": 1, "rgba": [255, 0, 0, 255]}},
      {{"label-value": 2, "rgba": [0, 255, 0, 255]}},
      {{"label-value": 3, "rgba": [0, 0, 255, 128]}}
    ]
  }}
}}"#,
        multiscales(3)
    )
}

/// Four flat quadrants: top-left 0, top-right 1, bottom-left 2, bottom-right 3.
fn label_value(gy: u64, gx: u64, size: u64) -> u8 {
    let half = size / 2;
    match (gy >= half, gx >= half) {
        (false, false) => 0,
        (false, true) => 1,
        (true, false) => 2,
        (true, true) => 3,
    }
}

/// Channel 0 ramps left-to-right, channel 1 top-to-bottom. Neither looks anything like the label
/// quadrants, and dropping either changes the picture in a way a test can name.
fn image_value(c: u64, gy: u64, gx: u64, size: u64) -> u8 {
    let along = if c == 0 { gx } else { gy };
    ((along * 255) / size.max(1)) as u8
}

/// One chunk of the array at `level_size`, chunk grid position `(cy, cx)`, 32x32 elements clipped
/// to the level's extent (a level smaller than the chunk still writes a full chunk; zarr pads).
fn chunk_bytes(level_size: u64, c: u64, cy: u64, cx: u64, label: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32 * 32);
    for y in 0..32u64 {
        for x in 0..32u64 {
            let gy = (cy * 32 + y).min(level_size - 1);
            let gx = (cx * 32 + x).min(level_size - 1);
            buf.push(if label {
                label_value(gy, gx, level_size)
            } else {
                image_value(c, gy, gx, level_size)
            });
        }
    }
    buf
}

/// Writes one multiscale group: `.zgroup`, `.zattrs`, and `levels` arrays halving each time.
fn write_pyramid(
    dir: &Path,
    zattrs: &str,
    levels: usize,
    base_size: u64,
    channels: u64,
    label: bool,
) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(dir.join(".zattrs"), zattrs).unwrap();
    for l in 0..levels {
        let size = base_size >> l;
        let ldir = dir.join(l.to_string());
        fs::create_dir_all(&ldir).unwrap();
        fs::write(ldir.join(".zarray"), zarray(size, channels, "|u1")).unwrap();
        let grid = size.div_ceil(32);
        for c in 0..channels {
            for cy in 0..grid {
                for cx in 0..grid {
                    fs::write(
                        ldir.join(format!("0.{c}.0.{cy}.{cx}")),
                        chunk_bytes(size, c, cy, cx, label),
                    )
                    .unwrap();
                }
            }
        }
    }
}

fn build(root: &Path) {
    write_pyramid(root, &image_zattrs(), 2, 64, 2, false);

    // `labels/` is a plain group whose attributes list the label images it contains. It is NOT
    // itself a multiscale image, which is why opening it as one has to be allowed to fail.
    let labels = root.join("labels");
    fs::create_dir_all(&labels).unwrap();
    fs::write(labels.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(labels.join(".zattrs"), r#"{"labels": ["nuclei"]}"#).unwrap();

    write_pyramid(&labels.join("nuclei"), &label_zattrs(), 3, 64, 1, true);
}

#[test]
fn builds_labels_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-zarr-core --test build_labels_fixture",
        |root| {
            build(root);
            assert!(root.join("0/.zarray").exists());
            assert!(root.join("1/.zarray").exists());
            assert!(!root.join("2").exists(), "image has two levels");
            assert!(root.join("labels/.zattrs").exists());
            assert!(
                root.join("labels/nuclei/2/.zarray").exists(),
                "labels have three"
            );
            assert!(
                root.join("0/0.1.0.0.0").exists(),
                "the image has two channels"
            );
            assert!(
                !root.join("labels/nuclei/0/0.1.0.0.0").exists(),
                "the label has one"
            );
            assert_eq!(
                fs::read(root.join("labels/nuclei/0/0.0.0.0.0"))
                    .unwrap()
                    .len(),
                1024
            );
        },
    );
}
