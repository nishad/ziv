//! End-to-end exporter tests against a REAL multi-tile pyramid (`sample_multi_tile.ome.zarr`,
//! built by `build_multi_tile_fixture.rs`: 1024x1024 level 0 -> a genuine 2x2 tile grid at
//! `ZarrTileEngine`'s fixed 512px tile size, 512x512 level 1 -> a single-cell grid at the
//! `<=` boundary).
//!
//! Every other committed fixture is <=64x64, so the writer/DZI `#[cfg(test)]` suites in
//! `src/writer.rs`/`src/dzi.rs` only ever exercise the "fits in one tile" enumerator branch
//! through a REAL `TileEngine` — the multi-tile grid math (coordinate regions, canonical
//! `w,h` sizing, edge clipping) is otherwise covered only by `enumerate.rs`'s own unit tests
//! against synthetic `ImageInfo` values, never against a real zarr read. These tests close
//! that gap: completeness (every enumerated URL exists, no orphans) and a pixel-identity
//! spot-check now actually exercise the 2x2 grid end-to-end.
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use exporter::dzi::{enumerate_dzi_tiles, export_dzi};
use exporter::{enumerate_request_space, export, ExportOptions};
use iiif::{ProjectionId, Region, Size};
use tiling::{TileEngine, ZarrTileEngine};
use zarr_core::ZarrImage;

fn engine() -> ZarrTileEngine {
    let img = ZarrImage::open("../../tests/fixtures/sample_multi_tile.ome.zarr").unwrap();
    ZarrTileEngine::new(img)
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

/// Sanity check on the fixture itself: level 0 is genuinely multi-tile (2x2 grid at the
/// 512px tile size), level 1 sits exactly at the single-cell boundary.
#[test]
fn fixture_has_a_real_multi_tile_pyramid() {
    let e = engine();
    let info = e.image_info(".");
    assert_eq!(info.width, 1024);
    assert_eq!(info.height, 1024);
    assert_eq!(info.tile_size, 512);
    assert_eq!(info.scale_factors, vec![1, 2]);
    assert_eq!(info.sizes, vec![(1024, 1024), (512, 512)]);

    let reqs = enumerate_request_space(&info);
    let level0_tiles = reqs
        .iter()
        .filter(|r| r.size_str == "512,512" && r.region_str != "full")
        .count();
    assert_eq!(level0_tiles, 4, "level 0 must produce a 2x2 tile grid");
}

/// EXPORTER COMPLETENESS over a real multi-tile grid: every enumerated URL exists on disk
/// AND there are no orphan files — now actually exercising the 2x2 grid at level 0, which
/// the 64x64 single-tile fixture never touches.
#[test]
fn multi_tile_export_is_complete_with_no_orphans() {
    let e = engine();
    let dir = tempfile::tempdir().unwrap();
    let count = export(&e, dir.path(), &ExportOptions::default())
        .unwrap()
        .tiles;
    assert!(count > 0);

    let info = e.image_info(".");
    let requests = enumerate_request_space(&info);
    // The level 0 contract (`write_tree`'s own doc comment) adds `full/max` on top of the bare
    // enumerated set for this fixture: its finest level never fits in one tile, so
    // `enumerate_request_space` alone has no whole-image entry to serve it, and its trimmed
    // `sizes` (`[(512, 512)]`) has no OTHER entry beyond the one the tiled branch's own boundary
    // case already writes (`full/512,512`, asserted below). Hard-coded, not recomputed via the
    // writer's own logic, so this stays an independent check of the fixture's real shape rather
    // than a tautology that would pass even if that logic itself were wrong.
    let level0_extra_relpaths: HashSet<String> = ["full/max/0/default.jpg".to_string()].into();
    assert_eq!(requests.len() + level0_extra_relpaths.len(), count);

    // Must include level 0's full 2x2 grid (not just single-tile levels).
    let level0_dirs = [
        "0,0,512,512",
        "512,0,512,512",
        "0,512,512,512",
        "512,512,512,512",
    ];
    for d in level0_dirs {
        let expected = dir.path().join(format!("{d}/512,512/0/default.jpg"));
        assert!(
            expected.exists(),
            "expected multi-tile grid file missing: {expected:?}"
        );
    }

    let mut expected_files: HashSet<PathBuf> = requests
        .iter()
        .map(|r| dir.path().join(r.relative_path()))
        .collect();
    for relpath in &level0_extra_relpaths {
        expected_files.insert(dir.path().join(relpath));
    }
    expected_files.insert(dir.path().join("info.json"));
    expected_files.insert(dir.path().join("index.html"));
    for f in &expected_files {
        assert!(f.exists(), "expected exported file missing: {f:?}");
    }

    let mut found_tiles: HashSet<PathBuf> = HashSet::new();
    walk_jpegs(dir.path(), &mut found_tiles);
    let mut expected_tiles: HashSet<PathBuf> = requests
        .iter()
        .map(|r| dir.path().join(r.relative_path()))
        .collect();
    for relpath in &level0_extra_relpaths {
        expected_tiles.insert(dir.path().join(relpath));
    }
    assert_eq!(
        found_tiles, expected_tiles,
        "orphan or missing tile files in multi-tile export"
    );
}

/// PIXEL IDENTITY on a MULTI-TILE tile: an exported interior grid tile at level 0 (region
/// `512,0,512,512`, the top-right cell of the 2x2 grid) has bytes == a direct
/// `engine.tile(same region, same size, 85)` call.
#[test]
fn multi_tile_grid_tile_bytes_match_direct_engine_tile_call() {
    let e = engine();
    let dir = tempfile::tempdir().unwrap();
    export(&e, dir.path(), &ExportOptions::default()).unwrap();

    let region = Region::Px {
        x: 512,
        y: 0,
        w: 512,
        h: 512,
    };
    let size = Size::Wh(512, 512);
    let direct = e.tile(&ProjectionId::Default, region, size, 85).unwrap();
    let on_disk = fs::read(dir.path().join("512,0,512,512/512,512/0/default.jpg")).unwrap();
    assert_eq!(direct, on_disk);
}

/// Same pixel-identity check on a DIFFERENT grid cell (bottom-left), to confirm the match
/// isn't a coincidence of one particular tile.
#[test]
fn multi_tile_grid_bottom_left_tile_bytes_match_direct_engine_tile_call() {
    let e = engine();
    let dir = tempfile::tempdir().unwrap();
    export(&e, dir.path(), &ExportOptions::default()).unwrap();

    let region = Region::Px {
        x: 0,
        y: 512,
        w: 512,
        h: 512,
    };
    let size = Size::Wh(512, 512);
    let direct = e.tile(&ProjectionId::Default, region, size, 85).unwrap();
    let on_disk = fs::read(dir.path().join("0,512,512,512/512,512/0/default.jpg")).unwrap();
    assert_eq!(direct, on_disk);
}

/// FIX2 end-to-end: the exported tree must NOT contain a `full/1024,1024` whole-image
/// derivative, since level 0 (the finest level) is genuinely multi-tile (2x2 grid) and OSD's
/// tile grid never requests one for such a level. This is the real-engine counterpart to
/// `enumerate::tests::multi_tile_level_has_no_whole_image_derivative`.
///
/// `full/max` is a DIFFERENT matter, and the level 0 CONTRACT (not OSD's own request space)
/// requires it regardless: this fixture's `sizes` is trimmed to `[(512, 512)]` (the finest,
/// 1024x1024 entry is dropped — see `iiif::level0_sizes`), so `full/max` is a copy of
/// `full/512,512`, the largest size this tree actually advertises. This is precisely the
/// defect this crate's whole `honest-level0` plan exists to fix: before it, this tree declared
/// `"profile": "level0"` while `full/max` 404ed.
#[test]
fn multi_tile_export_has_full_max_but_no_whole_image_derivative_for_the_finest_level() {
    let e = engine();
    let dir = tempfile::tempdir().unwrap();
    export(&e, dir.path(), &ExportOptions::default()).unwrap();

    assert!(!dir.path().join("full/1024,1024/0/default.jpg").exists());
    // Level 1 (512x512) IS a single-cell level (boundary case) and DOES get one.
    let largest = dir.path().join("full/512,512/0/default.jpg");
    assert!(largest.exists());

    let max_path = dir.path().join("full/max/0/default.jpg");
    assert!(max_path.exists(), "level0 requires full/max to resolve");
    assert_eq!(
        fs::read(&max_path).unwrap(),
        fs::read(&largest).unwrap(),
        "full/max must be the same bytes as the largest advertised size"
    );
}

/// Multi-tile DZI export: completeness (every enumerated DZI tile exists on disk) and a
/// pixel spot-check at the finest DZI level, which for a 1024x1024 image is tiled (512px
/// DZI tiles -> a 2x2 grid at the finest level, same shape as the IIIF level-0 grid).
#[test]
fn multi_tile_dzi_export_is_complete_with_pixel_spot_check() {
    let e = engine();
    let dir = tempfile::tempdir().unwrap();
    let count = export_dzi(&e, dir.path(), "sample", 512, 85).unwrap();
    assert!(count > 0);

    let tiles = enumerate_dzi_tiles(1024, 1024, 512);
    for t in &tiles {
        let path = dir.path().join(t.relative_path("sample_files"));
        assert!(path.exists(), "missing DZI tile: {path:?}");
    }

    let finest = exporter::dzi::max_level(1024, 1024);
    let finest_tiles: Vec<_> = tiles.iter().filter(|t| t.level == finest).collect();
    assert_eq!(
        finest_tiles.len(),
        4,
        "finest DZI level for a 1024x1024 image at 512px tiles must be a 2x2 grid"
    );

    // Pixel spot-check on one of the finest-level multi-tile cells.
    let cell = finest_tiles
        .iter()
        .find(|t| t.col == 1 && t.row == 0)
        .expect("expected finest-level tile at col=1,row=0");
    let on_disk = fs::read(dir.path().join(cell.relative_path("sample_files"))).unwrap();
    let direct = e
        .tile(&ProjectionId::Default, cell.region, cell.size, 85)
        .unwrap();
    assert_eq!(on_disk, direct);
}
