//! The decoded-chunk cache: correctness first, then proof that it actually stops re-fetching.
//!
//! IIIF tiles and Zarr chunks are different rectangles — tiles are typically 512x512, chunks
//! 1024x1024 — so adjacent tiles overlap the same chunk. Uncached, each one fetches and decodes
//! that chunk again, which against a remote store is the dominant cost of opening a region cold.
//!
//! A cache that returns wrong pixels is far worse than no cache, so most of this file is
//! equivalence: for every awkward geometry, the cached answer must be byte-identical to the
//! uncached one. The reuse proof then uses a counting object store, because "it got faster" is not
//! evidence — the store either received a second request for that chunk or it did not.
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use object_store::memory::InMemory;
use object_store::path::Path as StorePath;
use object_store::{
    GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore, ObjectStoreExt,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result as OsResult,
};

use zarr_core::store::RemoteStoreSpec;
use zarr_core::ZarrImage;

/// 64x64 over a 2x2 grid of 32x32 chunks, two channels whose values differ (ch0 varies with x,
/// ch1 with y) — small, and every interesting geometry fits inside it.
const V04: &str = "../../tests/fixtures/sample_v04.ome.zarr";
/// 1024x1024 over a 4x4 grid of 256x256 chunks.
const MULTI_TILE: &str = "../../tests/fixtures/sample_multi_tile.ome.zarr";
/// 3 timepoints x 3 channels x 5 z-planes, each plane a distinct flat value.
const MULTIDIM: &str = "../../tests/fixtures/sample_multidim.ome.zarr";

/// Wraps a store and counts the GETs that reach it, so a test can assert a chunk was not fetched
/// twice rather than inferring it from a stopwatch.
#[derive(Debug)]
struct CountingStore {
    inner: InMemory,
    gets: Arc<AtomicUsize>,
}

impl std::fmt::Display for CountingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CountingStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for CountingStore {
    async fn put_opts(
        &self,
        location: &StorePath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> OsResult<PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }
    async fn put_multipart_opts(
        &self,
        location: &StorePath,
        opts: PutMultipartOptions,
    ) -> OsResult<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }
    async fn get_opts(&self, location: &StorePath, options: GetOptions) -> OsResult<GetResult> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.inner.get_opts(location, options).await
    }
    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<'static, OsResult<StorePath>>,
    ) -> futures::stream::BoxStream<'static, OsResult<StorePath>> {
        self.inner.delete_stream(locations)
    }
    fn list(
        &self,
        prefix: Option<&StorePath>,
    ) -> futures::stream::BoxStream<'static, OsResult<ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(&self, prefix: Option<&StorePath>) -> OsResult<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(
        &self,
        from: &StorePath,
        to: &StorePath,
        options: object_store::CopyOptions,
    ) -> OsResult<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

fn load(store: &InMemory, dir: &Path, base: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            load(store, &path, base);
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

/// Opens `fixture` through the async object-store path — the only path the cache is on — with the
/// cache sized by `bytes` (0 disables it).
fn open_remote(fixture: &str, bytes: u64) -> ZarrImage {
    open_remote_counted(fixture, bytes).0
}

fn open_remote_counted(fixture: &str, bytes: u64) -> (ZarrImage, Arc<AtomicUsize>) {
    let dir = Path::new(fixture);
    let mem = InMemory::new();
    load(&mem, dir, dir);
    let gets = Arc::new(AtomicUsize::new(0));
    let store = CountingStore {
        inner: mem,
        gets: gets.clone(),
    };
    // Capacity is passed explicitly rather than through `ZIV_CHUNK_CACHE_BYTES`: these tests run
    // as threads in one process, and a shared environment variable would race between them.
    let img = ZarrImage::open_remote_store_with_chunk_cache(
        RemoteStoreSpec {
            store: Arc::new(store),
            group_path: "/".to_string(),
        },
        bytes,
    )
    .unwrap();
    (img, gets)
}

/// Every awkward geometry against a 2x2 chunk grid: inside one chunk, exactly one chunk, spanning
/// two, spanning all four, and running to the array edge. The cached answer must be identical to
/// the uncached one in all of them — a cache that returns wrong pixels is worse than none.
#[test]
fn cached_reads_are_identical_to_uncached() {
    let cached = open_remote(V04, 64 * 1024 * 1024);
    let uncached = open_remote(V04, 0);
    assert!(!uncached.chunk_cache_enabled(), "control must be uncached");
    assert!(cached.chunk_cache_enabled(), "subject must be cached");

    let cases: &[(&str, Range<u64>, Range<u64>)] = &[
        ("inside one chunk", 4..20, 4..20),
        ("exactly one chunk", 0..32, 0..32),
        ("spanning two horizontally", 0..32, 16..48),
        ("spanning two vertically", 16..48, 0..32),
        ("spanning all four", 16..48, 16..48),
        ("whole image", 0..64, 0..64),
        ("to the far edge", 40..64, 40..64),
        ("single pixel", 33..34, 47..48),
    ];
    for (what, y, x) in cases {
        for c in 0..2u64 {
            let a = cached
                .read_region_f64(0, 0, c, 0, y.clone(), x.clone())
                .unwrap();
            let b = uncached
                .read_region_f64(0, 0, c, 0, y.clone(), x.clone())
                .unwrap();
            assert_eq!(a, b, "{what}, channel {c}: cached read must match uncached");
        }
    }
}

/// The same, against a bigger grid (4x4 chunks of 256) with regions that straddle several chunks
/// at once — the assembly arithmetic has more ways to be wrong here.
#[test]
fn cached_reads_match_on_a_larger_chunk_grid() {
    let cached = open_remote(MULTI_TILE, 64 * 1024 * 1024);
    let uncached = open_remote(MULTI_TILE, 0);
    for (y, x) in [
        (0..512, 0..512),
        (100..900, 100..900),
        (255..257, 255..257), // straddles a chunk boundary by one pixel each way
        (768..1024, 768..1024),
    ] {
        let a = cached
            .read_region_f64(0, 0, 0, 0, y.clone(), x.clone())
            .unwrap();
        let b = uncached.read_region_f64(0, 0, 0, 0, y, x).unwrap();
        assert_eq!(a, b);
    }
}

/// The reuse the cache exists for: a second tile overlapping the same chunks must not send the
/// store another request. This is the property a stopwatch cannot establish.
#[test]
fn overlapping_reads_do_not_refetch_the_same_chunk() {
    let (img, gets) = open_remote_counted(MULTI_TILE, 64 * 1024 * 1024);

    // Cold: the top-left 256x256 chunk.
    let _ = img.read_region_f64(0, 0, 0, 0, 0..256, 0..256).unwrap();
    let after_first = gets.load(Ordering::SeqCst);
    assert!(
        after_first > 0,
        "the first read must actually hit the store"
    );

    // Three more reads wholly inside that same chunk.
    for (y, x) in [(0..128, 0..128), (128..256, 128..256), (64..192, 64..192)] {
        let _ = img.read_region_f64(0, 0, 0, 0, y, x).unwrap();
    }
    assert_eq!(
        gets.load(Ordering::SeqCst),
        after_first,
        "reads inside an already-cached chunk must not touch the store again"
    );

    // A read into a NEW chunk must still fetch — the cache must not be silently serving stale or
    // wrong data for regions it has never seen.
    let _ = img.read_region_f64(0, 0, 0, 0, 512..640, 512..640).unwrap();
    assert!(
        gets.load(Ordering::SeqCst) > after_first,
        "an uncached chunk must be fetched"
    );
}

/// Without the cache the same access pattern keeps hitting the store — this is the control that
/// makes the test above mean something.
#[test]
fn without_the_cache_overlapping_reads_refetch() {
    let (img, gets) = open_remote_counted(MULTI_TILE, 0);
    let _ = img.read_region_f64(0, 0, 0, 0, 0..256, 0..256).unwrap();
    let after_first = gets.load(Ordering::SeqCst);
    for (y, x) in [(0..128, 0..128), (128..256, 128..256), (64..192, 64..192)] {
        let _ = img.read_region_f64(0, 0, 0, 0, y, x).unwrap();
    }
    assert!(
        gets.load(Ordering::SeqCst) > after_first,
        "with the cache disabled, repeated overlapping reads must still reach the store"
    );
}

/// A cache key that ignored the channel would serve one channel's pixels for another — silently,
/// and only on images where the channels differ. The v04 fixture's channels differ by construction.
#[test]
fn channels_do_not_collide_in_the_cache() {
    let img = open_remote(V04, 64 * 1024 * 1024);
    let ch0 = img.read_region_f64(0, 0, 0, 0, 0..64, 0..64).unwrap();
    let ch1 = img.read_region_f64(0, 0, 1, 0, 0..64, 0..64).unwrap();
    assert_ne!(ch0, ch1, "the two channels must not return the same plane");

    // ...and re-reading each still gives its own data, not whichever was cached last.
    assert_eq!(img.read_region_f64(0, 0, 0, 0, 0..64, 0..64).unwrap(), ch0);
    assert_eq!(img.read_region_f64(0, 0, 1, 0, 0..64, 0..64).unwrap(), ch1);
}

/// The same, for the t and z axes. `sample_multidim` gives every (t, z) plane a distinct flat
/// value, so a key collision shows up as an exact wrong number.
#[test]
fn timepoints_and_z_planes_do_not_collide_in_the_cache() {
    let img = open_remote(MULTIDIM, 64 * 1024 * 1024);
    // value(t, z) = 80 + 40*t + 15*z, identical across channels of this fixture.
    for (t, z, want) in [
        (0u64, 0u64, 80.0),
        (0, 4, 140.0),
        (2, 0, 160.0),
        (2, 4, 220.0),
        (1, 2, 150.0),
    ] {
        let plane = img.read_region_f64(0, t, 0, z, 0..32, 0..32).unwrap();
        assert_eq!(
            plane[[0, 0]],
            want,
            "t={t} z={z} must not be served another plane"
        );
    }
    // Re-read in a different order: still correct, so nothing was overwritten by a later key.
    assert_eq!(
        img.read_region_f64(0, 0, 0, 0, 0..32, 0..32).unwrap()[[0, 0]],
        80.0
    );
    assert_eq!(
        img.read_region_f64(0, 2, 0, 4, 0..32, 0..32).unwrap()[[0, 0]],
        220.0
    );
}

/// The cache is byte-bounded, so a long session cannot grow it without limit. A capacity far
/// smaller than the data forces eviction, and the answers must stay correct through it.
#[test]
fn is_byte_bounded_and_stays_correct_under_eviction() {
    // 256x256 f64 chunk = 512 KB; 1 MB holds about two of the sixteen.
    let img = open_remote(MULTI_TILE, 1024 * 1024);
    let reference = open_remote(MULTI_TILE, 0);
    for cy in 0..4u64 {
        for cx in 0..4u64 {
            let (y, x) = (cy * 256..cy * 256 + 256, cx * 256..cx * 256 + 256);
            let got = img
                .read_region_f64(0, 0, 0, 0, y.clone(), x.clone())
                .unwrap();
            let want = reference.read_region_f64(0, 0, 0, 0, y, x).unwrap();
            assert_eq!(
                got, want,
                "chunk ({cy},{cx}) must survive eviction pressure"
            );
        }
    }
    assert!(
        img.chunk_cache_weighted_size() <= 1024 * 1024,
        "cache must respect its byte bound, holds {}",
        img.chunk_cache_weighted_size()
    );
}

/// Out-of-range requests must still be refused with the cache in the path — the bounds check must
/// not have been bypassed by the chunk-wise route.
#[test]
fn still_rejects_out_of_range_regions() {
    let img = open_remote(V04, 64 * 1024 * 1024);
    assert!(img.read_region_f64(0, 0, 0, 0, 0..9999, 0..64).is_err());
    assert!(img.read_region_f64(0, 0, 99, 0, 0..64, 0..64).is_err());
}
