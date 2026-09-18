//! Turning a plane of label VALUES into pixels.
//!
//! A label image's samples are object identifiers, not intensities, and that changes both halves
//! of the render:
//!
//! - **Resampling must be nearest-neighbour.** Averaging label 3 and label 7 gives label 5, a
//!   different object that the filter invented. See [`crate::resample::resize_nearest`].
//! - **Colour is a lookup, not a ramp.** There is no "between" two labels to interpolate.
use crate::TileError;
use ndarray::Array2;
use std::collections::HashMap;
use zarr_core::LabelInfo;

/// How to turn a label value into a colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LabelPalette {
    /// A deterministic distinct colour per value, ignoring the image's own colour table.
    ///
    /// This is the default, and the reason is empirical rather than aesthetic. In the IDR sample
    /// ziv was built against (`idr0062A-6001240`), all 61 declared label values carry the
    /// identical RGBA `(128, 128, 128, 128)`, so a spec-literal render is one flat grey blob that
    /// tells the viewer nothing about the segmentation they asked to see. Every labelled IDR image
    /// checked since does the same: IDR's curation scripts give each class of mask one hard-coded
    /// colour, and `omero-cli-zarr` copies it into `image-label` unchanged. In that data a declared
    /// colour names a kind of object, never the object, so honouring it by default optimises for
    /// the letter of the metadata over the person looking at the screen. `docs/labels.md` has the
    /// evidence.
    ///
    /// [`LabelPalette::Table`] is one identifier component away for anyone who does need the
    /// declared colours.
    #[default]
    Distinct,
    /// The colours the image itself declares in `image-label`. Values with no entry are
    /// transparent — see [`LabelInfo::color_for`].
    Table,
}

impl LabelPalette {
    /// Parses the palette component of a `label=NAME[:PALETTE]` identifier.
    ///
    /// An unrecognised name is an error rather than a silent fallback: a typo'd palette that
    /// quietly rendered the other one would be indistinguishable from the feature not working.
    pub fn parse(spec: Option<&str>) -> Result<Self, TileError> {
        match spec {
            None | Some("distinct") | Some("auto") => Ok(LabelPalette::Distinct),
            Some("table") => Ok(LabelPalette::Table),
            Some(other) => Err(TileError::OutOfRange(format!(
                "unknown label palette {other:?}; expected \"distinct\" or \"table\""
            ))),
        }
    }
}

/// The conjugate of the golden ratio. Stepping a hue circle by an irrational fraction of a turn
/// spreads any run of consecutive values about as far apart as a hue circle allows, and never
/// repeats — which is what makes neighbouring object IDs land on visibly different colours. Used
/// the same way by napari and vizarr for exactly this problem.
const GOLDEN_RATIO_CONJUGATE: f64 = 0.618_033_988_749_895;

/// The colour for one label value.
///
/// Value 0 is transparent in every palette: OME-NGFF treats it as background by convention, and
/// painting it would cover the image with a solid sheet of colour.
#[must_use]
pub fn label_rgba(info: &LabelInfo, palette: LabelPalette, value: i64) -> [u8; 4] {
    if value == 0 {
        return [0, 0, 0, 0];
    }
    match palette {
        LabelPalette::Table => info.color_for(value),
        LabelPalette::Distinct => distinct_rgba(value),
    }
}

/// A deterministic colour for a label value: the same value is the same colour in every tile, at
/// every zoom, in every session, which is what lets someone track one object while panning.
///
/// Hue steps by the golden ratio. Saturation and value alternate on short cycles so that two
/// values whose hues do land close together still differ in another dimension, and so the palette
/// as a whole does not read as a single fluorescent band.
fn distinct_rgba(value: i64) -> [u8; 4] {
    let n = value.unsigned_abs();
    let hue = (n as f64 * GOLDEN_RATIO_CONJUGATE).fract();
    let saturation = if n.is_multiple_of(2) { 0.90 } else { 0.65 };
    let brightness = if (n / 2).is_multiple_of(2) {
        1.00
    } else {
        0.78
    };
    let [r, g, b] = hsv_to_rgb(hue, saturation, brightness);
    [r, g, b, 255]
}

fn hsv_to_rgb(h: f64, s: f64, v: f64) -> [u8; 3] {
    let sector = h.rem_euclid(1.0) * 6.0;
    let index = sector.floor();
    let frac = sector - index;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * frac);
    let t = v * (1.0 - s * (1.0 - frac));
    let (r, g, b) = match index as u32 % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    let byte = |x: f64| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    [byte(r), byte(g), byte(b)]
}

/// Validates the opacity component of an `overlay=` identifier.
///
/// `None` means the caller did not ask, which is fully opaque: `overlay=nuclei` should draw the
/// mask as the image declares it, and a viewer that wants it faded says so. Out of range is an
/// error rather than a clamp, for the same reason an unknown palette is: a request that silently
/// became a different request is indistinguishable from the feature not working.
pub fn parse_opacity(spec: Option<f64>) -> Result<f64, TileError> {
    match spec {
        None => Ok(1.0),
        Some(v) if v.is_finite() && (0.0..=1.0).contains(&v) => Ok(v),
        Some(v) => Err(TileError::OutOfRange(format!(
            "label opacity {v} out of range; expected 0 to 1"
        ))),
    }
}

/// Map a plane of label values to tightly-packed RGBA8 (row-major, len = rows*cols*4).
///
/// The plane arrives as `f64` because that is what the widening zarr read produces for every
/// source dtype. Values are rounded to the nearest integer; a non-finite sample (only reachable
/// if a label image is stored as float, which is legal if odd) is treated as background rather
/// than as some arbitrary integer cast.
///
/// `opacity` scales every colour's alpha, and is applied inside the memo so it costs one multiply
/// per distinct label value rather than one per pixel.
///
/// Colours are memoised per value. A segmentation has orders of magnitude fewer objects than
/// pixels, so the table is small and the per-pixel cost collapses to a hash lookup.
#[must_use]
pub fn colorize(
    values: &Array2<f64>,
    info: &LabelInfo,
    palette: LabelPalette,
    opacity: f64,
) -> Vec<u8> {
    let (rows, cols) = values.dim();
    let mut out = vec![0u8; rows * cols * 4];
    let mut memo: HashMap<i64, [u8; 4]> = HashMap::new();
    for r in 0..rows {
        for c in 0..cols {
            let v = values[[r, c]];
            if !v.is_finite() {
                continue;
            }
            let value = v.round() as i64;
            let rgba = *memo.entry(value).or_insert_with(|| {
                let mut rgba = label_rgba(info, palette, value);
                rgba[3] = (rgba[3] as f64 * opacity.clamp(0.0, 1.0)).round() as u8;
                rgba
            });
            if rgba[3] == 0 {
                continue;
            }
            let idx = (r * cols + c) * 4;
            out[idx..idx + 4].copy_from_slice(&rgba);
        }
    }
    out
}

/// Source-over composite of an RGBA overlay onto an RGB base, in place.
///
/// This is the operation an overlay actually needs, and running the standalone label render
/// through it too (over a black base) means the separate-image view and the overlay view agree
/// on colour by construction rather than by coincidence.
pub fn composite_over(base_rgb: &mut [u8], overlay_rgba: &[u8]) {
    debug_assert_eq!(base_rgb.len() / 3, overlay_rgba.len() / 4);
    for (base, over) in base_rgb
        .chunks_exact_mut(3)
        .zip(overlay_rgba.chunks_exact(4))
    {
        let a = over[3] as u32;
        if a == 0 {
            continue;
        }
        if a == 255 {
            base.copy_from_slice(&over[..3]);
            continue;
        }
        for i in 0..3 {
            // Rounded 8-bit source-over: out = over*a + base*(1-a), with +127 for round-to-nearest.
            let blended = over[i] as u32 * a + base[i] as u32 * (255 - a);
            base[i] = ((blended + 127) / 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::arr2;
    use zarr_core::LabelColor;

    fn info() -> LabelInfo {
        LabelInfo {
            name: "nuclei".into(),
            colors: vec![
                LabelColor {
                    value: 1,
                    rgba: [255, 0, 0, 255],
                },
                LabelColor {
                    value: 2,
                    rgba: [0, 255, 0, 255],
                },
            ],
        }
    }

    #[test]
    fn table_palette_uses_the_declared_colours() {
        assert_eq!(
            label_rgba(&info(), LabelPalette::Table, 1),
            [255, 0, 0, 255]
        );
        assert_eq!(
            label_rgba(&info(), LabelPalette::Table, 2),
            [0, 255, 0, 255]
        );
        assert_eq!(label_rgba(&info(), LabelPalette::Table, 9), [0, 0, 0, 0]);
    }

    #[test]
    fn background_is_transparent_in_every_palette() {
        for p in [LabelPalette::Table, LabelPalette::Distinct] {
            assert_eq!(label_rgba(&info(), p, 0), [0, 0, 0, 0], "{p:?}");
        }
    }

    /// The whole point of the default palette. A colour table where every value declares the same
    /// grey — which is what the IDR sample actually ships — must still render as distinguishable
    /// objects.
    #[test]
    fn the_distinct_palette_separates_values_a_degenerate_table_would_merge() {
        let degenerate = LabelInfo {
            name: "0".into(),
            colors: (1..=61)
                .map(|value| LabelColor {
                    value,
                    rgba: [128, 128, 128, 128],
                })
                .collect(),
        };
        let table: Vec<[u8; 4]> = (1..=61)
            .map(|v| label_rgba(&degenerate, LabelPalette::Table, v))
            .collect();
        assert_eq!(
            table.iter().collect::<std::collections::HashSet<_>>().len(),
            1,
            "the declared table really is degenerate"
        );

        let distinct: Vec<[u8; 4]> = (1..=61)
            .map(|v| label_rgba(&degenerate, LabelPalette::Distinct, v))
            .collect();
        assert_eq!(
            distinct
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            61,
            "every one of the 61 values must get its own colour"
        );
    }

    /// Colours must not drift between tiles: the same value is looked up independently in every
    /// request that touches it.
    #[test]
    fn distinct_colours_are_stable_and_opaque() {
        for v in [1i64, 2, 7, 61, 4096, -3] {
            assert_eq!(distinct_rgba(v), distinct_rgba(v));
            assert_eq!(distinct_rgba(v)[3], 255);
        }
    }

    /// Neighbouring object IDs are the case that matters — a segmentation numbers adjacent
    /// objects consecutively, so 5 and 6 sitting next to each other must not look alike.
    #[test]
    fn consecutive_values_are_far_apart_in_colour() {
        for v in 1i64..200 {
            let a = distinct_rgba(v);
            let b = distinct_rgba(v + 1);
            let dist: i32 = (0..3)
                .map(|i| (a[i] as i32 - b[i] as i32).abs())
                .sum::<i32>();
            assert!(
                dist > 60,
                "value {v} and {} are too close: {a:?} {b:?}",
                v + 1
            );
        }
    }

    #[test]
    fn palette_parsing_accepts_the_documented_names_and_rejects_others() {
        assert_eq!(LabelPalette::parse(None).unwrap(), LabelPalette::Distinct);
        assert_eq!(
            LabelPalette::parse(Some("auto")).unwrap(),
            LabelPalette::Distinct
        );
        assert_eq!(
            LabelPalette::parse(Some("distinct")).unwrap(),
            LabelPalette::Distinct
        );
        assert_eq!(
            LabelPalette::parse(Some("table")).unwrap(),
            LabelPalette::Table
        );
        assert!(LabelPalette::parse(Some("Table")).is_err());
        assert!(LabelPalette::parse(Some("rainbow")).is_err());
    }

    #[test]
    fn colorize_maps_each_value_through_the_palette() {
        let plane = arr2(&[[0.0, 1.0], [2.0, 9.0]]);
        let rgba = colorize(&plane, &info(), LabelPalette::Table, 1.0);
        assert_eq!(&rgba[0..4], &[0, 0, 0, 0], "background");
        assert_eq!(&rgba[4..8], &[255, 0, 0, 255]);
        assert_eq!(&rgba[8..12], &[0, 255, 0, 255]);
        assert_eq!(&rgba[12..16], &[0, 0, 0, 0], "value with no entry");
    }

    /// **Deliberate aliasing, pinned so nobody "fixes" it by accident.** Every label value is
    /// widened to `f64` before it reaches here (`zarr_core::image`'s `widen!` macro does
    /// `v as f64` for every source dtype, `I64` included), and `f64` has only 53 bits of integer
    /// mantissa. Two DISTINCT `int64`/`uint64` label identifiers above `2^53` can therefore widen
    /// to the identical `f64`, or round to the identical `i64` here, and get the identical
    /// colour. See `zarr_core::DType::I64`'s doc comment for the full explanation of why this is
    /// accepted rather than fixed (a lossless fix needs an integer plane representation parallel
    /// to the `f64` one the whole render path shares, which is a much larger change than any
    /// reported defect has needed, and segmentation tooling numbers objects sequentially from 0
    /// or 1 in practice).
    #[test]
    fn values_above_2_53_can_alias_onto_the_same_colour_by_design() {
        const TWO_POW_53: i64 = 1 << 53; // 9_007_199_254_740_992

        // `2^53 + 1` is not representable as `f64` at this magnitude (the gap between adjacent
        // representable values, the ulp, is 2 here) and rounds DOWN to `2^53` — two distinct
        // label identifiers collapsing onto one colour.
        let low = arr2(&[[TWO_POW_53 as f64, (TWO_POW_53 + 1) as f64]]);
        let rgba = colorize(&low, &info(), LabelPalette::Distinct, 1.0);
        assert_eq!(
            &rgba[0..4],
            &rgba[4..8],
            "2^53 and 2^53+1 must alias to the same colour"
        );

        // Not always "rounds down": 9007199254740995 (2^53 + 3) rounds UP to 9007199254740996
        // (2^53 + 4) — the general case is whichever representable value is nearest, not a
        // consistent direction.
        let high = arr2(&[[
            9_007_199_254_740_995i64 as f64,
            9_007_199_254_740_996i64 as f64,
        ]]);
        let rgba = colorize(&high, &info(), LabelPalette::Distinct, 1.0);
        assert_eq!(
            &rgba[0..4],
            &rgba[4..8],
            "9007199254740995 must alias onto 9007199254740996's colour"
        );

        // `uint64` loses more: `colorize`'s memo key is `i64` (`.round() as i64`), and Rust's
        // float-to-int `as` cast SATURATES rather than wrapping, so every `u64` value from
        // `i64::MAX` (2^63 - 1) upward — the entire top half of `u64`'s range — reads back as
        // exactly `i64::MAX` and collapses onto one colour.
        let u64_near_max = (u64::MAX - 10) as f64; // widened exactly as `widen!(u64)` would
        let saturated = arr2(&[[i64::MAX as f64, u64_near_max]]);
        let rgba = colorize(&saturated, &info(), LabelPalette::Distinct, 1.0);
        assert_eq!(
            &rgba[0..4],
            &rgba[4..8],
            "i64::MAX and a u64 near u64::MAX must both saturate to the same colour"
        );
    }

    /// A float-stored label image can carry NaN. It must read as background, not as whatever
    /// integer a NaN cast happens to produce.
    #[test]
    fn non_finite_samples_are_background() {
        let plane = arr2(&[[f64::NAN, f64::INFINITY]]);
        let rgba = colorize(&plane, &info(), LabelPalette::Distinct, 1.0);
        assert_eq!(rgba, vec![0u8; 8]);
    }

    #[test]
    fn opacity_scales_every_colour_alpha() {
        let plane = arr2(&[[1.0, 3.0]]);
        let rgba = colorize(&plane, &info(), LabelPalette::Table, 0.5);
        assert_eq!(&rgba[0..4], &[255, 0, 0, 128], "declared 255 alpha, halved");
        // Value 3 has no entry: transparent stays transparent, whatever the opacity.
        assert_eq!(&rgba[4..8], &[0, 0, 0, 0]);
    }

    #[test]
    fn opacity_zero_draws_nothing() {
        let plane = arr2(&[[1.0, 2.0]]);
        let rgba = colorize(&plane, &info(), LabelPalette::Table, 0.0);
        let mut base = vec![10u8, 20, 30, 40, 50, 60];
        composite_over(&mut base, &rgba);
        assert_eq!(
            base,
            vec![10, 20, 30, 40, 50, 60],
            "the base must be untouched"
        );
    }

    #[test]
    fn opacity_parsing_accepts_the_range_and_rejects_the_rest() {
        assert_eq!(parse_opacity(None).unwrap(), 1.0);
        assert_eq!(parse_opacity(Some(0.0)).unwrap(), 0.0);
        assert_eq!(parse_opacity(Some(0.5)).unwrap(), 0.5);
        assert_eq!(parse_opacity(Some(1.0)).unwrap(), 1.0);
        for bad in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
            assert!(parse_opacity(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn composite_leaves_the_base_where_the_overlay_is_transparent() {
        let mut base = vec![10u8, 20, 30, 40, 50, 60];
        composite_over(&mut base, &[0, 0, 0, 0, 255, 255, 255, 255]);
        assert_eq!(base, vec![10, 20, 30, 255, 255, 255]);
    }

    #[test]
    fn composite_blends_a_partly_transparent_overlay() {
        let mut base = vec![0u8, 0, 0];
        composite_over(&mut base, &[255, 255, 255, 128]);
        // 255*128/255 = 128, rounded.
        assert_eq!(base, vec![128, 128, 128]);
    }
}
