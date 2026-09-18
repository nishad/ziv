//! Builds a committed fixture where `labels/.zattrs` declares TWO labels and only one of them can
//! actually be opened — the general "a label that fails to open must not silently disappear, and
//! must not take a working sibling down with it" case, as distinct from `build_i64_label_fixture`,
//! which covers the specific dtype that motivated this fix.
//!
//! `bad`'s group exists (so it is found and attempted) but its `.zattrs` carries no `multiscales`
//! key at all, which is `ZarrError::NoMultiscales` from `parse_multiscale` inside
//! `open_group_local` — a real, deterministic failure that needs no unsupported dtype to trigger.
//! Before this fix, `open_labels_local`'s `filter_map` discarded that `Err` with `.ok()?`, so
//! `labels()` silently came back with only `good` and nothing recorded why `bad` was missing.
//!
//! Run: `cargo test -p ziv-zarr-core --test build_broken_label_fixture`. Builds into a private
//! temp directory and checks it against the committed fixture rather than rewriting it in place
//! (see `fixture_test_support`); set `ZIV_REGENERATE_FIXTURES=1` to update the committed copy.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_broken_label.ome.zarr")
}

const SIZE: u64 = 8;

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

const GOOD_LABEL_ZATTRS: &str = r#"{
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
      {"label-value": 1, "rgba": [255, 0, 0, 255]}
    ]
  }
}"#;

fn zarray(dtype: &str) -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[{SIZE},{SIZE}],\"chunks\":[{SIZE},{SIZE}],\
         \"dtype\":\"{dtype}\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\
         \"dimension_separator\":\".\"}}"
    )
}

fn build(root: &Path) {
    fs::create_dir_all(root.join("0")).unwrap();
    fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(root.join(".zattrs"), IMAGE_ZATTRS).unwrap();
    fs::write(root.join("0/.zarray"), zarray("|u1")).unwrap();
    fs::write(root.join("0/0.0"), vec![64u8; (SIZE * SIZE) as usize]).unwrap();

    let labels = root.join("labels");
    fs::create_dir_all(&labels).unwrap();
    fs::write(labels.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    // Declares both, in this order, so a name-preserving implementation keeps `good` addressable
    // and reports `bad` by name rather than either vanishing or swapping identities.
    fs::write(labels.join(".zattrs"), r#"{"labels": ["good", "bad"]}"#).unwrap();

    let good = labels.join("good");
    fs::create_dir_all(good.join("0")).unwrap();
    fs::write(good.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(good.join(".zattrs"), GOOD_LABEL_ZATTRS).unwrap();
    fs::write(good.join("0/.zarray"), zarray("|u1")).unwrap();
    fs::write(good.join("0/0.0"), vec![1u8; (SIZE * SIZE) as usize]).unwrap();

    // `bad`'s group exists — it is found by `parse_label_names` and its group opens — but it
    // declares no `multiscales` at all, so `open_group_local`'s `parse_multiscale` call is the
    // one that fails. This is deliberately a DIFFERENT failure mode to the dtype one in
    // `build_i64_label_fixture.rs`, to prove the "a label can fail for any reason, not just an
    // unsupported dtype" half of the fix.
    let bad = labels.join("bad");
    fs::create_dir_all(&bad).unwrap();
    fs::write(bad.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(bad.join(".zattrs"), r#"{}"#).unwrap();
}

#[test]
fn builds_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-zarr-core --test build_broken_label_fixture",
        |root| {
            build(root);
            assert!(root.join("0/.zarray").exists());
            assert!(root.join("labels/.zattrs").exists());
            assert!(root.join("labels/good/0/.zarray").exists());
            assert!(root.join("labels/bad/.zgroup").exists());
            assert!(
                !root.join("labels/bad/0").exists(),
                "bad declares no multiscale levels at all"
            );
        },
    );
}
