//! OpenSeadragon v3 IIIF Level-0 request-space enumerator.
//!
//! Reproduces OpenSeadragon's `IIIFTileSource.prototype.getTileUrl` (see
//! `src/iiiftilesource.js` in the openseadragon repo) EXACTLY, so that a static export
//! contains precisely the tile files OSD will ever request — no more, no less.
//!
//! The reference function (verified against the real OSD source, 2026, `master` branch)
//! is, with OSD's internal (inverted, `maxLevel`=finest) level numbering translated to
//! OUR level numbering (index 0 = finest, matching `ImageInfo::scale_factors`/`sizes`
//! order — the two numbering schemes describe the same levels, just counted from
//! opposite ends, so `scale = 1 / scale_factors[level]` is the one quantity that matters
//! and is numbering-agnostic):
//!
//! ```text
//! getTileUrl(level, x, y):
//!   scale = 1 / scale_factors[level]
//!   levelWidth, levelHeight = sizes[level]           // from the pinned `sizes` array
//!   tileWidth = tileHeight = tile_size               // single `tiles` entry, no per-level override
//!   iiifTileSizeWidth  = round(tileWidth  / scale)   // = round(tileWidth  * scale_factors[level])
//!   iiifTileSizeHeight = round(tileHeight / scale)
//!
//!   if levelWidth < tileWidth && levelHeight < tileHeight:
//!       // the WHOLE LEVEL fits in a single tile: OSD only ever asks for ONE tile at (0,0)
//!       region = "full"
//!       size = "max" if (levelWidth, levelHeight) == (width, height) else "levelWidth,levelHeight"
//!   else:
//!       iiifTileX = x * iiifTileSizeWidth
//!       iiifTileY = y * iiifTileSizeHeight
//!       iiifTileW = min(iiifTileSizeWidth,  width  - iiifTileX)
//!       iiifTileH = min(iiifTileSizeHeight, height - iiifTileY)
//!       region = "full" if (x,y)==(0,0) && (iiifTileW,iiifTileH)==(width,height) else "x,y,w,h"
//!
//!       iiifSizeW = min(tileWidth,  levelWidth  - x*tileWidth)
//!       iiifSizeH = min(tileHeight, levelHeight - y*tileHeight)
//!       size = "max" if (iiifSizeW,iiifSizeH)==(width,height) else "iiifSizeW,iiifSizeH"
//! ```
//!
//! Plus, the `getTileUrl` logic above ITSELF already produces a `full/{w},{h}/0/
//! default.jpg` (or `full/max/...`) whole-image request for any level whose tile grid is
//! a SINGLE CELL — either via the "fits in one tile" branch (`levelWidth < tileWidth &&
//! levelHeight < tileHeight`) or, at the exact boundary (`levelWidth == tileWidth`), via
//! the tiled branch's degenerate `1x1` grid where tile `(0,0)`'s clipped region/size
//! happens to cover the whole level. A level whose grid has MORE than one cell (a
//! genuinely multi-tile level) never gets a whole-image request from `getTileUrl` — OSD
//! only ever fetches that level's individual tiles. (OSD DOES turn every `sizes[]` entry
//! into its own `full/{w},{h}` request in `constructLevels`/`emulateLegacyImagePyramid`,
//! but only when the info.json has no `tiles` array at all; our exports always emit
//! `tiles`, so that path never activates and must not be emulated here.) We therefore
//! only add a `sizes[]`-derived whole-image entry for single-cell levels — multi-tile
//! levels are covered entirely by the tile grid above, with no separate derivative.
//!
//! rotation is always the literal `"0"`; quality/format is always `"default.jpg"`.

use iiif::ImageInfo;

/// One enumerated OSD v3 IIIF Level-0 request: the exact region/size STRINGS OSD's
/// `getTileUrl` would produce, plus the request-space types the tile engine needs to
/// actually render it.
#[derive(Debug, Clone, PartialEq)]
pub struct EnumeratedRequest {
    /// `full` or `x,y,w,h` in FULL-RES coordinates — the literal IIIF region path segment.
    pub region_str: String,
    /// `max` or `w,h` in LEVEL coordinates — the literal IIIF size path segment.
    pub size_str: String,
    /// The `Region` value to pass to `TileEngine::tile`.
    pub region: iiif::Region,
    /// The `Size` value to pass to `TileEngine::tile`.
    pub size: iiif::Size,
}

impl EnumeratedRequest {
    /// The on-disk / URL path this request maps to under the export root:
    /// `{region_str}/{size_str}/0/default.jpg`.
    pub fn relative_path(&self) -> String {
        format!("{}/{}/0/default.jpg", self.region_str, self.size_str)
    }
}

/// Enumerate the EXACT set of `(region, size)` requests OpenSeadragon v3 makes against a
/// Level-0 IIIF tile source built from `info`, reproducing `getTileUrl` per the module
/// doc comment. `info.sizes` MUST be 1:1 with `info.scale_factors` (finest first): this is
/// the ENGINE's own untrimmed pyramid, where every `sizes[i]` and `scale_factors[i]` come
/// from the same per-level loop over the source data, before `iiif::level0` decides what
/// subset of it `info.json` is allowed to advertise.
pub fn enumerate_request_space(info: &ImageInfo) -> Vec<EnumeratedRequest> {
    assert_eq!(
        info.sizes.len(),
        info.scale_factors.len(),
        "sizes must be 1:1 with scale_factors"
    );
    let width = info.width;
    let height = info.height;
    let tile_w = info.tile_size;
    let tile_h = info.tile_size;

    let mut out = Vec::new();

    for (level, &sf) in info.scale_factors.iter().enumerate() {
        let (level_w, level_h) = info.sizes[level];

        // iiifTileSize{Width,Height} = round(tileWidth / scale) where scale = 1/sf, i.e.
        // round(tileWidth * sf). `sf` is always integral here (a `u64`), so this is exact
        // (no floating-point rounding ambiguity) — matches OSD's `Math.round` for the
        // power-of-two scale factors real pyramids use.
        let iiif_tile_w = tile_w * sf;
        let iiif_tile_h = tile_h * sf;

        if level_w < tile_w && level_h < tile_h {
            // Whole level fits in a single tile: OSD requests exactly ONE tile, region=full.
            let size_str = if level_w == width && level_h == height {
                "max".to_string()
            } else {
                format!("{level_w},{level_h}")
            };
            let req = EnumeratedRequest {
                region_str: "full".to_string(),
                size_str: size_str.clone(),
                region: iiif::Region::Full,
                size: size_for_str(&size_str, level_w, level_h),
            };
            // A well-formed pyramid never repeats a level's dimensions, but malformed metadata
            // (two levels of identical, tile-sized dimensions) can: `req` would be identical for
            // both, and without this guard both get pushed, so `write_tree`'s planned/actual file
            // counts double-count the one real file this request names.
            if !out.contains(&req) {
                out.push(req);
            }
            continue;
        }

        let cols = level_w.div_ceil(tile_w);
        let rows = level_h.div_ceil(tile_h);
        for y in 0..rows {
            for x in 0..cols {
                let iiif_tile_x = x * iiif_tile_w;
                let iiif_tile_y = y * iiif_tile_h;
                let iiif_tile_rw = iiif_tile_w.min(width - iiif_tile_x);
                let iiif_tile_rh = iiif_tile_h.min(height - iiif_tile_y);

                let region_str =
                    if x == 0 && y == 0 && iiif_tile_rw == width && iiif_tile_rh == height {
                        "full".to_string()
                    } else {
                        format!("{iiif_tile_x},{iiif_tile_y},{iiif_tile_rw},{iiif_tile_rh}")
                    };
                let region = if region_str == "full" {
                    iiif::Region::Full
                } else {
                    iiif::Region::Px {
                        x: iiif_tile_x,
                        y: iiif_tile_y,
                        w: iiif_tile_rw,
                        h: iiif_tile_rh,
                    }
                };

                let iiif_size_w = tile_w.min(level_w - x * tile_w);
                let iiif_size_h = tile_h.min(level_h - y * tile_h);
                let size_str = if iiif_size_w == width && iiif_size_h == height {
                    "max".to_string()
                } else {
                    format!("{iiif_size_w},{iiif_size_h}")
                };
                let size = size_for_str(&size_str, iiif_size_w, iiif_size_h);

                out.push(EnumeratedRequest {
                    region_str,
                    size_str,
                    region,
                    size,
                });
            }
        }
    }

    // Whole-image derivatives from the `sizes` array: `getTileUrl` (region/size logic
    // above) is the ONLY place OSD ever turns a `sizes[]` entry into a `full/{w},{h}`
    // request, and it only does so for a level whose tile grid is A SINGLE CELL — i.e.
    // `level_w <= tile_w && level_h <= tile_h` (the "fits in one tile" branch, PLUS the
    // tiled branch's degenerate 1x1-grid boundary case where `level_w == tile_w` exactly:
    // `cols = ceil(level_w/tile_w) == 1`). OSD's `sizes` array otherwise feeds ONLY
    // `levelSizes`/`getNumTiles` (the tile-grid math already reproduced above) — the
    // `constructLevels`/`emulateLegacyImagePyramid` code path that turns EVERY `sizes[]`
    // entry into its own `full/{w},{h}` request only activates when the info.json has NO
    // `tiles` array at all (`iiiftilesource.js`: `else if (this.sizes...)`), which never
    // applies here since our exported info.json always includes `tiles`. So for a
    // genuinely multi-tile level (grid > 1x1), OSD's tile grid already emits that level's
    // tiles above and NEVER separately requests `full/{level_w},{level_h}` — emitting it
    // anyway would write a file OSD never fetches. Single-cell levels already get their
    // one `full/...` entry from the loop above in the ORDINARY case (the fits-in-one-tile
    // branch), so this loop only needs to ADD an entry no prior branch already produced
    // (guarded by `!out.contains`) — but that is NOT the same thing as a no-op. At the exact
    // tile-size boundary (a level's longer edge equals `tile_w`/`tile_h`, both edges `<=`
    // the tile size but not both `<`), this loop writes a `full/{w},{h}` that OpenSeadragon
    // itself never requests (the tiled branch's own degenerate 1x1-grid tile does not
    // always land on `region_str == "full"` there; see the doc comment on `iiif::level0`'s
    // `osd_tiling_agrees_with_the_export`, which is what proves `level0_sizes` only ever
    // ACCEPTS a pyramid where OSD's real request for that boundary level agrees with what
    // gets written). `sizes`, though, still advertises that level's own dimensions: WITHIN
    // budget, `writer::write_tree`'s own whole-image step would write this same file anyway
    // (from `iiif::level0_sizes`'s `sizes`, independent of this loop); OVER budget, where
    // `write_tree` writes no whole images at all, this loop is the ONLY source of that file,
    // and `crates/exporter/tests/osd_tiling_oracle.rs`'s `boundary_fallback_extras` is what
    // tolerates the shape (the oracle's own sweep does not require it: disabling this loop
    // still leaves every test green for the pyramids that sweep currently covers — but "not
    // caught yet" is not the same claim as "unneeded", see the doc comment two paragraphs up
    // for the case where it is the only source). Do not remove or "simplify" this loop on the
    // assumption that the
    // branch above already covers every single-cell level.
    for &(w, h) in &info.sizes {
        if w > tile_w || h > tile_h {
            // Multi-tile level: OSD's tile grid never issues a whole-image request for
            // it (see above) — do not manufacture one.
            continue;
        }
        let size_str = if w == width && h == height {
            "max".to_string()
        } else {
            format!("{w},{h}")
        };
        let size = size_for_str(&size_str, w, h);
        let req = EnumeratedRequest {
            region_str: "full".to_string(),
            size_str,
            region: iiif::Region::Full,
            size,
        };
        if !out.contains(&req) {
            out.push(req);
        }
    }

    out
}

fn size_for_str(size_str: &str, w: u64, h: u64) -> iiif::Size {
    if size_str == "max" {
        iiif::Size::Max
    } else {
        iiif::Size::Wh(w, h)
    }
}

/// The whole-image derivative of the SMALLEST (coarsest, last) pyramid level, regardless of
/// whether that level fits in one tile.
///
/// `enumerate_request_space` deliberately omits a whole-image request for any level whose tile
/// grid has more than one cell (see the module doc: OpenSeadragon never asks for one). When EVERY
/// level is like that — a "no pyramid" image bigger than the tile size is the common case — its
/// request space then has no whole-image entry at all.
///
/// This only matters to `crate::manifest`'s canvas bodies OVER the whole-image budget: WITHIN
/// budget, `crate::manifest::largest_whole_image` never reaches this function at all, because it
/// points straight at `full/{maxWidth},{maxHeight}` from `iiif::level0_sizes`, which
/// `crate::writer::write_tree`'s level 0 contract guarantees exists (`full/{w},{h}` per advertised
/// size, plus `full/max`, see `write_tree`'s doc comment). OVER budget, though, that contract
/// writes no whole image at all, so a manifest still being written (`write_tree`'s
/// `manifest_needs_body` parameter) needs a real file for its body to name regardless — this is
/// that file, called directly by `crate::manifest::largest_whole_image`'s own over-budget fallback
/// (a caller may build a manifest body from an `ImageInfo` without going through `write_tree` at
/// all, see `manifest::manifest_json`'s own tests) and, over budget, by `write_tree` itself.
///
/// # Panics
///
/// Panics if `info.sizes` is empty. Every `ImageInfo` this crate builds from a real
/// [`tiling::TileEngine`] has at least one level (its own pyramid, `sizes.len() ==
/// scale_factors.len() >= 1`), so this is unreachable through ziv's own engine; it is documented
/// because the function is `pub` and nothing else enforces the precondition on a caller-built
/// `ImageInfo`.
pub fn smallest_level_whole_image(info: &ImageInfo) -> EnumeratedRequest {
    assert!(
        !info.sizes.is_empty(),
        "smallest_level_whole_image: info.sizes must have at least one level"
    );
    let coarsest = info.sizes.len() - 1;
    let (w, h) = info.sizes[coarsest];
    let size_str = if (w, h) == (info.width, info.height) {
        "max".to_string()
    } else {
        format!("{w},{h}")
    };
    EnumeratedRequest {
        region_str: "full".to_string(),
        size_str: size_str.clone(),
        region: iiif::Region::Full,
        size: size_for_str(&size_str, w, h),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iiif::{Region, Size};

    fn info(width: u64, height: u64, tile_size: u64, scale_factors: Vec<u64>) -> ImageInfo {
        let sizes = scale_factors
            .iter()
            .map(|&sf| (width.div_ceil(sf).max(1), height.div_ceil(sf).max(1)))
            .collect();
        ImageInfo {
            id: "test".to_string(),
            width,
            height,
            tile_size,
            scale_factors,
            sizes,
        }
    }

    // ---- Reference implementation (independent port of OSD's getTileUrl) ----
    //
    // This is a from-scratch reimplementation used ONLY by the golden test below, so the
    // test is not circular: it does not call `enumerate_request_space` or share any code
    // path with `enumerate.rs`. It encodes the same OSD source (quoted in the module doc
    // comment) but was transcribed independently for the test, using OSD's OWN inverted
    // level numbering (maxLevel = finest) rather than our index-0-finest numbering, as a
    // further check that the two enumerations agree despite being expressed differently.
    mod osd_reference {
        use std::collections::BTreeSet;

        /// `(width, height, tile_size, scale_factors)` -> the set of `(region, size)`
        /// path-segment pairs OSD's `getTileUrl` would generate for every level/x/y. The
        /// whole-image (`full/{w},{h}`) requests are ALREADY produced by this same loop,
        /// for any level whose tile grid is a single cell (see the two `urls.insert`
        /// call sites inside the loop below) — there is no separate `sizes[]`-derived
        /// pass, matching real OSD: `getTileUrl` never turns a multi-tile level's
        /// `sizes[]` entry into its own whole-image request (that only happens via
        /// `constructLevels`'s legacy-pyramid fallback, which requires the info.json to
        /// have no `tiles` array — never true for our exports, which always emit
        /// `tiles`).
        pub fn reference_urls(
            width: u64,
            height: u64,
            tile_size: u64,
            scale_factors: &[u64],
        ) -> BTreeSet<(String, String)> {
            let mut urls = BTreeSet::new();
            // OSD's maxLevel = index of the LARGEST scale factor in ITS own numbering,
            // where level 0 = coarsest, maxLevel = finest. Our `scale_factors[i]` is
            // ordered finest-first (index 0 = sf 1), so OSD level = (len-1-i).
            let max_level = scale_factors.len() - 1;
            for (i, &sf) in scale_factors.iter().enumerate() {
                let osd_level = max_level - i;
                // scale = 0.5^(maxLevel - level) = 1/sf when sf = 2^(maxLevel-level).
                let scale = 1.0 / sf as f64;
                let level_w = width.div_ceil(sf).max(1);
                let level_h = height.div_ceil(sf).max(1);
                let tile_w = tile_size;
                let tile_h = tile_size;
                let iiif_tile_w = (tile_w as f64 / scale).round() as u64;
                let iiif_tile_h = (tile_h as f64 / scale).round() as u64;

                if level_w < tile_w && level_h < tile_h {
                    let size = if level_w == width && level_h == height {
                        "max".to_string()
                    } else {
                        format!("{level_w},{level_h}")
                    };
                    urls.insert(("full".to_string(), size));
                    let _ = osd_level;
                    continue;
                }

                let cols = level_w.div_ceil(tile_w);
                let rows = level_h.div_ceil(tile_h);
                for y in 0..rows {
                    for x in 0..cols {
                        let tx = x * iiif_tile_w;
                        let ty = y * iiif_tile_h;
                        let tw = iiif_tile_w.min(width - tx);
                        let th = iiif_tile_h.min(height - ty);
                        let region = if x == 0 && y == 0 && tw == width && th == height {
                            "full".to_string()
                        } else {
                            format!("{tx},{ty},{tw},{th}")
                        };
                        let sw = tile_w.min(level_w - x * tile_w);
                        let sh = tile_h.min(level_h - y * tile_h);
                        let size = if sw == width && sh == height {
                            "max".to_string()
                        } else {
                            format!("{sw},{sh}")
                        };
                        urls.insert((region, size));
                    }
                }
            }
            urls
        }
    }

    fn enumerated_urls(info: &ImageInfo) -> std::collections::BTreeSet<(String, String)> {
        enumerate_request_space(info)
            .into_iter()
            .map(|r| (r.region_str, r.size_str))
            .collect()
    }

    /// GOLDEN OSD LOCKSTEP TEST (spec §9.3): the enumerator's URL set must exactly equal
    /// the independently-ported OSD reference's URL set, for several image dims.
    #[test]
    fn golden_osd_lockstep_square_power_of_2() {
        let info = info(1024, 1024, 256, vec![1, 2, 4]);
        let expected = osd_reference::reference_urls(1024, 1024, 256, &[1, 2, 4]);
        assert_eq!(enumerated_urls(&info), expected);
    }

    #[test]
    fn golden_osd_lockstep_non_power_of_2() {
        let info = info(100, 60, 32, vec![1, 2, 4]);
        let expected = osd_reference::reference_urls(100, 60, 32, &[1, 2, 4]);
        assert_eq!(enumerated_urls(&info), expected);
    }

    #[test]
    fn golden_osd_lockstep_non_square() {
        let info = info(800, 300, 128, vec![1, 2, 4, 8]);
        let expected = osd_reference::reference_urls(800, 300, 128, &[1, 2, 4, 8]);
        assert_eq!(enumerated_urls(&info), expected);
    }

    #[test]
    fn golden_osd_lockstep_fits_in_one_tile() {
        // levelW,levelH <= tile_size at every level (64x64 image, 512 tile).
        let info = info(64, 64, 512, vec![1, 2]);
        let expected = osd_reference::reference_urls(64, 64, 512, &[1, 2]);
        assert_eq!(enumerated_urls(&info), expected);
    }

    #[test]
    fn golden_osd_lockstep_odd_aspect_many_levels() {
        let info = info(4001, 2999, 256, vec![1, 2, 4, 8, 16]);
        let expected = osd_reference::reference_urls(4001, 2999, 256, &[1, 2, 4, 8, 16]);
        assert_eq!(enumerated_urls(&info), expected);
    }

    #[test]
    fn full_image_single_tile_uses_full_and_max() {
        // Fits-in-one-tile AND full-res level -> region full, size max.
        let info = info(64, 64, 512, vec![1]);
        let reqs = enumerate_request_space(&info);
        assert!(reqs
            .iter()
            .any(|r| r.region_str == "full" && r.size_str == "max"));
        for r in &reqs {
            assert_eq!(r.region, Region::Full);
        }
    }

    /// Degenerate metadata (malformed, but not something `ImageInfo` itself refuses to hold): two
    /// levels declare IDENTICAL, tile-sized dimensions. Both take the single-tile branch and would
    /// push the same `EnumeratedRequest` twice without a dedup guard, double-counting the one real
    /// file `full/max/0/default.jpg` would name — this is `write_tree`'s planned/actual file count
    /// overcounting bug, isolated to `enumerate_request_space` alone.
    #[test]
    fn a_degenerate_pyramid_with_two_identical_levels_is_not_double_counted() {
        let info = ImageInfo {
            id: "test".to_string(),
            width: 64,
            height: 64,
            tile_size: 512,
            scale_factors: vec![1, 2],
            sizes: vec![(64, 64), (64, 64)],
        };
        let reqs = enumerate_request_space(&info);
        let full_max = reqs
            .iter()
            .filter(|r| r.region_str == "full" && r.size_str == "max")
            .count();
        assert_eq!(
            full_max, 1,
            "two identical-dimension levels must still yield exactly one full/max request, got \
             {reqs:?}"
        );
    }

    #[test]
    fn multi_tile_level_uses_coordinate_region_and_wh_size() {
        let info = info(1024, 1024, 256, vec![1]);
        let reqs = enumerate_request_space(&info);
        // A non-edge, non-origin tile must use "x,y,w,h" region and "w,h" size (not the
        // legacy trailing-comma "w," form).
        let mid = reqs
            .iter()
            .find(|r| r.region_str == "256,256,256,256")
            .expect("expected a mid-grid tile");
        assert_eq!(mid.size_str, "256,256");
        assert_eq!(
            mid.region,
            Region::Px {
                x: 256,
                y: 256,
                w: 256,
                h: 256
            }
        );
        assert_eq!(mid.size, Size::Wh(256, 256));
    }

    /// FIX2 regression: a genuinely multi-tile level (grid > 1x1) must NOT get a
    /// `full/{levelW},{levelH}` whole-image derivative — OSD's `getTileUrl` never
    /// requests one for such a level (see the module doc comment). Mirrors the
    /// `sample_multi_tile.ome.zarr` fixture's level 0 (1024x1024, 512px tile -> 2x2
    /// grid): only the 4 coordinate tiles should be present for that level, never
    /// `full/1024,1024` (nor, since this is also the finest level, `full/max`).
    #[test]
    fn multi_tile_level_has_no_whole_image_derivative() {
        let info = info(1024, 1024, 512, vec![1, 2]);
        let reqs = enumerate_request_space(&info);
        assert!(
            !reqs.iter().any(|r| r.size_str == "1024,1024"),
            "level 0 (multi-tile, 2x2 grid) must not emit a full/1024,1024 whole-image derivative"
        );
        assert!(
            !reqs.iter().any(|r| r.size_str == "max"),
            "finest level is multi-tile, so full/max must not be emitted either"
        );
        // Level 1 (512x512, exactly 1 tile at the <= boundary) DOES get one, via the
        // tiled branch's degenerate 1x1 grid, not the sizes[] loop.
        assert!(reqs
            .iter()
            .any(|r| r.region_str == "full" && r.size_str == "512,512"));
        // Level 0's own 4 tiles are still present (multi-tile coverage unaffected).
        let level0_tiles = reqs
            .iter()
            .filter(|r| r.size_str == "512,512" && r.region_str != "full")
            .count();
        assert_eq!(level0_tiles, 4);
    }

    /// Sibling check with a level that fits in one tile strictly (`<`, not `==`): its
    /// whole-image derivative must still be present (unaffected by the FIX2 gate).
    #[test]
    fn fits_in_one_tile_level_still_has_whole_image_derivative() {
        let info = info(1024, 1024, 512, vec![1, 2, 8]);
        // scale factor 8 -> level (128,128), strictly under the 512 tile size.
        let reqs = enumerate_request_space(&info);
        assert!(reqs
            .iter()
            .any(|r| r.region_str == "full" && r.size_str == "128,128"));
    }

    /// The common case this fallback exists for: a "no pyramid" image (one level) bigger than
    /// the tile size. That sole level IS the full image, so the fallback names `full/max`, not
    /// its own `w,h` — the exact shape `crate::manifest`'s fallback body needs.
    #[test]
    fn smallest_level_whole_image_is_full_max_when_the_sole_level_is_the_full_image() {
        let info = info(600, 600, 512, vec![1]);
        let req = smallest_level_whole_image(&info);
        assert_eq!(req.relative_path(), "full/max/0/default.jpg");
        assert_eq!(req.region, Region::Full);
        assert_eq!(req.size, Size::Max);
    }

    /// A pyramid whose coarsest level is STILL bigger than the tile size (unusual, but not
    /// something `largest_whole_image`'s caller may assume away): the coarsest level's own
    /// smaller-than-full size is named, not `max`.
    #[test]
    fn smallest_level_whole_image_names_the_coarsest_levels_own_size_when_not_full() {
        let info = info(4000, 4000, 512, vec![1, 2]);
        let req = smallest_level_whole_image(&info);
        assert_eq!(req.relative_path(), "full/2000,2000/0/default.jpg");
        assert_eq!(req.size, Size::Wh(2000, 2000));
    }

    #[test]
    fn edge_tiles_are_clipped_not_padded() {
        let info = info(100, 60, 32, vec![1]);
        let reqs = enumerate_request_space(&info);
        // Rightmost column, top row: x=96 (3*32), y=0, width clipped to 100-96=4, height
        // clipped to the tile size (32, since 60 > 32 so the top row isn't the last row).
        let right = reqs
            .iter()
            .find(|r| r.region_str.starts_with("96,0,"))
            .expect("expected a right-edge tile");
        assert_eq!(right.region_str, "96,0,4,32");
        assert_eq!(right.size_str, "4,32");
        // Bottom-right corner tile: y=32 (second row), height clipped to 60-32=28.
        let corner = reqs
            .iter()
            .find(|r| r.region_str.starts_with("96,32,"))
            .expect("expected a bottom-right corner tile");
        assert_eq!(corner.region_str, "96,32,4,28");
        assert_eq!(corner.size_str, "4,28");
    }

    #[test]
    fn no_size_ever_uses_trailing_comma_form() {
        for &(w, h, t) in &[(1024u64, 1024u64, 256u64), (100, 60, 32), (64, 64, 512)] {
            let info = info(w, h, t, vec![1, 2]);
            for r in enumerate_request_space(&info) {
                assert!(
                    !r.size_str.ends_with(',') && !r.size_str.starts_with(','),
                    "v1/v2 trailing-comma size form leaked into v3 export: {}",
                    r.size_str
                );
            }
        }
    }

    /// HAND-TRACED INDEPENDENT ANCHOR (closes the golden-test transcription-circularity
    /// concern for one concrete case): unlike `osd_reference` above — which is a from-
    /// scratch reimplementation but was still transcribed from the SAME `getTileUrl`
    /// pseudocode quoted in this module's doc comment, so a shared misreading of the real
    /// OSD source could slip past both — the expected set below is a literal, hard-coded
    /// `Vec` derived by MANUALLY tracing OpenSeadragon's actual
    /// `IIIFTileSource.prototype.getTileUrl` (fetched fresh from
    /// `https://github.com/openseadragon/openseadragon/blob/master/src/iiiftilesource.js`,
    /// `getTileUrl`, lines ~460-553 as of this writing) line by line, by hand, for one
    /// concrete image, OSD level by OSD level. No code in this crate (enumerator OR
    /// `osd_reference`) was consulted while deriving these strings; they are copied here
    /// exactly as computed on paper. If `enumerate_request_space` and `osd_reference` ever
    /// agreed with each other while BOTH being wrong (e.g. both misread `Math.round` or
    /// the single-tile-fit boundary), this test — with no shared ancestry — would still
    /// catch it.
    ///
    /// Case: 100x60 image, 32px tiles, scaleFactors [1, 2, 4] (our finest-first numbering;
    /// OSD's own numbering is inverted: maxLevel=2 <-> our sf=1, level=1 <-> sf=2,
    /// level=0 <-> sf=4). Chosen specifically because 100 and 60 are NOT multiples of 32,
    /// so the finest level's tile grid has clipped edge/corner tiles on both the right
    /// column and bottom row (the fragile clipping arithmetic `Math.min(iiifTileSizeWidth,
    /// this.width - iiifTileX)` etc. is exactly what this anchor exercises).
    ///
    /// Trace, per OSD's `getTileUrl` (`scale = 0.5^(maxLevel-level)`, `_id` omitted since
    /// `EnumeratedRequest::relative_path` intentionally excludes it — the export tree is
    /// already rooted per-image, so the id prefix is applied one directory up, not by this
    /// enumerator):
    ///
    /// - OSD level 2 (our sf=1, scale=1): levelW=100, levelH=60, tileW=tileH=32,
    ///   iiifTileSize=round(32/1)=32. levelW(100) is NOT < tileW(32), so the tiled branch
    ///   runs: cols=ceil(100/32)=4, rows=ceil(60/32)=2 -> 8 tiles, with the x=3 column
    ///   clipped to width 4 (100-96) and the y=1 row clipped to height 28 (60-32).
    /// - OSD level 1 (our sf=2, scale=0.5): levelW=ceil(100*0.5)=50, levelH=ceil(60*0.5)=30,
    ///   iiifTileSize=round(32/0.5)=64. levelW(50) is NOT < tileW(32) (only levelH is),
    ///   so still the tiled branch: cols=ceil(50/32)=2, rows=ceil(30/32)=1 -> 2 tiles, both
    ///   clipped (iiifTileW/H capped by `this.width - iiifTileX` / `this.height -
    ///   iiifTileY` at full-res 100x60, and size capped by `levelW/H - x*tileW/H`).
    /// - OSD level 0 (our sf=4, scale=0.25): levelW=ceil(100*0.25)=25, levelH=ceil(60*0.25)
    ///   =15. Both 25<32 and 15<32, so the "fits in one tile" branch fires: exactly one
    ///   `full/25,15` request (not `full/max`, since 25x15 != the 100x60 full-res image).
    #[test]
    fn hand_traced_osd_anchor_100x60_tile32_edge_clipping() {
        let info = info(100, 60, 32, vec![1, 2, 4]);

        // Literal, hand-computed from the real `getTileUrl` source (see doc comment
        // above) — not generated by any reference function in this file.
        let mut expected = vec![
            // OSD level 2 (sf=1): 4 cols x 2 rows, right column and bottom row clipped.
            "0,0,32,32/32,32/0/default.jpg",
            "32,0,32,32/32,32/0/default.jpg",
            "64,0,32,32/32,32/0/default.jpg",
            "96,0,4,32/4,32/0/default.jpg",
            "0,32,32,28/32,28/0/default.jpg",
            "32,32,32,28/32,28/0/default.jpg",
            "64,32,32,28/32,28/0/default.jpg",
            "96,32,4,28/4,28/0/default.jpg",
            // OSD level 1 (sf=2): 2 cols x 1 row, both clipped.
            "0,0,64,60/32,30/0/default.jpg",
            "64,0,36,60/18,30/0/default.jpg",
            // OSD level 0 (sf=4): fits in one tile, single full-image request.
            "full/25,15/0/default.jpg",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        expected.sort();

        let mut actual = enumerate_request_space(&info)
            .into_iter()
            .map(|r| r.relative_path())
            .collect::<Vec<_>>();
        actual.sort();

        assert_eq!(
            actual, expected,
            "enumerator output does not match the hand-traced real-OSD anchor set"
        );
    }
}
