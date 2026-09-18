//! The oracle that found the round-3 over-refusal bug, committed as a permanent regression test.
//!
//! `iiif::level0_sizes` decides whether a pyramid may be advertised as a level0 `info.json`.
//! Two earlier roles of that decision (both since corrected) either under-refused (accepted a
//! pyramid OpenSeadragon cannot pin) or over-refused (rejected pyramids that were entirely safe,
//! because a per-axis dimension-ratio test and an unconditional `scaleFactors[i] == 2^i` test were
//! stricter than OpenSeadragon itself). This module is independent proof, built the way the
//! review that found the over-refusal built it: port OpenSeadragon's OWN tile arithmetic
//! (`getNumTiles`, `getTileUrl`) into Rust, from the vendored, minified source, and compare the
//! URL set it produces against the URL set [`exporter::enumerate_request_space`] actually writes,
//! for a deterministic sweep of realistic pyramids. Neither function here shares any code with
//! `iiif::level0` or `exporter::enumerate` (only the already-independently-tested
//! `iiif::osd_level_sizes`, which reproduces OSD's `IIIFTileSource` CONSTRUCTOR, is reused — this
//! module ports the two functions the constructor's reconstruction feeds into).
//!
//! The property under test, in two parts, whenever `level0_sizes(info)` is `Some` (a `None` is
//! never checked here — refusing is always safe):
//!
//! 1. Every URL OpenSeadragon would ever request for the advertised `sizes` must be present among
//!    the files `enumerate_request_space` writes (`requested ⊆ written`) — a MISSING one is a
//!    404, a blank tile in the exported viewer, exactly the failure mode this whole review chain
//!    exists to catch.
//! 2. Conversely, every file `enumerate_request_space` writes that OpenSeadragon never asks for
//!    (`written \ requested`) must be of ONE documented, harmless shape: see
//!    `boundary_fallback_extras`. `enumerate_request_space`'s own "whole-image derivatives from
//!    `sizes`" fallback loop (`crates/exporter/src/enumerate.rs`) writes an EXTRA,
//!    never-requested `full/{w},{h}` file whenever a level's dimensions are both `<= tile_size`
//!    but not both `< tile_size` (the longer edge sits exactly on the tile-size boundary) — real
//!    OpenSeadragon's `getTileUrl` uses a STRICT `r<c&&o<u` (`c`/`u` being the tile size), so that
//!    boundary level takes the TILED branch, not the fits-in-one-tile branch, and the tiled
//!    branch's own single-cell result does not always land on `"full"`. That loop's own doc
//!    comment explains this in full: WITHIN budget, `writer::write_tree`'s own whole-image step
//!    would write this same file anyway, from `sizes`, independent of this loop; OVER budget,
//!    where `write_tree` writes no whole images at all, this loop is the ONLY source of that
//!    file. Any OTHER extra file is an unexplained divergence and fails this test.

use std::collections::BTreeSet;

use iiif::{osd_level_sizes, ImageInfo};

/// A pyramid built the way a real export's engine would: each level is the previous level's
/// dimensions halved (floor or ceil), down to a minimum of 1px per axis — NOT `width / 2^k`
/// computed fresh each time, so floor/ceil rounding compounds exactly the way real halving does.
/// `scale_factors[i]` is derived the same way `crates/tiling/src/engine.rs`'s
/// `scale_factors_of` derives it: `round(full_width / level_i_width).max(1)`, from WIDTH alone —
/// the height axis is whatever the independent per-level halving produced, not re-derived to
/// match.
fn build_pyramid(
    width: u64,
    height: u64,
    levels: u32,
    ceil_halving: bool,
    tile_size: u64,
) -> ImageInfo {
    let mut sizes = Vec::with_capacity(levels as usize);
    let (mut w, mut h) = (width, height);
    for _ in 0..levels {
        sizes.push((w, h));
        w = if ceil_halving { w.div_ceil(2) } else { w / 2 }.max(1);
        h = if ceil_halving { h.div_ceil(2) } else { h / 2 }.max(1);
    }
    let scale_factors = sizes
        .iter()
        .map(|&(lw, _)| ((width as f64 / lw as f64).round() as u64).max(1))
        .collect();
    ImageInfo {
        id: "oracle".to_string(),
        width,
        height,
        tile_size,
        scale_factors,
        sizes,
    }
}

// ---- Independent port of OpenSeadragon's tile arithmetic ----
//
// Fetched fresh from the vendored build this project ships,
// `crates/viewer-assets/assets/viewer/openseadragon/openseadragon.min.js` (2026), the SAME
// artefact `crates/iiif/src/level0.rs`'s module doc quotes for the constructor. Reformatted for
// readability only; no logic added or removed.
//
// ```js
// getNumTiles:function(e){
//   if(this.levelSizes){
//     var t=this.levelSizes[e];
//     var i=Math.ceil(t.width/this.getTileWidth(e)),
//         t=Math.ceil(t.height/this.getTileHeight(e));
//     return new h.Point(i,t)
//   }
//   return h.TileSource.prototype.getNumTiles.call(this,e)
// }
// ```
//
// ```js
// getTileUrl:function(e,t,i){
//   ...
//   var r,o,s,a,l,h,c,u,d=Math.pow(.5,this.maxLevel-e);
//   if(this.levelSizes){r=this.levelSizes[e].width;o=this.levelSizes[e].height}
//   else{r=Math.ceil(this.width*d);o=Math.ceil(this.height*d)}
//   c=this.getTileWidth(e);u=this.getTileHeight(e);
//   a=Math.round(c/d);l=Math.round(u/d);
//   n="default."+this.tileFormat;
//   if(r<c&&o<u){
//     h=r===this.width&&o===this.height?"max":r+","+o;
//     s="full"
//   }else{
//     e=t*a;d=i*l;
//     a=Math.min(a,this.width-e);l=Math.min(l,this.height-d);
//     s=0===t&&0===i&&a===this.width&&l===this.height?"full":[e,d,a,l].join(",");
//     c=Math.min(c,r-t*c);u=Math.min(u,o-i*u);
//     h=c===this.width&&u===this.height?"max":c+","+u
//   }
//   return[this._id,s,h,"0",n].join("/")
// }
// ```
//
// `getTileWidth`/`getTileHeight` always return the plain declared tile size here (this export
// never sets `tileSizePerScaleFactor`), so `c`/`u` above are always `TILE`.
mod osd_port {
    /// `getNumTiles(level)`: how many tile columns/rows OSD's grid has for this level, from
    /// `levelSizes[level]` (OSD's OWN ascending-by-width numbering) — NOT from `scaleFactors`.
    pub fn get_num_tiles(level_sizes: &[(u64, u64)], level: usize, tile: u64) -> (u64, u64) {
        let (w, h) = level_sizes[level];
        (w.div_ceil(tile), h.div_ceil(tile))
    }

    /// `getTileUrl(level, x, y)`: the `(region, size)` IIIF path segments OSD requests, using ONLY
    /// `maxLevel`/`level` power-of-two arithmetic for the tile REGION size (`d = 0.5^(maxLevel -
    /// level)`, so `a = round(tile / d) = tile * 2^(maxLevel-level)`, exact since the exponent is
    /// an integer) — `scaleFactors` never appears in this function at all.
    #[allow(clippy::too_many_arguments)] // mirrors getTileUrl's own parameter list plus context
    pub fn get_tile_url(
        level_sizes: &[(u64, u64)],
        max_level: u64,
        level: usize,
        x: u64,
        y: u64,
        tile: u64,
        width: u64,
        height: u64,
    ) -> (String, String) {
        let (r, o) = level_sizes[level];
        let shift = max_level - level as u64;
        let a = tile << shift;
        let l = a; // tile width == tile height here

        if r < tile && o < tile {
            let size = if r == width && o == height {
                "max".to_string()
            } else {
                format!("{r},{o}")
            };
            return ("full".to_string(), size);
        }

        let tile_x = x * a;
        let tile_y = y * l;
        let region_w = a.min(width - tile_x);
        let region_h = l.min(height - tile_y);
        let region = if x == 0 && y == 0 && region_w == width && region_h == height {
            "full".to_string()
        } else {
            format!("{tile_x},{tile_y},{region_w},{region_h}")
        };

        let size_w = tile.min(r - x * tile);
        let size_h = tile.min(o - y * tile);
        let size = if size_w == width && size_h == height {
            "max".to_string()
        } else {
            format!("{size_w},{size_h}")
        };
        (region, size)
    }
}

/// The full URL set OpenSeadragon would request for a level0 `info.json` advertising `plan_sizes`
/// (via `iiif::osd_level_sizes`'s own reconstruction, exactly as `IIIFTileSource`'s constructor
/// does it), or `None` if it cannot pin `plan_sizes` at all.
fn osd_url_set(
    plan_sizes: &[(u64, u64)],
    scale_factors: &[u64],
    width: u64,
    height: u64,
    tile: u64,
) -> Option<BTreeSet<(String, String)>> {
    let level_sizes = osd_level_sizes(plan_sizes, scale_factors, width, height)?;
    let max_level = (level_sizes.len() - 1) as u64;
    let mut urls = BTreeSet::new();
    for level in 0..level_sizes.len() {
        let (tiles_x, tiles_y) = osd_port::get_num_tiles(&level_sizes, level, tile);
        let (lw, lh) = level_sizes[level];
        if lw < tile && lh < tile {
            urls.insert(osd_port::get_tile_url(
                &level_sizes,
                max_level,
                level,
                0,
                0,
                tile,
                width,
                height,
            ));
            continue;
        }
        for y in 0..tiles_y {
            for x in 0..tiles_x {
                urls.insert(osd_port::get_tile_url(
                    &level_sizes,
                    max_level,
                    level,
                    x,
                    y,
                    tile,
                    width,
                    height,
                ));
            }
        }
    }
    Some(urls)
}

fn enumerated_url_set(info: &ImageInfo) -> BTreeSet<(String, String)> {
    exporter::enumerate_request_space(info)
        .into_iter()
        .map(|r| (r.region_str, r.size_str))
        .collect()
}

/// The one shape `enumerate_request_space`'s own "whole-image derivatives from `sizes`" fallback
/// loop (`crates/exporter/src/enumerate.rs`) may legitimately write beyond what OpenSeadragon
/// requests: a level (from the UNTRIMMED `info.sizes`, which is what that loop iterates) whose
/// dimensions are both `<= tile` but NOT both `< tile` — i.e. the longer edge sits exactly on the
/// tile-size boundary. Real OpenSeadragon's strict `r<c&&o<u` sends such a level down the TILED
/// branch, whose own single-cell result does not always land on `"full"` (see the module doc), so
/// the fallback loop's unconditional `full/{w},{h}` there can be a genuine, harmless, unrequested
/// extra file — a pre-existing quirk in the WRITER, not `iiif::level0`, left alone (out of this
/// task's scope). Anything else in `written \ requested` is an unexplained divergence.
fn boundary_fallback_extras(info: &ImageInfo, tile: u64) -> BTreeSet<(String, String)> {
    info.sizes
        .iter()
        .filter(|&&(w, h)| w <= tile && h <= tile && !(w < tile && h < tile))
        .map(|&(w, h)| {
            let size = if (w, h) == (info.width, info.height) {
                "max".to_string()
            } else {
                format!("{w},{h}")
            };
            ("full".to_string(), size)
        })
        .collect()
}

/// Deterministic dimension pairs covering: 1px edges, small primes/odds, odd values whose halving
/// drifts (999/1023/1025), thin/anisotropic shapes, two large images (one of them a round-3 named
/// regression), the round-4 named regressions (a drifted scale factor lands a level's LONGER edge
/// exactly on the tile-size boundary, where OpenSeadragon's strict `r<c&&o<u` sends it down the
/// TILED branch, not the fits-in-one-tile branch; see the doc comment on the crate-private
/// `osd_tiling_agrees_with_the_export` in `crates/iiif/src/level0.rs`), and a small family of the
/// same shape: thin widths against heights that
/// sit one pixel either side of a tile-size multiple (`2^k * 512`), which is exactly where a
/// drifted, non-power-of-two factor lands an edge on that boundary.
fn dimensions() -> Vec<(u64, u64)> {
    let mut dims = vec![
        (1, 1),
        (3, 3),
        (7, 7),
        (1, 7),
        (7, 1),
        (999, 999),
        (1023, 1023),
        (1025, 1025),
        (1000, 3),
        (3, 1000),
        (4096, 5),
        (5, 4096),
        (100, 4096),
        (4096, 100),
        (2000, 1500),
        (27622, 4614),
        // Round-4 named regressions.
        (255, 8193),
        (3, 2047),
        (5, 1025),
        (63, 4097),
        (65, 16385),
    ];
    for &width in &[3u64, 5, 15, 63, 255] {
        for k in 0..=3u32 {
            let base = 512u64 << k;
            dims.push((width, base - 1));
            dims.push((width, base + 1));
        }
    }
    dims
}

/// Both tile sizes the broad review sweep covered.
const TILE_SIZES: &[u64] = &[512, 256];

/// The property that matters: for every pyramid `level0_sizes` is willing to advertise,
/// OpenSeadragon's real tile URLs (from an independent port of its own arithmetic) must all be
/// present among the files the export writes — no FEWER (a missing one is a 404) — and any file
/// the export writes beyond that must be no MORE than the one documented, harmless fallback shape
/// (a genuine unexplained extra is a dead file nobody asked for, and a sign something else is
/// wrong). Swept across dimensions, both halving directions, 2 tile sizes and 2-8 levels; kept
/// fast (integer arithmetic only, no image decoding) despite two large images in the sweep.
#[test]
fn osd_oracle_agrees_with_the_export_whenever_level0_sizes_accepts() {
    let mut checked = 0usize;
    let mut accepted = 0usize;
    for &(width, height) in &dimensions() {
        for &tile in TILE_SIZES {
            for ceil_halving in [false, true] {
                for levels in 2..=8u32 {
                    let info = build_pyramid(width, height, levels, ceil_halving, tile);
                    checked += 1;
                    let Some(plan) = iiif::level0_sizes(&info) else {
                        continue;
                    };
                    accepted += 1;
                    let written = enumerated_url_set(&info);
                    let requested =
                        osd_url_set(&plan.sizes, &info.scale_factors, width, height, tile).expect(
                            "level0_sizes accepted this pyramid, so OSD must be able to pin it \
                             too",
                        );

                    let missing: Vec<_> = requested.difference(&written).cloned().collect();
                    assert!(
                        missing.is_empty(),
                        "OpenSeadragon would request URLs this export never writes (404 / blank \
                         tile) for {width}x{height}, {levels} levels, tile {tile}, \
                         ceil_halving={ceil_halving}, plan={plan:?}: {missing:?}"
                    );

                    // The export may ALSO write files OpenSeadragon never asks for, but only of
                    // one documented shape (see the module doc): a level whose dimensions are
                    // both `<= tile` but not both `< tile` — the exact boundary where
                    // `enumerate_request_space`'s own "whole-image derivatives" fallback loop
                    // writes a `full/{w},{h}` entry the tiled branch's own single-cell result
                    // does not always produce. Anything else is an unexplained divergence.
                    let expected_extras = boundary_fallback_extras(&info, tile);
                    let unexpected: Vec<_> = written
                        .difference(&requested)
                        .filter(|extra| !expected_extras.contains(*extra))
                        .cloned()
                        .collect();
                    assert!(
                        unexpected.is_empty(),
                        "the export writes files OpenSeadragon never requests, beyond the \
                         documented tile-size-boundary fallback shape, for {width}x{height}, \
                         {levels} levels, tile {tile}, ceil_halving={ceil_halving}, \
                         plan={plan:?}: {unexpected:?}"
                    );
                }
            }
        }
    }
    // Sanity: the sweep actually exercised both the refused and the accepted path, so this test
    // is not vacuously trivial in either direction.
    assert!(checked > 0);
    assert!(
        accepted > 0,
        "the sweep never once accepted a pyramid — the oracle would prove nothing"
    );
}

/// Named regression (round 3): floor-halving a 2000x1500 image to 6 levels drifts the coarsest
/// level to 62x46, whose implied height ratio `round(1500/46) == 33 != 32` — the deleted
/// per-axis ratio test refused this, even though it exported and served correctly before this
/// task's earlier commits, because that level fits in a single tile and OSD never evaluates its
/// tile-region arithmetic there at all.
#[test]
fn regression_2000x1500_floor_halved_to_6_levels_is_accepted() {
    let info = build_pyramid(2000, 1500, 6, false, 512);
    assert!(
        iiif::level0_sizes(&info).is_some(),
        "a real, previously-exportable pyramid must not be refused"
    );
}

/// Named regression (round 3): ceil-halving a 1025x1025 image to 6 levels drifts the coarsest
/// declared factor to 31 (not the nominal 32) — the deleted unconditional `scaleFactors[i] ==
/// 2^i` test refused this outright, regardless of whether that level ever has more than one tile.
#[test]
fn regression_1025x1025_ceil_halved_to_6_levels_is_accepted() {
    let info = build_pyramid(1025, 1025, 6, true, 512);
    assert!(
        iiif::level0_sizes(&info).is_some(),
        "a real, previously-exportable pyramid must not be refused"
    );
}
