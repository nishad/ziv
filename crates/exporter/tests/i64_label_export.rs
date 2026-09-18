//! A real `--labels` export of an `int64`-labelled image, end to end.
//!
//! Fixture: `tests/fixtures/sample_i64_label.ome.zarr`, built by
//! `crates/zarr-core/tests/build_i64_label_fixture.rs`. Before `DType::I64` existed, the label's
//! `<i8` dtype made `classify_dtype` fail, and `ZarrImage::open_labels_local` silently discarded
//! that error — so `ziv export --labels` against a real `int64`-labelled image (IDR ships them;
//! see `idr0101A-13457537`) printed "adds nothing: the image has no label images" and wrote no
//! overlay tree at all. This test is the exporter-level half of the regression check;
//! `crates/zarr-core/tests/i64_label.rs` covers opening and reading the label directly.

use exporter::{export, ExportOptions};
use tiling::{TileEngine, ZarrTileEngine};
use zarr_core::ZarrImage;

const FIXTURE: &str = "../../tests/fixtures/sample_i64_label.ome.zarr";

#[test]
fn an_int64_label_export_produces_an_overlay_tree() {
    let e = ZarrTileEngine::new(ZarrImage::open(FIXTURE).unwrap());
    let dir = tempfile::tempdir().unwrap();
    let summary = export(
        &e,
        dir.path(),
        &ExportOptions {
            labels: true,
            ..ExportOptions::default()
        },
    )
    .unwrap();

    // Root (intensity) + one overlay tree — the same shape `sample_labels.ome.zarr`'s own
    // `labels_overlay_the_default_plane_when_planes_are_not_asked_for` test pins.
    assert_eq!(summary.views, 2);
    assert!(dir.path().join("planes/0/labels/0/info.json").exists());
    assert!(
        dir.path()
            .join("planes/0/labels/0/full/max/0/default.jpg")
            .exists(),
        "the overlay tree must actually render tiles, not just carry an info.json"
    );

    // A label that opened cleanly must not produce "adds nothing" or any other label warning.
    assert!(
        !summary.warnings.iter().any(|w| w.contains("label")),
        "{:?}",
        summary.warnings
    );

    let dims = e.dimensions();
    assert_eq!(dims.labels.len(), 1);
    assert_eq!(dims.labels[0].name, "cells");
    assert!(
        dims.label_open_failures.is_empty(),
        "{:?}",
        dims.label_open_failures
    );
}
