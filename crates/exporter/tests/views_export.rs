//! Multi-view exports end to end, against real fixtures.
//!
//! `sample_multidim.ome.zarr` has 5 z-planes (default z = 2) and no labels. `sample_labels.ome.zarr`
//! has one z-plane and one label, `nuclei`. Between them every tree kind is written: the root,
//! plane trees, and overlay trees. `sample_planes_labels.ome.zarr` has 4 z-planes AND one label,
//! `cells`, spanning every plane: the one fixture where a plane tree and an overlay tree combine,
//! used where the two need to be walked together rather than one at a time.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use exporter::{
    enumerate_request_space, export, manifest_json, plan_views, tree_id, ExportError,
    ExportOptions, ExportSummary, ViewSelection, DEFAULT_OVERLAY_OPACITY,
};
use iiif::assert_info_json_conforms;
use tiling::{TileEngine, ZarrTileEngine};
use zarr_core::ZarrImage;

const MULTIDIM: &str = "../../tests/fixtures/sample_multidim.ome.zarr";
const LABELS: &str = "../../tests/fixtures/sample_labels.ome.zarr";
const PLANES_LABELS: &str = "../../tests/fixtures/sample_planes_labels.ome.zarr";
const MULTI_TILE: &str = "../../tests/fixtures/sample_multi_tile.ome.zarr";
const NO_PYRAMID: &str = "../../tests/fixtures/sample_no_pyramid.ome.zarr";
const HUGE_LEVEL: &str = "../../tests/fixtures/sample_huge_level.ome.zarr";
const V04: &str = "../../tests/fixtures/sample_v04.ome.zarr";
const UNPINNABLE: &str = "../../tests/fixtures/sample_unpinnable.ome.zarr";

fn engine(fixture: &str) -> ZarrTileEngine {
    ZarrTileEngine::new(ZarrImage::open(fixture).unwrap())
}

fn export_to(e: &ZarrTileEngine, options: &ExportOptions) -> (tempfile::TempDir, ExportSummary) {
    let dir = tempfile::tempdir().unwrap();
    let summary = export(e, dir.path(), options).unwrap();
    (dir, summary)
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn tree_dir(root: &Path, folder: &str) -> PathBuf {
    if folder == "." {
        root.to_path_buf()
    } else {
        root.join(folder)
    }
}

fn walk_jpegs(dir: &Path, out: &mut HashSet<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk_jpegs(&path, out);
        } else if path.file_name().is_some_and(|n| n == "default.jpg") {
            out.insert(path);
        }
    }
}

fn selection(options: &ExportOptions) -> ViewSelection {
    ViewSelection {
        planes: options.planes,
        labels: options.labels,
        overlay_opacity: options.overlay_opacity,
    }
}

/// The `(width, height)` a JPEG file actually decodes to, read from its own SOF0/SOF2 marker.
///
/// No JPEG *decoder* is a dependency anywhere in this workspace (`jpeg-encoder` only encodes; see
/// `Cargo.toml`), so this reads just the handful of header bytes that carry the dimensions rather
/// than pulling one in for a check that never needs actual pixels. JPEG is a sequence of marker
/// segments after the SOI (`FFD8`); each non-standalone marker is followed by a 2-byte big-endian
/// length (including itself) and that many bytes of payload. The "Start Of Frame" markers
/// (`FFC0`-`FFC3`, `FFC5`-`FFC7`, `FFC9`-`FFCB`, `FFCD`-`FFCF` — `FFC4`/`FFC8`/`FFCC` are NOT SOF,
/// they are DHT/JPG-extension/DAC) carry `[precision:1][height:2 BE][width:2 BE][...]` as their
/// payload; every encoder this workspace could plausibly produce output from writes exactly one.
fn jpeg_pixel_dimensions(bytes: &[u8]) -> (u64, u64) {
    assert_eq!(
        &bytes[0..2],
        [0xFF, 0xD8],
        "not a JPEG (missing SOI marker)"
    );
    let mut i = 2;
    while i + 4 <= bytes.len() {
        assert_eq!(
            bytes[i], 0xFF,
            "malformed JPEG: expected a marker at offset {i}"
        );
        let marker = bytes[i + 1];
        // Standalone markers carry no length/payload of their own.
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        let seg_len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        let is_sof = matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF);
        if is_sof {
            let height = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u64;
            let width = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u64;
            return (width, height);
        }
        i += 2 + seg_len;
    }
    panic!("no SOF marker found in JPEG");
}

/// The level 0 contract: every size `info.json` advertises, and `max`, must be on disk — AND must
/// actually decode to the dimensions it claims. This is the assertion whose absence let an export
/// declare `level0` while `full/max` 404ed, then (once that was fixed) let `full/max` exist but
/// silently hold the wrong resolution: `sample_v04.ome.zarr`/`sample_labels.ome.zarr` declared
/// `maxWidth: 32` while `full/max` decoded as 64x64, because a level whose grid is a single tile
/// gets `full/max` from OpenSeadragon's own real request, at native resolution, regardless of
/// whatever bound `sizes` happens to declare.
fn assert_every_advertised_size_exists(root: &Path, tree: &str) {
    let dir = tree_dir(root, tree);
    let info = read_json(&dir.join("info.json"));
    let max_path = dir.join("full/max/0/default.jpg");
    assert!(
        max_path.exists(),
        "{tree}: full/max must exist for a level0 profile"
    );
    let max_bytes = fs::read(&max_path).unwrap();
    assert!(!max_bytes.is_empty(), "{tree}: full/max must not be empty");

    // `full/max` must be `(maxWidth, maxHeight)` when a bound is declared (IIIF requires `max` to
    // respect it), or the literal `(width, height)` when there is none (D9's "over budget, no
    // whole images, no bound" degradation is untouched by this check: it has no `full/max` at
    // all, so `assert_every_advertised_size_exists` is never called for that case).
    let want_max = match (info.get("maxWidth"), info.get("maxHeight")) {
        (Some(w), Some(h)) => (w.as_u64().unwrap(), h.as_u64().unwrap()),
        _ => (
            info["width"].as_u64().unwrap(),
            info["height"].as_u64().unwrap(),
        ),
    };
    assert_eq!(
        jpeg_pixel_dimensions(&max_bytes),
        want_max,
        "{tree}: full/max must decode to {want_max:?}"
    );

    for size in info["sizes"].as_array().unwrap() {
        let (w, h) = (
            size["width"].as_u64().unwrap(),
            size["height"].as_u64().unwrap(),
        );
        let path = dir.join(format!("full/{w},{h}/0/default.jpg"));
        assert!(
            path.exists(),
            "{tree}: sizes advertises {w}x{h} but {path:?} is missing"
        );
        let bytes = fs::read(&path).unwrap();
        assert_eq!(
            jpeg_pixel_dimensions(&bytes),
            (w, h),
            "{tree}: full/{w},{h} must decode to {w}x{h}"
        );
    }
}

/// The complete set of relative paths ONE tree holds: `enumerate_request_space`'s own output,
/// plus the level 0 contract's whole-image derivatives and `full/max` when the pyramid is within
/// budget — mirrors `writer::write_tree`'s own file set (via the public `iiif`/`exporter` API
/// only, not the writer's internals) closely enough to assert an export has no orphans and no
/// missing file, without hard-coding per-fixture counts by hand.
fn expected_tree_relpaths(info: &iiif::ImageInfo) -> HashSet<String> {
    let mut paths: HashSet<String> = enumerate_request_space(info)
        .iter()
        .map(|r| r.relative_path())
        .collect();
    if let Some(plan) = iiif::level0_sizes(info) {
        if plan.within_budget() {
            for &(w, h) in &plan.sizes {
                paths.insert(format!("full/{w},{h}/0/default.jpg"));
            }
            paths.insert("full/max/0/default.jpg".to_string());
        }
    }
    paths
}

/// The level 0 contract holds for a plain, single-view export of every kind of committed
/// fixture: an image with extra dims (`MULTIDIM`), one with a label (`LABELS`), a genuinely
/// multi-tile pyramid whose `sizes` gets trimmed (`MULTI_TILE`), and a single, untrimmed level
/// bigger than the tile size (`NO_PYRAMID`).
#[test]
fn a_plain_export_serves_every_size_it_advertises() {
    for fixture in [MULTIDIM, LABELS, MULTI_TILE, NO_PYRAMID] {
        let (dir, _) = export_to(&engine(fixture), &ExportOptions::default());
        assert_every_advertised_size_exists(dir.path(), ".");
    }
}

/// The same contract, per TREE, across a real multi-view export: every plane and every overlay
/// tree is its own complete level0 service, not just the root.
#[test]
fn every_tree_of_a_multi_view_export_serves_every_size_it_advertises() {
    let e = engine(PLANES_LABELS);
    let options = ExportOptions {
        planes: true,
        labels: true,
        ..ExportOptions::default()
    };
    let (dir, summary) = export_to(&e, &options);
    let plan = plan_views(
        &e.dimensions(),
        &ViewSelection {
            planes: true,
            labels: true,
            overlay_opacity: DEFAULT_OVERLAY_OPACITY,
        },
    );
    assert_eq!(summary.views, plan.views.len());
    for view in &plan.views {
        assert_every_advertised_size_exists(dir.path(), &view.folder);
    }
}

/// D9's budget: `sample_huge_level.ome.zarr` is 100000x100000, one level, far past both
/// `MAX_WHOLE_IMAGE_EDGE` (JPEG's own dimension ceiling) and `MAX_WHOLE_IMAGE_PIXELS`. A real
/// export of this fixture cannot be run end to end to check what `write_tree` actually WRITES:
/// its one declared chunk spans the whole image, so even a single 512x512 tile read trips the
/// zarr reader's decompression-bomb guard before any tile ever renders (see
/// `crates/tiling/src/engine.rs`'s `full_region_on_huge_single_level_is_rejected_not_oom` for the
/// same fixture hitting the same guard). This test therefore only pins `info.json`'s shape and
/// the plan's own over-budget flag, against the exact `ImageInfo` a real export would build from
/// this fixture (metadata only, no chunk read, so it is safe to run): no bound, untrimmed `sizes`,
/// over budget. The "actually writes no whole image" half this test's old name claimed is proven
/// separately, through a fake `TileEngine` that CAN be driven through a real `write_tree` call at
/// this scale — see `tests/whole_image_budget.rs`.
#[test]
fn an_export_above_the_whole_image_budget_declares_no_bound() {
    let info = engine(HUGE_LEVEL).image_info(".");
    let v = info.to_info_json_level0();
    assert!(v.get("maxWidth").is_none(), "over budget: no maxWidth");
    assert!(v.get("maxHeight").is_none(), "over budget: no maxHeight");
    assert_eq!(
        v["sizes"],
        serde_json::json!([{"width": 100_000, "height": 100_000}]),
        "over budget: sizes stays untrimmed"
    );

    let plan = iiif::level0_sizes(&info).expect("a single, untrimmed level always reconstructs");
    assert!(
        !plan.within_budget(),
        "this fixture is the one over the whole-image budget"
    );
}

/// Completeness for every tree at once: every enumerated file of every tree exists, and the
/// export holds no tile that belongs to no tree. Run with BOTH flags, against the one fixture
/// that has both z-planes and a label spanning them, so `assert_info_json_conforms` actually
/// covers every overlay tree, not just the plane trees (`docs/conformance.md`, `docs/labels.md`).
#[test]
fn every_plane_is_a_complete_conforming_tree_with_no_orphans() {
    let e = engine(PLANES_LABELS);
    let options = ExportOptions {
        planes: true,
        labels: true,
        ..ExportOptions::default()
    };
    let (dir, summary) = export_to(&e, &options);
    let plan = plan_views(&e.dimensions(), &selection(&options));
    assert_eq!(summary.views, 8, "4 planes x (1 intensity + 1 overlay)");

    let per_tree = expected_tree_relpaths(&e.image_info("."));
    let mut expected = HashSet::new();
    for view in &plan.views {
        let tree = tree_dir(dir.path(), &view.folder);
        let info = read_json(&tree.join("info.json"));
        let violations = assert_info_json_conforms(&info);
        assert!(violations.is_empty(), "{}: {violations:#?}", view.folder);
        for relpath in &per_tree {
            expected.insert(tree.join(relpath));
        }
    }
    let mut found = HashSet::new();
    walk_jpegs(dir.path(), &mut found);
    assert_eq!(found, expected, "orphan or missing tiles");
    assert_eq!(summary.tiles, expected.len());
}

/// Each tree holds its own projection's pixels: the first enumerated tile of every tree is
/// byte-identical to rendering that tile directly.
#[test]
fn every_tree_renders_its_own_projection() {
    for (fixture, options) in [
        (
            MULTIDIM,
            ExportOptions {
                planes: true,
                ..ExportOptions::default()
            },
        ),
        (
            LABELS,
            ExportOptions {
                labels: true,
                ..ExportOptions::default()
            },
        ),
    ] {
        let e = engine(fixture);
        let (dir, _) = export_to(&e, &options);
        let first = enumerate_request_space(&e.image_info(".")).remove(0);
        for view in plan_views(&e.dimensions(), &selection(&options)).views {
            let on_disk =
                fs::read(tree_dir(dir.path(), &view.folder).join(first.relative_path())).unwrap();
            let direct = e
                .tile(&view.projection, first.region, first.size, 85)
                .unwrap();
            assert_eq!(on_disk, direct, "{fixture} {}", view.folder);
        }
    }
}

#[test]
fn labels_overlay_the_default_plane_when_planes_are_not_asked_for() {
    let e = engine(LABELS);
    let (dir, summary) = export_to(
        &e,
        &ExportOptions {
            labels: true,
            ..ExportOptions::default()
        },
    );
    assert_eq!(summary.views, 2);
    assert!(dir.path().join("planes/0/labels/0/info.json").exists());
    assert!(
        !dir.path().join("planes/0/info.json").exists(),
        "the default plane stays at the root"
    );
}

#[test]
fn metadata_matches_the_server_and_names_only_real_trees() {
    let e = engine(LABELS);
    let (dir, _) = export_to(
        &e,
        &ExportOptions {
            labels: true,
            ..ExportOptions::default()
        },
    );

    assert_eq!(
        read_json(&dir.path().join("ziv/dimensions.json")),
        e.dimensions().to_json()
    );

    let views = read_json(&dir.path().join("ziv/views.json"));
    assert_eq!(
        views,
        serde_json::json!({
            "version": 1,
            "defaultZ": 0,
            "planes": {"0": "."},
            "labels": [{
                "index": 0, "name": "nuclei", "palette": "distinct", "opacity": 0.6,
                "planes": {"0": "planes/0/labels/0"}
            }]
        })
    );
    // Every folder the just-verified `views.json` names is a real tree: walked from the parsed
    // document itself, not restated as a literal, so this stays true if the fixture grows more
    // labels or planes.
    for label in views["labels"].as_array().unwrap() {
        for folder in label["planes"].as_object().unwrap().values() {
            let folder = folder.as_str().unwrap();
            assert!(
                dir.path().join(folder).join("info.json").exists(),
                "{folder}"
            );
        }
    }
}

#[test]
fn a_plain_export_still_writes_metadata_but_no_extra_trees() {
    let e = engine(MULTIDIM);
    let (dir, summary) = export_to(&e, &ExportOptions::default());
    assert_eq!(summary.views, 1);
    assert!(!dir.path().join("planes").exists());
    let views = read_json(&dir.path().join("ziv/views.json"));
    assert_eq!(views["planes"], serde_json::json!({"2": "."}));
    assert_eq!(views["labels"], serde_json::json!([]));
}

/// Every one of the 5 trees this export writes (root + 4 non-default planes), not just two of
/// them: widened from checking only the root and `planes/0` so the test name is true.
#[test]
fn an_absolute_id_reaches_every_tree() {
    let e = engine(MULTIDIM);
    let options = ExportOptions {
        id: "https://example.org/iiif/md".to_string(),
        planes: true,
        ..ExportOptions::default()
    };
    let (dir, _) = export_to(&e, &options);
    let plan = plan_views(&e.dimensions(), &selection(&options));
    assert_eq!(
        plan.views.len(),
        5,
        "every z-plane of MULTIDIM is its own tree"
    );
    for view in &plan.views {
        let expected = tree_id(&options.id, &view.folder);
        assert_eq!(
            read_json(&tree_dir(dir.path(), &view.folder).join("info.json"))["id"],
            expected,
            "{}",
            view.folder
        );
    }
}

#[test]
fn an_out_of_range_opacity_is_refused_before_anything_is_written() {
    // Documented as checked whether or not `--labels` is set (`ExportOptions::overlay_opacity`),
    // so both must be refused: guarding the check behind `options.labels` would still pass the
    // `labels: true` case alone.
    for labels in [true, false] {
        let e = engine(LABELS);
        let dir = tempfile::tempdir().unwrap();
        let options = ExportOptions {
            labels,
            overlay_opacity: 1.5,
            ..ExportOptions::default()
        };
        let err = export(&e, dir.path(), &options).unwrap_err();
        // The flag name prefixes the underlying message exactly once: no restating "out of
        // range" a second time around `tiling::parse_opacity`'s own "... out of range; expected
        // 0 to 1".
        assert_eq!(
            err.to_string(),
            "--overlay-opacity: label opacity 1.5 out of range; expected 0 to 1",
            "labels={labels}"
        );
        assert!(!dir.path().join("info.json").exists(), "labels={labels}");
    }
}

fn all_ids(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(id)) = map.get("id") {
                out.push(id.clone());
            }
            map.values().for_each(|v| all_ids(v, out));
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| all_ids(v, out)),
        _ => {}
    }
}

fn bodies(canvas: &serde_json::Value) -> Vec<serde_json::Value> {
    let body = &canvas["items"][0]["items"][0]["body"];
    if body["type"] == "Choice" {
        body["items"].as_array().unwrap().clone()
    } else {
        vec![body.clone()]
    }
}

#[test]
fn a_plain_export_writes_no_manifest() {
    let (dir, _) = export_to(&engine(MULTIDIM), &ExportOptions::default());
    assert!(!dir.path().join("manifest.json").exists());
}

/// The level 0 contract is unconditional, not tied to whether a manifest happens to exist:
/// `sample_no_pyramid.ome.zarr` is the one committed fixture whose sole level has no whole-image
/// derivative of its own (every other fixture's smallest level fits in one tile), so it is the
/// fixture that used to expose a plain, single-view export shipping a `level0` profile whose own
/// `full/max` 404ed. A plain export of it (one view, no `--planes`, no `--labels`) still writes no
/// `manifest.json` — but `full/max/0/default.jpg` MUST exist regardless, and its file set is
/// exactly the enumerated request space plus the level 0 contract's own additions, no more.
#[test]
fn a_plain_export_of_a_no_pyramid_image_writes_full_max_with_no_manifest() {
    let e = engine(NO_PYRAMID);
    let (dir, _) = export_to(&e, &ExportOptions::default());
    assert!(!dir.path().join("manifest.json").exists());
    assert_every_advertised_size_exists(dir.path(), ".");

    let expected: HashSet<PathBuf> = expected_tree_relpaths(&e.image_info("."))
        .iter()
        .map(|relpath| dir.path().join(relpath))
        .collect();
    let mut found = HashSet::new();
    walk_jpegs(dir.path(), &mut found);
    assert_eq!(found, expected, "orphan or missing tile files");
}

#[test]
fn the_manifest_has_one_canvas_per_plane_whose_bodies_exist() {
    let e = engine(MULTIDIM);
    let options = ExportOptions {
        planes: true,
        ..ExportOptions::default()
    };
    let (dir, summary) = export_to(&e, &options);
    let plan = plan_views(&e.dimensions(), &selection(&options));
    let manifest = read_json(&dir.path().join("manifest.json"));
    assert_eq!(manifest["type"], "Manifest");
    let canvases = manifest["items"].as_array().unwrap();
    assert_eq!(canvases.len(), 5);
    // MULTIDIM has no labels, so `plan.views` is one-per-plane, in the same ascending-z order
    // `manifest_json` builds canvases in: zipping ties canvas `i` to ITS OWN plane's view.
    for (canvas, view) in canvases.iter().zip(&plan.views) {
        for body in bodies(canvas) {
            let id = body["id"].as_str().unwrap();
            assert!(
                dir.path().join(id).exists(),
                "body {id} must name a file on disk"
            );
            // Ties the body to ITS OWN plane's folder, not merely to SOME real file: a manifest
            // that dropped `folder` when spelling ids (e.g. `Ids::in_tree` ignoring it) would
            // point every plane's body at the root's file instead — still a real, existing file
            // (the root's), so the plain existence check above cannot tell the difference.
            if view.folder == "." {
                assert!(
                    !id.contains("planes/"),
                    "root plane's body {id} must not name a planes/ file"
                );
            } else {
                assert!(
                    id.starts_with(&format!("{}/", view.folder)),
                    "body {id} must live under its own plane's folder {}",
                    view.folder
                );
            }
        }
    }
    assert!(
        summary.warnings.iter().any(|w| w.contains("relative ids")),
        "{:?}",
        summary.warnings
    );
}

/// Mirador (and any other spec-following IIIF 3 client) opens a manifest at `start`'s canvas, not
/// canvas 1: without it a viewer opens on z=0, which for a real IDR image can be far from the
/// nuclei/signal (see `manifest.rs`'s doc comment). `sample_planes_labels.ome.zarr` has 4
/// z-planes and `default_projection` picks the middle one (`size_z / 2` = 2), so this fixture
/// only passes if `start` genuinely tracks the image's default plane rather than always z=0.
#[test]
fn the_manifest_start_points_at_the_default_planes_canvas() {
    let e = engine(PLANES_LABELS);
    let dims = e.dimensions();
    assert_eq!(dims.default_z, 2, "fixture assumption: middle of 4 planes");
    let options = ExportOptions {
        planes: true,
        ..ExportOptions::default()
    };
    let (dir, _) = export_to(&e, &options);
    let manifest = read_json(&dir.path().join("manifest.json"));

    let start = &manifest["start"];
    assert_eq!(start["type"], "Canvas", "{manifest:#}");
    let start_id = start["id"].as_str().expect("start.id must be a string");

    let canvases = manifest["items"].as_array().unwrap();
    let canvas_ids: Vec<&str> = canvases.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert!(
        canvas_ids.contains(&start_id),
        "start {start_id:?} must name a canvas that exists in items: {canvas_ids:?}"
    );
    assert_eq!(start_id, format!("canvas/z{}", dims.default_z));
}

#[test]
fn overlays_are_choices_on_their_plane() {
    let e = engine(LABELS);
    let (dir, _) = export_to(
        &e,
        &ExportOptions {
            labels: true,
            ..ExportOptions::default()
        },
    );
    let manifest = read_json(&dir.path().join("manifest.json"));
    let canvas = &manifest["items"][0];
    assert_eq!(canvas["items"][0]["items"][0]["body"]["type"], "Choice");
    let bodies = bodies(canvas);
    assert_eq!(bodies.len(), 2);
    // A `Choice` never pairs a named option with a blank one (spec §6.2 amendment): the plain
    // intensity body is labelled too, not just the overlay.
    assert_eq!(
        bodies[0]["label"],
        serde_json::json!({"none": ["intensity"]})
    );
    assert_eq!(
        bodies[1]["label"],
        serde_json::json!({"none": ["nuclei overlay"]})
    );
    assert_eq!(bodies[1]["service"][0]["id"], "planes/0/labels/0");
}

#[test]
fn an_absolute_id_makes_every_manifest_id_absolute_and_silences_the_warning() {
    let e = engine(MULTIDIM);
    let base = "https://example.org/iiif/md";
    let options = ExportOptions {
        id: base.to_string(),
        planes: true,
        ..ExportOptions::default()
    };
    let (dir, summary) = export_to(&e, &options);
    let manifest = read_json(&dir.path().join("manifest.json"));
    let mut ids = Vec::new();
    all_ids(&manifest, &mut ids);
    assert!(!ids.is_empty());
    for id in &ids {
        assert!(id.starts_with(base), "{id}");
        let local = id.trim_start_matches(base).trim_start_matches('/');
        if local.ends_with("default.jpg") {
            assert!(dir.path().join(local).exists(), "{id}");
        }
    }
    assert!(!summary.warnings.iter().any(|w| w.contains("relative ids")));
}

/// A 1024x1024 image has no `full/max`: only levels that fit in one tile get a whole-image file.
///
/// Calls `manifest_json` directly rather than going through a real export: this fixture has one
/// z-plane, so under the `plan.views.len() > 1` gate (writer.rs) a real `--planes` export of it no
/// longer writes a manifest at all. Calling `manifest_json` with a plan built for it directly
/// still pins the largest-whole-image selection against a real fixture's `ImageInfo`, not just the
/// enumerator's own synthetic unit tests.
#[test]
fn a_large_image_points_its_body_at_the_largest_whole_image_file() {
    let e = engine("../../tests/fixtures/sample_multi_tile.ome.zarr");
    let dims = e.dimensions();
    let options = ExportOptions {
        planes: true,
        ..ExportOptions::default()
    };
    let plan = plan_views(&dims, &selection(&options));
    let (manifest, _warnings) = manifest_json(&plan, &dims, &e.image_info("."), ".", "image");
    let body = &bodies(&manifest["items"][0])[0];
    assert_eq!(body["id"], "full/512,512/0/default.jpg");
    assert_eq!(
        (body["width"].as_u64(), body["height"].as_u64()),
        (Some(512), Some(512))
    );
}

/// `sample_no_pyramid.ome.zarr` has one level (no downsampled dataset) bigger than the fixed 512px
/// tile size, so `enumerate_request_space` never reports a `full/...` derivative for it at all.
/// This image is within the whole-image budget, though, so `manifest.rs`'s body choice never looks
/// at `enumerate_request_space` in the first place: it names `full/{maxWidth},{maxHeight}`
/// directly, from the same `iiif::level0_sizes`/`within_budget` decision `write_tree`'s level 0
/// contract used to decide what to write, which is `full/600,600` here (not `full/max`, even
/// though for this single, untrimmed level both would be the same image) — proving the body names
/// a file the writer actually wrote even when OpenSeadragon's own request space has nothing of the
/// kind to have found on its own. Two z-planes (`--planes`) so the export's plan has more than one
/// view and the manifest gate actually fires.
#[test]
fn a_no_pyramid_image_gets_a_written_whole_image_file_for_its_manifest_body() {
    let e = engine("../../tests/fixtures/sample_no_pyramid.ome.zarr");
    let (dir, _) = export_to(
        &e,
        &ExportOptions {
            planes: true,
            ..ExportOptions::default()
        },
    );
    let manifest = read_json(&dir.path().join("manifest.json"));
    let canvases = manifest["items"].as_array().unwrap();
    assert_eq!(canvases.len(), 2);
    for canvas in canvases {
        let body = &bodies(canvas)[0];
        let id = body["id"].as_str().unwrap();
        assert!(id.ends_with("full/600,600/0/default.jpg"), "{id}");
        assert_eq!(
            (body["width"].as_u64(), body["height"].as_u64()),
            (Some(600), Some(600))
        );
        assert!(
            dir.path().join(id).exists(),
            "body {id} must name a file the writer actually wrote"
        );
    }
}

/// The level 0 contract does not just make the manifest body's file EXIST: within budget, that
/// file must actually decode to the size the body itself declares (`width`/`height`), the same
/// property `assert_every_advertised_size_exists` already pins for `info.json`'s own `sizes`.
/// `PLANES_LABELS` with both flags exercises both a plain body and a `Choice` (intensity + overlay)
/// body on the same canvas, so every body shape this manifest ever produces is covered here, not
/// just the simple one-body-per-canvas case.
#[test]
fn every_manifest_body_resolves_to_its_declared_pixel_size() {
    let e = engine(PLANES_LABELS);
    let (dir, _) = export_to(
        &e,
        &ExportOptions {
            planes: true,
            labels: true,
            ..ExportOptions::default()
        },
    );
    let manifest = read_json(&dir.path().join("manifest.json"));
    let canvases = manifest["items"].as_array().unwrap();
    assert!(!canvases.is_empty());
    for canvas in canvases {
        for body in bodies(canvas) {
            let id = body["id"].as_str().unwrap();
            let path = dir.path().join(id);
            assert!(path.exists(), "body {id} must name a file on disk");
            let (w, h) = (
                body["width"].as_u64().unwrap(),
                body["height"].as_u64().unwrap(),
            );
            let bytes = fs::read(&path).unwrap();
            assert_eq!(
                jpeg_pixel_dimensions(&bytes),
                (w, h),
                "body {id} declares {w}x{h} but its file decodes to a different size"
            );
        }
    }
}

#[test]
fn the_manifest_label_is_the_export_options_name() {
    let e = engine(MULTIDIM);
    let (dir, _) = export_to(
        &e,
        &ExportOptions {
            planes: true,
            name: "My Image".to_string(),
            ..ExportOptions::default()
        },
    );
    let manifest = read_json(&dir.path().join("manifest.json"));
    assert_eq!(manifest["label"], serde_json::json!({"none": ["My Image"]}));
}

/// An unpyramided image cannot drop its only advertised size, so the whole image is written at
/// full resolution. That is the expensive case, and the operator is told rather than left to
/// discover it from the directory size.
#[test]
fn an_unpyramided_image_warns_that_it_writes_a_full_resolution_whole_image() {
    let (_dir, summary) = export_to(&engine(NO_PYRAMID), &ExportOptions::default());
    assert!(
        summary
            .warnings
            .iter()
            .any(|w| w.contains("full resolution")),
        "{:?}",
        summary.warnings
    );
}

/// A trimmed pyramid (`MULTI_TILE`, OSD reconstructs the dropped entry on its own) and untrimmed
/// pyramids whose finest level already fits one tile (`V04`, `LABELS`: OSD already requests that
/// whole image, so writing it is free) cost nothing extra and must not warn about it. Each
/// fixture's plain export has exactly one view, so no manifest is written either, and
/// `summary.warnings` has no other source to populate it from.
#[test]
fn a_trimmed_or_cheaply_untrimmed_pyramid_has_no_level0_cost_warning() {
    for fixture in [MULTI_TILE, V04, LABELS] {
        let (_dir, summary) = export_to(&engine(fixture), &ExportOptions::default());
        assert!(
            summary.warnings.is_empty(),
            "{fixture}: {:?}",
            summary.warnings
        );
    }
}

/// A pyramid OpenSeadragon cannot pin is refused before anything is written, rather than
/// producing a tree whose own viewer cannot read it.
#[test]
fn a_pyramid_that_cannot_be_pinned_is_refused_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let err = export(&engine(UNPINNABLE), &out, &ExportOptions::default()).unwrap_err();
    assert!(
        matches!(err, ExportError::UnsupportedPyramid { .. }),
        "{err:?}"
    );
    // The operator is told what to do about it, not just that it failed: the image is still
    // reachable dynamically.
    assert!(err.to_string().contains("ziv serve"), "{err}");
    assert!(
        !out.exists(),
        "nothing may be written when the pyramid is refused"
    );
}
