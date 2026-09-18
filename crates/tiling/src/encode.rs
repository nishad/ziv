use crate::TileError;
use iiif::Format;
use jpeg_encoder::{ColorType, Encoder};

pub fn encode_jpeg_rgb8(rgb: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>, TileError> {
    if w > 65535 || h > 65535 {
        return Err(TileError::Encode("dimension exceeds 65535".into()));
    }
    if rgb.len() != (w as usize) * (h as usize) * 3 {
        return Err(TileError::Encode("rgb buffer length mismatch".into()));
    }
    let mut out = Vec::new();
    let encoder = Encoder::new(&mut out, quality);
    encoder
        .encode(rgb, w as u16, h as u16, ColorType::Rgb)
        .map_err(|e| TileError::Encode(e.to_string()))?;
    Ok(out)
}

/// Encodes RGB8 as PNG. Required at IIIF level 2 alongside JPEG.
///
/// PNG is lossless, so unlike the JPEG path there is no quality knob: the `quality` argument the
/// serving path threads through is deliberately NOT accepted here rather than silently ignored.
pub fn encode_png_rgb8(rgb: &[u8], w: u32, h: u32) -> Result<Vec<u8>, TileError> {
    if w == 0 || h == 0 {
        return Err(TileError::Encode("zero dimension".into()));
    }
    if rgb.len() != (w as usize) * (h as usize) * 3 {
        return Err(TileError::Encode("rgb buffer length mismatch".into()));
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| TileError::Encode(e.to_string()))?;
        writer
            .write_image_data(rgb)
            .map_err(|e| TileError::Encode(e.to_string()))?;
        writer
            .finish()
            .map_err(|e| TileError::Encode(e.to_string()))?;
    }
    Ok(out)
}

/// Encodes RGB8 in whichever format the request asked for. One place so the serving path never
/// has to branch on format itself, and so a new format cannot be added to `Format` without the
/// compiler pointing here.
pub fn encode_rgb8(
    rgb: &[u8],
    w: u32,
    h: u32,
    format: Format,
    jpeg_quality: u8,
) -> Result<Vec<u8>, TileError> {
    match format {
        Format::Jpg => encode_jpeg_rgb8(rgb, w, h, jpeg_quality),
        Format::Png => encode_png_rgb8(rgb, w, h),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_small_rgb_to_png_magic() {
        let png = encode_png_rgb8(&[128u8; 8 * 8 * 3], 8, 8).unwrap();
        assert_eq!(
            &png[0..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
    }

    /// PNG is lossless, so a flat image must round-trip to exactly the bytes put in — this is
    /// what distinguishes a real PNG encode from re-labelling JPEG output.
    #[test]
    fn png_is_lossless() {
        let mut rgb = vec![0u8; 4 * 4 * 3];
        for (i, px) in rgb.chunks_exact_mut(3).enumerate() {
            px[0] = (i * 7) as u8;
            px[1] = (i * 13) as u8;
            px[2] = (i * 29) as u8;
        }
        let encoded = encode_png_rgb8(&rgb, 4, 4).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(&encoded));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!(&buf[..info.buffer_size()], &rgb[..]);
    }

    #[test]
    fn png_rejects_length_mismatch() {
        assert!(encode_png_rgb8(&[0u8; 10], 8, 8).is_err());
    }

    #[test]
    fn encode_rgb8_dispatches_on_format() {
        let rgb = vec![64u8; 4 * 4 * 3];
        assert_eq!(
            &encode_rgb8(&rgb, 4, 4, Format::Jpg, 85).unwrap()[0..2],
            &[0xFF, 0xD8]
        );
        assert_eq!(
            &encode_rgb8(&rgb, 4, 4, Format::Png, 85).unwrap()[0..4],
            &[0x89, b'P', b'N', b'G']
        );
    }

    #[test]
    fn encodes_small_rgb_to_jpeg_magic() {
        let rgb = vec![128u8; 8 * 8 * 3];
        let jpeg = encode_jpeg_rgb8(&rgb, 8, 8, 85).unwrap();
        // JPEG SOI marker is 0xFFD8.
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
        assert!(jpeg.len() > 2);
    }

    #[test]
    fn rejects_length_mismatch() {
        assert!(encode_jpeg_rgb8(&[0u8; 10], 8, 8, 85).is_err());
    }
}
