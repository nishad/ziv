use crate::TileError;
use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{
    create_srgb_mapper, FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
};
use ndarray::Array2;

/// Resample an RGB8 buffer, treating the input/output bytes as sRGB-encoded (the standard
/// assumption for 8-bit display imagery, and what our JPEG output is interpreted as by every
/// viewer).
///
/// `fast_image_resize`'s convolution filters operate on the raw sample values with no
/// colorspace awareness: averaging/interpolating sRGB-encoded bytes directly resamples in
/// *gamma* space, not *linear-light* space. That's a well-known subtle quality bug — a 50%
/// black/white checkerboard downscaled to a single pixel should read back as ~sRGB 188 (the
/// sRGB encoding of 50% linear luminance), not the naive gamma-space average of ~127. To get
/// the physically-correct result we decode sRGB -> linear light (`forward_map`), resize in
/// linear space, then re-encode linear -> sRGB (`backward_map_inplace`) on the resized output.
///
/// The identity case (`src_w == dst_w && src_h == dst_h`) short-circuits before any of this: no
/// resize means no colorspace round-trip is needed, and skipping it avoids introducing any
/// floating-point round-trip error (or color shift) when the request isn't actually scaling.
pub fn resize_rgb8(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Result<Vec<u8>, TileError> {
    if src_w == dst_w && src_h == dst_h {
        return Ok(src.to_vec());
    }
    let src_image = ImageRef::new(src_w, src_h, src, PixelType::U8x3)
        .map_err(|e| TileError::Encode(format!("resize source: {e}")))?;

    // sRGB -> linear light, still U8x3 (the mapper builds u8<->u8 lookup tables internally).
    let mapper = create_srgb_mapper();
    let mut linear_src = Image::new(src_w, src_h, PixelType::U8x3);
    mapper
        .forward_map(&src_image, &mut linear_src)
        .map_err(|e| TileError::Encode(format!("srgb forward map: {e}")))?;

    let mut dst_image = Image::new(dst_w, dst_h, PixelType::U8x3);
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    let mut resizer = Resizer::new();
    resizer
        .resize(&linear_src, &mut dst_image, Some(&opts))
        .map_err(|e| TileError::Encode(format!("resize: {e}")))?;

    // linear light -> sRGB, in place on the resized output.
    mapper
        .backward_map_inplace(&mut dst_image)
        .map_err(|e| TileError::Encode(format!("srgb backward map: {e}")))?;

    Ok(dst_image.into_vec())
}

/// Nearest-neighbour resample of a plane of label VALUES.
///
/// Every filter in `resize_rgb8` interpolates, and interpolating label values is not a quality
/// tradeoff, it is a correctness bug: the samples are object identifiers, so the average of
/// label 3 and label 7 is label 5, an object that is not there. Nearest-neighbour is the only
/// resample that answers "which object is at this point" rather than inventing one.
///
/// Sampling is centre-aligned (`(dst + 0.5) * src / dst`), so a 2x downscale picks alternate
/// source pixels rather than skewing everything half a pixel toward the origin. Works in both
/// directions: a label pyramid coarser than the image it annotates is upsampled here, which is
/// how it should look — blocky at the mask's real resolution, not smoothed into false precision.
#[must_use]
pub fn resize_nearest(src: &Array2<f64>, dst_w: u32, dst_h: u32) -> Array2<f64> {
    let (src_h, src_w) = src.dim();
    if src_h == 0 || src_w == 0 {
        return Array2::zeros((dst_h as usize, dst_w as usize));
    }
    if src_h == dst_h as usize && src_w == dst_w as usize {
        return src.clone();
    }
    let sx = src_w as f64 / dst_w as f64;
    let sy = src_h as f64 / dst_h as f64;
    Array2::from_shape_fn((dst_h as usize, dst_w as usize), |(y, x)| {
        let ny = (((y as f64 + 0.5) * sy) as usize).min(src_h - 1);
        let nx = (((x as f64 + 0.5) * sx) as usize).min(src_w - 1);
        src[[ny, nx]]
    })
}

#[cfg(test)]
mod nearest_tests {
    use super::*;
    use ndarray::arr2;

    #[test]
    fn identity_when_same_size() {
        let src = arr2(&[[1.0, 2.0], [3.0, 4.0]]);
        assert_eq!(resize_nearest(&src, 2, 2), src);
    }

    /// The property the whole label render depends on: every output sample is a value that was
    /// actually in the input. A convolution filter fails this immediately.
    #[test]
    fn downscaling_never_invents_a_value() {
        let src = Array2::from_shape_fn((8, 8), |(y, x)| ((y / 4) * 2 + (x / 4)) as f64);
        let out = resize_nearest(&src, 3, 3);
        for &v in out.iter() {
            assert!(
                [0.0, 1.0, 2.0, 3.0].contains(&v),
                "resample produced {v}, which is not a label in the source"
            );
        }
    }

    /// Quadrants must stay put, not drift half a pixel toward the origin.
    #[test]
    fn downscaling_preserves_quadrants() {
        let src = Array2::from_shape_fn((64, 64), |(y, x)| ((y / 32) * 2 + (x / 32)) as f64);
        let out = resize_nearest(&src, 4, 4);
        assert_eq!(
            out,
            arr2(&[
                [0.0, 0.0, 1.0, 1.0],
                [0.0, 0.0, 1.0, 1.0],
                [2.0, 2.0, 3.0, 3.0],
                [2.0, 2.0, 3.0, 3.0],
            ])
        );
    }

    /// A label pyramid coarser than its parent gets upsampled. Each source pixel must become a
    /// solid block, with no interpolated edge.
    #[test]
    fn upscaling_replicates_rather_than_interpolates() {
        let src = arr2(&[[5.0, 9.0]]);
        let out = resize_nearest(&src, 4, 1);
        assert_eq!(out, arr2(&[[5.0, 5.0, 9.0, 9.0]]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_when_same_size() {
        let src = vec![1u8, 2, 3, 4, 5, 6]; // 2x1 RGB
        assert_eq!(resize_rgb8(&src, 2, 1, 2, 1).unwrap(), src);
    }

    #[test]
    fn downscale_produces_correct_length() {
        let src = vec![200u8; 4 * 4 * 3];
        let out = resize_rgb8(&src, 4, 4, 2, 2).unwrap();
        assert_eq!(out.len(), 2 * 2 * 3);
    }

    /// Locks in sRGB gamma-correct downscaling: a 2x2 black/white checkerboard (50% coverage
    /// each) downscaled to 1x1 must read back close to sRGB ~188 (the sRGB encoding of 50%
    /// *linear-light* luminance), not the ~127 a naive gamma-space average would produce. This
    /// is the numeric guard against the resample silently regressing back to gamma-space
    /// resampling.
    #[test]
    fn downscale_checkerboard_is_linear_light_correct() {
        // 2x2 RGB, black/white checkerboard: (0,0)=black, (1,0)=white, (0,1)=white, (1,1)=black.
        #[rustfmt::skip]
        let src: [u8; 2 * 2 * 3] = [
            0, 0, 0,       255, 255, 255,
            255, 255, 255, 0, 0, 0,
        ];
        let out = resize_rgb8(&src, 2, 2, 1, 1).unwrap();
        assert_eq!(out.len(), 3);
        for &channel in &out {
            assert!(
                (180..=195).contains(&channel),
                "expected linear-light-correct downscale to land near sRGB 188, got {channel}"
            );
            assert!(
                !(120..=134).contains(&channel),
                "downscale landed in the naive gamma-space-average range (~127): got {channel}"
            );
        }
    }
}
