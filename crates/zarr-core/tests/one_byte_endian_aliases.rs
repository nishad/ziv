//! Proves ziv accepts ALL FOUR endianness-prefixed Zarr V2 spellings of the one-byte numeric
//! types — `>u1`, `<u1`, `>i1`, `<i1` — through BOTH open paths (local `FilesystemStore` and the
//! async `AsyncObjectStore` path that every remote `s3://`/`gs://`/`az://`/`http(s)://` image
//! takes).
//!
//! Why this file exists rather than more committed fixtures: the fix is a single registration of
//! four aliases (`register_one_byte_endian_aliases` in `image.rs`), and the failure mode it must
//! be pinned against is someone narrowing that list — e.g. keeping `>u1` (the originally reported
//! spelling, and the only one with a committed fixture) while dropping `<u1`/`>i1`/`<i1` as
//! apparently redundant. A table over all four spellings makes that mutation fail loudly. The
//! async half matters for the same reason: the remote path is the one that reaches the public
//! IDR / BioImage-Archive images the fix was written for, so "works locally" is not the property
//! under test.
//!
//! The fixtures are built into a scratch directory rather than committed because they are
//! systematic variations of the committed `sample_be_u1.ome.zarr` (same shape, same gradient, only
//! the `.zarray` dtype string differs); committing four near-identical trees would add bytes to
//! the repo without adding signal.
use std::path::Path;
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

/// Writes a minimal single-level OME-Zarr 0.4 tree declaring `dtype`, one byte per pixel, with
/// `value(y, x) = x`. One byte per pixel means the chunk bytes are byte-order-independent, so the
/// ONLY thing that varies across the table is the dtype string zarrs has to resolve.
fn build_fixture(root: &Path, dtype: &str) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    std::fs::write(root.join(".zattrs"), ZATTRS).unwrap();
    let level = root.join("0");
    std::fs::create_dir_all(&level).unwrap();
    std::fs::write(
        level.join(".zarray"),
        format!(
            "{{\"zarr_format\":2,\"shape\":[{SIZE},{SIZE}],\"chunks\":[{SIZE},{SIZE}],\"dtype\":\"{dtype}\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}"
        ),
    )
    .unwrap();
    let chunk: Vec<u8> = (0..SIZE)
        .flat_map(|_y| (0..SIZE).map(|x| x as u8))
        .collect();
    std::fs::write(level.join("0.0"), chunk).unwrap();
}

/// Recursively copies every file under `dir` into `store`, keyed by its path relative to `base`
/// (matching how `zarrs`/`object_store` address keys — forward-slash-separated, no leading `/`).
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

fn assert_reads_gradient(img: &ZarrImage, expected: DType, what: &str) {
    assert_eq!(img.dtype(), expected, "{what}: dtype");
    let tile = img.read_region_f64(0, 0, 0, 0, 0..16, 0..16).unwrap();
    assert_eq!(tile.dim(), (16, 16), "{what}: shape");
    assert_eq!(tile[[0, 0]], 0.0, "{what}: value at x=0");
    assert_eq!(tile[[3, 7]], 7.0, "{what}: value at x=7");
    assert_eq!(tile[[0, 15]], 15.0, "{what}: value at x=15");
}

#[test]
fn every_one_byte_endian_prefix_opens_through_both_local_and_remote_paths() {
    let scratch = tempfile::tempdir().unwrap();

    for (dtype, expected) in [
        (">u1", DType::U8),
        ("<u1", DType::U8),
        (">i1", DType::I8),
        ("<i1", DType::I8),
    ] {
        // A path-safe directory name: the `>`/`<` prefix itself is not usable in a filename.
        let (prefix, kind) = dtype.split_at(1);
        let endian = if prefix == ">" { "be" } else { "le" };
        let root = scratch
            .path()
            .join(format!("sample_{endian}_{kind}.ome.zarr"));
        build_fixture(&root, dtype);

        // Local path: FilesystemStore -> Array::open.
        let local = ZarrImage::open(root.to_str().unwrap())
            .unwrap_or_else(|e| panic!("local open of dtype {dtype} failed: {e}"));
        assert_reads_gradient(&local, expected, &format!("{dtype} local"));

        // Remote path: AsyncObjectStore -> Array::async_open, the path every s3://, gs://, az://
        // and http(s):// image takes. InMemory stands in for the network exactly as
        // `remote_store_transparency.rs` does.
        let mem = InMemory::new();
        populate_from_dir(&mem, &root, &root);
        let remote = ZarrImage::open_remote_store(RemoteStoreSpec {
            store: Arc::new(mem),
            group_path: "/".to_string(),
        })
        .unwrap_or_else(|e| panic!("remote open of dtype {dtype} failed: {e}"));
        assert_reads_gradient(&remote, expected, &format!("{dtype} remote"));
    }
}

/// Negative control: registering the four one-byte aliases must NOT have made dtype resolution
/// permissive generally.
///
/// The four spellings are registered as EXACT string aliases. The cheap-looking alternative — one
/// broad regex such as `^[<>|].1$` on `UInt8DataType`'s alias list — would also swallow `>b1`,
/// `|S1`, `>V1` and `>f1`, silently opening each of them as `U8` and misreading the pixels. So the
/// spellings below, not `|b1`, are what actually pins the narrowness: `|b1` is refused by ziv's own
/// `classify_dtype` after resolving to `BoolDataType`, whose default name wins resolution no matter
/// how permissive `UInt8DataType` becomes, and it would still be refused under that mutation. It is
/// kept as the one case that proves the rejection can come from ziv rather than from zarrs.
#[test]
fn aliasing_did_not_make_dtype_resolution_permissive() {
    let scratch = tempfile::tempdir().unwrap();

    // Refused by ziv: resolves to a real zarrs data type that is not a supported ziv pixel type.
    // Insensitive to the alias breadth, so it proves only that ziv's own dtype ladder still bites.
    expect_refused(scratch.path(), "|b1", "bool");

    // Refused by zarrs' name resolution: no plugin claims these spellings. A broad-regex alias on
    // UInt8DataType would claim all four, and each would then open as U8 with misread pixels — so
    // these are the assertions that actually pin the four aliases to being exact strings.
    for dtype in [">b1", "|S1", ">V1", ">f1"] {
        expect_refused(scratch.path(), dtype, dtype);
    }
}

fn expect_refused(scratch: &Path, dtype: &str, expect_named: &str) {
    let root = scratch.join(format!(
        "refused_{}.ome.zarr",
        dtype.replace(['|', '>', '<'], "_")
    ));
    build_fixture(&root, dtype);
    let msg = match ZarrImage::open(root.to_str().unwrap()) {
        Ok(_) => panic!("dtype {dtype} must not open — it is not a supported ziv pixel dtype"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains(expect_named),
        "error for {dtype} should name the offending dtype, got: {msg}"
    );
}
