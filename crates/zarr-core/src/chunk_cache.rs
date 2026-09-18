//! A cache of DECODED chunk planes, for the remote read path.
//!
//! # Why this exists
//!
//! IIIF tiles and Zarr chunks are different rectangles. A typical OME-Zarr chunk is 1024x1024
//! while ziv serves 512x512 tiles, so four adjacent tiles each overlap the same chunk — and
//! without a cache each of them fetches and decodes that whole chunk again. Measured against IDR
//! `idr0054A/5025551.zarr` over HTTPS, one level-0 chunk is 789 KB and takes ~14s to arrive, so
//! that repetition is the dominant cost of opening a region cold.
//!
//! `zarrs` ships its own chunk cache (`zarrs::array::ChunkCache`), and it was the first thing
//! checked. It cannot be used here: it is entirely synchronous — it takes an
//! `Arc<Array<dyn ReadableStorageTraits>>` and every method on the trait is sync — whereas the
//! path that needs caching is the ASYNC object-store one. What zarrs does provide, and what this
//! module is built on rather than reimplementing, is the chunk geometry: `chunks_in_array_subset`
//! says which chunks a request touches and `chunk_subset_bounded` gives each one's extent clamped
//! to the array.
//!
//! # What is cached
//!
//! One entry is the decoded YX plane of ONE chunk for ONE `(t, c, z)`, widened to `f64`. Keying
//! per `(t, c, z)` rather than caching the chunk's full N-D extent keeps an entry to a single
//! plane whatever the chunk's shape along the other axes, and reuse still lands where it matters:
//! between the tiles of one view, which share a plane and differ only in YX.
//!
//! # Bounds
//!
//! Entries are weighed by their real byte size and the cache is byte-bounded, the same discipline
//! as the server's tile cache — an entry-count bound would be meaningless when one entry can be
//! 8 MB (1024x1024 f64) and another a few KB. Nothing is ever invalidated because nothing can go
//! stale: ziv opens an immutable array and never writes.
use std::sync::Arc;

use ndarray::Array2;

/// Identifies one decoded chunk plane: a chunk of a pyramid level, at one point on every
/// non-spatial axis, of one image.
///
/// `image` exists because the cache is process-wide and shared. Two different arrays routinely have
/// identical chunk geometry, so without it a request for image B's chunk (0, 0) would be answered
/// with image A's. It is `Arc<str>` rather than `String` so cloning the key on every lookup stays a
/// refcount bump rather than an allocation; a label image gets an identity distinct from its
/// parent's, because a label is a different array.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChunkPlaneKey {
    pub image: Arc<str>,
    pub level: usize,
    pub t: u64,
    pub c: u64,
    pub z: u64,
    /// The chunk's position in the chunk grid, as zarrs indexes it.
    pub chunk: Vec<u64>,
}

/// Default cache size. Chosen against the shape of a real request rather than a round number: a
/// browser opening a 2702x2700 image at native resolution touches a 3x3 grid of 1024x1024 chunks,
/// which at 8 MB per decoded f64 plane is ~72 MB for one channel. 256 MB therefore holds the
/// working set of a multi-channel view of one plane, which is exactly the reuse this cache exists
/// to capture, while staying small enough to sit alongside the tile cache on a modest box.
pub const DEFAULT_CHUNK_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// Environment override for [`DEFAULT_CHUNK_CACHE_BYTES`]. `0` disables the cache entirely, which
/// is the honest way to measure what it is worth on a given deployment.
pub const CHUNK_CACHE_BYTES_ENV: &str = "ZIV_CHUNK_CACHE_BYTES";

/// A byte-bounded, coalescing cache of decoded chunk planes.
#[derive(Clone)]
pub struct ChunkPlaneCache {
    inner: Option<moka::future::Cache<ChunkPlaneKey, Arc<Array2<f64>>>>,
}

impl std::fmt::Debug for ChunkPlaneCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkPlaneCache")
            .field("enabled", &self.inner.is_some())
            .finish()
    }
}

impl ChunkPlaneCache {
    /// Builds a cache sized from `ZIV_CHUNK_CACHE_BYTES`, falling back to
    /// [`DEFAULT_CHUNK_CACHE_BYTES`]. A value of `0` — or a value that does not parse — yields a
    /// disabled cache that always misses, so a misconfigured deployment degrades to the previous
    /// behaviour rather than failing to start.
    #[must_use]
    pub fn from_env() -> Self {
        let bytes = std::env::var(CHUNK_CACHE_BYTES_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_CHUNK_CACHE_BYTES);
        Self::with_capacity_bytes(bytes)
    }

    /// Builds a cache holding at most `bytes` of decoded planes. `0` disables it.
    #[must_use]
    pub fn with_capacity_bytes(bytes: u64) -> Self {
        if bytes == 0 {
            return Self { inner: None };
        }
        Self {
            inner: Some(
                moka::future::Cache::builder()
                    .max_capacity(bytes)
                    .weigher(|_k: &ChunkPlaneKey, v: &Arc<Array2<f64>>| {
                        // The real cost of the entry: its elements. `u32` is moka's weight type;
                        // an f64 plane large enough to saturate it would be 500+ GB, so the
                        // saturating cast can only ever under-report a plane no machine can hold.
                        u32::try_from(v.len() * std::mem::size_of::<f64>()).unwrap_or(u32::MAX)
                    })
                    .build(),
            ),
        }
    }

    /// Returns the cached plane for `key`, or computes it with `init` and caches that.
    ///
    /// Concurrent callers asking for the SAME key share one execution of `init` rather than each
    /// fetching the chunk — which is the case that matters here, because a viewer requests a whole
    /// grid of tiles at once and they overlap the same chunks. Without coalescing the first wave
    /// of tiles would gain nothing.
    ///
    /// An error from `init` is propagated to every waiter and is NOT cached, so a transient remote
    /// failure cannot poison a chunk for the life of the process.
    pub async fn get_or_try_insert<F, E>(
        &self,
        key: ChunkPlaneKey,
        init: F,
    ) -> Result<Arc<Array2<f64>>, E>
    where
        F: std::future::Future<Output = Result<Array2<f64>, E>>,
        E: Clone + Send + Sync + 'static,
    {
        let Some(cache) = &self.inner else {
            return init.await.map(Arc::new);
        };
        cache
            .try_get_with(key, async move { init.await.map(Arc::new) })
            .await
            .map_err(|e: Arc<E>| (*e).clone())
    }

    /// Bytes currently held. Best-effort: moka accounts asynchronously, so this can lag writes.
    #[must_use]
    pub fn weighted_size(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, moka::future::Cache::weighted_size)
    }

    /// Number of entries currently held. Best-effort, for the same reason.
    #[must_use]
    pub fn entry_count(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, moka::future::Cache::entry_count)
    }

    /// Whether caching is on at all.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Waits for moka's pending bookkeeping so `entry_count`/`weighted_size` reflect every
    /// completed write. Tests need this; production never does.
    pub async fn sync_for_tests(&self) {
        if let Some(c) = &self.inner {
            c.run_pending_tasks().await;
        }
    }
}
