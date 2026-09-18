use crate::lut::Rgb;
use crate::ChannelView;
use ndarray::Array2;

/// Maps a raw sample `value` through the display `window` (lo, hi) into a 0..=255 byte.
///
/// `value` is untrusted-in-shape f64 data straight from a widened zarr read (any of
/// u8/u16/u32/i8/i16/i32/f32/f64 source dtypes), so it can be NaN or +/-infinity as well as any
/// finite value (including negative, for signed-integer/float dtypes). Non-finite guard:
/// - NaN -> 0 (checked first, before any comparison, since NaN compares false to everything —
///   without this explicit check a NaN would otherwise fall through the `<lo`/`>hi` comparisons,
///   which are all false for NaN, and reach the `(v.clamp(...) * 255.0).round() as u8` cast,
///   where `v` itself would be NaN and `NaN as u8` is technically 0 per Rust's saturating float
///   cast rules — but we make this explicit rather than relying on that cast behavior).
/// - +infinity -> 255 (clamped high).
/// - -infinity, or any finite value < lo (including negative values below a >=0 lo) -> 0.
/// - any finite value > hi -> 255.
/// - values within [lo, hi] map linearly to [0, 255], rounded.
pub fn window_to_u8(value: f64, window: (f64, f64)) -> u8 {
    let (lo, hi) = window;
    if hi <= lo {
        return 0;
    }
    if value.is_nan() {
        return 0;
    }
    if value == f64::INFINITY {
        return 255;
    }
    if value == f64::NEG_INFINITY || value < lo {
        return 0;
    }
    if value > hi {
        return 255;
    }
    let v = (value - lo) / (hi - lo);
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Additive composite of enabled channels. `planes` pairs each channel view with its
/// same-shaped intensity plane. Returns tightly-packed RGB8 (row-major), len = rows*cols*3.
pub fn composite(planes: &[(&ChannelView, Array2<f64>)]) -> Vec<u8> {
    let (rows, cols) = planes.first().map(|(_, p)| p.dim()).unwrap_or((0, 0));
    let mut out = vec![0u8; rows * cols * 3];
    for (view, plane) in planes {
        if !view.enabled {
            continue;
        }
        for r in 0..rows {
            for c in 0..cols {
                let intensity = window_to_u8(plane[[r, c]], view.window);
                let Rgb(cr, cg, cb) = view.lut.apply(intensity);
                let idx = (r * cols + c) * 3;
                out[idx] = out[idx].saturating_add(cr);
                out[idx + 1] = out[idx + 1].saturating_add(cg);
                out[idx + 2] = out[idx + 2].saturating_add(cb);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lut::{Lut, Rgb};

    #[test]
    fn window_maps_endpoints() {
        assert_eq!(window_to_u8(0.0, (0.0, 100.0)), 0);
        assert_eq!(window_to_u8(100.0, (0.0, 100.0)), 255);
        assert_eq!(window_to_u8(50.0, (0.0, 100.0)), 128);
        assert_eq!(window_to_u8(200.0, (0.0, 100.0)), 255); // clamp
    }

    #[test]
    fn window_to_u8_degenerate_window_lo_eq_hi_maps_to_zero() {
        // `hi <= lo` (including the exact `lo == hi` degenerate case, not just `hi < lo`) must
        // return 0 rather than dividing by zero in `(value - lo) / (hi - lo)`.
        assert_eq!(window_to_u8(50.0, (50.0, 50.0)), 0);
        assert_eq!(window_to_u8(0.0, (0.0, 0.0)), 0);
        assert_eq!(window_to_u8(-10.0, (50.0, 50.0)), 0);
    }

    #[test]
    fn window_to_u8_nan_maps_to_zero() {
        assert_eq!(window_to_u8(f64::NAN, (0.0, 100.0)), 0);
    }

    #[test]
    fn window_to_u8_positive_infinity_clamps_to_255() {
        assert_eq!(window_to_u8(f64::INFINITY, (0.0, 100.0)), 255);
    }

    #[test]
    fn window_to_u8_negative_infinity_maps_to_zero() {
        assert_eq!(window_to_u8(f64::NEG_INFINITY, (0.0, 100.0)), 0);
    }

    #[test]
    fn window_to_u8_negative_below_lo_maps_to_zero() {
        // lo can itself be negative (signed-integer/float source dtypes); anything below it,
        // including further-negative values, must still map to 0, not underflow/wrap.
        assert_eq!(window_to_u8(-500.0, (-100.0, 100.0)), 0);
        assert_eq!(window_to_u8(-1.0, (0.0, 100.0)), 0);
    }

    #[test]
    fn window_to_u8_above_hi_maps_to_255() {
        assert_eq!(window_to_u8(1_000_000.0, (0.0, 100.0)), 255);
    }

    #[test]
    fn window_to_u8_normal_midpoint_correct() {
        assert_eq!(window_to_u8(500.0, (0.0, 1000.0)), 128);
        assert_eq!(window_to_u8(50.0, (-50.0, 150.0)), 128);
    }

    #[test]
    fn single_grey_channel_produces_grey() {
        let plane = Array2::from_shape_vec((1, 2), vec![0.0f64, 100.0]).unwrap();
        let cv = ChannelView {
            index: 0,
            window: (0.0, 100.0),
            lut: Lut::Grey,
            enabled: true,
        };
        let rgb = composite(&[(&cv, plane)]);
        assert_eq!(rgb, vec![0, 0, 0, 255, 255, 255]);
    }

    #[test]
    fn two_channels_add() {
        let red_plane = Array2::from_shape_vec((1, 1), vec![100.0f64]).unwrap();
        let green_plane = Array2::from_shape_vec((1, 1), vec![100.0f64]).unwrap();
        let red = ChannelView {
            index: 0,
            window: (0.0, 100.0),
            lut: Lut::Fixed(Rgb(255, 0, 0)),
            enabled: true,
        };
        let green = ChannelView {
            index: 1,
            window: (0.0, 100.0),
            lut: Lut::Fixed(Rgb(0, 255, 0)),
            enabled: true,
        };
        let rgb = composite(&[(&red, red_plane), (&green, green_plane)]);
        assert_eq!(rgb, vec![255, 255, 0]); // red + green = yellow
    }

    #[test]
    fn disabled_channel_skipped() {
        let plane = Array2::from_shape_vec((1, 1), vec![100.0f64]).unwrap();
        let cv = ChannelView {
            index: 0,
            window: (0.0, 100.0),
            lut: Lut::Grey,
            enabled: false,
        };
        assert_eq!(composite(&[(&cv, plane)]), vec![0, 0, 0]);
    }
}
