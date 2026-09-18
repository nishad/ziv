//! Post-composite pixel transforms: the IIIF `rotation` and `quality` parameters.
//!
//! Both are applied AFTER compositing and resampling, on the final RGB8 buffer, because that is
//! what the IIIF Image API specifies: the parameters are applied in the order region, size,
//! rotation, quality, format. Doing rotation before resampling would silently change which axis
//! `size` constrains.
use crate::TileError;
use iiif::Quality;

/// Rotates an RGB8 buffer clockwise by 0, 90, 180 or 270 degrees, returning the rotated buffer
/// and its new dimensions (swapped for the quarter turns).
///
/// `rotationBy90s` is required at IIIF 3.0 level 2. Arbitrary rotation and mirroring are optional
/// and unimplemented, so anything else is an error here rather than a silent no-op — though in
/// practice `iiif::ImageRequest::parse` rejects it earlier, at the edge.
pub fn rotate_rgb8(
    rgb: &[u8],
    w: u32,
    h: u32,
    degrees: u16,
) -> Result<(Vec<u8>, u32, u32), TileError> {
    let expected = (w as usize) * (h as usize) * 3;
    if rgb.len() != expected {
        return Err(TileError::Encode("rgb buffer length mismatch".into()));
    }
    let (wu, hu) = (w as usize, h as usize);
    let px = |x: usize, y: usize| -> &[u8] {
        let i = (y * wu + x) * 3;
        &rgb[i..i + 3]
    };
    match degrees {
        0 => Ok((rgb.to_vec(), w, h)),
        90 => {
            // Clockwise: destination (x, y) comes from source (x_src = y_dst_col, ...).
            let mut out = vec![0u8; expected];
            for y in 0..hu {
                for x in 0..wu {
                    let (dx, dy) = (hu - 1 - y, x);
                    let d = (dy * hu + dx) * 3;
                    out[d..d + 3].copy_from_slice(px(x, y));
                }
            }
            Ok((out, h, w))
        }
        180 => {
            let mut out = vec![0u8; expected];
            for y in 0..hu {
                for x in 0..wu {
                    let (dx, dy) = (wu - 1 - x, hu - 1 - y);
                    let d = (dy * wu + dx) * 3;
                    out[d..d + 3].copy_from_slice(px(x, y));
                }
            }
            Ok((out, w, h))
        }
        270 => {
            let mut out = vec![0u8; expected];
            for y in 0..hu {
                for x in 0..wu {
                    let (dx, dy) = (y, wu - 1 - x);
                    let d = (dy * hu + dx) * 3;
                    out[d..d + 3].copy_from_slice(px(x, y));
                }
            }
            Ok((out, h, w))
        }
        other => Err(TileError::OutOfRange(format!(
            "unsupported rotation {other} (supported: 0, 90, 180, 270)"
        ))),
    }
}

/// Applies the IIIF `quality` parameter to an RGB8 buffer, in place.
///
/// `default` and `color` are the composited colour image ziv already produces, so both are
/// no-ops. `gray` uses the Rec. 601 luma coefficients (the same weighting `default` quality
/// already implies for a single-channel greyscale source). `bitonal` thresholds that luma at the
/// midpoint, which is what the spec's "black and white" means for a server with no dithering.
pub fn apply_quality(rgb: &mut [u8], quality: Quality) {
    match quality {
        Quality::Default | Quality::Color => {}
        Quality::Gray => {
            for px in rgb.chunks_exact_mut(3) {
                let y = luma(px);
                px[0] = y;
                px[1] = y;
                px[2] = y;
            }
        }
        Quality::Bitonal => {
            for px in rgb.chunks_exact_mut(3) {
                let v = if luma(px) < 128 { 0 } else { 255 };
                px[0] = v;
                px[1] = v;
                px[2] = v;
            }
        }
    }
}

/// Rec. 601 luma, rounded. Integer arithmetic so the result is deterministic across platforms.
fn luma(px: &[u8]) -> u8 {
    let (r, g, b) = (px[0] as u32, px[1] as u32, px[2] as u32);
    ((299 * r + 587 * g + 114 * b + 500) / 1000).min(255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x1 image (red, green) rotated clockwise becomes 1x2 with red on top.
    #[test]
    fn rotate_90_swaps_dimensions_and_moves_pixels_clockwise() {
        let rgb = vec![255, 0, 0, 0, 255, 0];
        let (out, w, h) = rotate_rgb8(&rgb, 2, 1, 90).unwrap();
        assert_eq!((w, h), (1, 2));
        assert_eq!(out, vec![255, 0, 0, 0, 255, 0]);
    }

    #[test]
    fn rotate_180_reverses_pixel_order_and_keeps_dimensions() {
        let rgb = vec![255, 0, 0, 0, 255, 0];
        let (out, w, h) = rotate_rgb8(&rgb, 2, 1, 180).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, vec![0, 255, 0, 255, 0, 0]);
    }

    /// Four quarter turns must be the identity — the strongest cheap check that the index
    /// arithmetic is self-consistent rather than merely plausible.
    #[test]
    fn four_quarter_turns_round_trip_to_the_original() {
        let (w, h) = (5u32, 3u32);
        let rgb: Vec<u8> = (0..(w * h * 3) as u8).collect();
        let (a, aw, ah) = rotate_rgb8(&rgb, w, h, 90).unwrap();
        let (b, bw, bh) = rotate_rgb8(&a, aw, ah, 90).unwrap();
        let (c, cw, ch) = rotate_rgb8(&b, bw, bh, 90).unwrap();
        let (d, dw, dh) = rotate_rgb8(&c, cw, ch, 90).unwrap();
        assert_eq!((dw, dh), (w, h));
        assert_eq!(d, rgb);
    }

    /// 90 then 270 is also the identity, which pins 270 independently of 90's own correctness.
    #[test]
    fn rotate_90_then_270_is_the_identity() {
        let (w, h) = (4u32, 2u32);
        let rgb: Vec<u8> = (0..(w * h * 3) as u8).collect();
        let (a, aw, ah) = rotate_rgb8(&rgb, w, h, 90).unwrap();
        let (b, bw, bh) = rotate_rgb8(&a, aw, ah, 270).unwrap();
        assert_eq!((bw, bh), (w, h));
        assert_eq!(b, rgb);
    }

    #[test]
    fn rotate_rejects_unsupported_angles_and_bad_buffers() {
        assert!(rotate_rgb8(&[0u8; 6], 2, 1, 45).is_err());
        assert!(rotate_rgb8(&[0u8; 5], 2, 1, 90).is_err());
    }

    #[test]
    fn gray_collapses_channels_to_luma_and_color_does_not() {
        let original = vec![255u8, 0, 0, 0, 0, 255];
        let mut gray = original.clone();
        apply_quality(&mut gray, Quality::Gray);
        // Rec. 601: red -> 76, blue -> 29. Equal across channels is what "gray" means.
        assert_eq!(gray, vec![76, 76, 76, 29, 29, 29]);

        let mut color = original.clone();
        apply_quality(&mut color, Quality::Color);
        assert_eq!(color, original);

        let mut dflt = original.clone();
        apply_quality(&mut dflt, Quality::Default);
        assert_eq!(dflt, original);
    }

    #[test]
    fn bitonal_thresholds_at_the_midpoint() {
        // luma(red)=76 -> black; luma(white)=255 -> white.
        let mut rgb = vec![255u8, 0, 0, 255, 255, 255];
        apply_quality(&mut rgb, Quality::Bitonal);
        assert_eq!(rgb, vec![0, 0, 0, 255, 255, 255]);
    }
}
