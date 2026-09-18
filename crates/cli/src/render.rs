//! `ziv render`'s own logic beyond clap parsing: resolving the output format, and the whole-image
//! size budget's refusal message.
//!
//! Everything that actually reads pixels (opening the image, building the `ZarrTileEngine`,
//! calling `TileEngine::render`) stays in `lib.rs`'s `run()`, matching how `Command::Export` is
//! handled there — this module holds only the pure, easily unit-tested pieces: picking a
//! `Format` from `<OUT>`'s extension and `--format`, and turning an over-budget output size into
//! a message that names a size that would actually work.

use std::path::Path;

use iiif::Format;

/// Resolves the output format: `--format` wins when given, otherwise it is inferred from `out`'s
/// extension. An error names what was tried and how to fix it — silently defaulting to a format
/// nobody asked for would write bytes under an extension that lies about their content.
pub(crate) fn resolve_format(out: &Path, format_flag: Option<&str>) -> Result<Format, String> {
    if let Some(f) = format_flag {
        return format_from_str(f)
            .ok_or_else(|| format!("unrecognized --format {f:?} (supported: png, jpg)"));
    }
    let ext = out.extension().and_then(|e| e.to_str());
    match ext.and_then(format_from_str) {
        Some(format) => Ok(format),
        None => Err(match ext {
            Some(ext) => format!(
                "cannot infer an output format from \"{ext}\" (supported: png, jpg); pass \
                 --format png or --format jpg"
            ),
            None => format!(
                "{} has no file extension to infer an output format from; pass --format png or \
                 --format jpg",
                out.display()
            ),
        }),
    }
}

fn format_from_str(s: &str) -> Option<Format> {
    match s.to_ascii_lowercase().as_str() {
        "png" => Some(Format::Png),
        "jpg" | "jpeg" => Some(Format::Jpg),
        _ => None,
    }
}

/// Given an over-budget resolved output `(w, h)`, finds the largest same-aspect-ratio size that
/// would fit within `iiif::MAX_WHOLE_IMAGE_PIXELS`/`MAX_WHOLE_IMAGE_EDGE`, so the refusal message
/// can name a size that would actually work rather than only stating the limits.
///
/// Always returns a size within budget (each axis `>= 1`), including for a degenerate (zero)
/// input or an aspect ratio so extreme that one axis alone would exceed the edge limit.
pub(crate) fn suggest_within_budget(w: u64, h: u64) -> (u64, u64) {
    let w = w.max(1);
    let h = h.max(1);
    let pixel_scale = (iiif::MAX_WHOLE_IMAGE_PIXELS as f64 / (w as f64 * h as f64)).sqrt();
    let edge_scale = (iiif::MAX_WHOLE_IMAGE_EDGE as f64 / w as f64)
        .min(iiif::MAX_WHOLE_IMAGE_EDGE as f64 / h as f64);
    let scale = pixel_scale.min(edge_scale).min(1.0);
    let mut sw = ((w as f64 * scale).floor() as u64).max(1);
    let mut sh = ((h as f64 * scale).floor() as u64).max(1);
    // Floating-point rounding can leave the product a hair over budget; nudge the larger axis
    // down a pixel at a time until it actually fits, rather than trusting the float math to have
    // landed exactly on the edge.
    while !iiif::whole_image_within_budget(sw, sh) && (sw > 1 || sh > 1) {
        if sw >= sh {
            sw -= 1;
        } else {
            sh -= 1;
        }
    }
    (sw, sh)
}

/// The message `ziv render` refuses an over-budget request with: the resolved size that was
/// asked for, why it is refused, and — the point of doing this at all rather than just citing the
/// limits — a concrete size that would fit, expressed as the exact `--size` value to pass.
pub(crate) fn budget_refusal(out_w: u32, out_h: u32) -> String {
    let (sw, sh) = suggest_within_budget(out_w as u64, out_h as u64);
    format!(
        "requested output is {out_w}x{out_h} px, which exceeds the whole-image budget of {} \
         megapixels ({} px per edge); ask for at most {sw}x{sh} px instead, e.g. --size {sw},{sh}",
        iiif::MAX_WHOLE_IMAGE_PIXELS / 1_000_000,
        iiif::MAX_WHOLE_IMAGE_EDGE,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_format_infers_png_from_extension() {
        assert_eq!(
            resolve_format(Path::new("out.png"), None).unwrap(),
            Format::Png
        );
    }

    #[test]
    fn resolve_format_infers_jpg_from_extension() {
        assert_eq!(
            resolve_format(Path::new("out.jpg"), None).unwrap(),
            Format::Jpg
        );
        assert_eq!(
            resolve_format(Path::new("out.jpeg"), None).unwrap(),
            Format::Jpg
        );
    }

    #[test]
    fn resolve_format_flag_overrides_extension() {
        assert_eq!(
            resolve_format(Path::new("out.png"), Some("jpg")).unwrap(),
            Format::Jpg
        );
    }

    #[test]
    fn resolve_format_flag_is_case_insensitive() {
        assert_eq!(
            resolve_format(Path::new("out.bin"), Some("PNG")).unwrap(),
            Format::Png
        );
    }

    #[test]
    fn resolve_format_errors_on_unknown_flag() {
        let err = resolve_format(Path::new("out.png"), Some("webp")).unwrap_err();
        assert!(err.contains("webp"), "{err}");
    }

    #[test]
    fn resolve_format_errors_when_neither_determines_it() {
        let err = resolve_format(Path::new("out"), None).unwrap_err();
        assert!(err.contains("--format"), "{err}");
    }

    #[test]
    fn resolve_format_errors_on_unrecognized_extension() {
        let err = resolve_format(Path::new("out.tiff"), None).unwrap_err();
        assert!(err.contains("tiff"), "{err}");
    }

    #[test]
    fn suggest_within_budget_fits_the_spec_example() {
        // The design spec's own illustrative over-budget image.
        let (w, h) = suggest_within_budget(19_120, 13_350);
        assert!(iiif::whole_image_within_budget(w, h));
        // Should be close to the exact scale-down (not wildly conservative).
        assert!(w > 9_000 && h > 6_000, "got {w}x{h}");
    }

    #[test]
    fn suggest_within_budget_handles_extreme_aspect_ratio() {
        let (w, h) = suggest_within_budget(1_000_000, 1);
        assert!(iiif::whole_image_within_budget(w, h));
        assert!(w >= 1 && h >= 1);
    }

    #[test]
    fn suggest_within_budget_is_a_noop_when_already_within_budget() {
        let (w, h) = suggest_within_budget(1000, 1000);
        assert_eq!((w, h), (1000, 1000));
    }

    #[test]
    fn budget_refusal_names_a_size_that_fits_and_the_flag_to_use() {
        let msg = budget_refusal(19_120, 13_350);
        assert!(msg.contains("19120x13350"), "{msg}");
        assert!(msg.contains("--size "), "{msg}");
        assert!(msg.contains("64 megapixels"), "{msg}");
    }
}
