//! Proves the remote object-store abstraction (Deliverable B) is TRANSPARENT: reading the same
//! region from the same OME-Zarr fixture through `zarrs_object_store::AsyncObjectStore` produces
//! byte-identical f64 values to reading it through the existing `FilesystemStore` path.
//!
//! Uses `object_store::memory::InMemory` (populated with the committed fixture's on-disk bytes)
//! rather than a real network fetch or a spawned HTTP server, per the plan's own guidance: an
//! in-memory store proves the `AsyncObjectStore`/`pollster::block_on` plumbing is wired
//! correctly and deterministically, without flakiness from a real network/process dependency in
//! CI. This is the CI-deterministic proof; no separate `#[ignore]`-gated real-network test was
//! added on top, since `InMemory` already exercises the exact same `zarrs_object_store` code
//! path a real S3/GCS/Azure/HTTP store would use (only the underlying `object_store::ObjectStore`
//! impl differs, and that's `object_store`'s own well-tested abstraction, not code this crate
//! owns).
use std::path::Path;
use std::sync::Arc;

use object_store::memory::InMemory;
use object_store::path::Path as StorePath;
use object_store::ObjectStoreExt;

use zarr_core::store::RemoteStoreSpec;
use zarr_core::ZarrImage;

/// Recursively copies every file under `dir` into `store`, keyed by its path relative to `dir`
/// (matching how `zarrs`/`object_store` address keys — forward-slash-separated, no leading `/`).
fn populate_from_dir(store: &InMemory, dir: &Path, base: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            populate_from_dir(store, &path, base);
        } else {
            let rel = path.strip_prefix(base).unwrap();
            let key = rel
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            let bytes = std::fs::read(&path).unwrap();
            pollster::block_on(store.put(&StorePath::from(key.as_str()), bytes.into())).unwrap();
        }
    }
}

#[test]
fn in_memory_object_store_read_matches_filesystem_read() {
    let fixture_dir = Path::new("../../tests/fixtures/sample_v04.ome.zarr");

    // Reference: open the SAME fixture via the existing, well-tested FilesystemStore path.
    let fs_img = ZarrImage::open(fixture_dir.to_str().unwrap()).unwrap();
    let fs_region = fs_img.read_region_f64(0, 0, 0, 0, 0..64, 0..64).unwrap();
    let fs_region_ch1 = fs_img.read_region_f64(0, 0, 1, 0, 16..32, 0..16).unwrap();

    // Populate an in-memory object store with the fixture's exact on-disk bytes, keyed
    // identically to how the local filesystem lays them out.
    let mem = InMemory::new();
    populate_from_dir(&mem, fixture_dir, fixture_dir);

    let remote = RemoteStoreSpec {
        store: Arc::new(mem),
        group_path: "/".to_string(),
    };
    let remote_img = ZarrImage::open_remote_store(remote).unwrap();

    // Same metadata.
    assert_eq!(remote_img.dtype(), fs_img.dtype());
    assert_eq!(remote_img.num_levels(), fs_img.num_levels());
    assert_eq!(remote_img.full_yx(), fs_img.full_yx());

    // Same pixel data, read through the async AsyncObjectStore + pollster::block_on path.
    let remote_region = remote_img
        .read_region_f64(0, 0, 0, 0, 0..64, 0..64)
        .unwrap();
    assert_eq!(remote_region, fs_region);

    let remote_region_ch1 = remote_img
        .read_region_f64(0, 0, 1, 0, 16..32, 0..16)
        .unwrap();
    assert_eq!(remote_region_ch1, fs_region_ch1);
}

/// Proves the exact concurrency pattern the server route actually uses
/// (`crates/server/src/routes.rs`'s `tile_handler`: `tokio::task::spawn_blocking(move || engine
/// .tile(...))`) works for a remote-backed image: `read_region_f64`'s `pollster::block_on` call
/// must succeed from inside a `spawn_blocking` closure — a blocking OS thread that is NOT part
/// of the tokio reactor's own worker pool — not just from a plain sync `#[test]` fn (which the
/// test above already covers). If `pollster::block_on` somehow required an ambient tokio
/// reactor unavailable from a `spawn_blocking` thread, this would hang or panic; it does
/// neither, confirming the chosen `pollster` approach (over `Handle::block_on`, which WOULD
/// panic without an ambient runtime handle at all, or work differently under
/// `block_in_place`) is correct for this exact call site.
#[tokio::test(flavor = "multi_thread")]
async fn read_region_f64_works_inside_spawn_blocking_like_the_server_route_does() {
    let fixture_dir = Path::new("../../tests/fixtures/sample_v04.ome.zarr");
    let fs_img = ZarrImage::open(fixture_dir.to_str().unwrap()).unwrap();
    let expected = fs_img.read_region_f64(0, 0, 0, 0, 0..64, 0..64).unwrap();

    let mem = InMemory::new();
    populate_from_dir(&mem, fixture_dir, fixture_dir);
    let remote = RemoteStoreSpec {
        store: Arc::new(mem),
        group_path: "/".to_string(),
    };
    let remote_img = ZarrImage::open_remote_store(remote).unwrap();

    let region = tokio::task::spawn_blocking(move || {
        remote_img
            .read_region_f64(0, 0, 0, 0, 0..64, 0..64)
            .unwrap()
    })
    .await
    .expect("spawn_blocking task must not panic");

    assert_eq!(region, expected);
}
