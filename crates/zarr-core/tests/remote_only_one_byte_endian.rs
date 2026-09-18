//! Pins the alias registration on the ASYNC open path specifically.
//!
//! `register_one_byte_endian_aliases()` (see `image.rs`) mutates a PROCESS-GLOBAL registry inside
//! zarrs, and it is called from both `open_local` and `open_remote_async`. That global-ness makes
//! the obvious test worthless: `one_byte_endian_aliases.rs` opens each fixture locally and then
//! remotely in the same process, so the local open registers the aliases for the whole process and
//! the remote assertions ride on that side effect. Deleting the registration from
//! `open_remote_async` leaves that file — and the entire workspace suite — green, while breaking
//! every `>u1` image in a REMOTE-ONLY deployment (`ziv serve s3://...` / `https://...`), which is
//! precisely the deployment the big-endian work was written for: the public IDR and
//! BioImage-Archive images are remote.
//!
//! A cargo integration test gets its own binary, and a binary is the process boundary that keeps
//! the alias registry pristine. So this must stay its OWN FILE containing ONLY remote opens — a
//! sibling test doing a local open, in this file or added to it later, would re-poison the
//! registry and silently disarm this test. Nothing here may call `ZarrImage::open`.
//!
//! The fixtures are assembled straight into the object store rather than on disk, so there is no
//! filesystem path involved that could tempt a future edit toward a local open.
use std::sync::Arc;

use object_store::memory::InMemory;
use object_store::path::Path as StorePath;
use object_store::ObjectStoreExt;

use zarr_core::store::RemoteStoreSpec;
use zarr_core::{DType, ZarrImage};

const SIZE: usize = 16;

const ZATTRS: &str = r#"{
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
    {"color":"FFFFFF","window":{"start":0,"end":15},"active":true}
  ]}
}"#;

/// A minimal single-level OME-Zarr 0.4 group declaring `dtype`, one byte per pixel, laid out
/// directly as object-store keys — `value(y, x) = x`.
fn store_with_dtype(dtype: &str) -> InMemory {
    let store = InMemory::new();
    let zarray = format!(
        "{{\"zarr_format\":2,\"shape\":[{SIZE},{SIZE}],\"chunks\":[{SIZE},{SIZE}],\"dtype\":\"{dtype}\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}"
    );
    let chunk: Vec<u8> = (0..SIZE)
        .flat_map(|_y| (0..SIZE).map(|x| x as u8))
        .collect();

    for (key, bytes) in [
        (".zgroup", r#"{"zarr_format":2}"#.as_bytes().to_vec()),
        (".zattrs", ZATTRS.as_bytes().to_vec()),
        ("0/.zarray", zarray.into_bytes()),
        ("0/0.0", chunk),
    ] {
        pollster::block_on(store.put(&StorePath::from(key), bytes.into())).unwrap();
    }
    store
}

/// Every endianness-prefixed one-byte spelling, opened through the async `AsyncObjectStore` path
/// and NOTHING else, so this test binary's first zarrs operation is a remote open against an
/// unregistered alias.
#[test]
fn remote_only_process_opens_every_one_byte_endian_prefix() {
    for (dtype, expected) in [
        (">u1", DType::U8),
        ("<u1", DType::U8),
        (">i1", DType::I8),
        ("<i1", DType::I8),
    ] {
        let img = ZarrImage::open_remote_store(RemoteStoreSpec {
            store: Arc::new(store_with_dtype(dtype)),
            group_path: "/".to_string(),
        })
        .unwrap_or_else(|e| {
            panic!("remote-only open of dtype {dtype} failed: {e} — is the alias registration on the async open path still there?")
        });

        assert_eq!(img.dtype(), expected, "{dtype}: dtype");
        let tile = img.read_region_f64(0, 0, 0, 0, 0..16, 0..16).unwrap();
        assert_eq!(tile[[0, 0]], 0.0, "{dtype}: value at x=0");
        assert_eq!(tile[[3, 7]], 7.0, "{dtype}: value at x=7");
        assert_eq!(tile[[0, 15]], 15.0, "{dtype}: value at x=15");
    }
}
