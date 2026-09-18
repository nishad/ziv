//! A `ZarrImage` carries ONE `DType` for the whole pyramid, and `read_region_f64` dispatches on it
//! to pick the concrete element type it hands zarrs. So a multiscale group whose levels declare
//! different dtypes is unreadable at every level that differs from level 0.
//!
//! Before the fail-loud check, such a group opened successfully (level 0's dtype silently won) and
//! failed later, at tile-read time, with zarrs' own "Incompatible element type for data type" —
//! naming neither the level nor the mismatch. Served, that is an image that returns 200 at low
//! zoom and 500 at high zoom, with nothing in the error an operator can act on. These tests pin
//! the rejection at open time, through both the local and the remote open path, and pin the
//! control case (levels that agree) so the check cannot be satisfied by simply refusing pyramids.
use std::path::Path;
use std::sync::Arc;

use object_store::memory::InMemory;
use object_store::path::Path as StorePath;
use object_store::ObjectStoreExt;

use zarr_core::store::RemoteStoreSpec;
use zarr_core::ZarrImage;

const ZATTRS: &str = r#"{
  "multiscales": [{
    "version": "0.4",
    "axes": [
      {"name":"y","type":"space"}, {"name":"x","type":"space"}
    ],
    "datasets": [
      {"path":"0","coordinateTransformations":[{"type":"scale","scale":[1,1]}]},
      {"path":"1","coordinateTransformations":[{"type":"scale","scale":[2,2]}]}
    ]
  }],
  "omero": { "channels": [
    {"color":"FFFFFF","window":{"start":0,"end":15},"active":true}
  ]}
}"#;

fn write_level(root: &Path, name: &str, dtype: &str, size: usize, bytes_per_px: usize) {
    let level = root.join(name);
    std::fs::create_dir_all(&level).unwrap();
    std::fs::write(
        level.join(".zarray"),
        format!(
            "{{\"zarr_format\":2,\"shape\":[{size},{size}],\"chunks\":[{size},{size}],\"dtype\":\"{dtype}\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}"
        ),
    )
    .unwrap();
    std::fs::write(level.join("0.0"), vec![0u8; size * size * bytes_per_px]).unwrap();
}

/// Two-level OME-Zarr 0.4 group; `l1_dtype`/`l1_bytes` let a caller make level 1 agree with or
/// differ from level 0 (which is always `|u1`).
fn build_two_level(root: &Path, l1_dtype: &str, l1_bytes: usize) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    std::fs::write(root.join(".zattrs"), ZATTRS).unwrap();
    write_level(root, "0", "|u1", 16, 1);
    write_level(root, "1", l1_dtype, 8, l1_bytes);
}

fn populate_from_dir(store: &InMemory, dir: &Path, base: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            populate_from_dir(store, &path, base);
        } else {
            let key = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            let bytes = std::fs::read(&path).unwrap();
            pollster::block_on(store.put(&StorePath::from(key.as_str()), bytes.into())).unwrap();
        }
    }
}

fn open_remote(root: &Path) -> Result<ZarrImage, zarr_core::ZarrError> {
    let mem = InMemory::new();
    populate_from_dir(&mem, root, root);
    ZarrImage::open_remote_store(RemoteStoreSpec {
        store: Arc::new(mem),
        group_path: "/".to_string(),
    })
}

#[test]
fn levels_disagreeing_on_dtype_are_rejected_at_open_naming_both_levels() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("mismatched.ome.zarr");
    build_two_level(&root, "<f4", 4);

    for (what, result) in [
        ("local", ZarrImage::open(root.to_str().unwrap())),
        ("remote", open_remote(&root)),
    ] {
        let msg = match result {
            Ok(_) => panic!("{what}: a pyramid whose levels disagree on dtype must not open"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("levels disagree on dtype"),
            "{what}: error must say what is wrong, got: {msg}"
        );
        // The whole point of failing loud here rather than at read time is that the message is
        // actionable: it has to name the offending level and both dtypes.
        assert!(
            msg.contains("level 1"),
            "{what}: must name the level: {msg}"
        );
        assert!(
            msg.contains("U8"),
            "{what}: must name level 0's dtype: {msg}"
        );
        assert!(
            msg.contains("F32"),
            "{what}: must name the found dtype: {msg}"
        );
    }
}

/// Control: the check must reject only genuine disagreement. A normal pyramid — every level the
/// same dtype, different shapes — still opens and reports that dtype.
#[test]
fn levels_agreeing_on_dtype_still_open() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("consistent.ome.zarr");
    build_two_level(&root, "|u1", 1);

    for (what, result) in [
        ("local", ZarrImage::open(root.to_str().unwrap())),
        ("remote", open_remote(&root)),
    ] {
        let img = result.unwrap_or_else(|e| panic!("{what}: consistent pyramid must open: {e}"));
        assert_eq!(img.dtype(), zarr_core::DType::U8, "{what}");
        assert_eq!(img.num_levels(), 2, "{what}");
    }
}
