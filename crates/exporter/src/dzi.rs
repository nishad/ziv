//! Optional DeepZoom (DZI) export, sharing the same raster/tile engine as the IIIF export
//! (deliverable 7, spec §7.3). Behind `--dzi` on the CLI.
//!
//! DZI addresses tiles differently from IIIF but the underlying pixels are identical —
//! every DZI tile is produced by the same `engine.tile(region, size, quality)` call the
//! IIIF exporter and the live `serve` HTTP route use, just with DZI's own tile-grid
//! addressing and (inverted) level numbering:
//!
//! - **Level ladder:** DZI always has a FULL dyadic ladder from a 1x1 level 0 up to the
//!   full-resolution level `max_level = ceil(log2(max(fullW, fullH)))`. This is inverted
//!   from IIIF's `scaleFactors` (IIIF index 0 = finest); DZI level `max_level` = finest,
//!   DZI level `0` = coarsest (1x1 tail). The OME-Zarr pyramid itself almost never has
//!   this many levels (real pyramids stop well above 1x1) — for DZI levels the pyramid
//!   doesn't have a matching resolution for, `engine.tile`'s own `plan()` already picks
//!   the best available pyramid level and resamples down to whatever output size is
//!   requested, so no special-casing is needed here: every DZI level is produced by
//!   requesting a full-res-coordinate region at that level's dyadic output size, exactly
//!   like the IIIF enumerator's region math, just keyed by DZI's `2^(max_level-level)`
//!   scale instead of the pyramid's own `scale_factors`.
//! - **Overlap = 0** (IIIF has no overlap equivalent, so the shared raster path only
//!   supports the overlap-free case).
//! - **Tile addressing:** `{name}_files/{level}/{col}_{row}.jpg`, `{name}.dzi` XML
//!   descriptor with `Overlap="0"`, `TileSize`, `Format="jpg"`, `Size Width/Height`.

use std::fs;
use std::path::Path;

use iiif::{ProjectionId, Region, Size};
use rayon::prelude::*;
use tiling::TileEngine;

use crate::writer::ExportError;

/// Compute the DZI max level: `ceil(log2(max(width, height)))`. Level 0 is a 1x1 tail;
/// level `max_level` is full resolution.
pub fn max_level(width: u64, height: u64) -> u32 {
    let longest = width.max(height).max(1);
    if longest == 1 {
        0
    } else {
        (longest as f64).log2().ceil() as u32
    }
}

/// One DZI tile write target: the full-res region to render, the exact `(w,h)` output
/// size at this DZI level, and the `{level}/{col}_{row}.jpg` relative path.
#[derive(Debug, Clone, PartialEq)]
pub struct DziTile {
    pub level: u32,
    pub col: u64,
    pub row: u64,
    pub region: Region,
    pub size: Size,
}

impl DziTile {
    pub fn relative_path(&self, files_dir_name: &str) -> String {
        format!(
            "{}/{}/{}_{}.jpg",
            files_dir_name, self.level, self.col, self.row
        )
    }
}

/// Enumerate every DZI tile for every level 0..=max_level, given the full-res image
/// dimensions and DZI tile size (overlap always 0).
pub fn enumerate_dzi_tiles(width: u64, height: u64, tile_size: u64) -> Vec<DziTile> {
    let max_lvl = max_level(width, height);
    let mut out = Vec::new();
    for level in 0..=max_lvl {
        // DZI dyadic scale: level `max_lvl` = full res (sf=1), level 0 = 1x1 (sf =
        // 2^max_lvl). Using integer power-of-two division (not float) keeps this exact.
        let shift = max_lvl - level;
        let sf: u64 = 1u64 << shift;
        let level_w = width.div_ceil(sf).max(1);
        let level_h = height.div_ceil(sf).max(1);

        let cols = level_w.div_ceil(tile_size);
        let rows = level_h.div_ceil(tile_size);
        for row in 0..rows {
            for col in 0..cols {
                // Region in FULL-RES coords, same clipping rule as the IIIF enumerator:
                // tile origin scaled up by the level's downsample factor, width/height
                // clipped at the image edge.
                let iiif_tile_w = tile_size * sf;
                let iiif_tile_h = tile_size * sf;
                let rx = col * iiif_tile_w;
                let ry = row * iiif_tile_h;
                let rw = iiif_tile_w.min(width - rx);
                let rh = iiif_tile_h.min(height - ry);

                // Output size in LEVEL coords, clipped at the level edge (same as IIIF).
                let ow = tile_size.min(level_w - col * tile_size);
                let oh = tile_size.min(level_h - row * tile_size);

                out.push(DziTile {
                    level,
                    col,
                    row,
                    region: Region::Px {
                        x: rx,
                        y: ry,
                        w: rw,
                        h: rh,
                    },
                    size: Size::Wh(ow, oh),
                });
            }
        }
    }
    out
}

/// Render `{name}.dzi` descriptor XML.
pub fn dzi_descriptor_xml(width: u64, height: u64, tile_size: u64) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Image TileSize="{tile_size}" Overlap="0" Format="jpg" xmlns="http://schemas.microsoft.com/deepzoom/2008">
  <Size Width="{width}" Height="{height}"/>
</Image>
"#
    )
}

/// Validates a DZI `name` (from `--dzi-name` or equivalent) is a plain filename component, not
/// a path — rejects anything containing a path separator (`/` or `\`, so this also catches
/// Windows-style separators regardless of host OS) or a `..` component, which would otherwise let
/// `export_dzi`'s `out_dir.join(format!("{name}.dzi"))` / `out_dir.join(t.relative_path(...))`
/// escape `out_dir` entirely (e.g. `--dzi-name ../../etc/foo` writing outside the intended export
/// directory). `name` is operator-controlled (a CLI flag), not request-controlled, so this is a
/// low-severity hardening rather than a live attack path — but it's cheap and unambiguous to
/// reject outright rather than attempt to sanitize/truncate.
///
/// An empty name is also rejected (`out_dir.join("".dzi")` is a degenerate, almost-certainly-a-
/// mistake case, not a meaningful export target).
fn validate_dzi_name(name: &str) -> Result<(), ExportError> {
    let is_invalid = name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.split(['/', '\\']).any(|component| component == "..");
    if is_invalid {
        return Err(ExportError::InvalidDziName {
            name: name.to_string(),
        });
    }
    Ok(())
}

/// Write a DZI tree under `out_dir`: `{name}.dzi` + `{name}_files/{level}/{col}_{row}.jpg`.
/// Shares `engine.tile` with the IIIF export — every tile is byte-identical to what
/// `engine.tile(same region, same size, quality)` would produce directly.
pub fn export_dzi(
    engine: &(dyn TileEngine + Sync),
    out_dir: &Path,
    name: &str,
    tile_size: u64,
    quality: u8,
) -> Result<usize, ExportError> {
    validate_dzi_name(name)?;
    let info = engine.image_info(".");
    let (width, height) = (info.width, info.height);

    fs::create_dir_all(out_dir).map_err(|e| io_err(out_dir, e))?;
    let dzi_path = out_dir.join(format!("{name}.dzi"));
    fs::write(&dzi_path, dzi_descriptor_xml(width, height, tile_size))
        .map_err(|e| io_err(&dzi_path, e))?;

    let files_dir_name = format!("{name}_files");
    let tiles = enumerate_dzi_tiles(width, height, tile_size);

    // See `crate::ambient_runtime_handle`'s doc comment and `writer::write_tree`'s identical
    // guard: rayon's workers below have no tokio context of their own, so a remote-backed read's
    // `pollster::block_on` call needs the ambient runtime (captured here, on the calling thread)
    // re-entered inside each worker.
    let runtime_handle = crate::ambient_runtime_handle();
    let results: Vec<Result<(), ExportError>> = tiles
        .par_iter()
        .map(|t| {
            let _tokio_guard = runtime_handle.as_ref().map(tokio::runtime::Handle::enter);
            let bytes = engine
                .tile(&ProjectionId::Default, t.region, t.size, quality)
                .map_err(|source| ExportError::Tile {
                    relative_path: t.relative_path(&files_dir_name),
                    source,
                })?;
            let path = out_dir.join(t.relative_path(&files_dir_name));
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
            }
            fs::write(&path, &bytes).map_err(|e| io_err(&path, e))
        })
        .collect();
    for r in results {
        r?;
    }

    Ok(tiles.len())
}

fn io_err(path: &Path, source: std::io::Error) -> ExportError {
    ExportError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiling::ZarrTileEngine;
    use zarr_core::ZarrImage;

    fn engine() -> ZarrTileEngine {
        let img = ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
        ZarrTileEngine::new(img)
    }

    #[test]
    fn max_level_matches_ceil_log2_of_longest_side() {
        assert_eq!(max_level(1, 1), 0);
        assert_eq!(max_level(2, 1), 1);
        assert_eq!(max_level(1024, 768), 10); // 2^10 = 1024
        assert_eq!(max_level(1000, 1000), 10); // ceil(log2(1000)) = 10
        assert_eq!(max_level(100, 60), 7); // ceil(log2(100)) = 7
    }

    /// Level count == ceil(log2(max(w,h)))+1 (levels 0..=max_level inclusive).
    #[test]
    fn level_count_is_max_level_plus_one() {
        let tiles = enumerate_dzi_tiles(1024, 768, 256);
        let max_lvl = tiles.iter().map(|t| t.level).max().unwrap();
        assert_eq!(max_lvl, max_level(1024, 768));
        let distinct_levels: std::collections::BTreeSet<u32> =
            tiles.iter().map(|t| t.level).collect();
        assert_eq!(distinct_levels.len(), (max_lvl + 1) as usize);
    }

    /// A 1x1 tail level exists (level 0 is always a single 1x1-or-smaller tile).
    #[test]
    fn level_zero_is_a_single_tiny_tile() {
        let tiles = enumerate_dzi_tiles(1024, 768, 256);
        let level0: Vec<&DziTile> = tiles.iter().filter(|t| t.level == 0).collect();
        assert_eq!(level0.len(), 1);
        match level0[0].size {
            Size::Wh(w, h) => {
                assert_eq!(w, 1);
                assert_eq!(h, 1);
            }
            _ => panic!("expected Wh size"),
        }
    }

    /// Inverted indexing: the FINEST DZI level (max_level) covers the full image extent
    /// (its tile grid's union of regions spans (0,0)..(width,height)), unlike IIIF where
    /// index 0 is finest.
    #[test]
    fn finest_level_is_max_level_not_zero() {
        let (w, h) = (100u64, 60u64);
        let tiles = enumerate_dzi_tiles(w, h, 256); // tile_size 256 > image, single tile
        let finest_level = max_level(w, h);
        let finest: Vec<&DziTile> = tiles.iter().filter(|t| t.level == finest_level).collect();
        assert_eq!(finest.len(), 1);
        assert_eq!(finest[0].region, Region::Px { x: 0, y: 0, w, h });
        assert_eq!(finest[0].size, Size::Wh(w, h));
    }

    #[test]
    fn descriptor_xml_parses_and_has_expected_fields() {
        let xml = dzi_descriptor_xml(1024, 768, 256);
        assert!(xml.contains(r#"TileSize="256""#));
        assert!(xml.contains(r#"Overlap="0""#));
        assert!(xml.contains(r#"Format="jpg""#));
        assert!(xml.contains(r#"Width="1024""#));
        assert!(xml.contains(r#"Height="768""#));
        // Must be well-formed enough for a trivial parse: single root <Image> with one
        // <Size> child in the DeepZoom namespace.
        assert!(xml.contains("<Image "));
        assert!(xml.contains("<Size "));
        assert!(xml.contains("xmlns=\"http://schemas.microsoft.com/deepzoom/2008\""));
    }

    #[test]
    fn tile_grid_edges_are_clipped_not_padded() {
        let tiles = enumerate_dzi_tiles(100, 60, 32);
        let finest = max_level(100, 60);
        let edge = tiles
            .iter()
            .find(|t| t.level == finest && t.col == 3 && t.row == 0)
            .expect("expected a right-edge tile at the finest level");
        match (edge.region, edge.size) {
            (Region::Px { x, y, w, h }, Size::Wh(sw, sh)) => {
                assert_eq!((x, y), (96, 0));
                assert_eq!(w, 4); // 100 - 96
                assert_eq!(sw, 4);
                assert_eq!(h, 32); // tile-size-clipped (image height 60 > tile size 32)
                assert_eq!(sh, h);
            }
            _ => panic!("expected Px region + Wh size"),
        }
    }

    /// DZI descriptor XML on disk parses (trivial structural parse: root `<Image>` with
    /// `TileSize`/`Overlap`/`Format` attributes and a `<Size>` child) and the level count
    /// matches `ceil(log2(max(w,h)))+1` with a 1x1 tail and inverted indexing, exercised
    /// end-to-end via `export_dzi` against a real `TileEngine` rather than the pure
    /// `enumerate_dzi_tiles`/`dzi_descriptor_xml` unit tests above.
    #[test]
    fn export_dzi_writes_parseable_descriptor_and_full_level_ladder() {
        let e = engine();
        let dir = tempfile::tempdir().unwrap();
        let count = export_dzi(&e, dir.path(), "sample", 512, 85).unwrap();
        assert!(count > 0);

        let xml = fs::read_to_string(dir.path().join("sample.dzi")).unwrap();
        assert!(xml.contains("<Image "));
        assert!(xml.contains(r#"TileSize="512""#));
        assert!(xml.contains(r#"Overlap="0""#));
        assert!(xml.contains(r#"Format="jpg""#));
        assert!(xml.contains(r#"Width="64""#));
        assert!(xml.contains(r#"Height="64""#));

        let expected_max_level = max_level(64, 64);
        assert!(dir
            .path()
            .join(format!("sample_files/{expected_max_level}/0_0.jpg"))
            .exists());
        assert!(dir.path().join("sample_files/0/0_0.jpg").exists());
    }

    /// PIXEL SPOT-CHECK for DZI: a written DZI tile's bytes == `engine.tile(same region,
    /// same size, quality)` bytes — the DZI export is also just materialized `serve`,
    /// sharing the exact same render call as the IIIF exporter.
    #[test]
    fn dzi_tile_bytes_match_direct_engine_tile_call() {
        let e = engine();
        let dir = tempfile::tempdir().unwrap();
        export_dzi(&e, dir.path(), "sample", 512, 85).unwrap();

        let finest = max_level(64, 64);
        let on_disk = fs::read(dir.path().join(format!("sample_files/{finest}/0_0.jpg"))).unwrap();
        let direct = e
            .tile(
                &ProjectionId::Default,
                Region::Px {
                    x: 0,
                    y: 0,
                    w: 64,
                    h: 64,
                },
                Size::Wh(64, 64),
                85,
            )
            .unwrap();
        assert_eq!(on_disk, direct);
    }

    /// Every enumerated DZI tile file is written to disk (completeness, mirroring the
    /// IIIF exporter's completeness test) — no missing tiles across the full level ladder.
    #[test]
    fn export_dzi_writes_every_enumerated_tile() {
        let e = engine();
        let dir = tempfile::tempdir().unwrap();
        export_dzi(&e, dir.path(), "sample", 512, 85).unwrap();

        let tiles = enumerate_dzi_tiles(64, 64, 512);
        for t in &tiles {
            let path = dir.path().join(t.relative_path("sample_files"));
            assert!(path.exists(), "missing DZI tile: {path:?}");
        }
    }

    // --- `--dzi-name` path-traversal validation (Deliverable, FIX 3) ---

    #[test]
    fn validate_dzi_name_accepts_plain_filename() {
        assert!(validate_dzi_name("sample").is_ok());
        assert!(validate_dzi_name("my-slide_01").is_ok());
    }

    #[test]
    fn validate_dzi_name_rejects_parent_dir_traversal() {
        for bad in [
            "..",
            "../../etc/foo",
            "..\\..\\windows\\evil",
            "foo/../../bar",
        ] {
            let err = validate_dzi_name(bad).unwrap_err();
            assert!(
                matches!(err, ExportError::InvalidDziName { .. }),
                "expected InvalidDziName for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn validate_dzi_name_rejects_path_separators() {
        for bad in ["a/b", "a\\b", "/etc/passwd", "\\windows\\system32"] {
            let err = validate_dzi_name(bad).unwrap_err();
            assert!(
                matches!(err, ExportError::InvalidDziName { .. }),
                "expected InvalidDziName for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn validate_dzi_name_rejects_empty_name() {
        let err = validate_dzi_name("").unwrap_err();
        assert!(matches!(err, ExportError::InvalidDziName { .. }));
    }

    /// End-to-end: `export_dzi` itself rejects a path-traversal name before writing anything,
    /// and — crucially — does NOT write outside `out_dir` (the whole point of the fix).
    #[test]
    fn export_dzi_rejects_path_traversal_name_and_writes_nothing_outside_out_dir() {
        let e = engine();
        let dir = tempfile::tempdir().unwrap();
        let err = export_dzi(&e, dir.path(), "../../escaped", 512, 85).unwrap_err();
        assert!(matches!(err, ExportError::InvalidDziName { .. }), "{err:?}");

        // Nothing should have been written under out_dir either (validation runs first, before
        // any `fs::create_dir_all`/`fs::write`).
        let entries: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert!(
            entries.is_empty(),
            "export_dzi wrote files despite rejecting the name: {entries:?}"
        );
    }
}
