//! What a level 0 `info.json` may advertise, given what OpenSeadragon will do with it.
//!
//! A static export serves a finite set of files. `info.json` must therefore advertise exactly what
//! is there: IIIF Image API 3.0 defines `sizes` as the sizes "that the server has available", and
//! compliance level 0 requires `full/max/0/default.jpg` to resolve. Trimming `sizes` to the truth
//! is not free, though, because the viewer ziv ships reads that array to learn the pyramid's real
//! level dimensions.
//!
//! From `crates/viewer-assets/assets/viewer/openseadragon/openseadragon.min.js`,
//! `IIIFTileSource`'s constructor:
//!
//! ```js
//! if (this.sizes) {
//!   l = this.sizes.length;
//!   if (l === e.maxLevel || l === e.maxLevel + 1) {
//!     this.levelSizes = this.sizes.slice().sort((a, b) => a.width - b.width);
//!     if (l === e.maxLevel) this.levelSizes.push({ width: this.width, height: this.height });
//!   }
//! }
//! ```
//!
//! and in `getTileUrl`, `levelSizes[level]` is used when it exists and `ceil(width * 2^-k)` when it
//! does not. An OME-Zarr pyramid's levels are frequently not exact power-of-two divisions, so
//! losing `levelSizes` changes the URLs OpenSeadragon asks for, and every one of them would miss a
//! tree whose files were enumerated from the real level sizes. The result is not a slightly less
//! conformant export: it is a blank viewer.

use crate::ImageInfo;

/// The level sizes OpenSeadragon will use, given what `info.json` declares, ascending by width the
/// way OSD sorts them. `None` means it will not trust `sizes` at all and will invent its own.
///
/// This is `IIIFTileSource`'s constructor reproduced exactly, including `maxLevel`'s arithmetic
/// (`Math.round(Math.log(maxScaleFactor) * Math.LOG2E)`).
pub fn osd_level_sizes(
    sizes: &[(u64, u64)],
    scale_factors: &[u64],
    width: u64,
    height: u64,
) -> Option<Vec<(u64, u64)>> {
    let max_factor = *scale_factors.iter().max()?;
    // JavaScript gets `Math.round(Math.log(0) * Math.LOG2E) == -Infinity`, which equals no array
    // length, so OSD assigns no `levelSizes` at all. Rust would saturate the cast to 0 and match
    // a length of 0 or 1 instead. Unreachable through ziv's own engine, which clamps every factor
    // to at least 1, but this function is `pub` and claims to reproduce OSD exactly.
    if max_factor == 0 {
        return None;
    }
    let max_level = (max_factor as f64).log2().round() as usize;
    let n = sizes.len();
    if n != max_level && n != max_level + 1 {
        return None;
    }
    let mut level_sizes = sizes.to_vec();
    // Width only, and STABLE, both deliberately: OSD's comparator is `(a, b) => a.width - b.width`
    // and `Array.prototype.sort` has been stable since ES2019. An anisotropic pyramid whose width
    // bottoms out has tied widths at different heights, and the tie order is what decides whether
    // the reconstruction matches. Do not "tidy" this into `sort_unstable_by_key`.
    level_sizes.sort_by_key(|&(w, _)| w);
    if n == max_level {
        level_sizes.push((width, height));
    }
    Some(level_sizes)
}

/// What a level 0 `info.json` should advertise for one image.
#[derive(Debug, Clone, PartialEq)]
pub struct Level0Sizes {
    /// The `sizes` array to emit, finest-first as `ImageInfo` stores it. Every entry is a whole
    /// image the export writes.
    pub sizes: Vec<(u64, u64)>,
    /// `maxWidth`: the largest whole image this export will produce.
    pub max_width: u64,
    /// `maxHeight`, the companion to `max_width`.
    pub max_height: u64,
    /// Whether the full-resolution entry was dropped. False does NOT by itself mean the
    /// full-resolution whole image is expensive to write: when the finest level already fits in
    /// a single tile, OpenSeadragon already requests `full/max` at that size on its own, so
    /// writing it is free (`sample_v04`/`sample_labels` are exactly this shape). The expensive
    /// case is narrower — untrimmed AND the finest level does not fit in one tile — which is why
    /// `exporter::writer` gates its cost warning on `!trimmed && !finest_level_fits_one_tile(..)`
    /// rather than on `!trimmed` alone.
    pub trimmed: bool,
}

/// The largest whole image an export will encode, in pixels. About 8000x8000, roughly 190 MB to
/// hold in memory while encoding. Beyond this a tree serves tiles only: see `Level0Sizes::within_budget`.
pub const MAX_WHOLE_IMAGE_PIXELS: u64 = 64_000_000;

/// JPEG cannot encode a dimension above this, whatever the budget says.
pub const MAX_WHOLE_IMAGE_EDGE: u64 = 65_535;

/// Whether a single `width`x`height` whole image is within what an export will encode, independent
/// of any [`Level0Sizes`] plan. [`Level0Sizes::within_budget`] is this check applied to a plan's own
/// bound; exposed separately because a manifest's fallback body (the SMALLEST level's whole image,
/// `exporter::enumerate::smallest_level_whole_image`) needs the identical check even when the tree
/// as a whole is over budget and so has no `Level0Sizes::within_budget` of its own to ask.
pub fn whole_image_within_budget(width: u64, height: u64) -> bool {
    width <= MAX_WHOLE_IMAGE_EDGE
        && height <= MAX_WHOLE_IMAGE_EDGE
        && width.saturating_mul(height) <= MAX_WHOLE_IMAGE_PIXELS
}

impl Level0Sizes {
    /// Whether the largest advertised whole image is within what an export will encode. When it is
    /// not, the tree keeps its untrimmed `sizes`, declares no bound, and writes no whole images:
    /// all of it or none, because `sizes` cannot be trimmed by more than one entry, so advertising
    /// a size whose file was skipped would reintroduce the dishonesty this design removes.
    pub fn within_budget(&self) -> bool {
        whole_image_within_budget(self.max_width, self.max_height)
    }
}

/// Whether OpenSeadragon's OWN tile-region arithmetic for a rebuilt `level_sizes` array (OSD's
/// ascending-by-width order, as [`osd_level_sizes`] returns it — coarsest first) agrees with the
/// region size [`crate::level0`]'s caller (`enumerate_request_space` in the `exporter` crate)
/// actually writes, given the declared `scale_factors` (finest first, `ImageInfo`/`info.json`
/// order), the full image `(width, height)`, and `tile_size`.
///
/// `osd_level_sizes` alone proves OpenSeadragon will read `sizes` as `levelSizes` and reconstruct
/// the given LEVEL DIMENSIONS, but that is not enough: OpenSeadragon's tile-region math in
/// `getTileUrl` (`d = 0.5^(maxLevel-level)`, region size `a = round(tileWidth / d)`) is driven by
/// `maxLevel`/`level` power-of-two arithmetic ALONE — it never looks at the declared
/// `scaleFactors` VALUE at all. `enumerate_request_space`, on the other hand, writes files sized
/// by `tile_size * scale_factors[level]` (this crate's own declared, possibly non-power-of-two,
/// factor). The two formulas can disagree, and this is proven per level, per axis.
///
/// A level's `getTileUrl` only skips ALL region arithmetic when `IIIFTileSource`'s own condition
/// is met on BOTH axes at once: `if(r<c&&o<u){h=r===this.width&&o===this.height?"max":r+","+o;
/// s="full"}` (`r`/`o` the level's width/height, `c`/`u` the tile size — note the STRICT `<`). A
/// level whose LONGER edge equals the tile size exactly (both edges `<= tile`, not both `<`) does
/// NOT take that branch: it falls into the tiled branch below, which computes a region even for a
/// single-cell axis and clips it against the full image — `a=Math.min(a,this.width-e)` — so the
/// value that actually reaches the URL is `min(region, full)`, not the raw region. This is why a
/// per-axis `tiles > 1` gate is not enough on its own: a single-cell axis on a level that is NOT
/// wholly single-tile still has its (clamped) region evaluated. Genuinely multi-tile axes
/// (`tiles > 1`) still need an EXACT match — clamping only matters at the one edge tile a
/// single-cell axis has, since a middle tile's boundary is never clipped.
///
/// A level that DOES satisfy the joint `r<c&&o<u` on both axes evaluates no region arithmetic at
/// all — this is precisely what a stricter, UNCONDITIONAL version of this check over-refused:
/// real pyramids drift from an exact power-of-two ratio at levels small enough that BOTH axes
/// land under the tile size (see the module's test-time regressions).
///
/// Regardless of that, no tile OpenSeadragon's grid walks on an axis may START past the image's
/// edge (`(tiles - 1) * region < full`) — this is what refuses a pyramid that declares a factor
/// for an axis that never actually downsamples by it (the tile grid still walks as many rows or
/// columns as the level's own, real dimension implies, and the last one starts beyond the edge).
///
/// `level_sizes[e]` (OSD's own numbering, coarsest first, `e` from `0` to `maxLevel`) is the SAME
/// physical level as `scale_factors[maxLevel - e]` (finest first): the two numbering schemes just
/// count from opposite ends. `maxLevel = level_sizes.len() - 1`, always, for any array
/// `osd_level_sizes` actually returned (both of its arms produce exactly `maxLevel + 1` entries).
///
/// Also requires `level_sizes` to be non-decreasing on BOTH axes as `e` increases: matching on
/// array shape alone (length, sort order, endpoint) tolerates geometric nonsense, because
/// `osd_level_sizes` sorts by WIDTH only, so a pyramid where one axis grows the wrong way as the
/// other shrinks (an OME-Zarr whose second array is half as wide but ten times as tall — reachable
/// because `scale_factors_of`, `crates/tiling/src/engine.rs`, derives factors from width alone)
/// can still reconstruct an array that ends at the full image and never exceeds it. This is the
/// ONE rule shared, via this function, by both the emitter (`level0_sizes`, through its
/// `reconstructs` closure) and the validator (`crate::conformance::check_level0_pinning`) — it
/// lives here, not duplicated in `conformance.rs`, so the two cannot drift apart again.
pub(crate) fn osd_tiling_agrees_with_the_export(
    level_sizes: &[(u64, u64)],
    scale_factors: &[u64],
    width: u64,
    height: u64,
    tile_size: u64,
) -> bool {
    if tile_size == 0 || level_sizes.is_empty() || level_sizes.len() != scale_factors.len() {
        return false;
    }
    for pair in level_sizes.windows(2) {
        let (prev_w, prev_h) = pair[0];
        let (next_w, next_h) = pair[1];
        if next_w < prev_w || next_h < prev_h {
            return false;
        }
    }
    let max_level = (level_sizes.len() - 1) as u64;
    for (e, &(level_w, level_h)) in level_sizes.iter().enumerate() {
        let i = max_level as usize - e;
        let sf = scale_factors[i];
        let shift = max_level - e as u64;
        let Some(pow2) = 1u64.checked_shl(shift as u32) else {
            return false;
        };
        for (level_dim, full) in [(level_w, width), (level_h, height)] {
            let tiles = level_dim.div_ceil(tile_size);
            let Some(region_export) = tile_size.checked_mul(sf) else {
                return false;
            };
            if tiles > 1 {
                let Some(region_osd) = tile_size.checked_mul(pow2) else {
                    return false;
                };
                if region_osd != region_export {
                    return false;
                }
            } else if !(level_w < tile_size && level_h < tile_size) {
                // This axis is a single cell, but the LEVEL as a whole does not take OSD's
                // fits-in-one-tile branch (the other axis, or this one, is `>= tile_size`), so
                // OSD still evaluates this axis's region and clips it to the image edge.
                let Some(region_osd) = tile_size.checked_mul(pow2) else {
                    return false;
                };
                if region_osd.min(full) != region_export.min(full) {
                    return false;
                }
            }
            let span = tiles.saturating_sub(1).saturating_mul(region_export);
            if span >= full {
                return false;
            }
        }
    }
    true
}

/// Whether `info`'s finest (native-resolution) level fits inside a single tile on both axes, the
/// one shape [`level0_sizes`] never trims (see its doc comment): OpenSeadragon requests
/// `full/max` for such a level directly, so writing that level's whole image costs nothing beyond
/// what OSD already asks for. Shared with `exporter::writer`, which uses the identical condition
/// to decide whether writing a level0 tree's full-resolution whole image is a real extra cost
/// worth warning about, rather than reimplementing this arithmetic a second time.
pub fn finest_level_fits_one_tile(info: &ImageInfo) -> bool {
    info.width <= info.tile_size && info.height <= info.tile_size
}

/// Decide what `info` may advertise at level 0, or `None` if OpenSeadragon cannot pin this
/// pyramid at all and the export would be silently broken.
///
/// Drops the full-resolution entry when OSD provably reconstructs it (the `l == maxLevel` arm
/// above), and never drops more than that one: a second drop matches neither arm. Requires, on
/// the RAW (untrimmed) pyramid the tiling engine built: at least one level, `sizes` 1:1 with
/// `scale_factors`, and the finest entry equal to the literal full image — then requires, on
/// whichever candidate reconstructs, that [`osd_tiling_agrees_with_the_export`] holds too:
/// `osd_level_sizes` alone proves the ADVERTISED `sizes` array is internally consistent, but not
/// that OpenSeadragon's own tile-region arithmetic for it agrees with what the export writes.
pub fn level0_sizes(info: &ImageInfo) -> Option<Level0Sizes> {
    if info.sizes.is_empty() || info.sizes.len() != info.scale_factors.len() {
        return None;
    }
    if info.sizes[0] != (info.width, info.height) {
        return None;
    }
    // The truth this has to preserve: the real pyramid, ascending, as OSD sorts it.
    let truth: Vec<(u64, u64)> = info.sizes.iter().rev().copied().collect();
    let reconstructs = |candidate: &[(u64, u64)]| -> bool {
        let Some(level_sizes) =
            osd_level_sizes(candidate, &info.scale_factors, info.width, info.height)
        else {
            return false;
        };
        level_sizes == truth
            && osd_tiling_agrees_with_the_export(
                &level_sizes,
                &info.scale_factors,
                info.width,
                info.height,
                info.tile_size,
            )
    };

    // The one case the trim must never apply to: when the finest (full-resolution) level's own
    // tile grid is a single cell, OpenSeadragon requests `full/max` for it directly (see
    // `crate::exporter`'s `enumerate_request_space` doc comment — this holds at the exact
    // tile-size boundary too, not just strictly under it, because for the FINEST level the level
    // dimensions ARE `width`/`height`, so a single-cell grid there always resolves to region
    // `full`, size `max`, on both branches of OSD's `getTileUrl`). That file is already the
    // correct, full native-resolution image; advertising a smaller bound would misdescribe it
    // (IIIF requires `max` to respect a declared `maxWidth`/`maxHeight`), and copying a smaller
    // rendering over it would blur the one file that must stay exact. So: never trim here,
    // regardless of levels or budget — the untrimmed branch below always reconstructs in this
    // case, and the file this bound describes was going to be written whole either way.
    let finest_level_is_single_tile = finest_level_fits_one_tile(info);

    if !finest_level_is_single_tile && info.sizes.len() > 1 && reconstructs(&info.sizes[1..]) {
        let sizes = info.sizes[1..].to_vec();
        let (max_width, max_height) = sizes[0];
        return Some(Level0Sizes {
            sizes,
            max_width,
            max_height,
            trimmed: true,
        });
    }
    // Reached for a single-level image (spec D6) OR a multi-level pyramid whose finest level is a
    // single tile (see above): in both cases the untrimmed array reconstructs whenever the trimmed
    // one would have, so this branch is what advertises the full pyramid rather than dropping the
    // one entry OSD would otherwise reconstruct on its own. The bound comes from `sizes[0]` rather
    // than `width`/`height` so that it names a size this array actually advertises, even if a
    // caller hands us an `ImageInfo` whose finest entry is not the full image.
    if reconstructs(&info.sizes) {
        let (max_width, max_height) = *info.sizes.first()?;
        return Some(Level0Sizes {
            sizes: info.sizes.clone(),
            max_width,
            max_height,
            trimmed: false,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pyramid halving down from `width`x`height`, finest first, as `ImageInfo` stores it.
    fn pyramid(width: u64, height: u64, levels: u32) -> (Vec<(u64, u64)>, Vec<u64>) {
        let mut sizes = Vec::new();
        let mut factors = Vec::new();
        for k in 0..levels {
            let f = 1u64 << k;
            sizes.push((
                (width as f64 / f as f64).round() as u64,
                (height as f64 / f as f64).round() as u64,
            ));
            factors.push(f);
        }
        (sizes, factors)
    }

    #[test]
    fn untrimmed_sizes_reconstruct_the_pyramid() {
        let (sizes, factors) = pyramid(1024, 1024, 2);
        // OSD sorts ascending, so the expectation is the pyramid coarsest-first.
        let want: Vec<(u64, u64)> = sizes.iter().rev().copied().collect();
        assert_eq!(osd_level_sizes(&sizes, &factors, 1024, 1024), Some(want));
    }

    /// The one trim the spec permits: OSD pushes `(width, height)` back on for us.
    #[test]
    fn dropping_only_the_full_resolution_entry_still_reconstructs() {
        let (sizes, factors) = pyramid(1024, 1024, 2);
        let want: Vec<(u64, u64)> = sizes.iter().rev().copied().collect();
        assert_eq!(
            osd_level_sizes(&sizes[1..], &factors, 1024, 1024),
            Some(want)
        );
    }

    /// Dropping a second entry lands on neither arm, and OSD invents power-of-two sizes instead.
    #[test]
    fn dropping_two_entries_loses_the_reconstruction() {
        let (sizes, factors) = pyramid(2048, 2048, 3);
        assert_eq!(osd_level_sizes(&sizes[2..], &factors, 2048, 2048), None);
    }

    #[test]
    fn a_single_level_image_is_never_trimmed() {
        let info = info_for(600, 600, vec![(600, 600)], vec![1]);
        let plan = level0_sizes(&info).unwrap();
        assert_eq!(plan.sizes, vec![(600, 600)]);
        assert_eq!((plan.max_width, plan.max_height), (600, 600));
        assert!(!plan.trimmed);
    }

    #[test]
    fn a_pyramid_is_trimmed_to_its_largest_sub_full_level() {
        let (sizes, factors) = pyramid(1024, 1024, 2);
        let info = info_for(1024, 1024, sizes, factors);
        let plan = level0_sizes(&info).unwrap();
        assert_eq!(plan.sizes, vec![(512, 512)]);
        assert_eq!((plan.max_width, plan.max_height), (512, 512));
        assert!(plan.trimmed);
    }

    /// The trim's one exception: a multi-level pyramid whose FINEST (native-resolution) level
    /// itself fits inside a single tile (64x64 against the 512px tile `info_for` uses).
    /// OpenSeadragon requests `full/max` for that level directly, and that file is already the
    /// correct, full native rendering — trimming `sizes` here would declare a smaller `maxWidth`
    /// than what `full/max` actually contains, which is dishonest the other way around from what
    /// the trim exists to prevent. `sample_v04.ome.zarr`/`sample_labels.ome.zarr` are exactly this
    /// shape (64x64, 2 levels).
    #[test]
    fn a_pyramid_whose_finest_level_fits_one_tile_is_never_trimmed() {
        let (sizes, factors) = pyramid(64, 64, 2);
        let info = info_for(64, 64, sizes, factors);
        let plan = level0_sizes(&info).unwrap();
        assert_eq!(plan.sizes, vec![(64, 64), (32, 32)]);
        assert_eq!((plan.max_width, plan.max_height), (64, 64));
        assert!(!plan.trimmed);
    }

    /// The same exception at the exact tile-size BOUNDARY (finest level == the tile size on both
    /// axes, not strictly under it): still a single-cell grid (`ceil(512/512) == 1`), so OSD still
    /// requests `full/max` for it directly, and the trim must still not apply.
    #[test]
    fn a_pyramid_whose_finest_level_exactly_equals_the_tile_size_is_never_trimmed() {
        let (sizes, factors) = pyramid(512, 512, 2);
        let info = info_for(512, 512, sizes, factors);
        let plan = level0_sizes(&info).unwrap();
        assert_eq!(plan.sizes, vec![(512, 512), (256, 256)]);
        assert_eq!((plan.max_width, plan.max_height), (512, 512));
        assert!(!plan.trimmed);
    }

    /// A pyramid whose finest level does NOT fit in a single tile (bigger than the 512px tile on
    /// at least one axis) is still trimmed as before: the exception above must not over-apply.
    #[test]
    fn a_pyramid_whose_finest_level_does_not_fit_one_tile_is_still_trimmed() {
        let (sizes, factors) = pyramid(1024, 512, 2);
        let info = info_for(1024, 512, sizes, factors);
        let plan = level0_sizes(&info).unwrap();
        assert!(plan.trimmed, "{plan:?}");
    }

    /// [`whole_image_within_budget`] is the free-function check `Level0Sizes::within_budget`
    /// delegates to; exercised directly here since `exporter`'s manifest-fallback budget check
    /// calls it on a bare `(width, height)` with no `Level0Sizes` to hand.
    #[test]
    fn whole_image_within_budget_matches_the_level0sizes_method() {
        assert!(whole_image_within_budget(8000, 8000));
        assert!(!whole_image_within_budget(8001, 8000));
        assert!(!whole_image_within_budget(100_000, 100_000));
    }

    /// Every advertised size must be one the export will write, and the bound must hold.
    #[test]
    fn every_advertised_size_is_within_the_declared_bound() {
        let (sizes, factors) = pyramid(2048, 2048, 4);
        let info = info_for(2048, 2048, sizes, factors);
        let plan = level0_sizes(&info).unwrap();
        for &(w, h) in &plan.sizes {
            assert!(w <= plan.max_width && h <= plan.max_height, "{w}x{h}");
        }
        assert_eq!(plan.sizes[0], (plan.max_width, plan.max_height));
    }

    /// A pyramid OpenSeadragon cannot pin at all: factors of 3, so its `maxLevel` arithmetic
    /// (`round(log2(9)) == 3`) matches neither 3 nor 4 levels. Refusing is the point: exporting
    /// it would silently produce a tree whose own viewer requests files it does not contain.
    #[test]
    fn a_pyramid_openseadragon_cannot_pin_is_refused() {
        let info = info_for(
            900,
            900,
            vec![(900, 900), (300, 300), (100, 100)],
            vec![1, 3, 9],
        );
        assert_eq!(level0_sizes(&info), None);
    }

    /// OpenSeadragon's tile-region math (`getTileUrl`'s `d = 0.5^(maxLevel-level)`) assumes the
    /// pyramid's declared factors literally ARE powers of two, regardless of what `scaleFactors`
    /// says: a factor of 3 still gets treated as level arithmetic based on `maxLevel`, not on the
    /// value 3 itself. Large enough that the factor-3 level spans more than one tile (so
    /// OpenSeadragon's per-tile region math is actually evaluated there, not skipped as a
    /// single-cell level the way a smaller version of this same pyramid would be — see
    /// `osd_tiling_agrees_with_the_export`'s doc comment on why the check is conditional).
    #[test]
    fn a_pyramid_with_a_non_power_of_two_factor_is_refused() {
        let info = info_for(
            7680,
            7680,
            vec![(7680, 7680), (3840, 3840), (2560, 2560)],
            vec![1, 2, 3],
        );
        assert_eq!(level0_sizes(&info), None);
    }

    /// Round-3 regression: floor-halving 2000x1500 to 6 levels drifts the coarsest level to
    /// 62x46, whose implied height ratio (`round(1500/46) == 33 != 32`) an earlier, unconditional
    /// per-axis ratio test refused outright — even though that level fits in a single tile, where
    /// OpenSeadragon never evaluates any per-tile region arithmetic at all, so the drift is
    /// invisible to it. A real, previously-exportable pyramid must not be refused.
    #[test]
    fn a_2000x1500_floor_halved_pyramid_to_6_levels_is_accepted() {
        let (sizes, factors) = halved_pyramid(2000, 1500, 6, false);
        let info = info_for(2000, 1500, sizes, factors);
        assert!(level0_sizes(&info).is_some());
    }

    /// Round-3 regression: ceil-halving 1025x1025 to 6 levels drifts the coarsest declared factor
    /// to 31 (not the nominal 32) — an earlier, unconditional `scaleFactors[i] == 2^i` test
    /// refused this outright, regardless of whether that level ever spans more than one tile.
    #[test]
    fn a_1025x1025_ceil_halved_pyramid_to_6_levels_is_accepted() {
        let (sizes, factors) = halved_pyramid(1025, 1025, 6, true);
        let info = info_for(1025, 1025, sizes, factors);
        assert!(level0_sizes(&info).is_some());
    }

    /// Round-4 regression, "factor above": floor-halving 255x8193 to 5 levels lands the coarsest
    /// level at (15, 512) — its longer edge (512) equals the tile size EXACTLY, so real
    /// OpenSeadragon's strict `r<c&&o<u` sends it down the tiled branch, not the fits-in-one-tile
    /// branch, and evaluates a region there. The declared factor (17, from `round(8193/512)`)
    /// implies a region OpenSeadragon's own arithmetic disagrees with, so it would request
    /// `0,0,255,8192/15,512/0/default.jpg`, a file `enumerate_request_space` never writes.
    #[test]
    fn a_255x8193_floor_halved_pyramid_to_5_levels_is_refused() {
        let (sizes, factors) = halved_pyramid(255, 8193, 5, false);
        let info = info_for(255, 8193, sizes, factors);
        assert_eq!(level0_sizes(&info), None);
    }

    /// Round-4 regression, "factor below": ceil-halving 3x2047 to 3 levels lands the coarsest
    /// level at (1, 512), its longer edge again exactly the tile size. Here the drift runs the
    /// other way: the export writes a tile OpenSeadragon never asks for. Still refused, because
    /// nothing about `level0_sizes` can tell "safe by coincidence of a separate writer fallback"
    /// apart from "unsafe" from declared metadata alone.
    #[test]
    fn a_3x2047_ceil_halved_pyramid_to_3_levels_is_refused() {
        let (sizes, factors) = halved_pyramid(3, 2047, 3, true);
        let info = info_for(3, 2047, sizes, factors);
        assert_eq!(level0_sizes(&info), None);
    }

    /// A pyramid built by repeated halving (floor or ceil), the way a real multiscale pyramid is
    /// built — NOT `width / 2^k` freshly computed each time, so drift compounds realistically.
    /// `scale_factors[i]` is derived from WIDTH ALONE, `round(width/level_i_width).max(1)`,
    /// matching `crates/tiling/src/engine.rs`'s `scale_factors_of` exactly.
    fn halved_pyramid(
        width: u64,
        height: u64,
        levels: u32,
        ceil_halving: bool,
    ) -> (Vec<(u64, u64)>, Vec<u64>) {
        let mut sizes = Vec::with_capacity(levels as usize);
        let (mut w, mut h) = (width, height);
        for _ in 0..levels {
            sizes.push((w, h));
            w = if ceil_halving { w.div_ceil(2) } else { w / 2 }.max(1);
            h = if ceil_halving { h.div_ceil(2) } else { h / 2 }.max(1);
        }
        let factors = sizes
            .iter()
            .map(|&(lw, _)| ((width as f64 / lw as f64).round() as u64).max(1))
            .collect();
        (sizes, factors)
    }

    /// An x-only downsampled pyramid: width halves but height never does. `osd_level_sizes`
    /// happily reconstructs this (the array is monotone and ends at the full image), but
    /// OpenSeadragon would still request a y-range this image doesn't have.
    #[test]
    fn an_x_only_downsampled_pyramid_is_refused() {
        let info = info_for(1024, 1000, vec![(1024, 1000), (512, 1000)], vec![1, 2]);
        assert_eq!(level0_sizes(&info), None);
    }

    /// The reviewer's exact reproduction: `scale_factors_of` derives factors from width ALONE
    /// (`crates/tiling/src/engine.rs`'s `scale_factors_of`), so an OME-Zarr whose second array is
    /// half as wide but TEN TIMES as tall reconstructs an array that is non-decreasing in width
    /// (100 -> 50 as declared, sorted ascending it is 50, 100) while height goes the WRONG way as
    /// the level gets coarser (10 -> 100). `check_level0_pinning` in `crate::conformance` already
    /// rejects this (`sizes advertises (50, 100), taller than maxHeight 10`); `level0_sizes` must
    /// refuse it too, for the same reason, rather than advertise a `sizes` array its own
    /// conformance check would reject.
    #[test]
    fn an_axis_growing_the_wrong_way_pyramid_is_refused() {
        let info = info_for(100, 10, vec![(100, 10), (50, 100)], vec![1, 2]);
        assert_eq!(level0_sizes(&info), None);
    }

    /// The finest entry must be the literal full image, not merely something OSD's array-shape
    /// check happens to tolerate.
    #[test]
    fn a_finest_entry_that_is_not_the_full_image_is_refused() {
        let info = info_for(
            1024,
            768,
            vec![(999, 999), (512, 384), (256, 192)],
            vec![1, 2, 4],
        );
        assert_eq!(level0_sizes(&info), None);
    }

    /// A plan whose bound is a small, ordinary image is within budget.
    #[test]
    fn a_512x512_plan_is_within_budget() {
        assert!(plan_for(512, 512).within_budget());
    }

    /// Exactly 64 megapixels, the budget's own edge: still within budget.
    #[test]
    fn an_8000x8000_plan_is_exactly_within_budget() {
        assert!(plan_for(8000, 8000).within_budget());
    }

    /// One pixel over the 64 megapixel budget.
    #[test]
    fn an_8001x8000_plan_is_not_within_budget() {
        assert!(!plan_for(8001, 8000).within_budget());
    }

    /// Tiny area, but a single edge beyond what JPEG can encode: still refused.
    #[test]
    fn a_70000x10_plan_is_not_within_budget() {
        assert!(!plan_for(70_000, 10).within_budget());
    }

    /// Far beyond both the edge limit and the pixel budget.
    #[test]
    fn a_100000x100000_plan_is_not_within_budget() {
        assert!(!plan_for(100_000, 100_000).within_budget());
    }

    /// A minimal `Level0Sizes` bounded at `width`x`height`, for exercising `within_budget` alone.
    fn plan_for(width: u64, height: u64) -> Level0Sizes {
        Level0Sizes {
            sizes: vec![(width, height)],
            max_width: width,
            max_height: height,
            trimmed: false,
        }
    }

    fn info_for(
        width: u64,
        height: u64,
        sizes: Vec<(u64, u64)>,
        scale_factors: Vec<u64>,
    ) -> ImageInfo {
        ImageInfo {
            id: ".".into(),
            width,
            height,
            tile_size: 512,
            scale_factors,
            sizes,
        }
    }
}
