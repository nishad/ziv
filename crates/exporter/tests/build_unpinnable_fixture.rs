//! Builds a committed OME-Zarr fixture whose pyramid OpenSeadragon cannot pin: three levels at
//! scale factors 1, 3 and 9 rather than powers of two.
//!
//! `IIIFTileSource` derives `maxLevel = round(log2(maxScaleFactor))`, which is `round(log2(9))`,
//! that is 3, and then trusts `sizes` only when its length is 3 or 4. This pyramid has 3 levels,
//! so OSD takes the `length == maxLevel` arm and pushes `(width, height)` on TOP of a `sizes`
//! array that already ends there, reconstructing four level sizes for a three-level image. Every
//! tile URL it then builds is indexed against the wrong level.
//!
//! No real-world OME-Zarr this project has met is built this way, which is exactly why the
//! refusal needs a committed fixture: without one, the branch that protects against it would
//! never execute.
//!
//! Shape: 1 z-plane, single channel, 900x900 (y, x), three datasets at 900, 300 and 100. Kept
//! small on disk the same way `build_no_pyramid_fixture.rs` is: `fill_value: 0` plus sparse
//! chunks, with only chunk (0,0,0) of each level written.
//!
//! Run: `cargo test -p ziv-exporter --test build_unpinnable_fixture`. Builds into a private temp
//! directory and checks it against the committed fixture; set `ZIV_REGENERATE_FIXTURES=1` to
//! update the committed copy.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_unpinnable.ome.zarr")
}

/// (dataset path, edge length, scale factor). Factors of three, which is the whole point.
const LEVELS: [(&str, u64, u64); 3] = [("0", 900, 1), ("1", 300, 3), ("2", 100, 9)];

const ZATTRS: &str = r#"{
  "multiscales": [{
    "version": "0.4",
    "axes": [
      {"name":"z","type":"space"}, {"name":"y","type":"space"}, {"name":"x","type":"space"}
    ],
    "datasets": [
      {"path":"0","coordinateTransformations":[{"type":"scale","scale":[1,1,1]}]},
      {"path":"1","coordinateTransformations":[{"type":"scale","scale":[1,3,3]}]},
      {"path":"2","coordinateTransformations":[{"type":"scale","scale":[1,9,9]}]}
    ]
  }],
  "omero": { "channels": [
    {"color":"FFFFFF","window":{"start":0,"end":255},"active":true}
  ]}
}"#;

fn zarray(edge: u64, chunk: u64) -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[1,{edge},{edge}],\"chunks\":[1,{chunk},{chunk}],\
         \"dtype\":\"|u1\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\
         \"filters\":null,\"dimension_separator\":\".\"}}"
    )
}

fn build(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(root.join(".zattrs"), ZATTRS).unwrap();

    for (path, edge, _) in LEVELS {
        // One chunk covers the whole level, so each level is a single small file.
        let chunk = edge.min(300);
        fs::create_dir_all(root.join(path)).unwrap();
        fs::write(root.join(path).join(".zarray"), zarray(edge, chunk)).unwrap();
        // Only chunk (0,0,0) is written; the rest read back as fill_value 0.
        let bytes = vec![128u8; (chunk * chunk) as usize];
        fs::write(root.join(path).join("0.0.0"), bytes).unwrap();
    }
}

#[test]
fn builds_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-exporter --test build_unpinnable_fixture",
        |root| {
            build(root);
            assert!(root.join(".zattrs").exists());
            for (path, _, _) in LEVELS {
                assert!(root.join(path).join(".zarray").exists(), "{path}");
                assert!(root.join(path).join("0.0.0").exists(), "{path}");
            }
        },
    );
}
