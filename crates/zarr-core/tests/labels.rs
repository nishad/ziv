//! Opening an OME-NGFF `labels/` group.
//!
//! Fixture: `tests/fixtures/sample_labels.ome.zarr`, built by `build_labels_fixture.rs`. See that
//! file for what it deliberately does NOT share with its parent.
use zarr_core::ZarrImage;

const WITH_LABELS: &str = "../../tests/fixtures/sample_labels.ome.zarr";
const WITHOUT_LABELS: &str = "../../tests/fixtures/sample_v04.ome.zarr";

#[test]
fn discovers_the_label_images_an_image_declares() {
    let img = ZarrImage::open(WITH_LABELS).unwrap();
    let names: Vec<&str> = img.labels().iter().map(|l| l.info.name.as_str()).collect();
    assert_eq!(names, vec!["nuclei"]);
    assert!(img.label("nuclei").is_some());
    assert!(img.label("no-such-label").is_none());
}

#[test]
fn reads_the_colour_table_off_the_label_image() {
    let img = ZarrImage::open(WITH_LABELS).unwrap();
    let info = &img.label("nuclei").unwrap().info;
    assert_eq!(info.color_for(1), [255, 0, 0, 255]);
    assert_eq!(info.color_for(2), [0, 255, 0, 255]);
    assert_eq!(info.color_for(3), [0, 0, 255, 128]);
    // 0 is background: absent from the table, and therefore transparent rather than guessed.
    assert_eq!(info.color_for(0), [0, 0, 0, 0]);
}

/// The property the whole label render path is built around: a label image has its own pyramid,
/// which is not its parent's. Reading level 2 of this image would be an out-of-range panic; on
/// the label it is a legitimate 16x16 plane.
#[test]
fn the_label_pyramid_is_independent_of_the_parent_pyramid() {
    let img = ZarrImage::open(WITH_LABELS).unwrap();
    let label = &img.label("nuclei").unwrap().image;
    assert_eq!(img.num_levels(), 2);
    assert_eq!(label.num_levels(), 3);
    assert_eq!(img.level_yx(1), (32, 32));
    assert_eq!(label.level_yx(2), (16, 16));
}

/// The label's pixels are reachable as an ordinary image read, which is what lets the renderer
/// treat a label exactly like any other plane source.
#[test]
fn reads_label_values_as_pixels() {
    let img = ZarrImage::open(WITH_LABELS).unwrap();
    let label = &img.label("nuclei").unwrap().image;
    let plane = label.read_region_f64(0, 0, 0, 0, 0..64, 0..64).unwrap();
    // Quadrants: TL 0, TR 1, BL 2, BR 3.
    assert_eq!(plane[[0, 0]], 0.0);
    assert_eq!(plane[[0, 63]], 1.0);
    assert_eq!(plane[[63, 0]], 2.0);
    assert_eq!(plane[[63, 63]], 3.0);
}

/// Labels do not nest: a label image opened as part of its parent must not go looking for labels
/// of its own, or a malicious/odd tree could recurse.
#[test]
fn a_label_image_carries_no_labels_of_its_own() {
    let img = ZarrImage::open(WITH_LABELS).unwrap();
    assert!(img.label("nuclei").unwrap().image.labels().is_empty());
}

/// The overwhelmingly common case. An image with no `labels/` group must open exactly as before,
/// with no error and no extra cost surfacing as a failure.
#[test]
fn an_image_without_labels_opens_with_an_empty_list() {
    let img = ZarrImage::open(WITHOUT_LABELS).unwrap();
    assert!(img.labels().is_empty());
}
