//! Builds a committed, minimal OME-Zarr fixture whose one label array is stored as `int64`
//! (`<i8`) — numpy's default integer dtype, and the dtype the real IDR sample this fix was built
//! against (`idr0101A-13457537`) ships for its own label (`labels/0/0/.zarray`'s
//! `"dtype": "<i8"`). Also builds a `uint64` (`<u8`) sibling fixture, so `classify_dtype`'s
//! `uint64` branch and the `widen!(u64)` read path are exercised by CI and not merely spot-checked
//! by hand (see `DType::U64`'s doc comment for why `U64` otherwise has no committed fixture of its
//! own — this one is the exception, cheap enough to add because every value the label writes is
//! non-negative, so its on-disk bytes are IDENTICAL to the `int64` fixture's; only the declared
//! dtype string and the Rust-side cast differ).
//!
//! Before `DType::I64` existed, `classify_dtype` rejected this dtype with
//! `ZarrError::UnsupportedDtype("int64 / <i8", ...)`, and `open_labels_local`/`open_labels_remote`
//! silently discarded that `Err` rather than reporting it — so an image whose label genuinely
//! existed on disk reported "no label images". This fixture keeps that regression caught: see
//! `crates/zarr-core/tests/i64_label.rs` (opens it directly and reads its pixels) and
//! `crates/exporter/tests/i64_label_export.rs` (exports it with `--labels` and checks for an
//! overlay tree).
//!
//! Run: `cargo test -p ziv-zarr-core --test build_i64_label_fixture`. Each of the two fixtures
//! below is built into its own private temp directory and checked against its committed copy;
//! set `ZIV_REGENERATE_FIXTURES=1` to update the committed copy.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_i64_label.ome.zarr")
}

fn u64_fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_u64_label.ome.zarr")
}

/// Full extent of both arrays. Small on purpose — this fixture exists to prove one dtype opens
/// and reads correctly, not to exercise pyramids or multi-tile layout (other fixtures cover that).
const SIZE: u64 = 16;
/// Chunk size: a 2x2 grid over `SIZE`, only one of which is ever written (see `build`).
const CHUNK: u64 = 8;

/// Single-level, y/x-only image. Axes besides y/x default to extent 1 (`ZarrImage::axis_len`), so
/// omitting t/c/z here is not a shortcut around real behaviour — it is a real, minimal image.
const IMAGE_ZATTRS: &str = r#"{
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
    {"color":"FFFFFF","window":{"start":0,"end":255},"active":true}
  ]}
}"#;

fn image_zarray() -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[{SIZE},{SIZE}],\"chunks\":[{CHUNK},{CHUNK}],\
         \"dtype\":\"|u1\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\
         \"dimension_separator\":\".\"}}"
    )
}

/// The label's own multiscale metadata, plus the `image-label` colour table. One entry
/// (`5000000000`) is outside `i32`'s range (`i32::MAX` is ~2.1 billion): the fixture would still
/// "work" with only small values if the read path silently narrowed to 32 bits somewhere, so this
/// value exists specifically to catch that.
const LABEL_ZATTRS: &str = r#"{
  "multiscales": [{
    "version": "0.4",
    "axes": [
      {"name":"y","type":"space"}, {"name":"x","type":"space"}
    ],
    "datasets": [
      {"path":"0","coordinateTransformations":[{"type":"scale","scale":[1,1]}]}
    ]
  }],
  "image-label": {
    "version": "0.4",
    "colors": [
      {"label-value": 1, "rgba": [255, 0, 0, 255]},
      {"label-value": 2, "rgba": [0, 255, 0, 255]},
      {"label-value": 5000000000, "rgba": [0, 0, 255, 255]}
    ]
  }
}"#;

fn label_zarray(dtype: &str) -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[{SIZE},{SIZE}],\"chunks\":[{CHUNK},{CHUNK}],\
         \"dtype\":\"{dtype}\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\
         \"dimension_separator\":\".\"}}"
    )
}

/// Quadrants of the one written chunk: top-left 0 (background, transparent in every palette),
/// top-right 1, bottom-left 2, bottom-right the deliberately-outside-`i32` value described above.
fn label_value(y: u64, x: u64) -> i64 {
    let half = CHUNK / 2;
    match (y >= half, x >= half) {
        (false, false) => 0,
        (false, true) => 1,
        (true, false) => 2,
        (true, true) => 5_000_000_000,
    }
}

/// One chunk's worth of `int64` little-endian element bytes — `zarr_format: 2` with
/// `compressor: null` stores raw, undelimited element bytes in array (here: row-major) order.
fn label_chunk_bytes() -> Vec<u8> {
    let mut buf = Vec::with_capacity((CHUNK * CHUNK * 8) as usize);
    for y in 0..CHUNK {
        for x in 0..CHUNK {
            buf.extend_from_slice(&label_value(y, x).to_le_bytes());
        }
    }
    buf
}

/// Builds the tree at `root`, with the label array declared as `dtype`. Every value the label
/// writes is non-negative (see `label_value`), so the on-disk chunk bytes are identical whether
/// `dtype` is `<i8` or `<u8` — the sign only matters to the READER, not to what gets written here.
fn build(root: &Path, dtype: &str) {
    fs::create_dir_all(root.join("0")).unwrap();
    fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(root.join(".zattrs"), IMAGE_ZATTRS).unwrap();
    fs::write(root.join("0/.zarray"), image_zarray()).unwrap();
    // Sparse: only the (0,0) chunk of the 2x2 grid is written; the rest read back as fill_value 0,
    // ordinary sparse-chunk Zarr v2 semantics (same trick `build_no_pyramid_fixture.rs` uses).
    fs::write(root.join("0/0.0"), vec![128u8; (CHUNK * CHUNK) as usize]).unwrap();

    // `labels/` is a plain group whose attributes list the label images it contains — see
    // `build_labels_fixture.rs` for why opening it as a multiscale image must be allowed to fail.
    let labels = root.join("labels");
    fs::create_dir_all(&labels).unwrap();
    fs::write(labels.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(labels.join(".zattrs"), r#"{"labels": ["cells"]}"#).unwrap();

    let cells = labels.join("cells");
    fs::create_dir_all(cells.join("0")).unwrap();
    fs::write(cells.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(cells.join(".zattrs"), LABEL_ZATTRS).unwrap();
    fs::write(cells.join("0/.zarray"), label_zarray(dtype)).unwrap();
    fs::write(cells.join("0/0.0"), label_chunk_bytes()).unwrap();
}

#[test]
fn builds_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-zarr-core --test build_i64_label_fixture",
        |root| {
            build(root, "<i8");
            assert!(root.join(".zgroup").exists());
            assert!(root.join("0/.zarray").exists());
            assert!(root.join("labels/.zattrs").exists());
            assert!(root.join("labels/cells/.zattrs").exists());
            assert!(root.join("labels/cells/0/.zarray").exists());
            // Sparse: the (0,1)/(1,0)/(1,1) chunks of both arrays are deliberately never written.
            assert!(!root.join("0/0.1").exists());
            assert!(!root.join("labels/cells/0/0.1").exists());
            assert_eq!(
                fs::read(root.join("labels/cells/0/0.0")).unwrap().len(),
                (CHUNK * CHUNK * 8) as usize,
                "int64 elements are 8 bytes each"
            );
        },
    );
}

/// The `uint64` sibling — see the module doc comment for why this one gets a committed fixture
/// when `U64` otherwise does not.
#[test]
fn builds_u64_fixture() {
    fixture_test_support::check_or_regenerate(
        &u64_fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-zarr-core --test build_i64_label_fixture",
        |root| {
            build(root, "<u8");
            assert!(root.join("labels/cells/0/.zarray").exists());
            assert!(
                fs::read_to_string(root.join("labels/cells/0/.zarray"))
                    .unwrap()
                    .contains("\"dtype\":\"<u8\""),
                "declared dtype must be uint64, not the int64 sibling's"
            );
        },
    );
}
