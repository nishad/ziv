use crate::lut::{Lut, Rgb};
use crate::{ChannelView, Projection, ZSelector};
use ndarray::Array2;
use serde_json::Value;

pub fn parse_hex_color(s: &str) -> Option<Rgb> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Rgb(r, g, b))
}

const FALLBACK_LUTS: [Rgb; 3] = [Rgb(255, 0, 0), Rgb(0, 255, 0), Rgb(0, 0, 255)];

pub fn default_projection(size_c: u64, size_z: u64, omero: Option<&Value>) -> Projection {
    let z = ZSelector::Plane(size_z / 2);
    let t = 0;

    if let Some(channels) = omero
        .and_then(|o| o.get("channels"))
        .and_then(|c| c.as_array())
    {
        let views = channels
            .iter()
            .enumerate()
            .map(|(i, ch)| {
                let color = ch
                    .get("color")
                    .and_then(|v| v.as_str())
                    .and_then(parse_hex_color)
                    .unwrap_or(FALLBACK_LUTS[i % 3]);
                let start = ch
                    .get("window")
                    .and_then(|w| w.get("start"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let end = ch
                    .get("window")
                    .and_then(|w| w.get("end"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(65535.0);
                let active = ch.get("active").and_then(|v| v.as_bool()).unwrap_or(true);
                ChannelView {
                    index: i,
                    window: (start, end),
                    lut: Lut::Fixed(color),
                    enabled: active,
                }
            })
            .collect();
        return Projection {
            t,
            z,
            channels: views,
        };
    }

    // No omero metadata: enable up to first 3 channels with distinguishable LUTs.
    let n = size_c.min(3) as usize;
    let channels = (0..n)
        .map(|i| ChannelView {
            index: i,
            window: (0.0, 65535.0),
            lut: if size_c == 1 {
                Lut::Grey
            } else {
                Lut::Fixed(FALLBACK_LUTS[i])
            },
            enabled: true,
        })
        .collect();
    Projection { t, z, channels }
}

/// Computes a display window `(lo, hi)` from the `lo_pct`/`hi_pct` percentiles of `plane`'s
/// values, with `lo <= hi` always guaranteed.
///
/// `plane` is untrusted-in-shape f64 data (widened from any source dtype) and may contain NaN.
/// NaN is not a meaningful sample for a display range, so NaN values are excluded from the
/// percentile computation entirely (a NaN endpoint would be unusable downstream: `window_to_u8`
/// checks like `hi <= lo` and `value > hi` are always false against a NaN operand, so a NaN `lo`
/// or `hi` would silently defeat the clamp/degenerate-window guards there). The remaining
/// finite/infinite values are sorted with `f64::total_cmp` (rather than `partial_cmp`/
/// `sort_unstable`, which panics or is undefined for NaN — defensive here even post-filter, and
/// correct for +/-infinity which `total_cmp` orders normally) so the result is deterministic
/// regardless of the NaN values' original positions. If every value is NaN, falls back to
/// `(0.0, 1.0)`, same as the empty-plane case.
pub fn percentile_window(plane: &Array2<f64>, lo_pct: f64, hi_pct: f64) -> (f64, f64) {
    let mut vals: Vec<f64> = plane.iter().copied().filter(|v| !v.is_nan()).collect();
    if vals.is_empty() {
        return (0.0, 1.0);
    }
    vals.sort_by(f64::total_cmp);
    let idx = |p: f64| ((p / 100.0) * (vals.len() - 1) as f64).round() as usize;
    let lo = vals[idx(lo_pct)];
    let hi = vals[idx(hi_pct)];
    if lo <= hi {
        (lo, hi)
    } else {
        (hi, lo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn honors_omero_channels() {
        let omero = json!({"channels": [
            {"color":"FF0000","window":{"start":0,"end":100},"active":true},
            {"color":"00FF00","window":{"start":10,"end":200},"active":false}
        ]});
        let p = default_projection(2, 1, Some(&omero));
        assert_eq!(p.z, ZSelector::Plane(0));
        assert_eq!(p.channels.len(), 2);
        assert_eq!(p.channels[0].lut, Lut::Fixed(Rgb(255, 0, 0)));
        assert_eq!(p.channels[0].window, (0.0, 100.0));
        assert!(p.channels[0].enabled);
        assert!(!p.channels[1].enabled);
    }

    #[test]
    fn falls_back_to_luts_without_omero() {
        let p = default_projection(2, 5, None);
        assert_eq!(p.z, ZSelector::Plane(2)); // middle z
        assert_eq!(p.channels.len(), 2);
        assert_eq!(p.channels[0].lut, Lut::Fixed(Rgb(255, 0, 0)));
        assert_eq!(p.channels[1].lut, Lut::Fixed(Rgb(0, 255, 0)));
    }

    #[test]
    fn single_channel_defaults_to_grey() {
        let p = default_projection(1, 1, None);
        assert_eq!(p.channels[0].lut, Lut::Grey);
    }

    #[test]
    fn percentile_window_picks_endpoints() {
        let plane = Array2::from_shape_vec((1, 5), vec![0.0f64, 25.0, 50.0, 75.0, 100.0]).unwrap();
        assert_eq!(percentile_window(&plane, 0.0, 100.0), (0.0, 100.0));
    }

    /// A NaN in the plane must not panic (raw f64 `sort_unstable`/`partial_cmp` would), must not
    /// produce a NaN endpoint (NaN is excluded from the percentile computation — see doc
    /// comment), and must be deterministic across repeated calls, with lo <= hi.
    #[test]
    fn percentile_window_with_nan_is_deterministic_and_lo_le_hi() {
        let plane = Array2::from_shape_vec((1, 6), vec![0.0f64, 25.0, f64::NAN, 50.0, 75.0, 100.0])
            .unwrap();
        let first = percentile_window(&plane, 2.0, 98.0);
        for _ in 0..10 {
            assert_eq!(percentile_window(&plane, 2.0, 98.0), first);
        }
        assert!(
            !first.0.is_nan() && !first.1.is_nan(),
            "expected finite endpoints, got {first:?}"
        );
        assert!(first.0 <= first.1, "expected lo <= hi, got {first:?}");
    }

    /// An all-NaN plane must not panic and must fall back to the same default as an empty plane.
    #[test]
    fn percentile_window_all_nan_falls_back_to_default() {
        let plane = Array2::from_shape_vec((1, 3), vec![f64::NAN, f64::NAN, f64::NAN]).unwrap();
        assert_eq!(percentile_window(&plane, 2.0, 98.0), (0.0, 1.0));
    }

    #[test]
    fn parses_hex() {
        assert_eq!(parse_hex_color("00FF80"), Some(Rgb(0, 255, 128)));
        assert_eq!(parse_hex_color("xyz"), None);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// `percentile_window`'s core invariant — `lo <= hi` always holds — over
        /// arbitrary finite plane data (any mix of positive/negative/zero values, any
        /// shape, any lo/hi percentile pair including inverted ones) and must never
        /// panic.
        #[test]
        fn percentile_window_lo_le_hi_on_arbitrary_finite_data(
            values in proptest::collection::vec(-1e12f64..1e12, 1..64),
            rows in 1usize..8,
            lo_pct in 0.0f64..100.0,
            hi_pct in 0.0f64..100.0,
        ) {
            // Reshape the flat `values` into a `rows x cols` grid (cols chosen so every
            // row is fully populated; truncate any remainder so `from_shape_vec` never
            // errors on a partial row).
            let cols = values.len() / rows;
            prop_assume!(cols > 0);
            let trimmed = &values[..rows * cols];
            let plane = Array2::from_shape_vec((rows, cols), trimmed.to_vec()).unwrap();

            let (lo, hi) = percentile_window(&plane, lo_pct, hi_pct);
            prop_assert!(lo <= hi, "expected lo <= hi, got lo={lo} hi={hi}");
            prop_assert!(lo.is_finite() && hi.is_finite());
        }
    }
}
