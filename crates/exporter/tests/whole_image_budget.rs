//! The whole-image budget gate (`iiif::level0::MAX_WHOLE_IMAGE_PIXELS`/`MAX_WHOLE_IMAGE_EDGE`),
//! exercised end to end through a fake `TileEngine` rather than a real zarr fixture.
//!
//! `sample_huge_level.ome.zarr` (the only committed fixture actually over this budget) cannot be
//! driven through a real export at all: its one declared chunk spans the whole 100000x100000
//! image, so even a single tile read trips the zarr reader's decompression-bomb guard before any
//! tile renders (see `crates/tiling/src/engine.rs`'s
//! `full_region_on_huge_single_level_is_rejected_not_oom`). A prior version of this test suite
//! therefore only ever checked `iiif::ImageInfo::to_info_json_level0`'s OUTPUT for that fixture —
//! never `writer::write_tree`'s own budget gate, which decides whether to actually WRITE a whole
//! image. A reviewer proved the gap by disabling that gate (`level0_contract_requests`'s
//! `if !plan.within_budget()` check) and finding every one of the crate's then-599 tests still
//! passed. These tests close it, using a `FakeTileEngine` whose `render` returns a few dummy
//! bytes instantly, so dimensions well past the budget stay a fast, in-memory test rather than an
//! attempted multi-gigapixel render.

use std::fs;
use std::path::Path;

use exporter::{export, export_with_progress, write_tree, ExportOptions};
use iiif::ProjectionId;
use tiling::{ChannelDescriptor, ImageDimensions, RenderSpec, TileEngine};

/// A `TileEngine` whose shape (dimensions, pyramid, z-planes) is entirely up to the caller, and
/// whose `render` never touches real pixels: it returns a handful of fixed bytes for every
/// request, so tests can drive dimensions well past any real budget without actually attempting
/// to encode an image that size.
struct FakeEngine {
    info: iiif::ImageInfo,
    size_z: u64,
}

impl TileEngine for FakeEngine {
    fn image_info(&self, id_base: &str) -> iiif::ImageInfo {
        iiif::ImageInfo {
            id: id_base.to_string(),
            ..self.info.clone()
        }
    }

    fn dimensions(&self) -> ImageDimensions {
        ImageDimensions {
            size_t: 1,
            size_z: self.size_z,
            size_c: 1,
            default_t: 0,
            default_z: 0,
            channels: vec![ChannelDescriptor {
                index: 0,
                label: None,
                color: "FFFFFF".to_string(),
                window: (0.0, 255.0),
                active: true,
            }],
            labels: vec![],
            label_open_failures: vec![],
        }
    }

    fn render(&self, _id: &ProjectionId, _spec: &RenderSpec) -> Result<Vec<u8>, tiling::TileError> {
        // Never decoded as a real JPEG by anything in this test file: existence and identity are
        // all these tests need, and a real render at these dimensions is exactly what the budget
        // gate exists to avoid attempting.
        Ok(vec![0xFFu8; 16])
    }
}

/// A clean, single-level "no pyramid" fake image at `(width, height)`, one z-plane.
fn single_level(width: u64, height: u64, tile_size: u64) -> FakeEngine {
    FakeEngine {
        info: iiif::ImageInfo {
            id: ".".into(),
            width,
            height,
            tile_size,
            scale_factors: vec![1],
            sizes: vec![(width, height)],
        },
        size_z: 1,
    }
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// Over the pixel budget (8001x8000 = 64,008,000 px, one over `MAX_WHOLE_IMAGE_PIXELS`): no whole
/// image is written at all, and `info.json` declares no bound. Driven directly through
/// `write_tree`, the function whose own gate was the untested one.
#[test]
fn write_tree_writes_no_whole_image_when_over_the_pixel_budget() {
    let engine = single_level(8001, 8000, 512);
    let dir = tempfile::tempdir().unwrap();
    write_tree(
        &engine,
        &ProjectionId::Default,
        dir.path(),
        ".",
        ".",
        85,
        false,
    )
    .unwrap();

    assert!(!dir.path().join("full/max/0/default.jpg").exists());
    assert!(!dir.path().join("full/8001,8000/0/default.jpg").exists());
    let info = read_json(&dir.path().join("info.json"));
    assert!(info.get("maxWidth").is_none());
    assert!(info.get("maxHeight").is_none());
}

/// The same over-budget shape, driven through `export` so `ExportSummary::warnings` is reachable:
/// the operator is told these trees serve tiles only and do not answer `full/max`, rather than
/// left to notice the absence from the directory listing. The message must name the budget
/// itself, not just say something is over it (D9), so this checks for the budget figure, not
/// merely the "tiles only" phrase.
#[test]
fn export_over_the_pixel_budget_warns_that_tiles_only_are_served() {
    let engine = single_level(8001, 8000, 512);
    let dir = tempfile::tempdir().unwrap();
    let summary = export(&engine, dir.path(), &ExportOptions::default()).unwrap();
    assert!(
        summary
            .warnings
            .iter()
            .any(|w| w.contains("tiles only") && w.contains("64 megapixels")),
        "{:?}",
        summary.warnings
    );
}

/// The same warning must reach the operator as an `ExportEvent::Warning`, not merely be folded
/// into the final `ExportSummary`: `ziv export` (the real CLI) prints warnings only from that
/// event stream (`crates/cli/src/main.rs`'s `print_export_event`), never from the summary, so a
/// build that silently dropped the event while still populating `summary.warnings` would leave
/// the operator with no warning on stderr at all, with every exporter-crate test still green.
#[test]
fn export_over_the_pixel_budget_emits_the_warning_as_an_event() {
    let engine = single_level(8001, 8000, 512);
    let dir = tempfile::tempdir().unwrap();
    let mut events = Vec::new();
    export_with_progress(&engine, dir.path(), &ExportOptions::default(), &mut |ev| {
        events.push(ev)
    })
    .unwrap();
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            exporter::ExportEvent::Warning(w)
                if w.contains("tiles only") && w.contains("64 megapixels")
        )),
        "{events:?}"
    );
}

/// The in-budget twin (8000x8000 = 64,000,000 px, exactly `MAX_WHOLE_IMAGE_PIXELS`): the whole
/// image and `full/max` both exist, and `info.json` declares the bound.
#[test]
fn write_tree_writes_the_whole_image_when_within_the_pixel_budget() {
    let engine = single_level(8000, 8000, 512);
    let dir = tempfile::tempdir().unwrap();
    write_tree(
        &engine,
        &ProjectionId::Default,
        dir.path(),
        ".",
        ".",
        85,
        false,
    )
    .unwrap();

    assert!(dir.path().join("full/max/0/default.jpg").exists());
    assert!(dir.path().join("full/8000,8000/0/default.jpg").exists());
    let info = read_json(&dir.path().join("info.json"));
    assert_eq!(info["maxWidth"], 8000);
    assert_eq!(info["maxHeight"], 8000);
}

/// A "no pyramid" image (one level) over budget: its only level IS its coarsest level, so there is
/// no smaller fallback to fall back to. A multi-view export (`--planes`, `size_z: 2`) must write no
/// `manifest.json` at all, and must say why.
#[test]
fn export_writes_no_manifest_when_even_the_smallest_level_is_over_budget() {
    let engine = FakeEngine {
        info: iiif::ImageInfo {
            id: ".".into(),
            width: 9000,
            height: 9000,
            tile_size: 4096,
            scale_factors: vec![1],
            sizes: vec![(9000, 9000)],
        },
        size_z: 2,
    };
    let dir = tempfile::tempdir().unwrap();
    let options = ExportOptions {
        planes: true,
        ..ExportOptions::default()
    };
    let summary = export(&engine, dir.path(), &options).unwrap();

    assert_eq!(summary.views, 2, "size_z: 2 with --planes is two views");
    assert!(
        !dir.path().join("manifest.json").exists(),
        "no file could ever exist for a canvas body to name at this budget"
    );
    assert!(
        summary
            .warnings
            .iter()
            .any(|w| w == exporter::NO_MANIFEST_OVER_BUDGET_WARNING),
        "{:?}",
        summary.warnings
    );
}

/// The pyramid `export_writes_the_manifest_fallback_body_when_the_coarsest_level_is_within_budget`
/// and `planned_totals_include_the_manifest_fallback_when_over_budget` share: 16384x16384 down to
/// 2048x2048 (4 levels, clean halving), `tile_size: 512`. The tile size is the whole point — see
/// the first test's doc comment for why a bigger one would prove nothing.
fn pyramid_over_budget_with_a_multi_tile_coarsest_level() -> FakeEngine {
    FakeEngine {
        info: iiif::ImageInfo {
            id: ".".into(),
            width: 16384,
            height: 16384,
            tile_size: 512,
            scale_factors: vec![1, 2, 4, 8],
            sizes: vec![(16384, 16384), (8192, 8192), (4096, 4096), (2048, 2048)],
        },
        size_z: 2,
    }
}

fn walk_default_jpgs(dir: &Path, out: &mut std::collections::HashSet<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk_default_jpgs(&path, out);
        } else if path.file_name().is_some_and(|n| n == "default.jpg") {
            out.insert(path);
        }
    }
}

/// A pyramided image whose TRIMMED bound (8192x8192) is over budget, but whose COARSEST level
/// (2048x2048) is not: the manifest is still written, and its canvas bodies resolve to a real file
/// — `write_tree`'s `manifest_needs_body` path, not the level 0 contract's own (which writes
/// nothing at all for this tree, being over budget).
///
/// `tile_size: 512` is deliberate and load-bearing: an earlier version of this test used `8192`,
/// under which the 2048x2048 coarsest level ALREADY fits in a single tile, so
/// `enumerate_request_space` writes it anyway with no help from `manifest_needs_body` at all — a
/// reviewer proved that version guarded nothing by finding it (and all 609 other tests) still
/// green with `manifest_needs_body` hard-coded to `false`, and, separately, by running the exact
/// same test against the commit that introduced the bug this whole file exists to catch, and
/// finding it passed there too. At `tile_size: 512`, 2048x2048 needs a 4x4 tile grid
/// (checked below), so `enumerate_request_space` has no whole-image entry for it and the ONLY way
/// it can exist is through the fallback this test is actually meant to guard.
#[test]
fn export_writes_the_manifest_fallback_body_when_the_coarsest_level_is_within_budget() {
    let engine = pyramid_over_budget_with_a_multi_tile_coarsest_level();
    // Sanity on the shape this test relies on, so a future edit to the pyramid above cannot
    // silently stop exercising the branch this test is for.
    let plan = iiif::level0_sizes(&engine.info).expect("a valid pyramid OSD can pin");
    assert!(!plan.within_budget(), "{plan:?}");
    assert!(iiif::whole_image_within_budget(2048, 2048));
    let bare = exporter::enumerate_request_space(&engine.info);
    assert!(
        !bare
            .iter()
            .any(|r| r.relative_path() == "full/2048,2048/0/default.jpg"),
        "sanity: the coarsest level must NOT already fit one tile, or this test proves nothing"
    );

    let dir = tempfile::tempdir().unwrap();
    let options = ExportOptions {
        planes: true,
        ..ExportOptions::default()
    };
    let summary = export(&engine, dir.path(), &options).unwrap();

    assert_eq!(summary.views, 2);
    assert!(dir.path().join("manifest.json").exists());
    let manifest = read_json(&dir.path().join("manifest.json"));
    let canvases = manifest["items"].as_array().unwrap();
    assert_eq!(canvases.len(), 2);
    for canvas in canvases {
        let body = &canvas["items"][0]["items"][0]["body"];
        let id = body["id"].as_str().unwrap();
        assert!(
            id.ends_with("full/2048,2048/0/default.jpg"),
            "expected the coarsest level's own whole image, got {id}"
        );
        // The id is relative to the export root the same way every other id in this suite is
        // (`docs`'s own convention): join it under `dir` to find the file on disk.
        assert!(
            dir.path().join(id).exists(),
            "manifest body {id} must name a file the writer actually wrote"
        );
    }
    // No trace of the level 0 contract's own (over-budget) additions.
    assert!(!dir.path().join("full/max/0/default.jpg").exists());
}

/// Task 4 rescope: within budget, each manifest canvas body must point at the LARGEST advertised
/// whole image (`maxWidth`x`maxHeight`), not merely the largest one that happens to already exist
/// in OpenSeadragon's own request space. A 2048x2048 pyramid at tile 512 (levels 2048, 1024, 512,
/// 256) has a whole image in OSD's own request space only for the two SINGLE-CELL levels (512 and
/// 256): the intermediate level (1024, a 2x2 grid) is genuinely multi-tile, so OSD never asks for
/// its whole image at all (see `enumerate`'s module doc). Before this change, `manifest.rs`'s
/// `largest_whole_image` picked its body from exactly that (weaker) request space, landing on
/// 512x512 even though `full/1024,1024` exists and is advertised as `maxWidth` (Task 3's level 0
/// contract writes a whole image for every advertised size, within budget). A viewer would then
/// paint the plain image at half the resolution the tree actually offers.
#[test]
fn within_budget_the_manifest_body_points_at_the_largest_advertised_image() {
    let engine = FakeEngine {
        info: iiif::ImageInfo {
            id: ".".into(),
            width: 2048,
            height: 2048,
            tile_size: 512,
            scale_factors: vec![1, 2, 4, 8],
            sizes: vec![(2048, 2048), (1024, 1024), (512, 512), (256, 256)],
        },
        size_z: 2,
    };
    // Sanity on the shape this test relies on, so a future edit to the pyramid above cannot
    // silently stop exercising the branch this test is for.
    let plan = iiif::level0_sizes(&engine.info).expect("a clean power-of-two pyramid pins");
    assert!(plan.within_budget(), "{plan:?}");
    assert_eq!(
        (plan.max_width, plan.max_height),
        (1024, 1024),
        "sanity: the trimmed bound must be the genuinely multi-tile level, or this test proves \
         nothing"
    );
    let bare = exporter::enumerate_request_space(&engine.info);
    assert!(
        !bare
            .iter()
            .any(|r| r.relative_path() == "full/1024,1024/0/default.jpg"),
        "sanity: OpenSeadragon's own request space must NOT already contain this whole image, or \
         the old (pre-fix) body choice would find it too and this test would prove nothing"
    );

    let dir = tempfile::tempdir().unwrap();
    let options = ExportOptions {
        planes: true,
        ..ExportOptions::default()
    };
    let summary = export(&engine, dir.path(), &options).unwrap();
    assert_eq!(summary.views, 2, "size_z: 2 with --planes is two views");

    let manifest = read_json(&dir.path().join("manifest.json"));
    let canvases = manifest["items"].as_array().unwrap();
    assert_eq!(canvases.len(), 2);
    for canvas in canvases {
        let body = &canvas["items"][0]["items"][0]["body"];
        let id = body["id"].as_str().unwrap();
        assert!(
            id.ends_with("full/1024,1024/0/default.jpg"),
            "expected the largest ADVERTISED whole image, got {id}"
        );
        assert_eq!(
            (body["width"].as_u64(), body["height"].as_u64()),
            (Some(1024), Some(1024))
        );
        assert!(
            dir.path().join(id).exists(),
            "manifest body {id} must name a file the writer actually wrote"
        );
    }
}

/// `export_with_progress`'s `Planned` total must include the manifest's fallback body exactly
/// when it fires, matching what `export` actually writes — the same "planned equals written"
/// property `writer.rs`'s own within-budget tests pin, now for the over-budget-with-manifest case,
/// AND cross-checked against the files that actually land on disk (2722 either way: `views: 2` x
/// `tiles_per_view: 1361`, the bare enumerated grid across 4 levels — 1024 + 256 + 64 + 16 tiles —
/// plus exactly one fallback whole image per tree).
#[test]
fn planned_totals_include_the_manifest_fallback_when_over_budget() {
    let engine = pyramid_over_budget_with_a_multi_tile_coarsest_level();
    let dir = tempfile::tempdir().unwrap();
    let options = ExportOptions {
        planes: true,
        ..ExportOptions::default()
    };
    let mut events = Vec::new();
    let summary =
        export_with_progress(&engine, dir.path(), &options, &mut |ev| events.push(ev)).unwrap();

    let (views, tiles_per_view) = events
        .iter()
        .find_map(|ev| match ev {
            exporter::ExportEvent::Planned {
                views,
                tiles_per_view,
            } => Some((*views, *tiles_per_view)),
            _ => None,
        })
        .expect("a Planned event must be reported");
    assert_eq!(views, 2);
    assert_eq!(
        tiles_per_view, 1361,
        "1024 + 256 + 64 + 16 grid tiles, plus one fallback"
    );
    let planned_total = views * tiles_per_view;
    assert_eq!(planned_total, 2722);
    assert_eq!(
        summary.tiles, planned_total,
        "the planned total before rendering must match the tiles actually written"
    );

    let mut found = std::collections::HashSet::new();
    walk_default_jpgs(dir.path(), &mut found);
    assert_eq!(
        found.len(),
        planned_total,
        "the planned and written totals must also match what actually landed on disk"
    );
}
