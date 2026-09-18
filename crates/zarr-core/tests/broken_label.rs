//! A declared label that cannot be opened must be named, not silently dropped, and must not stop
//! a working sibling label from opening. Fixture: `tests/fixtures/sample_broken_label.ome.zarr`,
//! built by `build_broken_label_fixture.rs` — see that file for exactly how `bad` is made to fail.
use zarr_core::ZarrImage;

const FIXTURE: &str = "../../tests/fixtures/sample_broken_label.ome.zarr";

#[test]
fn a_broken_label_is_reported_by_name_and_reason() {
    let img = ZarrImage::open(FIXTURE).unwrap();
    let failures = img.label_open_failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].name, "bad");
    assert!(
        failures[0].reason.contains("multiscales"),
        "{:?}",
        failures[0].reason
    );
}

/// The whole point: `bad` failing must not take `good` down with it, and must not make the image
/// itself fail to open at all.
#[test]
fn a_broken_label_does_not_take_down_its_working_sibling() {
    let img = ZarrImage::open(FIXTURE).unwrap();
    let names: Vec<&str> = img.labels().iter().map(|l| l.info.name.as_str()).collect();
    assert_eq!(names, vec!["good"]);
    assert!(img.label("good").is_some());
    let plane = img
        .label("good")
        .unwrap()
        .image
        .read_region_f64(0, 0, 0, 0, 0..8, 0..8)
        .unwrap();
    assert_eq!(plane[[0, 0]], 1.0);
}
