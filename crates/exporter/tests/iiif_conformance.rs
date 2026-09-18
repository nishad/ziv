//! IIIF Image API 3.0 conformance (CI-runnable, hermetic layer — see
//! `iiif::conformance::assert_info_json_conforms`'s module doc) run against a REAL `info.json`
//! written to disk by [`exporter::export`] over a real `ZarrTileEngine` reading a committed
//! fixture — the level0/static counterpart to `server`'s live-serve conformance test.
use std::fs;

use exporter::{export, ExportOptions};
use iiif::assert_info_json_conforms;
use tiling::ZarrTileEngine;
use zarr_core::ZarrImage;

fn engine() -> ZarrTileEngine {
    let img = ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
    ZarrTileEngine::new(img)
}

/// The exported `info.json` (level0, static tree) must conform to the IIIF Image API 3.0 shape
/// requirements when published with an absolute `id` (the realistic "publish under a
/// dereferenceable URL" case).
#[test]
fn exported_level0_info_json_conforms_to_iiif_3_0_with_absolute_id() {
    let dir = tempfile::tempdir().unwrap();
    let options = ExportOptions {
        id: "https://example.org/iiif/sample".to_string(),
        ..ExportOptions::default()
    };
    export(&engine(), dir.path(), &options).unwrap();

    let bytes = fs::read(dir.path().join("info.json")).unwrap();
    let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let violations = assert_info_json_conforms(&info);
    assert!(
        violations.is_empty(),
        "exported level0 info.json failed IIIF 3.0 conformance: {violations:#?}\ninfo.json: {info:#}"
    );
    assert_eq!(info["profile"], "level0");
    assert_eq!(info["extraFeatures"], serde_json::json!(["sizeByWh"]));
}

/// The default export `id` (`"."`, a relative reference — see the crate-level docs) is still a
/// non-empty, non-trailing-slash string, so it must ALSO pass conformance unchanged.
#[test]
fn exported_level0_info_json_conforms_to_iiif_3_0_with_default_relative_id() {
    let dir = tempfile::tempdir().unwrap();
    export(&engine(), dir.path(), &ExportOptions::default()).unwrap();

    let bytes = fs::read(dir.path().join("info.json")).unwrap();
    let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let violations = assert_info_json_conforms(&info);
    assert!(
        violations.is_empty(),
        "exported level0 info.json (default id) failed IIIF 3.0 conformance: {violations:#?}"
    );
    assert_eq!(info["id"], ".");
}
