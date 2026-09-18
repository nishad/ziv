use std::borrow::Cow;
use std::ops::Range;
use std::sync::{Arc, Once};
use std::time::Duration;

use ndarray::Array2;
use zarrs::array::data_type::{Int8DataType, UInt8DataType};
use zarrs::array::{Array, ArraySubset};
use zarrs::filesystem::FilesystemStore;
use zarrs::group::Group;
use zarrs::plugin::ExtensionAliasesV2;
use zarrs::storage::{AsyncReadableStorageTraits, ReadableStorage, ReadableStorageTraits};

use crate::chunk_cache::{ChunkPlaneCache, ChunkPlaneKey};
use crate::error::{DType, ZarrError};
use crate::label::{parse_image_label, parse_label_names, LabelInfo};
use crate::metadata::{parse_multiscale, AxesModel, AxisKind};
use crate::store::{parse_store_spec, StoreSpec};

/// Teaches zarrs to recognize `>u1`/`<u1`/`>i1`/`<i1` as Zarr V2 spellings of `uint8`/`int8`.
///
/// Endianness is meaningless for a single byte — `>u1`, `<u1` and `|u1` describe byte-identical
/// data — and real-world writers do emit the prefixed forms. But zarrs 0.23.13 registers only the
/// single V2 alias `|u1` for `UInt8DataType` and `|i1` for `Int8DataType` (`src/array/data_type/
/// {uint,int}.rs`: `impl_extension_aliases!(UInt8DataType, v3: "uint8", [], v2: "|u1", ["|u1"])`),
/// unlike every MULTI-byte type, which registers both prefixes for the same underlying `DataType`
/// (e.g. `v2: "<u2", ["<u2", ">u2"]`). So a `.zarray` declaring `>u1` is rejected inside zarrs'
/// own plugin name resolution — `Err("data type >u1 is not supported")` — during `Array::open`,
/// before any `DataType` reaches ziv's `classify_dtype`. The fix therefore has to land in dtype
/// NAME RESOLUTION, upstream of `Array::open`; loosening `classify_dtype` cannot work, because it
/// is never reached.
///
/// zarrs exposes exactly that hook: V2 data-type plugins match names through a runtime-mutable
/// alias list (`zarrs::plugin::ExtensionAliasesV2`, re-exported from `zarrs_plugin`), so adding
/// the four prefixed spellings as first-class aliases makes plain `Array::open`/`Array::async_open`
/// accept them with NO fork of zarrs' open path — no hand-rolled `.zarray`/`.zattrs` fetch-and-patch
/// to keep in sync with upstream `open_metadata` on every zarrs bump, no duplicated sync/async
/// copies, no extra metadata round trip, and `arr.metadata()` still reports the true on-disk dtype
/// string rather than a rewritten one.
///
/// MULTI-byte big-endian types are deliberately NOT touched here. They already work end-to-end:
/// zarrs registers both prefixes as aliases of one `DataType`, AND its V2->V3 metadata conversion
/// threads the parsed endianness into the `bytes` codec's `endian` config, which byte-swaps every
/// component on decode. The `dtype_be_u2_round_trip_*` and `dtype_be_i4_*` tests in this module
/// prove that empirically against genuinely big-endian-authored chunk bytes. Aliasing THEIR
/// prefixes would be unnecessary and, done carelessly, actively wrong — it would discard the real
/// byte order the data was written in. For the one-byte case the same `endian` config is reached
/// (`>u1` still parses as big-endian) but swapping a 1-byte component is a no-op, which is why
/// aliasing is safe here and only here.
///
/// The alias registry is process-global and behind an `RwLock`, so registration is guarded by a
/// `Once`: repeated `ZarrImage::open` calls must not push duplicate aliases onto the list.
fn register_one_byte_endian_aliases() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        UInt8DataType::aliases_v2_mut()
            .aliases_str
            .extend([Cow::Borrowed(">u1"), Cow::Borrowed("<u1")]);
        Int8DataType::aliases_v2_mut()
            .aliases_str
            .extend([Cow::Borrowed(">i1"), Cow::Borrowed("<i1")]);
    });
}

/// Records the pyramid's dtype on the first level and, on every level after it, REJECTS any level
/// that disagrees.
///
/// A `ZarrImage` carries exactly one `DType`, and `read_region_f64` dispatches on it — it hands
/// zarrs a single concrete element type for whichever level the tile engine picked (see
/// `retrieve_widened`). So a pyramid whose levels disagree is not merely unusual, it is unreadable
/// at any level that differs from level 0. Accepting it at open time and keeping only level 0's
/// dtype would defer the failure to tile-read time, where it surfaces as an opaque zarrs type
/// error ("Incompatible element type for data type") naming neither the level nor the mismatch —
/// i.e. a served image that returns 200 at low zoom and 500 at high zoom. This project fails loud
/// at open instead, and names both levels and both dtypes so an operator can act on it.
///
/// Shared by the sync and async open loops so the rule is defined exactly once.
fn check_level_dtype(
    seen: &mut Option<DType>,
    dt: DType,
    level: usize,
    level_path: &str,
    level_paths: &[String],
) -> Result<(), ZarrError> {
    match seen {
        None => {
            *seen = Some(dt);
            Ok(())
        }
        Some(first) if *first != dt => Err(ZarrError::InconsistentLevelDtype {
            level,
            level_path: level_path.to_string(),
            first_path: level_paths.first().cloned().unwrap_or_default(),
            first: *first,
            found: dt,
        }),
        Some(_) => Ok(()),
    }
}

/// Classifies a `zarrs` data type into our `DType` enum, or `Err(UnsupportedDtype)` for anything
/// else. Shared between the sync (`FilesystemStore`) and async (`AsyncObjectStore`) open paths
/// so the dtype ladder is defined exactly once.
fn classify_dtype(dt: &zarrs::array::DataType) -> Result<DType, ZarrError> {
    if *dt == zarrs::array::data_type::uint8() {
        Ok(DType::U8)
    } else if *dt == zarrs::array::data_type::uint16() {
        Ok(DType::U16)
    } else if *dt == zarrs::array::data_type::uint32() {
        Ok(DType::U32)
    } else if *dt == zarrs::array::data_type::uint64() {
        Ok(DType::U64)
    } else if *dt == zarrs::array::data_type::int8() {
        Ok(DType::I8)
    } else if *dt == zarrs::array::data_type::int16() {
        Ok(DType::I16)
    } else if *dt == zarrs::array::data_type::int32() {
        Ok(DType::I32)
    } else if *dt == zarrs::array::data_type::int64() {
        Ok(DType::I64)
    } else if *dt == zarrs::array::data_type::float32() {
        Ok(DType::F32)
    } else if *dt == zarrs::array::data_type::float64() {
        Ok(DType::F64)
    } else {
        Err(ZarrError::UnsupportedDtype(dt.to_string()))
    }
}

/// The pyramid levels, held as either sync (local filesystem, `FilesystemStore`) or async
/// (remote object store, `AsyncObjectStore` via `zarrs_object_store`) zarrs arrays.
///
/// Two variants exist — rather than one generic `Array<dyn SomeSharedTrait>` — because zarrs
/// draws a hard sync/async line: `zarrs_object_store::AsyncObjectStore` only implements the
/// ASYNC storage traits (there is no sync object_store-backed zarrs storage type), while the
/// existing, well-tested local filesystem path keeps using the SYNC `FilesystemStore` unchanged
/// (zero behavioral risk to it). `read_region_f64` dispatches per-variant: the `Async` arm drives
/// its zarrs futures with `pollster::block_on` (see that call site for why).
enum Levels {
    Sync(Vec<Array<dyn ReadableStorageTraits>>),
    Async(Vec<Array<dyn AsyncReadableStorageTraits>>),
}

impl Levels {
    fn len(&self) -> usize {
        match self {
            Levels::Sync(v) => v.len(),
            Levels::Async(v) => v.len(),
        }
    }
}

/// Extracts the `omero` metadata block from group attributes, checking the OME-Zarr 0.5 nested
/// location (`attributes.ome.omero`) before the 0.4 top-level location (`attributes.omero`).
fn extract_omero(attrs: &serde_json::Map<String, serde_json::Value>) -> Option<serde_json::Value> {
    attrs
        .get("ome")
        .and_then(|o| o.get("omero"))
        .cloned()
        .or_else(|| attrs.get("omero").cloned())
}

/// A label image opened alongside its parent: the colour table, plus the pixels as a full image.
///
/// The image is a `ZarrImage` rather than a bare array because a label image genuinely is one —
/// it has its own axes, dtype and resolution pyramid, and that pyramid does NOT have to match its
/// parent's. Treating it as anything smaller leads to reading the wrong level.
pub struct LabelLayer {
    pub info: LabelInfo,
    pub image: ZarrImage,
}

/// One label a `labels/.zattrs` declared that could not actually be opened.
///
/// A malformed or unsupported label used to vanish silently: `open_labels_local`/`_remote`
/// discarded the `Err` and the parent image opened as if the label had never been declared. That
/// made "the image has no label images" a lie whenever a label existed but failed to open — the
/// original report was exactly this, against a real IDR sample whose label array was `int64`
/// (`<i8`), a dtype [`DType::I64`] did not yet exist to classify. Keeping the name and reason here
/// lets every caller (the exporter's `--labels` warning, the server's open-time log) say what
/// actually happened instead of reporting an empty list.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelOpenFailure {
    /// The name under `labels/` that failed to open.
    pub name: String,
    /// `ZarrError`'s `Display` text for why it failed.
    pub reason: String,
}

pub struct ZarrImage {
    axes: AxesModel,
    dtype: DType,
    levels: Levels,
    level_shapes: Vec<Vec<u64>>,
    omero: Option<serde_json::Value>,
    /// Label images (OME-NGFF `labels/`), each opened as a full image in its own right. Empty for
    /// the overwhelming majority of images, and empty inside a label image itself — labels do not
    /// nest.
    labels: Vec<LabelLayer>,
    /// Labels `labels/.zattrs` declared that failed to open, alongside why. Empty in the normal
    /// case; always empty inside a label image itself — see `labels`.
    label_open_failures: Vec<LabelOpenFailure>,
    /// This image's identity in the chunk cache. Distinct per array, INCLUDING per label image,
    /// because a label is a different array and must not share a key with its parent. See
    /// [`crate::chunk_cache::ChunkPlaneKey`] for why the key carries it at all.
    cache_identity: Arc<str>,
    /// Decoded chunk planes, for the remote path only — see [`crate::chunk_cache`]. Local reads
    /// pay decompression, not latency, and hit the page cache underneath; caching 8 MB planes to
    /// save a filesystem read would spend memory to buy nothing.
    chunk_cache: ChunkPlaneCache,
}

impl ZarrImage {
    /// Opens an OME-Zarr image from a store spec: a local filesystem path, or a remote URL
    /// (`s3://`, `gs://`/`gcs://`, `az://`, `http://`/`https://`) — see
    /// `crate::store::parse_store_spec` for the exact dispatch rules and
    /// `crate::store::RemoteStoreSpec`'s doc comment for credential env vars per scheme.
    ///
    /// Runtime-flavor caveat for the remote path: remote reads use `pollster::block_on` (see
    /// `open_remote`) to drive async `object_store` I/O from this sync function. That block runs
    /// safely from a plain thread with no ambient tokio runtime, or from inside
    /// `tokio::task::spawn_blocking` on any runtime flavor, or from a multi-threaded tokio
    /// runtime's own worker thread. It does NOT run safely as the sole worker of a
    /// `current_thread` runtime — `block_on` would then block the only thread available to drive
    /// the future it's waiting on, deadlocking forever. Callers MUST invoke `open()` from a
    /// blocking context (e.g. `spawn_blocking`, as the CLI does) or a multi-threaded runtime —
    /// never as inline work on a `current_thread` runtime.
    ///
    /// # Examples
    ///
    /// ```
    /// use zarr_core::{DType, ZarrImage};
    ///
    /// let img = ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr")?;
    ///
    /// assert_eq!(img.dtype(), DType::U16);
    /// assert_eq!(img.full_yx(), (64, 64));
    /// assert_eq!(img.num_levels(), 2);
    ///
    /// // Axes come from the image's own OME-Zarr metadata, never from position.
    /// assert_eq!(img.size_c(), 2);
    ///
    /// // Pixels are widened to f64 regardless of the on-disk dtype, so callers have one path.
    /// let region = img.read_region_f64(0, 0, 0, 0, 0..8, 0..8)?;
    /// assert_eq!(region.dim(), (8, 8));
    /// # Ok::<(), zarr_core::ZarrError>(())
    /// ```
    ///
    /// Unsupported or malformed input fails loudly at open rather than being coerced:
    ///
    /// ```
    /// use zarr_core::ZarrImage;
    ///
    /// assert!(ZarrImage::open("../../tests/fixtures/does-not-exist.ome.zarr").is_err());
    /// ```
    pub fn open(spec: &str) -> Result<ZarrImage, ZarrError> {
        match parse_store_spec(spec)? {
            StoreSpec::Local(path) => Self::open_local(&path),
            StoreSpec::Remote(remote) => Self::open_remote(remote, ChunkPlaneCache::from_env()),
        }
    }

    /// Same as [`ZarrImage::open`], but with the SSRF `--allow-internal-hosts` escape hatch
    /// (see `crate::store::ssrf`) passed explicitly rather than read from the
    /// `ZIV_ALLOW_INTERNAL_HOSTS` env var — this is what the CLI's `--allow-internal-hosts` flag
    /// wires into (see `crates/cli/src/main.rs`), so an operator-supplied flag takes effect even
    /// if the env var isn't set, without every other `open()` call site in the codebase needing
    /// to change.
    pub fn open_with_options(
        spec: &str,
        allow_internal_hosts: bool,
    ) -> Result<ZarrImage, ZarrError> {
        match crate::store::parse_store_spec_with_options(spec, allow_internal_hosts)? {
            StoreSpec::Local(path) => Self::open_local(&path),
            StoreSpec::Remote(remote) => Self::open_remote(remote, ChunkPlaneCache::from_env()),
        }
    }

    /// Opens `spec` against a chunk cache SHARED with other images.
    ///
    /// `identity` distinguishes this image's cache entries from every other image's and must be
    /// unique per image across the process; the server passes the image's name. Label images
    /// opened underneath get `{identity}/labels/{label}`, because a label is a different array and
    /// must not share a key with its parent.
    ///
    /// A local image ignores `cache` and keeps a disabled one: local reads pay decompression
    /// rather than latency and sit on the page cache underneath, so spending a shared byte budget
    /// on them would buy nothing. `open_shared` therefore does not mean "cached", it means "cached
    /// if caching would help".
    pub fn open_shared(
        spec: &str,
        allow_internal_hosts: bool,
        identity: &str,
        cache: ChunkPlaneCache,
    ) -> Result<ZarrImage, ZarrError> {
        match crate::store::parse_store_spec_with_options(spec, allow_internal_hosts)? {
            StoreSpec::Local(path) => Self::open_local_with_identity(&path, identity),
            StoreSpec::Remote(remote) => Self::open_remote_with_identity(remote, cache, identity),
        }
    }

    /// Opens an already-constructed remote store spec directly, bypassing URL-scheme dispatch.
    ///
    /// This is the same code path `open()` uses for `s3://`/`gs://`/`az://`/`http(s)://` URLs
    /// (see `open_remote`) — it exists as its own public entry point so tests (and any future
    /// caller that already has an `object_store::ObjectStore`, e.g. a pre-authenticated client)
    /// can drive the exact `AsyncObjectStore` read path against an arbitrary store, such as
    /// `object_store::memory::InMemory`, without needing a real network fetch. This is how the
    /// "the store abstraction is transparent" integration test proves the remote path reads
    /// identically to `FilesystemStore` — see `tests/remote_store_transparency.rs`.
    pub fn open_remote_store(
        remote: crate::store::RemoteStoreSpec,
    ) -> Result<ZarrImage, ZarrError> {
        Self::open_remote(remote, ChunkPlaneCache::from_env())
    }

    /// Same, with the decoded-chunk cache sized explicitly instead of from the environment.
    ///
    /// Exists so tests can pin a capacity (including `0`, which disables the cache) without
    /// mutating a process-global environment variable that every other test in the binary shares.
    pub fn open_remote_store_with_chunk_cache(
        remote: crate::store::RemoteStoreSpec,
        chunk_cache_bytes: u64,
    ) -> Result<ZarrImage, ZarrError> {
        Self::open_remote(
            remote,
            ChunkPlaneCache::with_capacity_bytes(chunk_cache_bytes),
        )
    }

    /// Whether this image's reads go through the decoded-chunk cache. False for every local image
    /// (the cache is a remote-latency optimisation) and for a remote one opened with capacity 0.
    #[must_use]
    pub fn chunk_cache_enabled(&self) -> bool {
        self.chunk_cache.is_enabled()
    }

    /// Bytes of decoded chunk planes currently held. Runs moka's pending bookkeeping first, so the
    /// number reflects every completed write rather than lagging behind it.
    #[must_use]
    pub fn chunk_cache_weighted_size(&self) -> u64 {
        pollster::block_on(self.chunk_cache.sync_for_tests());
        self.chunk_cache.weighted_size()
    }

    fn open_local(path: &std::path::Path) -> Result<ZarrImage, ZarrError> {
        let identity = path.to_string_lossy().into_owned();
        Self::open_local_with_identity(path, &identity)
    }

    fn open_local_with_identity(
        path: &std::path::Path,
        identity: &str,
    ) -> Result<ZarrImage, ZarrError> {
        register_one_byte_endian_aliases();
        let store: ReadableStorage =
            Arc::new(FilesystemStore::new(path).map_err(|e| ZarrError::Open(e.to_string()))?);
        let mut image = Self::open_group_local(&store, "/", identity)?;
        let (labels, failures) = Self::open_labels_local(&store, identity);
        image.labels = labels;
        image.label_open_failures = failures;
        Ok(image)
    }

    /// Opens ONE multiscale group at `node` — the image itself at `/`, or a label image at
    /// `/labels/<name>`. Knows nothing about labels, which is what stops a label image from
    /// recursively discovering labels of its own.
    fn open_group_local(
        store: &ReadableStorage,
        node: &str,
        identity: &str,
    ) -> Result<ZarrImage, ZarrError> {
        let group = Group::open(store.clone(), node).map_err(|e| ZarrError::Open(e.to_string()))?;
        let attrs = group.attributes().clone();
        let info = parse_multiscale(&attrs)?;
        let omero = extract_omero(&attrs);
        let base = node.trim_end_matches('/');

        let mut levels = Vec::new();
        let mut level_shapes = Vec::new();
        let mut dtype: Option<DType> = None;
        for (level, rel) in info.level_paths.iter().enumerate() {
            let path = format!("{base}/{}", rel.trim_start_matches('/'));
            let arr =
                Array::open(store.clone(), &path).map_err(|e| ZarrError::Open(e.to_string()))?;
            let dt = classify_dtype(arr.data_type())?;
            check_level_dtype(&mut dtype, dt, level, rel, &info.level_paths)?;
            level_shapes.push(arr.shape().to_vec());
            levels.push(arr);
        }
        Ok(ZarrImage {
            axes: info.axes,
            dtype: dtype.ok_or(ZarrError::NoMultiscales)?,
            levels: Levels::Sync(levels),
            level_shapes,
            omero,
            labels: Vec::new(),
            label_open_failures: Vec::new(),
            cache_identity: Arc::from(format!("{identity}{node}")),
            chunk_cache: ChunkPlaneCache::with_capacity_bytes(0),
        })
    }

    /// Opens whatever `labels/` advertises. Never fails the parent open: an image with no labels
    /// is the normal case, and a label that is missing or malformed is the image's problem to
    /// report, not a reason to refuse to serve the pixels somebody actually asked for.
    ///
    /// A label that fails to open — its group metadata, or the multiscale image underneath — is
    /// no longer discarded: it comes back in the second element, named and with `ZarrError`'s own
    /// message, so a caller can say what actually happened instead of reporting an empty list. See
    /// [`LabelOpenFailure`].
    fn open_labels_local(
        store: &ReadableStorage,
        identity: &str,
    ) -> (Vec<LabelLayer>, Vec<LabelOpenFailure>) {
        let Ok(group) = Group::open(store.clone(), "/labels") else {
            return (Vec::new(), Vec::new());
        };
        let mut labels = Vec::new();
        let mut failures = Vec::new();
        for name in parse_label_names(group.attributes()) {
            let node = format!("/labels/{name}");
            let g = match Group::open(store.clone(), &node) {
                Ok(g) => g,
                Err(e) => {
                    failures.push(LabelOpenFailure {
                        name,
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            let info = parse_image_label(&name, g.attributes());
            match Self::open_group_local(store, &node, identity) {
                Ok(image) => labels.push(LabelLayer { info, image }),
                Err(e) => failures.push(LabelOpenFailure {
                    name,
                    reason: e.to_string(),
                }),
            }
        }
        (labels, failures)
    }

    /// Opens a remote store. `open()` is a synchronous API (called from a plain sync
    /// `main`/tests, no ambient tokio runtime guaranteed), but `zarrs_object_store` is
    /// ASYNC-ONLY, so the async open sequence is driven to completion here with
    /// `pollster::block_on` — see the doc comment on `Cargo.toml`'s `pollster` dependency entry
    /// for why `pollster` specifically was chosen over a dedicated `tokio::runtime::Runtime` or
    /// `Handle::block_on`/`block_in_place`: `pollster::block_on` has no dependency on any
    /// ambient runtime (unlike `Handle::block_on`, which panics without one), which matters
    /// because `open()` must work both from a plain sync context (CLI `main` before any runtime
    /// exists, or a `#[test]`) AND from inside a tokio `spawn_blocking` thread (deliberately not
    /// an async context). A dedicated `tokio::runtime::Runtime::new()` per call would also work
    /// but pulls in the full multi-threaded runtime machinery (thread pool, reactor) just to run
    /// one open sequence; `pollster` is a single-function, zero-thread-pool executor built
    /// exactly for "run this future to completion on whatever thread I'm already on."
    fn open_remote(
        remote: crate::store::RemoteStoreSpec,
        chunk_cache: ChunkPlaneCache,
    ) -> Result<ZarrImage, ZarrError> {
        let identity = remote.group_path.clone();
        pollster::block_on(Self::open_remote_async(remote, chunk_cache, identity))
    }

    fn open_remote_with_identity(
        remote: crate::store::RemoteStoreSpec,
        chunk_cache: ChunkPlaneCache,
        identity: &str,
    ) -> Result<ZarrImage, ZarrError> {
        pollster::block_on(Self::open_remote_async(
            remote,
            chunk_cache,
            identity.to_string(),
        ))
    }

    async fn open_remote_async(
        remote: crate::store::RemoteStoreSpec,
        chunk_cache: ChunkPlaneCache,
        identity: String,
    ) -> Result<ZarrImage, ZarrError> {
        register_one_byte_endian_aliases();
        let store: zarrs::storage::AsyncReadableStorage =
            Arc::new(zarrs_object_store::AsyncObjectStore::new(remote.store));
        let base = remote.group_path.trim_end_matches('/').to_string();
        let group = Group::async_open(store.clone(), &remote.group_path)
            .await
            .map_err(|e| ZarrError::Open(e.to_string()))?;

        let attrs = group.attributes().clone();
        let info = parse_multiscale(&attrs)?;
        let omero = extract_omero(&attrs);

        let mut levels = Vec::new();
        let mut level_shapes = Vec::new();
        let mut dtype: Option<DType> = None;
        for (level, rel) in info.level_paths.iter().enumerate() {
            let node = format!("{base}/{}", rel.trim_start_matches('/'));
            let arr = Array::async_open(store.clone(), &node)
                .await
                .map_err(|e| ZarrError::Open(e.to_string()))?;
            let dt = classify_dtype(arr.data_type())?;
            check_level_dtype(&mut dtype, dt, level, rel, &info.level_paths)?;
            level_shapes.push(arr.shape().to_vec());
            levels.push(arr);
        }

        // Label discovery costs extra metadata round trips before the server binds, which on a
        // slow remote is time the operator waits. It happens once at open rather than per request
        // because `/ziv/dimensions.json` has to be able to say what exists — and only after the
        // parent has opened, so an image that is not going to open at all pays nothing for it.
        let (labels, label_open_failures) =
            Self::open_labels_remote(&store, &base, chunk_cache.clone(), &identity).await;

        Ok(ZarrImage {
            axes: info.axes,
            dtype: dtype.ok_or(ZarrError::NoMultiscales)?,
            levels: Levels::Async(levels),
            level_shapes,
            omero,
            labels,
            label_open_failures,
            cache_identity: Arc::from(identity.as_str()),
            chunk_cache,
        })
    }

    /// Async counterpart of `open_group_local`: opens ONE multiscale group at `node`.
    async fn open_group_remote(
        store: &zarrs::storage::AsyncReadableStorage,
        node: &str,
        chunk_cache: ChunkPlaneCache,
        identity: &str,
    ) -> Result<ZarrImage, ZarrError> {
        let group = Group::async_open(store.clone(), node)
            .await
            .map_err(|e| ZarrError::Open(e.to_string()))?;
        let attrs = group.attributes().clone();
        let info = parse_multiscale(&attrs)?;
        let omero = extract_omero(&attrs);
        let base = node.trim_end_matches('/');

        let mut levels = Vec::new();
        let mut level_shapes = Vec::new();
        let mut dtype: Option<DType> = None;
        for (level, rel) in info.level_paths.iter().enumerate() {
            let path = format!("{base}/{}", rel.trim_start_matches('/'));
            let arr = Array::async_open(store.clone(), &path)
                .await
                .map_err(|e| ZarrError::Open(e.to_string()))?;
            let dt = classify_dtype(arr.data_type())?;
            check_level_dtype(&mut dtype, dt, level, rel, &info.level_paths)?;
            level_shapes.push(arr.shape().to_vec());
            levels.push(arr);
        }
        Ok(ZarrImage {
            axes: info.axes,
            dtype: dtype.ok_or(ZarrError::NoMultiscales)?,
            levels: Levels::Async(levels),
            level_shapes,
            omero,
            labels: Vec::new(),
            label_open_failures: Vec::new(),
            cache_identity: Arc::from(format!("{identity}{node}")),
            chunk_cache,
        })
    }

    /// Async counterpart of `open_labels_local`. Never fails the parent open — see that function,
    /// and [`LabelOpenFailure`] for why a label that does not open is reported rather than dropped.
    async fn open_labels_remote(
        store: &zarrs::storage::AsyncReadableStorage,
        base: &str,
        chunk_cache: ChunkPlaneCache,
        identity: &str,
    ) -> (Vec<LabelLayer>, Vec<LabelOpenFailure>) {
        let Ok(group) = Group::async_open(store.clone(), &format!("{base}/labels")).await else {
            return (Vec::new(), Vec::new());
        };
        let mut out = Vec::new();
        let mut failures = Vec::new();
        for name in parse_label_names(group.attributes()) {
            let node = format!("{base}/labels/{name}");
            let g = match Group::async_open(store.clone(), &node).await {
                Ok(g) => g,
                Err(e) => {
                    failures.push(LabelOpenFailure {
                        name,
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            let info = parse_image_label(&name, g.attributes());
            match Self::open_group_remote(store, &node, chunk_cache.clone(), identity).await {
                Ok(image) => out.push(LabelLayer { info, image }),
                Err(e) => failures.push(LabelOpenFailure {
                    name,
                    reason: e.to_string(),
                }),
            }
        }
        (out, failures)
    }

    /// The label images this image carries, in the order it declares them.
    #[must_use]
    pub fn labels(&self) -> &[LabelLayer] {
        &self.labels
    }

    /// Labels `labels/.zattrs` declared but that failed to open, each with the reason. Empty in
    /// the normal case. See [`LabelOpenFailure`].
    #[must_use]
    pub fn label_open_failures(&self) -> &[LabelOpenFailure] {
        &self.label_open_failures
    }

    /// Looks up one label image by the name it is published under.
    #[must_use]
    pub fn label(&self, name: &str) -> Option<&LabelLayer> {
        self.labels.iter().find(|l| l.info.name == name)
    }

    pub fn axes(&self) -> &AxesModel {
        &self.axes
    }
    pub fn dtype(&self) -> DType {
        self.dtype
    }
    pub fn num_levels(&self) -> usize {
        Levels::len(&self.levels)
    }
    pub fn level_shape(&self, level: usize) -> &[u64] {
        &self.level_shapes[level]
    }
    pub fn omero(&self) -> Option<&serde_json::Value> {
        self.omero.as_ref()
    }

    fn axis_len(&self, level: usize, kind: AxisKind) -> u64 {
        match self.axes.index_of(kind) {
            Some(i) => self.level_shapes[level][i],
            None => 1,
        }
    }
    pub fn size_c(&self) -> u64 {
        self.axis_len(0, AxisKind::C)
    }
    pub fn size_z(&self) -> u64 {
        self.axis_len(0, AxisKind::Z)
    }
    pub fn size_t(&self) -> u64 {
        self.axis_len(0, AxisKind::T)
    }

    /// (Y, X) of level 0.
    pub fn full_yx(&self) -> (u64, u64) {
        self.level_yx(0)
    }

    /// (Y, X) of `level`, using the `AxesModel` to locate the Y/X axes — never assumes they
    /// are positionally the last two dims of `level_shape`.
    pub fn level_yx(&self, level: usize) -> (u64, u64) {
        (
            self.axis_len(level, AxisKind::Y),
            self.axis_len(level, AxisKind::X),
        )
    }

    /// Read one YX plane at (t,c,z) from `level` as a 2D f64 array (rows=y, cols=x).
    ///
    /// Every supported dtype (u8/u16/u32, i8/i16/i32, f32/f64) is widened to f64 here — this
    /// mirrors the old u8->u16 widening in the two-dtype era, and keeps the projection windows
    /// (already f64-typed) working against a single numeric type regardless of source dtype.
    ///
    /// For a remote (async-backed) image, this drives the async zarrs retrieve with
    /// `pollster::block_on` — see `open_remote`'s doc comment for the full reasoning. This
    /// method is called from inside `spawn_blocking` in the server route (a blocking OS thread,
    /// deliberately NOT an async context), which is exactly the context `pollster::block_on` is
    /// suited for.
    pub fn read_region_f64(
        &self,
        level: usize,
        t: u64,
        c: u64,
        z: u64,
        y: Range<u64>,
        x: Range<u64>,
    ) -> Result<Array2<f64>, ZarrError> {
        // One code path: this is the single-channel case of `read_regions_f64`, which is also
        // where the decoded-chunk cache lives. Keeping a separate implementation here is how the
        // cache came to be silently bypassed by every caller of this function.
        self.read_regions_f64(level, t, &[c], z, y, x)?
            .pop()
            .ok_or_else(|| ZarrError::Read("no plane returned for a single channel".into()))
    }

    /// Reads the SAME (t, z, y, x) region across SEVERAL channels, concurrently on the remote
    /// path.
    ///
    /// Compositing a multichannel image needs one plane per enabled channel, and reading them one
    /// after another makes a tile cost the sum of every channel's latency. That is invisible on a
    /// local filesystem and dominant against a remote store: measured against a 27-channel IDR
    /// image over HTTPS, a single 1024x1024 chunk took ~14s to fetch, so one channel of a
    /// native-resolution tile took ~20s and the default seven-channel selection could not complete
    /// inside the server's 30s request timeout at all. The channel reads are independent, so the
    /// fix is to have them in flight together.
    ///
    /// The concurrency is deliberately ASYNC, not threads. Every channel's read is issued inside a
    /// single `pollster::block_on`, so N channels cost one blocking thread rather than N — which
    /// matters because the server bounds concurrent renders with a semaphore, and a render that
    /// silently multiplied itself by the channel count would defeat that bound (16 permits x 27
    /// channels is not a bound anyone chose).
    ///
    /// The LOCAL path stays sequential. Its cost is decompression, not latency, and both callers
    /// already keep the cores busy at a coarser grain — the exporter with a `rayon` loop over
    /// tiles, the server with concurrent renders — so intra-tile parallelism there would mostly
    /// steal from neighbouring tiles rather than add throughput.
    ///
    /// The aggregate [`PER_TILE_READ_DEADLINE`] wraps the WHOLE join rather than each channel.
    /// That is a real tightening: reading N channels sequentially, each under its own deadline,
    /// permitted N x 60s before anything gave up.
    pub fn read_regions_f64(
        &self,
        level: usize,
        t: u64,
        channels: &[u64],
        z: u64,
        y: Range<u64>,
        x: Range<u64>,
    ) -> Result<Vec<Array2<f64>>, ZarrError> {
        if channels.is_empty() {
            return Ok(Vec::new());
        }
        // Plan every channel up front: `plan_ranges` is pure and cheap, and doing it before any
        // I/O means an out-of-range channel index fails fast instead of after N-1 remote reads.
        let planned = channels
            .iter()
            .map(|&c| {
                let (ranges, rows, cols) =
                    self.plan_ranges(level, t, c, z, y.clone(), x.clone())?;
                Ok((ArraySubset::new_with_ranges(&ranges), rows, cols))
            })
            .collect::<Result<Vec<_>, ZarrError>>()?;

        let elems: Vec<Vec<f64>> = match &self.levels {
            Levels::Sync(levels) => planned
                .iter()
                .map(|(subset, _, _)| retrieve_widened(&levels[level], subset, self.dtype))
                .collect::<Result<_, _>>()?,
            Levels::Async(levels) => pollster::block_on(with_read_deadline(
                futures::future::try_join_all(planned.iter().zip(channels).map(
                    |((subset, rows, cols), &c)| {
                        read_plane_via_chunk_cache(
                            &levels[level],
                            &self.chunk_cache,
                            &self.cache_identity,
                            self.dtype,
                            &self.axes,
                            level,
                            (t, c, z),
                            subset,
                            (*rows, *cols),
                        )
                    },
                )),
            ))?,
        };

        elems
            .into_iter()
            .zip(planned.iter())
            .map(|(vals, (_, rows, cols))| {
                Array2::from_shape_vec((*rows, *cols), vals)
                    .map_err(|e| ZarrError::Read(format!("reshape to {rows}x{cols}: {e}")))
            })
            .collect()
    }

    /// Builds the per-axis `Range<u64>` list (in axis order) for a (t,c,z,y,x) region request at
    /// `level`, bounds-checking every axis against the level's declared extent, and returns it
    /// alongside the Y/X output dims (rows, cols). Shared by both the sync and async read paths.
    fn plan_ranges(
        &self,
        level: usize,
        t: u64,
        c: u64,
        z: u64,
        y: Range<u64>,
        x: Range<u64>,
    ) -> Result<(Vec<Range<u64>>, usize, usize), ZarrError> {
        let ndim = self.axes.ndim();
        let mut ranges: Vec<Range<u64>> = Vec::with_capacity(ndim);
        for (i, kind) in self.axes.order.iter().enumerate() {
            let r = match kind {
                AxisKind::T => t..t + 1,
                AxisKind::C => c..c + 1,
                AxisKind::Z => z..z + 1,
                AxisKind::Y => y.clone(),
                AxisKind::X => x.clone(),
            };
            let axis_extent = self.level_shapes[level][i];
            if r.end > axis_extent {
                return Err(ZarrError::Read(format!(
                    "range {}..{} for axis {i} ({kind:?}) exceeds level {level} extent {axis_extent}",
                    r.start, r.end
                )));
            }
            ranges.push(r);
        }
        let yi = self
            .axes
            .index_of(AxisKind::Y)
            .ok_or_else(|| ZarrError::Read("image has no Y axis".into()))?;
        let xi = self
            .axes
            .index_of(AxisKind::X)
            .ok_or_else(|| ZarrError::Read("image has no X axis".into()))?;
        let rows = (ranges[yi].end - ranges[yi].start) as usize;
        let cols = (ranges[xi].end - ranges[xi].start) as usize;
        Ok((ranges, rows, cols))
    }
}

/// Decompression-bomb guard: the maximum decompressed byte size ANY SINGLE chunk this crate will
/// accept, checked BEFORE calling into zarrs' `retrieve_array_subset`/`async_retrieve_array_subset`
/// (which decompresses the ENTIRE containing chunk regardless of how small the requested subset
/// is — confirmed against the zarrs 0.23 source: the codec chain decodes the full chunk byte
/// stream, then array-to-bytes/subsetting logic extracts the requested region afterward, so a
/// 16x16 subset request against an oversized chunk still triggers full-chunk decompression).
///
/// This exists because zarrs 0.23's bytes-to-bytes codecs (blosc/gzip/zstd) do NOT bound
/// decompression output to the array's declared chunk size on their own: blosc allocates
/// `Vec::with_capacity(destsize)` where `destsize` comes from the compressed bytes' OWN header
/// (attacker-controlled if the bytes are hostile/corrupt); gzip does a fully unbounded
/// `read_to_end` streaming decode; zstd is bounded only by the compressed frame's own embedded
/// (still attacker-controlled) content-size field. A hostile or corrupt remote chunk — a handful
/// of compressed bytes whose header/stream claims an enormous decompressed size — could otherwise
/// trigger an unbounded allocation attempt before this crate ever sees the resulting bytes.
///
/// The bound used here is each chunk's OWN declared shape (from `Array::chunk_shape`, which
/// comes from the array's `.zarray`/zarr.json `chunks` metadata fixed at `Array::open`/
/// `async_open` time — NOT from the chunk file's bytes, so it is trusted, non-attacker-controlled
/// structural metadata) times the dtype's byte width, i.e. "how many bytes would this chunk
/// legitimately decompress to, per the array's own metadata". A lying/corrupt chunk that
/// decompresses to MORE than that (the actual decompression-bomb shape: tiny compressed bytes,
/// huge declared-in-the-compressed-stream size) is rejected before the attempt.
///
/// 64 MiB is set well above typical OME-Zarr chunking conventions (chunk sizes are usually chosen
/// in the low hundreds of KiB to a few MiB) so legitimate chunks are never rejected, while still
/// bounding worst-case per-chunk memory far below a true multi-GB bomb.
const DECOMPRESSED_CHUNK_BYTE_CAP: u64 = 64 * 1024 * 1024;

fn dtype_byte_width(dtype: DType) -> u64 {
    match dtype {
        DType::U8 | DType::I8 => 1,
        DType::U16 | DType::I16 => 2,
        DType::U32 | DType::I32 | DType::F32 => 4,
        DType::U64 | DType::I64 | DType::F64 => 8,
    }
}

/// Checks every chunk the given `subset` touches (within `arr`) against
/// [`DECOMPRESSED_CHUNK_BYTE_CAP`], using the array's own trusted (metadata-derived) chunk shape
/// — NOT the requested subset size, since (per this function's caller's doc comment) zarrs
/// decompresses whole chunks regardless of how small a requested subset is. Returns
/// `Err(ZarrError::Read(..))` on the first chunk that would exceed the cap; `Ok(())` if every
/// touched chunk is within bounds. Generic over `TStorage` (unconstrained, matching
/// `Array<TStorage>`'s own `impl<TStorage: ?Sized> Array<TStorage>` block for these particular
/// metadata-only methods) so the SAME check covers both the sync (`ReadableStorageTraits`) and
/// async (`AsyncReadableStorageTraits`) array types with no duplicated logic.
fn check_chunk_byte_cap<TStorage: ?Sized>(
    arr: &Array<TStorage>,
    subset: &ArraySubset,
    dtype: DType,
) -> Result<(), ZarrError> {
    // `chunks_in_array_subset` returning `Err`/`None` means the subset itself is malformed
    // relative to the chunk grid — bounds-checking already caught legitimate out-of-range
    // requests upstream (`plan_ranges`), so this is a defensive fallback: fail loud rather than
    // silently skip the cap check.
    let chunks = arr
        .chunks_in_array_subset(subset)
        .map_err(|e| ZarrError::Read(format!("cannot determine touched chunks: {e}")))?
        .ok_or_else(|| {
            ZarrError::Read("cannot determine touched chunks (no intersection)".into())
        })?;
    let byte_width = dtype_byte_width(dtype);
    for chunk_indices in chunks.indices() {
        let shape = arr
            .chunk_shape(&chunk_indices)
            .map_err(|e| ZarrError::Read(format!("cannot determine chunk shape: {e}")))?;
        let elems: u128 = shape.iter().map(|d| d.get() as u128).product();
        let bytes = elems * byte_width as u128;
        if bytes > DECOMPRESSED_CHUNK_BYTE_CAP as u128 {
            return Err(ZarrError::Read(format!(
                "chunk {chunk_indices:?} declares a decompressed size of {bytes} bytes, \
                 exceeding the {DECOMPRESSED_CHUNK_BYTE_CAP}-byte decompression-bomb guard"
            )));
        }
    }
    Ok(())
}

/// Reads one array subset and widens every native dtype to f64 (sync path).
fn retrieve_widened(
    arr: &Array<dyn ReadableStorageTraits>,
    subset: &ArraySubset,
    dtype: DType,
) -> Result<Vec<f64>, ZarrError> {
    check_chunk_byte_cap(arr, subset, dtype)?;
    macro_rules! widen {
        ($t:ty) => {
            arr.retrieve_array_subset::<ndarray::ArrayD<$t>>(subset)
                .map_err(|e| ZarrError::Read(e.to_string()))?
                .mapv(|v| v as f64)
                .into_iter()
                .collect()
        };
    }
    Ok(match dtype {
        DType::U8 => widen!(u8),
        DType::U16 => widen!(u16),
        DType::U32 => widen!(u32),
        DType::I8 => widen!(i8),
        DType::I16 => widen!(i16),
        DType::I32 => widen!(i32),
        DType::U64 => widen!(u64),
        DType::I64 => widen!(i64),
        DType::F32 => widen!(f32),
        DType::F64 => arr
            .retrieve_array_subset::<ndarray::ArrayD<f64>>(subset)
            .map_err(|e| ZarrError::Read(e.to_string()))?
            .into_iter()
            .collect(),
    })
}

/// Aggregate wall-clock deadline for ONE tile render's remote reads, enforced across however many
/// sequential chunk reads `zarrs` performs to satisfy a single `retrieve_widened_async` call.
///
/// `store.rs`'s `default_client_options()` already sets a 30s PER-REQUEST timeout, but a single
/// tile render can touch N chunks (each a separate remote request internally driven by zarrs), so
/// a remote that answers each individual chunk request just under 30s could still occupy this
/// call's calling thread for up to `30s * N`. The render semaphore (see the server crate) already
/// bounds HOW MANY such reads can be in flight at once — so this is latency-hardening on top of
/// that, not a resource-exhaustion fix — but a single render pinning a thread for minutes on a
/// pathological many-chunk-slow-remote combination is still worth failing loud on.
///
/// 60s is deliberately generous: well above the 30s per-request ceiling (so it can only fire on a
/// genuinely multi-chunk-slow read, never a single merely-slow-but-within-budget request), while
/// still bounding worst-case per-tile latency far below "hangs indefinitely".
///
/// `pub` so `server`'s shutdown drain can size itself against the SAME number: a render can touch
/// this deadline more than once (an `overlay=` request reads the image AND the label, each under
/// its own instance of this deadline — see `tiling::engine::render_overlay_rgb`), so the render
/// semaphore's shutdown drain must wait at least that long, not just this long, before a render
/// still holding a permit is a shutdown bug rather than legitimate in-progress work.
pub const PER_TILE_READ_DEADLINE: Duration = Duration::from_secs(60);

/// Races `fut` against a [`PER_TILE_READ_DEADLINE`] timer, returning `Err(ZarrError::Read(..))`
/// if the timer wins.
///
/// Uses `futures_timer::Delay` + `futures::future::select` rather than `tokio::time::timeout`
/// because this crate's async remote-read path is driven via `pollster::block_on` (see
/// `open_remote`'s doc comment for why `pollster` specifically), which provides no ambient tokio
/// reactor — `tokio::time::sleep`/`timeout` panic ("there is no reactor running") without one.
/// `futures_timer::Delay` runs its own background helper thread instead of depending on a runtime
/// reactor, so it resolves correctly under bare `pollster::block_on` with no ambient runtime at
/// all (verified: see the `Cargo.toml` dependency comment for `futures-timer`). This makes the
/// deadline genuinely runtime-agnostic — it works whether `read_region_f64` is called from a
/// plain thread, `spawn_blocking`, or a multi-threaded tokio runtime's worker.
async fn with_read_deadline<F, T>(fut: F) -> Result<T, ZarrError>
where
    F: std::future::Future<Output = Result<T, ZarrError>>,
{
    with_deadline(fut, PER_TILE_READ_DEADLINE).await
}

/// The actual race logic behind [`with_read_deadline`], parameterized over the deadline so tests
/// can exercise the "timer wins" branch without waiting out the real (60s) production deadline.
async fn with_deadline<F, T>(fut: F, deadline: Duration) -> Result<T, ZarrError>
where
    F: std::future::Future<Output = Result<T, ZarrError>>,
{
    use futures::future::{select, Either};
    let timer = futures_timer::Delay::new(deadline);
    match select(Box::pin(fut), Box::pin(timer)).await {
        Either::Left((result, _)) => result,
        Either::Right((_, _)) => Err(ZarrError::Read(format!(
            "read exceeded per-tile deadline ({deadline:?})"
        ))),
    }
}

/// Reads one channel's YX plane for `subset`, chunk by chunk, through the decoded-chunk cache.
///
/// Assembles the answer from cached chunk planes instead of asking zarrs for the whole subset in
/// one call. The point is reuse ACROSS requests: IIIF tiles (512x512) are smaller than typical
/// chunks (1024x1024), so four adjacent tiles overlap the same chunk and, uncached, each pays to
/// fetch and decode it again — the dominant cost of opening a region cold over a remote store.
///
/// The chunk GEOMETRY is zarrs': `chunks_in_array_subset` says which chunks the request touches
/// and `chunk_subset_bounded` gives each one's extent clamped to the array edge. Only the
/// rectangle intersection that copies each chunk's overlap into the output is ours. Chunks are
/// fetched concurrently, and the cache coalesces, so a wave of tiles sharing a chunk waits on one
/// fetch rather than racing to duplicate it.
///
/// Falls back to a single direct read whenever the chunk grid cannot describe the request — an
/// empty subset, or a chunk grid that does not map onto it — so an unusual layout degrades to the
/// previous behaviour rather than failing.
#[allow(clippy::too_many_arguments)]
async fn read_plane_via_chunk_cache(
    arr: &Array<dyn AsyncReadableStorageTraits>,
    cache: &ChunkPlaneCache,
    identity: &Arc<str>,
    dtype: DType,
    axes: &AxesModel,
    level: usize,
    tcz: (u64, u64, u64),
    subset: &ArraySubset,
    dims: (usize, usize),
) -> Result<Vec<f64>, ZarrError> {
    let (t, c, z) = tcz;
    let (rows, cols) = dims;
    let direct = || retrieve_widened_async_inner(arr, subset, dtype);

    if !cache.is_enabled() {
        return direct().await;
    }
    let (Some(iy), Some(ix)) = (axes.index_of(AxisKind::Y), axes.index_of(AxisKind::X)) else {
        return direct().await;
    };
    let chunk_range = match arr.chunks_in_array_subset(subset) {
        Ok(Some(r)) if !r.is_empty() => r,
        _ => return direct().await,
    };

    let want_y = subset.start()[iy]..subset.end_exc()[iy];
    let want_x = subset.start()[ix]..subset.end_exc()[ix];

    // One future per touched chunk, carrying the geometry needed to place it afterwards.
    let mut reads = Vec::new();
    for chunk in chunk_range.indices() {
        let bounds = arr
            .chunk_subset_bounded(&chunk)
            .map_err(|e| ZarrError::Read(format!("chunk {chunk:?} bounds: {e}")))?;
        let cy = bounds.start()[iy]..bounds.end_exc()[iy];
        let cx = bounds.start()[ix]..bounds.end_exc()[ix];
        let oy = cy.start.max(want_y.start)..cy.end.min(want_y.end);
        let ox = cx.start.max(want_x.start)..cx.end.min(want_x.end);
        if oy.start >= oy.end || ox.start >= ox.end {
            continue; // the chunk grid rectangle can include chunks the subset misses
        }

        // The cached plane is the chunk's WHOLE YX extent, not just the part this request wants —
        // otherwise two tiles overlapping the same chunk would cache different rectangles and
        // neither would hit.
        let plane_ranges: Vec<Range<u64>> = (0..subset.dimensionality())
            .map(|d| {
                if d == iy {
                    cy.clone()
                } else if d == ix {
                    cx.clone()
                } else {
                    subset.start()[d]..subset.end_exc()[d]
                }
            })
            .collect();
        let plane_rows = (cy.end - cy.start) as usize;
        let plane_cols = (cx.end - cx.start) as usize;
        let key = ChunkPlaneKey {
            image: identity.clone(),
            level,
            t,
            c,
            z,
            chunk: chunk.to_vec(),
        };
        reads.push(async move {
            let plane = cache
                .get_or_try_insert(key, async {
                    let elems = retrieve_widened_async_inner(
                        arr,
                        &ArraySubset::new_with_ranges(&plane_ranges),
                        dtype,
                    )
                    .await?;
                    Array2::from_shape_vec((plane_rows, plane_cols), elems).map_err(|e| {
                        ZarrError::Read(format!("chunk reshape {plane_rows}x{plane_cols}: {e}"))
                    })
                })
                .await?;
            Ok::<_, ZarrError>((cy.clone(), cx.clone(), oy, ox, plane))
        });
    }

    let pieces = futures::future::try_join_all(reads).await?;

    let mut out = Array2::<f64>::zeros((rows, cols));
    for (cy, cx, oy, ox, plane) in pieces {
        let src_y = (oy.start - cy.start) as usize..(oy.end - cy.start) as usize;
        let src_x = (ox.start - cx.start) as usize..(ox.end - cx.start) as usize;
        let dst_y = (oy.start - want_y.start) as usize..(oy.end - want_y.start) as usize;
        let dst_x = (ox.start - want_x.start) as usize..(ox.end - want_x.start) as usize;
        out.slice_mut(ndarray::s![dst_y, dst_x])
            .assign(&plane.slice(ndarray::s![src_y, src_x]));
    }
    Ok(out.into_raw_vec_and_offset().0)
}

async fn retrieve_widened_async_inner(
    arr: &Array<dyn AsyncReadableStorageTraits>,
    subset: &ArraySubset,
    dtype: DType,
) -> Result<Vec<f64>, ZarrError> {
    check_chunk_byte_cap(arr, subset, dtype)?;
    macro_rules! widen {
        ($t:ty) => {
            arr.async_retrieve_array_subset::<ndarray::ArrayD<$t>>(subset)
                .await
                .map_err(|e| ZarrError::Read(e.to_string()))?
                .mapv(|v| v as f64)
                .into_iter()
                .collect()
        };
    }
    Ok(match dtype {
        DType::U8 => widen!(u8),
        DType::U16 => widen!(u16),
        DType::U32 => widen!(u32),
        DType::I8 => widen!(i8),
        DType::I16 => widen!(i16),
        DType::I32 => widen!(i32),
        DType::U64 => widen!(u64),
        DType::I64 => widen!(i64),
        DType::F32 => widen!(f32),
        DType::F64 => arr
            .async_retrieve_array_subset::<ndarray::ArrayD<f64>>(subset)
            .await
            .map_err(|e| ZarrError::Read(e.to_string()))?
            .into_iter()
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> ZarrImage {
        ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap()
    }

    // --- Per-tile read deadline (whole-branch review #4) ---

    /// A future that resolves well within the deadline must return its `Ok` value unchanged —
    /// the deadline must never fire on a normal, fast read.
    #[tokio::test(flavor = "multi_thread")]
    async fn with_deadline_returns_ok_when_future_completes_first() {
        let fast = async { Ok::<_, ZarrError>(42) };
        let result = with_deadline(fast, Duration::from_millis(200)).await;
        assert_eq!(result.unwrap(), 42);
    }

    /// A future that never resolves within the deadline must be preempted by the timer, surfacing
    /// `Err(ZarrError::Read(..))` mentioning the deadline — the core property under test for the
    /// whole-branch review's aggregate-remote-read-deadline gap. Uses a short (50ms) deadline
    /// rather than the real 60s `PER_TILE_READ_DEADLINE` so the test itself stays fast; the race
    /// logic (`with_deadline`) is identical regardless of the duration value.
    #[tokio::test(flavor = "multi_thread")]
    async fn with_deadline_errors_when_future_never_completes() {
        let never = std::future::pending::<Result<u32, ZarrError>>();
        let result = with_deadline(never, Duration::from_millis(50)).await;
        let err = result.unwrap_err();
        assert!(
            matches!(err, ZarrError::Read(ref m) if m.contains("deadline")),
            "expected a deadline-mentioning Read error, got: {err}"
        );
    }

    /// Same "timer wins" property, but proven to also hold under bare `pollster::block_on` with
    /// NO ambient tokio runtime at all — this is the actual execution context
    /// `retrieve_widened_async`/`with_read_deadline` run in via `read_region_f64`'s async arm
    /// (see that call site's doc comment). This is the test that would fail if
    /// `futures_timer::Delay` secretly depended on a runtime reactor the way `tokio::time::sleep`
    /// does (verified experimentally during implementation: `tokio::time::sleep` panics under
    /// bare `pollster::block_on` with "there is no reactor running"; `futures_timer::Delay` does
    /// not, because it runs its own background helper thread).
    #[test]
    fn with_deadline_fires_under_bare_pollster_block_on_no_tokio_runtime() {
        let never = std::future::pending::<Result<u32, ZarrError>>();
        let result = pollster::block_on(with_deadline(never, Duration::from_millis(50)));
        assert!(
            matches!(result, Err(ZarrError::Read(ref m)) if m.contains("deadline")),
            "expected a deadline error under bare pollster::block_on, got: {result:?}"
        );
    }

    /// `PER_TILE_READ_DEADLINE` itself must stay well above `store.rs`'s 30s per-REQUEST timeout
    /// (see that constant's own doc comment for why: it must only fire on a genuinely pathological
    /// many-chunk-slow read, never a single request that's merely slow-but-within-budget) — a
    /// simple guard against the two constants drifting out of the intended relationship.
    #[test]
    fn per_tile_read_deadline_is_generously_above_per_request_timeout() {
        assert!(
            PER_TILE_READ_DEADLINE > Duration::from_secs(30),
            "PER_TILE_READ_DEADLINE ({PER_TILE_READ_DEADLINE:?}) must exceed the 30s \
             per-request timeout in store.rs's default_client_options"
        );
    }

    // --- Decompression-bomb guard (Deliverable B, point 3) ---

    #[test]
    fn dtype_byte_width_matches_every_native_size() {
        assert_eq!(dtype_byte_width(DType::U8), 1);
        assert_eq!(dtype_byte_width(DType::I8), 1);
        assert_eq!(dtype_byte_width(DType::U16), 2);
        assert_eq!(dtype_byte_width(DType::I16), 2);
        assert_eq!(dtype_byte_width(DType::U32), 4);
        assert_eq!(dtype_byte_width(DType::I32), 4);
        assert_eq!(dtype_byte_width(DType::F32), 4);
        assert_eq!(dtype_byte_width(DType::U64), 8);
        assert_eq!(dtype_byte_width(DType::I64), 8);
        assert_eq!(dtype_byte_width(DType::F64), 8);
    }

    /// `check_chunk_byte_cap` accepts a read against the committed (small, well within the cap)
    /// fixture — the guard must not reject legitimate, ordinary-sized reads. The end-to-end
    /// malformed-input rejection is covered by `tests/decode_fuzz_corpus.rs`'s
    /// `decompression_bomb_shaped_chunk_fails_loud_not_oom` (which needs a real gzip-compressed
    /// fixture zarrs actually decodes chunk-by-chunk, not available as a unit-test-local fixture
    /// here), so this unit test only proves the "does NOT falsely reject a normal read" half.
    #[test]
    fn check_chunk_byte_cap_accepts_small_legitimate_chunk() {
        let img = fixture();
        // Reading level 0 fully exercises the same subset->chunk lookup the guard runs, against
        // this crate's normal, well-under-the-cap committed fixture.
        let result = img.read_region_f64(0, 0, 0, 0, 0..64, 0..64);
        assert!(result.is_ok());
    }

    #[test]
    fn opens_and_reports_shape() {
        let img = fixture();
        assert_eq!(img.dtype(), crate::DType::U16);
        assert_eq!(img.num_levels(), 2);
        assert_eq!(img.full_yx(), (64, 64));
        assert_eq!(img.size_c(), 2);
        assert!(img.omero().is_some());
    }

    #[test]
    fn reads_channel0_horizontal_gradient() {
        let img = fixture();
        // channel 0 is a horizontal gradient: value == x
        let tile = img.read_region_f64(0, 0, 0, 0, 0..64, 0..64).unwrap();
        assert_eq!(tile.dim(), (64, 64));
        assert_eq!(tile[[10, 5]], 5.0); // row 10, col 5 -> x==5
        assert_eq!(tile[[10, 63]], 63.0);
    }

    #[test]
    fn reads_channel1_vertical_gradient_subregion() {
        let img = fixture();
        // channel 1 is a vertical gradient: value == y
        let tile = img.read_region_f64(0, 0, 1, 0, 16..32, 0..16).unwrap();
        assert_eq!(tile.dim(), (16, 16));
        assert_eq!(tile[[0, 0]], 16.0); // first row is y==16
        assert_eq!(tile[[15, 0]], 31.0);
    }

    /// The fixture's level 0 is 64x64. An out-of-bounds Y range must return a real `Err` in
    /// EVERY build profile (not rely on `debug_assert!`, which release builds compile out and
    /// would otherwise hand an out-of-bounds subset straight to zarrs).
    #[test]
    fn out_of_bounds_y_range_returns_err() {
        let img = fixture();
        let err = img.read_region_f64(0, 0, 0, 0, 0..1000, 0..64).unwrap_err();
        assert!(matches!(err, ZarrError::Read(_)));
    }

    #[test]
    fn out_of_bounds_x_range_returns_err() {
        let img = fixture();
        let err = img.read_region_f64(0, 0, 0, 0, 0..64, 0..1000).unwrap_err();
        assert!(matches!(err, ZarrError::Read(_)));
    }

    #[test]
    fn level_yx_matches_full_yx_at_level_0() {
        let img = fixture();
        assert_eq!(img.level_yx(0), img.full_yx());
    }

    #[test]
    fn level_yx_reflects_coarser_level() {
        let img = fixture();
        // Fixture level 1 is downsampled 2x in y/x (see .zattrs scale [1,1,1,2,2]).
        assert_eq!(img.level_yx(1), (32, 32));
    }

    /// `level_yx` must be model-driven (use the `AxesModel`), not positional
    /// (`shape[len-2..]`). Build a synthetic `ZarrImage`-shaped scenario via a non-`…,Y,X`
    /// axes order and confirm the right dims come out. Since `ZarrImage` fields are private
    /// and constructed only via `open()`, we test the model-driven lookup directly through
    /// `axis_len`'s underlying mechanism using a hand-built `AxesModel` + shape, which is what
    /// `level_yx` delegates to. This is the smallest true unit that would catch a regression to
    /// positional indexing (see `tiling::engine` tests for the end-to-end regression lock).
    #[test]
    fn axes_model_index_of_is_order_independent() {
        // axes = [c, x, y] — Y and X are NOT the last two axes in Y,X order (X before Y, and
        // neither is last-two-as-Y-then-X). A positional `shape[len-2]`/`shape[len-1]` read
        // would get Y=shape[1](x's slot) and X=shape[2](y's slot) — wrong. The model must be
        // consulted instead.
        let axes = AxesModel {
            order: vec![AxisKind::C, AxisKind::X, AxisKind::Y],
        };
        let shape = [3u64, 100u64, 50u64]; // c=3, x=100, y=50
        let yi = axes.index_of(AxisKind::Y).unwrap();
        let xi = axes.index_of(AxisKind::X).unwrap();
        assert_eq!((shape[yi], shape[xi]), (50, 100));
    }

    // --- Full dtype support (Deliverable A) ---

    /// `DType::U8` end-to-end: opens the committed u8-only fixture (single channel, 16x16,
    /// horizontal gradient value == x) and confirms the widened f64 values are exact.
    #[test]
    fn dtype_u8_reads_widen_correctly() {
        let img = ZarrImage::open("../../tests/fixtures/sample_u8.ome.zarr").unwrap();
        assert_eq!(img.dtype(), crate::DType::U8);
        let tile = img.read_region_f64(0, 0, 0, 0, 0..16, 0..16).unwrap();
        assert_eq!(tile.dim(), (16, 16));
        assert_eq!(tile[[3, 7]], 7.0);
        assert_eq!(tile[[0, 15]], 15.0);
    }

    /// `DType::F32` end-to-end: opens the committed f32 fixture (single channel, 16x16,
    /// horizontal gradient spanning 0.0..=1000.0 — NOT in the 0..255 byte range) and confirms
    /// the widened f64 values are exact (f32 -> f64 widening is lossless for these values).
    #[test]
    fn dtype_f32_reads_widen_correctly_and_exceed_byte_range() {
        let img = ZarrImage::open("../../tests/fixtures/sample_f32.ome.zarr").unwrap();
        assert_eq!(img.dtype(), crate::DType::F32);
        let tile = img.read_region_f64(0, 0, 0, 0, 0..16, 0..16).unwrap();
        assert_eq!(tile.dim(), (16, 16));
        // x=15 (last column) -> 15 * (1000/15) == 1000.0, well above 255.
        let last_col_val = tile[[0, 15]];
        assert!(
            (last_col_val - 1000.0).abs() < 1e-3,
            "expected ~1000.0, got {last_col_val}"
        );
        assert_eq!(tile[[0, 0]], 0.0);
    }

    /// `>u1` (big-endian-*labeled* u8) end-to-end. Endianness is meaningless for a 1-byte type —
    /// `>u1`, `<u1`, and `|u1` are byte-identical — but zarrs 0.23.13 only registers the `|u1` V2
    /// alias for `UInt8DataType` (confirmed against the zarrs source: `uint.rs`'s
    /// `impl_extension_aliases!(UInt8DataType, v3: "uint8", [], v2: "|u1", ["|u1"])` — no `>u1`/
    /// `<u1` aliases), so `Array::open` on a real `>u1`-declared `.zarray` fails with the error
    /// text "data type >u1 is not supported", *before* `classify_dtype` ever runs. This is the
    /// actual root cause of the reported bug (verified by direct reproduction):
    /// `register_one_byte_endian_aliases` registers the prefixed spellings as V2 aliases so zarrs'
    /// own name resolution accepts them.
    #[test]
    fn dtype_be_u1_opens_and_reads_correctly() {
        let img = ZarrImage::open("../../tests/fixtures/sample_be_u1.ome.zarr").unwrap();
        assert_eq!(img.dtype(), crate::DType::U8);
        let tile = img.read_region_f64(0, 0, 0, 0, 0..16, 0..16).unwrap();
        assert_eq!(tile.dim(), (16, 16));
        assert_eq!(tile[[3, 7]], 7.0);
        assert_eq!(tile[[0, 15]], 15.0);
    }

    /// `>u2` (genuinely big-endian u16) end-to-end, with chunk bytes hand-written in REAL
    /// big-endian byte order (`to_be_bytes`, see `build_be_fixture.rs`). This is the critical
    /// round-trip proof: unlike `>u1`, zarrs 0.23.13 DOES register both `<u2`/`>u2` as V2 aliases
    /// for the same `UInt16DataType` (confirmed against the zarrs source), and its V2->V3
    /// metadata conversion threads the parsed endianness into the `bytes` codec's `endian` config,
    /// which byte-swaps every 2-byte component during decode when the source endianness differs
    /// from the host's — confirmed by reading `zarrs_data_type`'s `_impl_bytes_data_type_traits!`
    /// macro. So `>u2` already opens AND decodes correctly with NO ziv-side dtype-matching change
    /// needed; this test proves it empirically rather than trusting the source reading alone — if
    /// the codec did NOT byte-swap, `tile[[0, 15]]` would read as `15 << 8 == 3840`, not `15.0`.
    #[test]
    fn dtype_be_u2_round_trip_decodes_correct_values_not_byte_swapped_garbage() {
        let img = ZarrImage::open("../../tests/fixtures/sample_be_u2.ome.zarr").unwrap();
        assert_eq!(img.dtype(), crate::DType::U16);
        let tile = img.read_region_f64(0, 0, 0, 0, 0..16, 0..16).unwrap();
        assert_eq!(tile.dim(), (16, 16));
        // If the big-endian bytes were misread as little-endian, x=15 would decode as 15<<8=3840
        // (or any garbage value), not 15 — this assertion is the actual endianness proof.
        assert_eq!(tile[[3, 7]], 7.0);
        assert_eq!(tile[[0, 15]], 15.0);
    }

    /// `>i4` (genuinely big-endian i32) end-to-end — a second multi-byte type (differently signed
    /// and differently sized than the `>u2` case above) proving the "zarrs already decodes
    /// multi-byte big-endian correctly" finding is not an artifact specific to u16. Also
    /// empirically spot-checked (not as committed fixtures/tests, to avoid excessive fixture
    /// bloat) for `>i2`/`>u4`/`>f4`/`>f8` during investigation — all behaved identically: opened
    /// and decoded correctly with no ziv-side change.
    #[test]
    fn dtype_be_i4_round_trip_decodes_correct_values() {
        let img = ZarrImage::open("../../tests/fixtures/sample_be_i4.ome.zarr").unwrap();
        assert_eq!(img.dtype(), crate::DType::I32);
        let tile = img.read_region_f64(0, 0, 0, 0, 0..16, 0..16).unwrap();
        assert_eq!(tile.dim(), (16, 16));
        assert_eq!(tile[[3, 7]], 7.0);
        assert_eq!(tile[[0, 15]], 15.0);
    }

    /// Opens the OME-Zarr 0.5-LAYOUT fixture (V2 on-disk array storage + 0.5-shaped
    /// `attributes.ome.multiscales` `.zattrs`) end-to-end through `ZarrImage::open`, proving the
    /// 0.5 metadata-parsing path is exercised via a real store read, not just the synthetic
    /// in-memory JSON unit test in `metadata.rs`.
    #[test]
    fn opens_v05_layout_fixture_end_to_end() {
        let img = ZarrImage::open("../../tests/fixtures/sample_v05.ome.zarr").unwrap();
        assert_eq!(img.dtype(), crate::DType::U16);
        assert_eq!(img.full_yx(), (16, 16));
        assert_eq!(img.size_c(), 2);
        assert!(img.omero().is_some());
        // channel 0 -> x, channel 1 -> y (see build_v05_fixture.rs).
        let ch0 = img.read_region_f64(0, 0, 0, 0, 0..16, 0..16).unwrap();
        assert_eq!(ch0[[3, 7]], 7.0);
        let ch1 = img.read_region_f64(0, 0, 1, 0, 0..16, 0..16).unwrap();
        assert_eq!(ch1[[7, 3]], 7.0);
    }

    // --- open_with_options / SSRF escape hatch plumbing (Deliverable A) ---

    /// `open_with_options` behaves exactly like `open` for a local filesystem spec regardless
    /// of `allow_internal_hosts` — the escape hatch only affects `http(s)://` host checking.
    #[test]
    fn open_with_options_matches_open_for_local_spec() {
        let img = ZarrImage::open_with_options("../../tests/fixtures/sample_v04.ome.zarr", false)
            .unwrap();
        assert_eq!(img.full_yx(), (64, 64));
    }

    /// A blocked-host `http://` spec is rejected through `open_with_options` with
    /// `allow_internal_hosts=false`, proving the SSRF guard is wired all the way through
    /// `ZarrImage::open`, not just the lower-level `parse_store_spec`.
    #[test]
    fn open_with_options_rejects_blocked_host_by_default() {
        let err =
            match ZarrImage::open_with_options("http://169.254.169.254/sample.ome.zarr", false) {
                Err(e) => e,
                Ok(_) => panic!("expected an error"),
            };
        assert!(matches!(err, ZarrError::BlockedHost { .. }), "{err}");
    }

    /// With `allow_internal_hosts=true`, a loopback `http://` spec must NOT be rejected with
    /// `BlockedHost` — proving `open_with_options` actually forwards its `allow_internal_hosts`
    /// parameter down to `parse_store_spec_with_options` (not just that the lower-level function
    /// works in isolation, already covered in `store.rs`'s own tests). Uses `127.0.0.1:1` (port 1
    /// is essentially never listening) rather than a real link-local/WAN address so the
    /// resulting connection attempt fails FAST with a connection-refused error instead of a slow
    /// WAN-timeout, keeping this a fast unit test while still exercising the real `open_remote`
    /// code path past the SSRF check.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_with_options_escape_hatch_forwards_flag() {
        let err = tokio::task::spawn_blocking(|| {
            match ZarrImage::open_with_options("http://127.0.0.1:1/sample.ome.zarr", true) {
                Err(e) => e,
                Ok(_) => panic!("expected an error (nothing listens on port 1)"),
            }
        })
        .await
        .expect("spawn_blocking task must not panic");
        assert!(
            !matches!(err, ZarrError::BlockedHost { .. }),
            "expected the escape hatch to bypass the SSRF guard (a connection-refused error is \
             fine), got: {err}"
        );
    }
}
