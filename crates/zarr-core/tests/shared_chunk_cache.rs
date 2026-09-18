//! Two images sharing one decoded-chunk cache must never see each other's planes.
//!
//! This is the correctness half of the shared-cache change. `ChunkPlaneKey` carries an image
//! identity precisely so that two different arrays with identical chunk geometry — which is the
//! NORMAL case, since fixtures and real pyramids alike use round chunk sizes — cannot collide.
use zarr_core::{ChunkPlaneCache, ZarrImage};

/// Both fixtures are 64x64 with 32x32 chunks, so every `(level, t, c, z, chunk)` tuple one of them
/// produces, the other produces too. Without the image in the key, the second read would return
/// the first image's pixels.
const A: &str = "../../tests/fixtures/sample_v04.ome.zarr";
const B: &str = "../../tests/fixtures/sample_labels.ome.zarr";

#[test]
fn two_images_sharing_a_cache_do_not_collide() {
    let cache = ChunkPlaneCache::with_capacity_bytes(64 * 1024 * 1024);
    let a = ZarrImage::open_shared(A, false, "a", cache.clone()).unwrap();
    let b = ZarrImage::open_shared(B, false, "b", cache.clone()).unwrap();

    let pa = a.read_region_f64(0, 0, 0, 0, 0..64, 0..64).unwrap();
    let pb = b.read_region_f64(0, 0, 0, 0, 0..64, 0..64).unwrap();

    // sample_v04 channel 0 ramps along x with value = x; sample_labels channel 0 ramps with
    // value = x*255/64. They agree at the origin and diverge immediately, so a collision shows up
    // as pb equalling pa.
    assert_eq!(pa[[0, 1]], 1.0, "sample_v04 c0 is value = x");
    assert_eq!(pb[[0, 1]], 3.0, "sample_labels c0 is value = x*255/64");
    assert_ne!(
        pa, pb,
        "a shared cache must not serve one image's plane for another"
    );
}

/// A local image reads from disk, not from the cache, whatever cache it is handed. Local reads pay
/// decompression rather than latency and sit on the page cache underneath, so spending the shared
/// byte budget on them would buy nothing.
#[test]
fn local_images_do_not_consume_the_shared_budget() {
    let cache = ChunkPlaneCache::with_capacity_bytes(64 * 1024 * 1024);
    let a = ZarrImage::open_shared(A, false, "a", cache).unwrap();
    assert!(!a.chunk_cache_enabled());
    let _ = a.read_region_f64(0, 0, 0, 0, 0..64, 0..64).unwrap();
    assert_eq!(a.chunk_cache_weighted_size(), 0);
}
