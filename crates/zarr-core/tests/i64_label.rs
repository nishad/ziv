//! Opening an OME-NGFF label stored as `int64` (`<i8`) — the regression this fix exists for.
//!
//! Fixture: `tests/fixtures/sample_i64_label.ome.zarr`, built by `build_i64_label_fixture.rs`. See
//! that file for the exact reason it carries a value outside `i32`'s range.
use zarr_core::ZarrImage;

const FIXTURE: &str = "../../tests/fixtures/sample_i64_label.ome.zarr";

/// Before `DType::I64`, this failed inside `classify_dtype` with
/// `ZarrError::UnsupportedDtype("int64 / <i8", ...)`, and `open_labels_local` discarded that `Err`
/// silently — so `labels()` came back empty and `label_open_failures()` said nothing either. Both
/// must now be right: the label is discovered, and nothing was swallowed to get there.
#[test]
fn an_int64_label_is_discovered_not_silently_dropped() {
    let img = ZarrImage::open(FIXTURE).unwrap();
    let names: Vec<&str> = img.labels().iter().map(|l| l.info.name.as_str()).collect();
    assert_eq!(names, vec!["cells"]);
    assert!(img.label("cells").is_some());
    assert!(
        img.label_open_failures().is_empty(),
        "{:?}",
        img.label_open_failures()
    );
}

/// The label's own dtype really is `int64`, not merely accepted and then narrowed to something
/// smaller under the hood.
#[test]
fn the_label_reports_its_own_dtype_as_i64() {
    let img = ZarrImage::open(FIXTURE).unwrap();
    let label = &img.label("cells").unwrap().image;
    assert_eq!(label.dtype(), zarr_core::DType::I64);
}

/// Pixels read back correctly across the full `int64` range the fixture writes, including the
/// value beyond `i32::MAX` — proof the widening-to-`f64` read path does not truncate to 32 bits
/// anywhere between the chunk decode and here.
#[test]
fn reads_int64_label_values_including_one_outside_i32_range() {
    let img = ZarrImage::open(FIXTURE).unwrap();
    let label = &img.label("cells").unwrap().image;
    let plane = label.read_region_f64(0, 0, 0, 0, 0..8, 0..8).unwrap();
    // Quadrants of the one written chunk: TL 0, TR 1, BL 2, BR 5_000_000_000.
    assert_eq!(plane[[0, 0]], 0.0);
    assert_eq!(plane[[0, 7]], 1.0);
    assert_eq!(plane[[7, 0]], 2.0);
    assert_eq!(plane[[7, 7]], 5_000_000_000.0);
    assert!(
        5_000_000_000i64 > i32::MAX as i64,
        "the fixture's own claim: this value must not fit in an i32"
    );
}

/// The colour table's own value beyond `i32::MAX` round-trips too — the metadata parse, not just
/// the pixel read, must carry a full 64-bit label value.
#[test]
fn the_colour_table_carries_the_out_of_i32_range_value() {
    let img = ZarrImage::open(FIXTURE).unwrap();
    let info = &img.label("cells").unwrap().info;
    assert_eq!(info.color_for(5_000_000_000), [0, 0, 255, 255]);
}
