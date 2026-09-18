//! Mapping an image name to a store, and holding the opened result.
//!
//! Every way ziv can learn about an image (names on the command line, names in a config file, a
//! directory root resolved lazily, a store the caller names in the request) is the same question
//! asked at a different moment: *given this name, which store, and is it open yet?* So there is
//! one registry with pluggable sources rather than four features.
//!
//! This module holds the trait, the one source Plan A needs, and the registry itself. Later phases
//! add a directory-root source and a caller-named-remote source behind the same trait and change
//! nothing else.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use moka::future::Cache;
use tiling::{TileEngine, ZarrTileEngine};
use tokio::sync::Semaphore;
use zarr_core::{ChunkPlaneCache, ZarrImage};

use crate::name::{ImageName, NameError};

/// What a source can say about its own contents.
///
/// `NotListable` is not "empty". A lazy directory source genuinely cannot enumerate without walking
/// a filesystem it was chosen precisely to avoid walking, and reporting `[]` would make the
/// viewer's picker say "no images" when it means "type a name". The distinction is carried all the
/// way to `/ziv/images.json` so the UI can be honest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    Enumerable(Vec<ImageName>),
    NotListable,
}

/// One way of turning a name into a store spec.
pub trait CatalogSource: Send + Sync {
    /// The name prefix this source owns, or `""` for a source that contributes unprefixed names.
    /// Prefixes are what stop two directory roots from colliding, and what keep the
    /// caller-named-remote source from being a special case.
    fn prefix(&self) -> &str;

    /// The store spec for `name`, or `None` if this source does not have it.
    ///
    /// Returned unparsed because parsing needs the SSRF policy, which belongs to the registry
    /// rather than to a source.
    fn resolve(&self, name: &ImageName) -> Option<String>;

    fn list(&self) -> Listing;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    #[error("two images are named {0:?}")]
    DuplicateName(ImageName),
}

/// A fixed set of names the operator supplied: argv today, a config file in a later phase.
///
/// `BTreeMap` rather than `HashMap` so `list()` is sorted, which makes `/ziv/images.json` stable
/// across restarts. A catalogue whose order changes on every boot is a small thing that makes every
/// diff of it useless.
#[derive(Debug)]
pub struct ExplicitSource {
    entries: BTreeMap<ImageName, String>,
}

impl ExplicitSource {
    pub fn from_pairs(pairs: Vec<(ImageName, String)>) -> Result<Self, CatalogError> {
        let mut entries = BTreeMap::new();
        for (name, spec) in pairs {
            if entries.insert(name.clone(), spec).is_some() {
                return Err(CatalogError::DuplicateName(name));
            }
        }
        Ok(ExplicitSource { entries })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl CatalogSource for ExplicitSource {
    fn prefix(&self) -> &str {
        ""
    }

    fn resolve(&self, name: &ImageName) -> Option<String> {
        self.entries.get(name).cloned()
    }

    fn list(&self) -> Listing {
        Listing::Enumerable(self.entries.keys().cloned().collect())
    }
}

/// Derives an image name from a store spec given on the command line: the final path component with
/// a trailing `.ome.zarr` or `.zarr` removed.
///
/// Deliberately crude, and documented as such in `docs/multi-image.md`: a name derived from a path
/// is unstable by construction, because moving the file changes the URL. Only an explicit catalogue
/// produces a name safe to cite.
pub fn name_from_path(spec: &str) -> Result<ImageName, NameError> {
    let trimmed = spec.trim_end_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let stem = last
        .strip_suffix(".ome.zarr")
        .or_else(|| last.strip_suffix(".zarr"))
        .unwrap_or(last);
    ImageName::parse(stem)
}

/// Default cap on simultaneously open images.
///
/// This bounds HANDLES, not bytes: an open engine holds zarrs array handles, which hold file
/// descriptors locally and connection pools remotely. Memory is bounded separately and
/// process-wide by the shared chunk cache. 64 is chosen to sit well under a default 1024 FD limit
/// with room for connections and listeners.
pub const DEFAULT_MAX_OPEN_IMAGES: u64 = 64;

/// Default cap on simultaneous OPENS.
///
/// Deliberately not the render semaphore. `ZarrTileEngine::new` performs a percentile auto-stretch
/// read of the coarsest level for any image without `omero`, which was harmless when it ran once
/// before binding and is not once it runs inside a request: a burst of first-hits across fifty
/// images would mean fifty concurrent coarsest-level reads. `MAX_READ_PIXELS` caps each one's size,
/// nothing caps their number. Keeping this separate from renders means a wave of cold opens cannot
/// starve images that are already warm.
pub const DEFAULT_MAX_CONCURRENT_OPENS: usize = 4;

#[derive(Debug, Clone, thiserror::Error)]
pub enum RegistryError {
    #[error("no image named {0}")]
    NotFound(ImageName),
    #[error("image {0} could not be opened: {1}")]
    Open(ImageName, String),
}

/// Resolves names to images and holds the open ones.
pub struct ImageRegistry {
    sources: Vec<Box<dyn CatalogSource>>,
    /// An engine supplied already open at construction, checked before any source and never
    /// evicted.
    ///
    /// Exists for `AppState::new`, the single-image compatibility constructor: twenty-five call
    /// sites across ten files hand the server a `ZarrTileEngine` they built themselves, and there
    /// is no store spec to reopen it from if it were ever dropped.
    preopened: Option<(ImageName, Arc<dyn TileEngine + Send + Sync>)>,
    open: Cache<ImageName, Arc<dyn TileEngine + Send + Sync>>,
    open_semaphore: Arc<Semaphore>,
    /// How many permits `open_semaphore` was built with. Needed at shutdown for the same reason
    /// `AppState::render_permits` is: acquiring ALL of them via `crate::drain_opens` is how the
    /// server establishes that no open is still running, and `Semaphore` does not report its own
    /// original capacity.
    open_permits: usize,
    chunk_cache: ChunkPlaneCache,
    allow_internal_hosts: bool,
}

pub struct ImageRegistryBuilder {
    sources: Vec<Box<dyn CatalogSource>>,
    max_open_images: u64,
    max_concurrent_opens: usize,
    chunk_cache: Option<ChunkPlaneCache>,
    allow_internal_hosts: bool,
}

impl ImageRegistry {
    #[must_use]
    pub fn builder() -> ImageRegistryBuilder {
        ImageRegistryBuilder {
            sources: Vec::new(),
            max_open_images: DEFAULT_MAX_OPEN_IMAGES,
            max_concurrent_opens: DEFAULT_MAX_CONCURRENT_OPENS,
            chunk_cache: None,
            allow_internal_hosts: false,
        }
    }

    /// Builds a registry around one already-open engine. See the `preopened` field.
    #[must_use]
    pub fn from_single_engine(
        name: ImageName,
        engine: Arc<dyn TileEngine + Send + Sync>,
    ) -> ImageRegistry {
        let mut registry = ImageRegistry::builder().build();
        registry.preopened = Some((name, engine));
        registry
    }

    /// Resolves `name` and returns its engine, opening it if necessary.
    ///
    /// Concurrent first-requests for one name are coalesced by `try_get_with` into a single open.
    /// That is not a nicety: opening a remote image costs metadata round trips, label discovery and
    /// possibly a percentile read, so ten viewers hitting one cold image must produce one open, not
    /// ten.
    ///
    /// An `Err` from the closure is propagated to every waiter and never stored, so a store that
    /// was briefly unreachable stays retryable.
    ///
    /// **Permit lifetime.** `open_semaphore` is acquired INLINE in this future, then the
    /// ALREADY-HELD permit is moved into its own detached `tokio::spawn`ed task before the
    /// (uncancellable) `spawn_blocking` open begins — the exact shape `server::routes::serve_tile`
    /// uses for the render semaphore, and for the same reason. `moka::future::ValueInitializer`
    /// polls this whole future INLINE in whichever caller's task first misses the cache (see
    /// `moka`'s `value_initializer.rs`: there is no internal spawn insulating the init future from
    /// that caller going away); if that caller is cancelled — `tower_http::timeout::TimeoutLayer`'s
    /// 30s firing, or the caller simply disconnecting — moka drops this future to record
    /// `WaiterValue::EnclosingFutureAborted` for any other coalesced waiters. Dropping this future
    /// with the permit held INLINE (the old shape) drops the permit guard with it, even though the
    /// `spawn_blocking` closure it guarded cannot itself be cancelled and keeps running, undetected,
    /// on its own OS thread — driving `object_store` retries that arm `tokio::time::sleep` timers
    /// long after this future, and the permit that was supposed to represent it, are gone. That is
    /// the same shutdown-panic shape a one-hour remote soak found in the render
    /// path (see `docs/operations.md`), reproduced here for the open path by
    /// `crates/server/tests/open_drain.rs`. Moving the held permit into a `tokio::spawn`ed task
    /// (detached, so it runs to completion once started regardless of whether anything is still
    /// awaiting it) ties the permit's release to the open's REAL completion instead.
    pub async fn get(
        &self,
        name: &ImageName,
    ) -> Result<Arc<dyn TileEngine + Send + Sync>, RegistryError> {
        if let Some((preopened, engine)) = &self.preopened {
            if preopened == name {
                return Ok(engine.clone());
            }
        }

        let spec = self
            .sources
            .iter()
            .find_map(|s| s.resolve(name))
            .ok_or_else(|| RegistryError::NotFound(name.clone()))?;

        let semaphore = self.open_semaphore.clone();
        let chunk_cache = self.chunk_cache.clone();
        let allow_internal_hosts = self.allow_internal_hosts;
        let name_for_err = name.clone();
        let identity = name.as_str().to_string();

        self.open
            .try_get_with(name.clone(), async move {
                // See this method's doc comment: acquired INLINE (so a caller abandoned while
                // merely queued for a permit is still cancelled exactly as before — no open
                // starts, nothing is left queued), then moved ALREADY-HELD into the detached task
                // below before any further `.await`, so nothing can release it out from under the
                // still-running open.
                let permit = semaphore.acquire_owned().await.map_err(|_| {
                    RegistryError::Open(
                        name_for_err.clone(),
                        "open semaphore closed unexpectedly".to_string(),
                    )
                })?;
                let name_for_task = name_for_err.clone();
                let open_task = tokio::spawn(async move {
                    let _permit = permit;
                    let started = Instant::now();
                    // `ZarrImage::open_*` drives async object-store reads through `pollster::block_on`
                    // and `ZarrTileEngine::new` may do a percentile read, so both belong on a blocking
                    // thread rather than a runtime worker. Same reasoning as the tile render path.
                    let opened = tokio::task::spawn_blocking(move || {
                        ZarrImage::open_shared(&spec, allow_internal_hosts, &identity, chunk_cache)
                            .map(|image| {
                                // Captured before `ZarrTileEngine::new` takes ownership of `image`:
                                // a label that failed to open (see
                                // `ZarrImage::label_open_failures`) used to vanish here with no
                                // trace at all. The exporter's `--labels` warning covers the static
                                // path; this is the server's equivalent for `ziv serve`, logged once
                                // per open rather than per request.
                                let label_open_failures = image.label_open_failures().to_vec();
                                (ZarrTileEngine::new(image), label_open_failures)
                            })
                    })
                    .await;
                    let engine = match opened {
                        Ok(Ok((engine, label_open_failures))) => {
                            for failure in &label_open_failures {
                                tracing::warn!(
                                    image = %name_for_task,
                                    label = %failure.name,
                                    reason = %failure.reason,
                                    "label declared in labels/.zattrs could not be opened; serving without it"
                                );
                            }
                            engine
                        }
                        Ok(Err(e)) => {
                            metrics::counter!("ziv_image_opens_total", "outcome" => "error")
                                .increment(1);
                            return Err(RegistryError::Open(name_for_task, e.to_string()));
                        }
                        Err(join) => {
                            metrics::counter!("ziv_image_opens_total", "outcome" => "error")
                                .increment(1);
                            return Err(RegistryError::Open(name_for_task, join.to_string()));
                        }
                    };
                    metrics::counter!("ziv_image_opens_total", "outcome" => "ok").increment(1);
                    metrics::histogram!("ziv_image_open_seconds")
                        .record(started.elapsed().as_secs_f64());
                    Ok(Arc::new(engine) as Arc<dyn TileEngine + Send + Sync>)
                });
                match open_task.await {
                    Ok(result) => result,
                    Err(join_err) => Err(RegistryError::Open(name_for_err, join_err.to_string())),
                }
            })
            .await
            .map_err(|shared| (*shared).clone())
    }

    /// The open semaphore, for shutdown draining (`crate::drain_opens`) — mirrors
    /// `AppState::render_semaphore`.
    #[must_use]
    pub fn open_semaphore(&self) -> Arc<Semaphore> {
        self.open_semaphore.clone()
    }

    /// How many permits `open_semaphore` was built with — mirrors `AppState::render_permits`.
    #[must_use]
    pub fn open_permits(&self) -> usize {
        self.open_permits
    }

    /// The union of every source's listing, plus any pre-opened engine.
    ///
    /// `NotListable` if ANY source cannot enumerate, since a partial list presented as complete is
    /// worse than an honest refusal.
    #[must_use]
    pub fn list(&self) -> Listing {
        let mut all: Vec<ImageName> = self.preopened.iter().map(|(n, _)| n.clone()).collect();
        for source in &self.sources {
            match source.list() {
                Listing::Enumerable(names) => all.extend(names),
                Listing::NotListable => return Listing::NotListable,
            }
        }
        all.sort();
        all.dedup();
        Listing::Enumerable(all)
    }

    /// The one image in the catalogue, when there is exactly one.
    ///
    /// Drives the single-image root alias, which is what keeps every URL a single-image deployment
    /// publishes today working unchanged.
    #[must_use]
    pub fn sole_image(&self) -> Option<ImageName> {
        match self.list() {
            Listing::Enumerable(names) if names.len() == 1 => names.into_iter().next(),
            _ => None,
        }
    }

    /// Images currently open, excluding any pre-opened engine.
    ///
    /// `async` rather than blocking on moka's maintenance, so `server` needs no `pollster`
    /// dependency. Every caller is already in an async context.
    pub async fn open_count(&self) -> u64 {
        self.open.run_pending_tasks().await;
        self.open.entry_count()
    }
}

impl ImageRegistryBuilder {
    #[must_use]
    pub fn source(mut self, source: Box<dyn CatalogSource>) -> Self {
        self.sources.push(source);
        self
    }

    #[must_use]
    pub fn max_open_images(mut self, n: u64) -> Self {
        self.max_open_images = n;
        self
    }

    #[must_use]
    pub fn max_concurrent_opens(mut self, n: usize) -> Self {
        self.max_concurrent_opens = n;
        self
    }

    #[must_use]
    pub fn chunk_cache(mut self, cache: ChunkPlaneCache) -> Self {
        self.chunk_cache = Some(cache);
        self
    }

    #[must_use]
    pub fn allow_internal_hosts(mut self, allow: bool) -> Self {
        self.allow_internal_hosts = allow;
        self
    }

    #[must_use]
    pub fn build(self) -> ImageRegistry {
        let open = Cache::builder()
            .max_capacity(self.max_open_images)
            .eviction_listener(|_k, _v, _cause| {
                metrics::counter!("ziv_image_evictions_total").increment(1);
            })
            .build();
        ImageRegistry {
            sources: self.sources,
            preopened: None,
            open,
            open_semaphore: Arc::new(Semaphore::new(self.max_concurrent_opens)),
            open_permits: self.max_concurrent_opens,
            chunk_cache: self.chunk_cache.unwrap_or_else(ChunkPlaneCache::from_env),
            allow_internal_hosts: self.allow_internal_hosts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> ImageName {
        ImageName::parse(s).unwrap()
    }

    fn source() -> ExplicitSource {
        ExplicitSource::from_pairs(vec![
            (name("demo"), "./a.ome.zarr".to_string()),
            (
                name("idr0062A/6001240"),
                "s3://bucket/b.ome.zarr".to_string(),
            ),
        ])
        .unwrap()
    }

    #[test]
    fn resolves_a_known_name() {
        assert_eq!(
            source().resolve(&name("demo")).as_deref(),
            Some("./a.ome.zarr")
        );
        assert_eq!(
            source().resolve(&name("idr0062A/6001240")).as_deref(),
            Some("s3://bucket/b.ome.zarr")
        );
    }

    #[test]
    fn does_not_resolve_an_unknown_name() {
        assert!(source().resolve(&name("nope")).is_none());
    }

    /// An explicit catalogue knows exactly what it holds, and the listing is sorted so
    /// `/ziv/images.json` is stable across restarts rather than reflecting hash order.
    #[test]
    fn lists_everything_it_holds_in_order() {
        match source().list() {
            Listing::Enumerable(names) => {
                assert_eq!(
                    names.iter().map(ImageName::as_str).collect::<Vec<_>>(),
                    vec!["demo", "idr0062A/6001240"]
                );
            }
            Listing::NotListable => panic!("an explicit catalogue is enumerable"),
        }
    }

    /// Two images under one name is an operator error that must be caught at boot, when it can
    /// still be fixed, rather than serving whichever one won the insert.
    #[test]
    fn a_duplicate_name_is_an_error() {
        let err = ExplicitSource::from_pairs(vec![
            (name("demo"), "./a.zarr".to_string()),
            (name("demo"), "./b.zarr".to_string()),
        ])
        .unwrap_err();
        assert_eq!(err, CatalogError::DuplicateName(name("demo")));
    }

    /// The argv derivation: final path component with a trailing `.ome.zarr` or `.zarr` removed.
    #[test]
    fn derives_a_name_from_a_path() {
        for (path, want) in [
            ("data/6001240.ome.zarr", "6001240"),
            ("./demo.zarr", "demo"),
            ("/abs/path/nuclei.ome.zarr/", "nuclei"),
            ("plain", "plain"),
        ] {
            assert_eq!(name_from_path(path).unwrap().as_str(), want, "{path}");
        }
    }

    /// A remote URL on argv still has to yield a usable name.
    #[test]
    fn derives_a_name_from_a_remote_url() {
        assert_eq!(
            name_from_path("https://host/idr/zarr/v0.4/6001240.zarr")
                .unwrap()
                .as_str(),
            "6001240"
        );
    }

    fn registry() -> ImageRegistry {
        ImageRegistry::builder()
            .source(Box::new(
                ExplicitSource::from_pairs(vec![
                    (
                        name("a"),
                        "../../tests/fixtures/sample_v04.ome.zarr".to_string(),
                    ),
                    (
                        name("b"),
                        "../../tests/fixtures/sample_labels.ome.zarr".to_string(),
                    ),
                    (
                        name("broken"),
                        "../../tests/fixtures/does-not-exist.ome.zarr".to_string(),
                    ),
                ])
                .unwrap(),
            ))
            .build()
    }

    #[tokio::test]
    async fn opens_a_known_image() {
        let r = registry();
        let engine = r.get(&name("a")).await.unwrap();
        assert_eq!(engine.dimensions().size_c, 2);
    }

    /// Two names must give two different images, which is the whole point.
    #[tokio::test]
    async fn different_names_give_different_images() {
        let r = registry();
        let a = r.get(&name("a")).await.unwrap();
        let b = r.get(&name("b")).await.unwrap();
        assert_eq!(a.image_info("x").width, 64);
        assert_eq!(b.image_info("x").width, 64);
        assert!(a.dimensions().labels.is_empty());
        assert_eq!(
            b.dimensions().labels.len(),
            1,
            "sample_labels has one label"
        );
    }

    /// A second request must reuse the open engine rather than paying the open again.
    #[tokio::test]
    async fn reuses_an_open_image() {
        let r = registry();
        let a1 = r.get(&name("a")).await.unwrap();
        let a2 = r.get(&name("a")).await.unwrap();
        assert!(std::sync::Arc::ptr_eq(&a1, &a2));
        assert_eq!(r.open_count().await, 1);
    }

    #[tokio::test]
    async fn an_unknown_name_is_not_found() {
        let err = registry()
            .get(&name("nope"))
            .await
            .err()
            .expect("an unknown name must not resolve");
        assert!(matches!(err, RegistryError::NotFound(_)), "got {err:?}");
    }

    /// Resolving but failing to open is a different problem from not existing, and an operator
    /// reading logs needs to be able to tell them apart.
    #[tokio::test]
    async fn a_name_that_will_not_open_is_an_open_error() {
        let err = registry()
            .get(&name("broken"))
            .await
            .err()
            .expect("a missing store must not open");
        assert!(matches!(err, RegistryError::Open(..)), "got {err:?}");
    }

    /// A failed open must not be cached: a store that was briefly unreachable has to be retryable
    /// rather than poisoned for the life of the process.
    #[tokio::test]
    async fn a_failed_open_is_not_cached() {
        let r = registry();
        assert!(r.get(&name("broken")).await.is_err());
        assert!(r.get(&name("broken")).await.is_err());
        assert_eq!(r.open_count().await, 0);
    }

    /// The single-image root alias needs to know whether the catalogue holds exactly one image,
    /// statically, without opening anything.
    #[tokio::test]
    async fn reports_a_sole_image_only_when_there_is_exactly_one() {
        assert!(registry().sole_image().is_none());
        let one = ImageRegistry::builder()
            .source(Box::new(
                ExplicitSource::from_pairs(vec![(
                    name("only"),
                    "../../tests/fixtures/sample_v04.ome.zarr".to_string(),
                )])
                .unwrap(),
            ))
            .build();
        assert_eq!(one.sole_image(), Some(name("only")));
    }

    /// Sources are tried in declared order, so an earlier one wins a contested name.
    #[tokio::test]
    async fn earlier_sources_win() {
        let r = ImageRegistry::builder()
            .source(Box::new(
                ExplicitSource::from_pairs(vec![(
                    name("x"),
                    "../../tests/fixtures/sample_labels.ome.zarr".to_string(),
                )])
                .unwrap(),
            ))
            .source(Box::new(
                ExplicitSource::from_pairs(vec![(
                    name("x"),
                    "../../tests/fixtures/sample_v04.ome.zarr".to_string(),
                )])
                .unwrap(),
            ))
            .build();
        let x = r.get(&name("x")).await.unwrap();
        assert_eq!(
            x.dimensions().labels.len(),
            1,
            "the first source should win"
        );
    }

    /// A pre-opened engine is reachable and listed, and never needs a store spec.
    #[tokio::test]
    async fn a_pre_opened_engine_is_reachable() {
        let img = zarr_core::ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
        let engine: Arc<dyn TileEngine + Send + Sync> = Arc::new(ZarrTileEngine::new(img));
        let r = ImageRegistry::from_single_engine(name("only"), engine);
        assert!(r.get(&name("only")).await.is_ok());
        assert_eq!(r.sole_image(), Some(name("only")));
        assert!(r.get(&name("other")).await.is_err());
    }
}
