//! Writes the enumerated request space to disk as a self-contained IIIF Level-0 tree.

use std::fs;
use std::path::{Path, PathBuf};

use iiif::ProjectionId;
use rayon::prelude::*;
use thiserror::Error;
use tiling::TileEngine;

use crate::enumerate::{enumerate_request_space, EnumeratedRequest};
use crate::viewer;

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("tile render failed for {relative_path}: {source}")]
    Tile {
        relative_path: String,
        #[source]
        source: tiling::TileError,
    },
    #[error(
        "invalid DZI name {name:?}: must be a plain filename component (no `/`, `\\`, or `..`)"
    )]
    InvalidDziName { name: String },
    #[error("{0}")]
    InvalidOption(String),
    /// A pyramid whose level sizes OpenSeadragon cannot pin (`iiif::level0_sizes` returned
    /// `None`): exporting it would produce a tree whose own viewer requests tiles the export does
    /// not contain. Returned before anything is written, so a refused export leaves `out_dir`
    /// untouched; the image can still be served dynamically via `ziv serve`, which is level 2 and
    /// reads `sizes` on every request rather than trusting a static pyramid.
    #[error(
        "cannot export this image as a static IIIF level 0 tree: its pyramid's scale factors \
         {scale_factors:?} ({levels} levels) don't match the tile layout OpenSeadragon expects, \
         so the exported viewer would request tiles the export does not contain. View it with \
         `ziv serve` instead, which is unaffected."
    )]
    UnsupportedPyramid {
        /// The pyramid's declared scale factors, so the message names the real cause.
        scale_factors: Vec<u64>,
        /// How many levels the pyramid has.
        levels: usize,
    },
}

/// Options controlling a static export.
pub struct ExportOptions {
    /// The IIIF `id` of the root tree, and with more than one view the base every other tree's id
    /// is joined to (see [`crate::views::tree_id`]). Defaults to `"."`, a relative reference to
    /// the export root: see the crate-level docs for why that is the simplest layout that still
    /// resolves correctly with no server-side configuration.
    pub id: String,
    /// JPEG quality (1-100) used for every rendered tile.
    pub quality: u8,
    /// Also export every z-plane (`--planes`).
    pub planes: bool,
    /// Also export an overlay of every label image on each exported plane (`--labels`).
    pub labels: bool,
    /// Overlay opacity, 0 to 1. Checked before anything renders, whether or not `labels` is set.
    pub overlay_opacity: f64,
    /// The image's name, used as the manifest's label.
    pub name: String,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            id: ".".to_string(),
            quality: 85,
            planes: false,
            labels: false,
            overlay_opacity: crate::views::DEFAULT_OVERLAY_OPACITY,
            name: "image".to_string(),
        }
    }
}

/// What the exporter reports while it works, so a caller can show progress without the library
/// printing anything itself.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportEvent {
    /// The plan is settled: how many trees, and how many tiles each holds.
    Planned { views: usize, tiles_per_view: usize },
    /// Something the person running the export should know. Never fatal.
    Warning(String),
    /// One tree is complete on disk.
    TreeWritten {
        index: usize,
        total: usize,
        folder: String,
        tiles: usize,
    },
}

/// What an export wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportSummary {
    /// Trees written, one per projection the export covered.
    pub views: usize,
    /// Tile files across every tree (excludes `info.json`, metadata and viewer assets).
    pub tiles: usize,
    /// Everything reported as `ExportEvent::Warning`, in the order it was reported.
    pub warnings: Vec<String>,
}

/// Export `engine` as a self-contained static IIIF Level-0 tree under `out_dir` (created if it
/// doesn't exist).
///
/// This is `serve` materialized to disk: [`enumerate_request_space`] produces the exact
/// `(region, size)` pairs OpenSeadragon will request against the emitted `info.json`, and each is
/// rendered via `engine.tile(...)`, the same call the live HTTP server makes.
pub fn export(
    engine: &(dyn TileEngine + Sync),
    out_dir: &Path,
    options: &ExportOptions,
) -> Result<ExportSummary, ExportError> {
    export_with_progress(engine, out_dir, options, &mut |_| {})
}

/// [`export`], reporting each step through `on_event`.
pub fn export_with_progress(
    engine: &(dyn TileEngine + Sync),
    out_dir: &Path,
    options: &ExportOptions,
    on_event: &mut dyn FnMut(ExportEvent),
) -> Result<ExportSummary, ExportError> {
    // Checked first, flag or not, so a typo never costs a long render before it is reported.
    let overlay_opacity = tiling::parse_opacity(Some(options.overlay_opacity)).map_err(|e| {
        // `TileError::OutOfRange`'s `Display` prefixes "out of range:", and `parse_opacity`'s own
        // message already ends "... out of range; expected 0 to 1" — stacking both restates the
        // error kind twice. Take the inner text; the flag name is the useful thing this wrapper
        // adds, not a second statement of what kind of error it is.
        let detail = match &e {
            tiling::TileError::OutOfRange(m) => m.clone(),
            other => other.to_string(),
        };
        ExportError::InvalidOption(format!("--overlay-opacity: {detail}"))
    })?;

    // Refuse before creating anything: a tree OpenSeadragon cannot pin is worse than no tree at
    // all, and the operator can still reach the image through `ziv serve`, which is level 2 and
    // reads `sizes` dynamically on every request rather than trusting a static pyramid. Every view
    // shares one tile list (`write_tree`'s doc comment: "every view has the same info.json
    // dimensions and pyramid"), so this is decided once, from this shared `ImageInfo`, before
    // `out_dir` exists or any tree is written.
    let shared_info = engine.image_info(&options.id);
    let level0_plan =
        iiif::level0_sizes(&shared_info).ok_or_else(|| ExportError::UnsupportedPyramid {
            scale_factors: shared_info.scale_factors.clone(),
            levels: shared_info.sizes.len(),
        })?;

    // What writing this pyramid at level0 actually costs, told to the operator rather than left
    // for them to discover from the directory listing. Two cases are worth a warning:
    //   - over budget: no whole image is written at all (see `Level0Sizes::within_budget`), so
    //     these trees serve tiles only and `full/max` never resolves;
    //   - within budget but untrimmed, with the finest (native-resolution) level too big for a
    //     single tile: every tree pays for one extra full-resolution whole image it would not
    //     otherwise need, to satisfy level0's `full/max` requirement.
    // Everything else — a trimmed pyramid (OSD reconstructs the dropped entry on its own), or an
    // untrimmed pyramid whose finest level already fits one tile (OSD already requests that whole
    // image, so writing it is free) — costs nothing extra and gets no warning.
    let level0_cost_warning = if !level0_plan.within_budget() {
        Some(format!(
            "the largest whole image this {}x{} image would advertise ({}x{}) exceeds the \
             whole-image budget of {} megapixels (and {} px per edge), so these trees serve \
             tiles only and do not answer full/max. Tiles and the viewer are unaffected.",
            shared_info.width,
            shared_info.height,
            level0_plan.max_width,
            level0_plan.max_height,
            iiif::MAX_WHOLE_IMAGE_PIXELS / 1_000_000,
            iiif::MAX_WHOLE_IMAGE_EDGE,
        ))
    } else if !level0_plan.trimmed && !iiif::finest_level_fits_one_tile(&shared_info) {
        Some(format!(
            "this image has no pyramid level below full resolution, so each tree writes a \
             {}x{} whole image at full resolution to satisfy level0's full/max",
            level0_plan.max_width, level0_plan.max_height
        ))
    } else {
        None
    };

    fs::create_dir_all(out_dir).map_err(|e| io_err(out_dir, e))?;

    let dims = engine.dimensions();
    let plan = crate::views::plan_views(
        &dims,
        &crate::views::ViewSelection {
            planes: options.planes,
            labels: options.labels,
            overlay_opacity,
        },
    );

    // A manifest presents several trees as one object, so it is written only when there is more
    // than one to present — UNLESS this tree is over the whole-image budget AND even its
    // SMALLEST level is too large to serve as the manifest's fallback body
    // (`manifest::manifest_json`'s own fallback, `enumerate::smallest_level_whole_image`): there is
    // then no file any canvas body could ever name, at this budget, so writing manifest.json would
    // just be another 404 waiting to happen. Trimming never drops the LAST entry of `sizes` (only
    // ever the finest), so the smallest level's own dimensions are always `shared_info.sizes`'s
    // last entry, regardless of whether this plan is trimmed.
    let mut writing_manifest = plan.views.len() > 1;
    let mut manifest_needs_body = false;
    if writing_manifest && !level0_plan.within_budget() {
        let (coarsest_w, coarsest_h) = *shared_info
            .sizes
            .last()
            .expect("a pyramid this crate builds always has at least one level");
        if iiif::whole_image_within_budget(coarsest_w, coarsest_h) {
            manifest_needs_body = true;
        } else {
            writing_manifest = false;
        }
    }

    // Folded in here so the planned total matches what rendering actually produces
    // (`docs/viewer.md`'s "prints the same totals before and after rendering"), rather than
    // reporting the bare enumerated count and under-counting by however many whole images (or the
    // manifest's own fallback image) `write_tree` adds.
    let (per_view_requests, per_view_copies) =
        level0_contract_requests(&shared_info, manifest_needs_body);
    let tiles_per_view = per_view_requests.len() + per_view_copies.len();
    on_event(ExportEvent::Planned {
        views: plan.views.len(),
        tiles_per_view,
    });
    if let Some(warning) = &level0_cost_warning {
        on_event(ExportEvent::Warning(warning.clone()));
    }
    for warning in &plan.warnings {
        on_event(ExportEvent::Warning(warning.clone()));
    }

    let mut tiles = 0;
    for (i, view) in plan.views.iter().enumerate() {
        let written = write_tree(
            engine,
            &view.projection,
            out_dir,
            &view.folder,
            &crate::views::tree_id(&options.id, &view.folder),
            options.quality,
            manifest_needs_body,
        )?;
        tiles += written;
        on_event(ExportEvent::TreeWritten {
            index: i + 1,
            total: plan.views.len(),
            folder: view.folder.clone(),
            tiles: written,
        });
    }

    write_file(
        &out_dir.join("ziv/dimensions.json"),
        &pretty(&dims.to_json()),
    )?;
    write_file(
        &out_dir.join("ziv/views.json"),
        &pretty(&crate::views::views_json(&plan, &dims)),
    )?;

    // Same order as the events emitted above: the level0 cost warning first, then the plan's own.
    let mut warnings: Vec<String> = level0_cost_warning.into_iter().collect();
    warnings.extend(plan.warnings.clone());
    // Gated on the PLAN, not the flags (the same `writing_manifest` decided above, before the
    // view loop): a manifest means exactly one thing, presenting several trees as one object, so
    // it is written only when there is more than one to present AND at least one file can exist
    // for its bodies to name. A `--labels` (or `--planes`) that adds nothing already warns about
    // that on its own; gating on the flags instead would additionally write a single-canvas
    // manifest nobody asked for, with its own spurious relative-id warning on top. A one-view
    // export is already fully usable through its own `info.json`.
    if writing_manifest {
        let (manifest, manifest_warnings) =
            crate::manifest::manifest_json(&plan, &dims, &shared_info, &options.id, &options.name);
        for warning in &manifest_warnings {
            on_event(ExportEvent::Warning(warning.clone()));
        }
        warnings.extend(manifest_warnings);
        write_file(&out_dir.join("manifest.json"), &pretty(&manifest))?;
    } else if plan.views.len() > 1 {
        // We would otherwise have written a manifest (more than one view), but even this image's
        // smallest level is over the whole-image budget: no file exists, or ever will at this
        // budget, for a canvas body to name.
        let warning = crate::manifest::NO_MANIFEST_OVER_BUDGET_WARNING.to_string();
        on_event(ExportEvent::Warning(warning.clone()));
        warnings.push(warning);
    }
    viewer::write_viewer(out_dir)?;

    Ok(ExportSummary {
        views: plan.views.len(),
        tiles,
        warnings,
    })
}

/// [`enumerate_request_space`]'s own output for `info`, plus what the level 0 contract adds on
/// top: a whole-image file for every size [`iiif::level0_sizes`] may advertise, and `full/max`. Or,
/// when the tree itself is over budget but a manifest still needs a body to point at, just the
/// smallest level's own whole image (see `manifest_needs_body` below).
///
/// Returns the requests to RENDER (via `engine.tile`, exactly like `enumerate_request_space`'s own
/// output), plus a separate list of files to COPY rather than render again: `(destination relative
/// path, source relative path)` pairs, where the source is always one of the just-rendered
/// `requests`. An advertised size that is pixel-identical to a whole image already produced under a
/// different name is copied, not rendered a second time (D4's "copy, don't re-render" principle):
/// `full/max` and a literal `full/{width},{height}` entry are pixel-identical whenever both exist,
/// since both are the tree's native full-resolution rendering. This is exactly the case
/// `sample_v04.ome.zarr`/`sample_labels.ome.zarr` (finest level fits one tile, so `sizes` is no
/// longer trimmed there — see `iiif::level0_sizes`) and `sample_multidim.ome.zarr`/
/// `sample_planes_labels.ome.zarr` (single level, always untrimmed) both hit.
///
/// `manifest_needs_body`, when true, is meaningful only when the plan is OVER budget (the
/// within-budget branch already guarantees the smallest level's whole image exists, because
/// trimming only ever drops the FINEST entry of `sizes`, never the last): it adds exactly the
/// smallest level's own whole image (`enumerate::smallest_level_whole_image`), deduplicated the
/// same way, so a manifest the caller has already decided to write always has a real file for its
/// fallback body to name. See `export_with_progress`'s own budget gate for why this is only ever
/// asked for when that file is itself within budget — this function does not check that itself.
///
/// # Panics
/// Panics if [`iiif::level0_sizes`] returns `None`: `write_tree`'s `info.json` write panics on the
/// same condition (see its doc comment), so this can only be reached through a pyramid that export
/// would refuse to serve anyway.
fn level0_contract_requests(
    info: &iiif::ImageInfo,
    manifest_needs_body: bool,
) -> (Vec<EnumeratedRequest>, Vec<(String, String)>) {
    let mut requests = enumerate_request_space(info);
    let plan = iiif::level0_sizes(info)
        .expect("level0_sizes: to_info_json_level0 would refuse this pyramid too");
    let mut copies = Vec::new();

    if !plan.within_budget() {
        if manifest_needs_body {
            let req = crate::enumerate::smallest_level_whole_image(info);
            if !requests.contains(&req) {
                requests.push(req);
            }
        }
        return (requests, copies);
    }

    let full_max_relpath = "full/max/0/default.jpg".to_string();
    let has_full_max = requests
        .iter()
        .any(|r| r.region_str == "full" && r.size_str == "max");
    for &(w, h) in &plan.sizes {
        let req = EnumeratedRequest {
            region_str: "full".to_string(),
            size_str: format!("{w},{h}"),
            region: iiif::Region::Full,
            size: iiif::Size::Wh(w, h),
        };
        if requests.contains(&req) {
            continue;
        }
        if has_full_max && (w, h) == (info.width, info.height) {
            copies.push((req.relative_path(), full_max_relpath.clone()));
        } else {
            requests.push(req);
        }
    }
    if !has_full_max {
        let largest_relpath = format!("full/{},{}/0/default.jpg", plan.max_width, plan.max_height);
        copies.push((full_max_relpath, largest_relpath));
    }
    (requests, copies)
}

/// Writes one complete Level 0 tree for `projection` into `out_dir/folder` (`"."` is `out_dir`
/// itself): every enumerated tile, plus the level 0 contract's whole-image derivatives and
/// `full/max` (see [`level0_contract_requests`]), then `info.json` with `tree_id` as its `id`.
/// `manifest_needs_body` is threaded straight through to `level0_contract_requests`: true when a
/// manifest is coming and this tree is over budget, so the manifest's own fallback body still gets
/// a real file. Returns the number of files written.
///
/// Every view of one image shares the same `info.json` dimensions and pyramid, so every tree has
/// the same tile list; only the projection differs.
pub fn write_tree(
    engine: &(dyn TileEngine + Sync),
    projection: &ProjectionId,
    out_dir: &Path,
    folder: &str,
    tree_id: &str,
    quality: u8,
    manifest_needs_body: bool,
) -> Result<usize, ExportError> {
    let dir = if folder == "." {
        out_dir.to_path_buf()
    } else {
        out_dir.join(folder)
    };
    let info = engine.image_info(tree_id);
    let (requests, copies) = level0_contract_requests(&info, manifest_needs_body);

    // See `crate::ambient_runtime_handle`'s doc comment: captured HERE, on the thread that called
    // into `write_tree` (which has ambient tokio context whenever `ziv export`'s own runtime is
    // what got us here), so it can be re-entered inside each rayon worker below -- otherwise a
    // remote-backed tile read's `pollster::block_on` call has no reactor to reach and panics with
    // "there is no reactor running, must be called from the context of a Tokio 1.x runtime".
    let runtime_handle = crate::ambient_runtime_handle();
    let written: Vec<Result<(), ExportError>> = requests
        .par_iter()
        .map(|req| {
            // `None` on a local-filesystem export (including every test in this crate): this
            // closure then runs exactly as before the fix. See the doc comment above.
            let _tokio_guard = runtime_handle.as_ref().map(tokio::runtime::Handle::enter);
            let bytes = engine
                .tile(projection, req.region, req.size, quality)
                .map_err(|source| ExportError::Tile {
                    // Named with its folder, so a failure in a 50-tree export says which tree.
                    relative_path: if folder == "." {
                        req.relative_path()
                    } else {
                        format!("{folder}/{}", req.relative_path())
                    },
                    source,
                })?;
            write_file(&dir.join(req.relative_path()), &bytes)
        })
        .collect();
    for r in written {
        r?;
    }

    // Every copy source is guaranteed to be among the `requests` just rendered above (see
    // `level0_contract_requests`'s doc comment), so this always runs after its source exists.
    for (dest_relpath, source_relpath) in &copies {
        let source = dir.join(source_relpath);
        let dest = dir.join(dest_relpath);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }
        fs::copy(&source, &dest).map_err(|e| io_err(&dest, e))?;
    }

    write_file(
        &dir.join("info.json"),
        serde_json::to_vec_pretty(&info.to_info_json_level0())
            .expect("ImageInfo serializes to valid JSON")
            .as_slice(),
    )?;
    Ok(requests.len() + copies.len())
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), ExportError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
    }
    fs::write(path, bytes).map_err(|e| io_err(path, e))
}

fn io_err(path: &Path, source: std::io::Error) -> ExportError {
    ExportError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn pretty(value: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec_pretty(value).expect("a serde_json::Value always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use iiif::{Region, Size};
    use std::collections::HashSet;
    use tiling::ZarrTileEngine;
    use zarr_core::ZarrImage;

    fn engine() -> ZarrTileEngine {
        let img = ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
        ZarrTileEngine::new(img)
    }

    /// A bigger fixture (multi-tile levels) built purely from the same 64x64 sample by
    /// forcing a tiny `tile_size` isn't available (tile_size is fixed at 512 in
    /// `ZarrTileEngine::new`), so the sample_v04 fixture (64x64, 2 levels, both fit in one
    /// 512 tile) already exercises the fits-in-one-tile path end-to-end; the enumerator's
    /// own unit tests (crate `enumerate` module) cover the multi-tile grid math
    /// independently of a real `TileEngine`. This test suite focuses on writer behavior:
    /// every enumerated file exists, no orphans, bytes match `engine.tile`, info.json
    /// shape.
    #[test]
    fn export_writes_every_enumerated_file_and_no_orphans() {
        let e = engine();
        let dir = tempfile::tempdir().unwrap();
        let count = export(&e, dir.path(), &ExportOptions::default())
            .unwrap()
            .tiles;
        assert!(count > 0);

        let info = e.image_info(".");
        // This fixture (64x64, 2 levels, both fit in one 512 tile) is untrimmed
        // (`iiif::level0_sizes`: the finest level fits one tile), so the level 0 contract adds
        // one file beyond the bare enumerated set: `full/64,64`, a copy of the already-rendered
        // `full/max` (see `level0_contract_requests`'s doc comment).
        let (requests, copies) = level0_contract_requests(&info, false);
        assert_eq!(requests.len() + copies.len(), count);

        let mut expected_files: HashSet<PathBuf> = requests
            .iter()
            .map(|r| dir.path().join(r.relative_path()))
            .collect();
        for (dest, _source) in &copies {
            expected_files.insert(dir.path().join(dest));
        }
        expected_files.insert(dir.path().join("info.json"));
        expected_files.insert(dir.path().join("index.html"));

        // EXPORTER COMPLETENESS: every enumerated URL exists on disk.
        for f in &expected_files {
            assert!(f.exists(), "expected exported file missing: {f:?}");
        }

        // No orphan tile files: walk the tree, collect every `default.jpg`, and confirm
        // it's exactly the enumerated set (the info.json/index.html/viewer assets are
        // excluded from this check via `is_tile_jpeg`).
        let mut found_tiles: HashSet<PathBuf> = HashSet::new();
        walk_jpegs(dir.path(), &mut found_tiles);
        let mut expected_tiles: HashSet<PathBuf> = requests
            .iter()
            .map(|r| dir.path().join(r.relative_path()))
            .collect();
        for (dest, _source) in &copies {
            expected_tiles.insert(dir.path().join(dest));
        }
        assert_eq!(found_tiles, expected_tiles, "orphan or missing tile files");
    }

    fn walk_jpegs(dir: &Path, out: &mut HashSet<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                walk_jpegs(&path, out);
            } else if path
                .file_name()
                .map(|n| n == "default.jpg")
                .unwrap_or(false)
            {
                out.insert(path);
            }
        }
    }

    /// PIXEL SPOT-CHECK: an exported region tile's bytes == `engine.tile(same region, same
    /// size, quality)` bytes — export is just materialized serve, byte for byte.
    #[test]
    fn exported_tile_bytes_match_direct_engine_tile_call() {
        let e = engine();
        let dir = tempfile::tempdir().unwrap();
        export(&e, dir.path(), &ExportOptions::default()).unwrap();

        let direct = e
            .tile(&ProjectionId::Default, Region::Full, Size::Max, 85)
            .unwrap();
        let on_disk = fs::read(dir.path().join("full/max/0/default.jpg")).unwrap();
        assert_eq!(direct, on_disk);
    }

    /// Regression for a mutation that leaves every test above green: `views.rs` renders every
    /// overlay at `DEFAULT_OVERLAY_OPACITY` regardless of what `--overlay-opacity` asked for,
    /// while still writing the REQUESTED value into `ziv/views.json`. Nothing before this pinned
    /// the rendered bytes at a non-default opacity, so the tiles could silently disagree with what
    /// the viewer advertises and applies. Proven the same way
    /// `exported_tile_bytes_match_direct_engine_tile_call` proves the plain path: the exported
    /// overlay tile must equal a direct `engine.tile` call for the SAME projection (same z, same
    /// palette, the requested opacity), not `DEFAULT_OVERLAY_OPACITY`.
    #[test]
    fn exported_overlay_tile_bytes_honour_a_non_default_overlay_opacity() {
        let e = ZarrTileEngine::new(
            ZarrImage::open("../../tests/fixtures/sample_labels.ome.zarr").unwrap(),
        );
        let requested_opacity = 0.25;
        assert_ne!(
            requested_opacity,
            crate::views::DEFAULT_OVERLAY_OPACITY,
            "the test is meaningless if the requested value happens to be the default"
        );
        let dir = tempfile::tempdir().unwrap();
        let options = ExportOptions {
            labels: true,
            overlay_opacity: requested_opacity,
            ..ExportOptions::default()
        };
        export(&e, dir.path(), &options).unwrap();

        // The exact projection `plan_views` builds for plane 0's `nuclei` overlay (see
        // `views::plan_views`/`views::plane`): same z, same palette, but the OPACITY THE PERSON
        // ASKED FOR, not the default.
        let projection = ProjectionId::Dynamic(iiif::DynamicProj {
            z: Some(0),
            label: Some(iiif::DynLabel {
                name: "nuclei".to_string(),
                palette: Some(crate::views::OVERLAY_PALETTE.to_string()),
                overlay: true,
                opacity: Some(requested_opacity),
            }),
            ..iiif::DynamicProj::default()
        });
        let direct = e.tile(&projection, Region::Full, Size::Max, 85).unwrap();
        let on_disk =
            fs::read(dir.path().join("planes/0/labels/0/full/max/0/default.jpg")).unwrap();
        assert_eq!(
            direct, on_disk,
            "the rendered overlay must be drawn at the requested opacity, not the default"
        );

        // `ziv/views.json` must agree with what was actually rendered.
        let views: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join("ziv/views.json")).unwrap()).unwrap();
        assert_eq!(views["labels"][0]["opacity"], requested_opacity);
    }

    #[test]
    fn info_json_is_level0_with_relative_id() {
        let e = engine();
        let dir = tempfile::tempdir().unwrap();
        export(&e, dir.path(), &ExportOptions::default()).unwrap();

        let bytes = fs::read(dir.path().join("info.json")).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["profile"], "level0");
        assert_eq!(v["id"], ".");
        assert_eq!(v["extraFeatures"], serde_json::json!(["sizeByWh"]));
    }

    #[test]
    fn custom_id_is_honored() {
        let e = engine();
        let dir = tempfile::tempdir().unwrap();
        let opts = ExportOptions {
            id: "https://example.org/iiif/foo".to_string(),
            ..ExportOptions::default()
        };
        export(&e, dir.path(), &opts).unwrap();
        let bytes = fs::read(dir.path().join("info.json")).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["id"], "https://example.org/iiif/foo");
    }

    #[test]
    fn export_via_dyn_tile_engine_trait_object() {
        let e: Box<dyn TileEngine + Sync> = Box::new(engine());
        let dir = tempfile::tempdir().unwrap();
        let count = export(e.as_ref(), dir.path(), &ExportOptions::default())
            .unwrap()
            .tiles;
        assert!(count > 0);
    }

    /// `write_tree` renders the projection it is given, into the folder it is given. `@c=0` on
    /// `sample_labels` (a red horizontal ramp plus a blue vertical one) is a visibly different
    /// picture from the default, so a tree that silently rendered the default fails the comparison.
    #[test]
    fn write_tree_renders_its_projection_into_its_folder() {
        let e = ZarrTileEngine::new(
            ZarrImage::open("../../tests/fixtures/sample_labels.ome.zarr").unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        let projection = iiif::parse_identifier("@c=0");
        let tiles = write_tree(&e, &projection, dir.path(), "planes/0", ".", 85, false).unwrap();
        assert!(tiles > 0);

        let on_disk = fs::read(dir.path().join("planes/0/full/max/0/default.jpg")).unwrap();
        let direct = e.tile(&projection, Region::Full, Size::Max, 85).unwrap();
        let default = e
            .tile(&ProjectionId::Default, Region::Full, Size::Max, 85)
            .unwrap();
        assert_eq!(on_disk, direct);
        assert_ne!(
            on_disk, default,
            "the fixture must tell the two projections apart"
        );

        let info: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join("planes/0/info.json")).unwrap())
                .unwrap();
        assert_eq!(info["id"], ".");
        assert!(
            !dir.path().join("info.json").exists(),
            "nothing is written outside the folder"
        );
    }

    /// A tile that fails to render still names the tree it belongs to: `write_tree`'s
    /// `relative_path` is prefixed with the folder whenever the folder isn't the root, so a
    /// failure in a many-tree export says which tree, not just which tile.
    #[test]
    fn write_tree_names_its_folder_in_a_tile_error() {
        let e = engine();
        let dir = tempfile::tempdir().unwrap();
        // Out of range for every fixture this crate uses: `render` checks `z` against
        // `size_z` before anything else, so every enumerated request in this tree fails the
        // same way.
        let unrenderable = iiif::parse_identifier("@z=999");
        let err =
            write_tree(&e, &unrenderable, dir.path(), "planes/0", ".", 85, false).unwrap_err();
        match err {
            ExportError::Tile { relative_path, .. } => {
                assert!(relative_path.starts_with("planes/0/"), "{relative_path}");
            }
            other => panic!("expected ExportError::Tile, got {other:?}"),
        }
    }

    #[test]
    fn a_plain_export_reports_one_view_and_its_tiles() {
        let e = engine();
        let dir = tempfile::tempdir().unwrap();
        let mut events = Vec::new();
        let summary = export_with_progress(&e, dir.path(), &ExportOptions::default(), &mut |ev| {
            events.push(ev)
        })
        .unwrap();
        let (requests, copies) = level0_contract_requests(&e.image_info("."), false);
        let expected = requests.len() + copies.len();
        assert_eq!(
            summary,
            ExportSummary {
                views: 1,
                tiles: expected,
                warnings: vec![]
            }
        );
        assert_eq!(
            events.last(),
            Some(&ExportEvent::TreeWritten {
                index: 1,
                total: 1,
                folder: ".".into(),
                tiles: expected
            })
        );
    }

    /// `docs/viewer.md` promises `ziv export` "prints the same totals before and after
    /// rendering". `ExportEvent::Planned`'s `tiles_per_view` must therefore already include the
    /// level 0 contract's whole-image derivatives and `full/max` (`level0_contract_requests`'s
    /// additions beyond the bare `enumerate_request_space` count), not just the enumerated count,
    /// which under-reports by however many of those this tree's export actually writes.
    ///
    /// `sample_no_pyramid.ome.zarr` (2 z-planes, single level, 600x600, bigger than the 512px
    /// tile, untrimmed since it has only one level) is exactly this case: its sole level has no
    /// `full/...` of its own, so `level0_contract_requests` adds one literal whole-image entry
    /// (`full/600,600`, from its own untrimmed `sizes`) plus a `full/max` copy — 2 extra files
    /// per tree, unconditionally, regardless of `--planes`/the manifest gate (unlike the deleted
    /// manifest-only fallback this replaced). Before that: `Planned` reported the 4 bare
    /// enumerated tiles (`views: 2, tiles_per_view: 4` -> a planned total of 8) while rendering
    /// actually wrote 6 tiles per tree (12 total): the exact under-count the docs promise cannot
    /// happen.
    #[test]
    fn planned_totals_include_the_level0_contract_files_and_match_what_is_written() {
        let e = ZarrTileEngine::new(
            ZarrImage::open("../../tests/fixtures/sample_no_pyramid.ome.zarr").unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        let options = ExportOptions {
            planes: true,
            ..ExportOptions::default()
        };
        let mut events = Vec::new();
        let summary =
            export_with_progress(&e, dir.path(), &options, &mut |ev| events.push(ev)).unwrap();

        let bare_enumerated = enumerate_request_space(&e.image_info(".")).len();
        assert_eq!(
            bare_enumerated, 4,
            "this fixture's own request space, no whole image of its own"
        );

        let planned = events
            .iter()
            .find_map(|ev| match ev {
                ExportEvent::Planned {
                    views,
                    tiles_per_view,
                } => Some((*views, *tiles_per_view)),
                _ => None,
            })
            .expect("a Planned event must be reported");
        assert_eq!(
            planned,
            (2, 6),
            "tiles_per_view must count the level0 contract's whole image and full/max, not just \
             the bare enumerated set"
        );

        let planned_total = planned.0 * planned.1;
        assert_eq!(planned_total, 12, "planning 2 views x 6 tiles = 12 tiles");
        assert_eq!(
            summary.tiles, planned_total,
            "the planned total before rendering must match the tiles actually written"
        );
    }

    /// The same property as `planned_totals_include_the_level0_contract_files_and_match_what_is_written`,
    /// on a fixture whose plan is TRIMMED rather than untrimmed: `sample_multi_tile.ome.zarr`
    /// (1024x1024, 2 levels, 512px tile) drops its full-resolution entry (`iiif::level0_sizes`
    /// trims it), and its bare enumerated request space already has a literal `full/512,512` (the
    /// coarsest level's own tile-boundary whole image), so `level0_contract_requests`'s
    /// size-loop adds nothing new there; only `full/max` is a genuinely new file, since this
    /// pyramid's finest (native) level never fits in a single tile.
    #[test]
    fn planned_totals_for_a_trimmed_pyramid_match_what_is_written() {
        let e = ZarrTileEngine::new(
            ZarrImage::open("../../tests/fixtures/sample_multi_tile.ome.zarr").unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        let mut events = Vec::new();
        let summary = export_with_progress(&e, dir.path(), &ExportOptions::default(), &mut |ev| {
            events.push(ev)
        })
        .unwrap();

        let bare_enumerated = enumerate_request_space(&e.image_info(".")).len();
        assert_eq!(
            bare_enumerated, 5,
            "4 grid tiles plus the coarsest level's own full/512,512"
        );

        let planned = events
            .iter()
            .find_map(|ev| match ev {
                ExportEvent::Planned {
                    views,
                    tiles_per_view,
                } => Some((*views, *tiles_per_view)),
                _ => None,
            })
            .expect("a Planned event must be reported");
        assert_eq!(
            planned,
            (1, 6),
            "tiles_per_view must count the new full/max on top of the bare enumerated set"
        );
        assert_eq!(
            summary.tiles, 6,
            "the planned total before rendering must match the tiles actually written"
        );
    }

    /// `ExportEvent::Planned` and `ExportSummary::warnings` reach the caller, not just
    /// `TreeWritten`: a build that emitted `Planned` but dropped it, or that replaced
    /// `warnings: plan.warnings` with `Vec::new()`, would still pass every other test in this
    /// module (none of them ask for a flag that has nothing to warn about).
    #[test]
    fn a_labels_flag_with_no_labels_in_the_image_reports_planned_and_the_warning() {
        let e = ZarrTileEngine::new(
            ZarrImage::open("../../tests/fixtures/sample_multidim.ome.zarr").unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        let mut events = Vec::new();
        let options = ExportOptions {
            labels: true,
            ..ExportOptions::default()
        };
        let summary =
            export_with_progress(&e, dir.path(), &options, &mut |ev| events.push(ev)).unwrap();

        assert!(
            matches!(events.first(), Some(ExportEvent::Planned { views: 1, .. })),
            "{events:?}"
        );
        let warning = "--labels adds nothing: the image has no label images".to_string();
        assert_eq!(events[1], ExportEvent::Warning(warning.clone()));
        // A single-view plan (this one: `--labels` added nothing) never triggers the manifest, so
        // no second, manifest-only warning should follow this one onto `summary.warnings`.
        assert_eq!(summary.warnings, vec![warning]);
    }

    /// `level0_contract_requests`'s `manifest_needs_body` branch, exercised directly against a
    /// synthetic `ImageInfo` (no `TileEngine`, no rendering): a clean 5-level halving pyramid whose
    /// TRIMMED bound (16384x16384) is over budget but whose COARSEST level (2048x2048) is not, and
    /// — unlike `sample_multi_tile.ome.zarr` or the fake-engine integration tests in
    /// `tests/whole_image_budget.rs` — is deliberately still bigger than the 512px tile, so it does
    /// NOT already have a `full/...` entry of its own in `enumerate_request_space`'s bare output.
    /// This is the one shape that proves the manifest-fallback push actually adds a file, not just
    /// finds one already there.
    #[test]
    fn level0_contract_requests_adds_the_manifest_fallback_only_when_asked_and_only_when_missing() {
        let info = iiif::ImageInfo {
            id: ".".into(),
            width: 32768,
            height: 32768,
            tile_size: 512,
            scale_factors: vec![1, 2, 4, 8, 16],
            sizes: vec![
                (32768, 32768),
                (16384, 16384),
                (8192, 8192),
                (4096, 4096),
                (2048, 2048),
            ],
        };
        let plan =
            iiif::level0_sizes(&info).expect("a clean power-of-two pyramid must be pinnable");
        assert!(
            !plan.within_budget(),
            "the trimmed max (16384x16384) must be over budget for this test to mean anything: {plan:?}"
        );
        assert!(
            iiif::whole_image_within_budget(2048, 2048),
            "the coarsest level must itself be within budget"
        );

        let bare = enumerate_request_space(&info);
        assert!(
            !bare
                .iter()
                .any(|r| r.relative_path() == "full/2048,2048/0/default.jpg"),
            "sanity: the coarsest level must NOT already fit one tile (512px), or this test would \
             pass even if the fallback push were deleted"
        );

        let (without_body, copies_without) = level0_contract_requests(&info, false);
        assert!(copies_without.is_empty());
        assert_eq!(
            without_body.len(),
            bare.len(),
            "over budget, no manifest coming: the contract adds nothing"
        );

        let (with_body, copies_with) = level0_contract_requests(&info, true);
        assert!(
            copies_with.is_empty(),
            "the fallback is a render, not a copy"
        );
        assert_eq!(
            with_body.len(),
            bare.len() + 1,
            "exactly the smallest level's own whole image is added"
        );
        assert!(with_body
            .iter()
            .any(|r| r.relative_path() == "full/2048,2048/0/default.jpg"));
    }
}
