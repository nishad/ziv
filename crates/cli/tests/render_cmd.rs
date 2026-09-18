//! End-to-end tests for `ziv render`, spawning the real binary (same style as
//! `export_progress.rs`/`error_reporting.rs`): parsing alone (`crates/cli/src/lib.rs`'s own unit
//! tests) proves flags are accepted, but says nothing about whether the command actually produces
//! the pictures it claims to, at the sizes it claims to, or genuinely bakes an overlay in rather
//! than just writing a plain render under a different name.
//!
//! Three claims this file exists to prove on real bytes, not just "a file was written":
//! - a PNG and a JPEG of the same projection differ in format and decode to the same dimensions;
//! - a request whose resolved output exceeds the whole-image budget is refused, non-zero exit,
//!   with a message naming a size that would actually work;
//! - `--at ...,overlay=NAME` genuinely bakes the label's palette into the output: decoded pixels
//!   match the palette colour, and the same projection without the overlay does not contain them.

use std::path::Path;
use std::process::Command;

use tiling::{label_rgba, LabelPalette};
use zarr_core::LabelInfo;

const U8_FIXTURE: &str = "../../tests/fixtures/sample_u8.ome.zarr";
const OVER_BUDGET_FIXTURE: &str = "../../tests/fixtures/sample_over_render_budget.ome.zarr";
const PLANES_LABELS_FIXTURE: &str = "../../tests/fixtures/sample_planes_labels.ome.zarr";

fn run_ziv(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ziv"))
        .args(args)
        .output()
        .expect("failed to run the ziv binary")
}

/// Decodes a PNG file back to `(pixels_rgb, width, height)`. Mirrors
/// `crates/tiling/src/engine.rs`'s own `decode_png` test helper.
fn decode_png_file(path: &Path) -> (Vec<u8>, u32, u32) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    (buf, info.width, info.height)
}

/// Reads a baseline JPEG's pixel dimensions straight off its SOF0 marker, rather than pulling in
/// a JPEG decoding dependency just to check two numbers this test already knows how to find: a
/// JPEG is a sequence of `0xFF <marker> <big-endian u16 length> <payload>` segments (a handful of
/// markers carry no length and are skipped by their marker byte alone), and the frame header
/// (`SOF0`, marker `0xC0`, for the baseline encoder `jpeg-encoder` always produces) stores
/// `precision(1) height(2) width(2)` as the first five payload bytes.
fn jpeg_dimensions(bytes: &[u8]) -> (u32, u32) {
    assert_eq!(&bytes[0..2], &[0xFF, 0xD8], "not a JPEG (missing SOI)");
    let mut i = 2usize;
    while i + 1 < bytes.len() {
        assert_eq!(bytes[i], 0xFF, "expected a marker at offset {i}");
        let marker = bytes[i + 1];
        i += 2;
        // Markers with no length field: standalone RSTn/TEM/fill bytes.
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            continue;
        }
        let len = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
        let is_sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            let payload = i + 2;
            let height = u16::from_be_bytes([bytes[payload + 1], bytes[payload + 2]]);
            let width = u16::from_be_bytes([bytes[payload + 3], bytes[payload + 4]]);
            return (width as u32, height as u32);
        }
        i += len;
    }
    panic!("no SOF marker found in JPEG bytes");
}

/// A PNG and a JPEG of the exact same projection: different magic bytes (different formats), same
/// decoded dimensions (both faithfully rendered the same request, not two different ones).
#[test]
fn png_and_jpeg_of_the_same_projection_differ_in_format_but_agree_on_dimensions() {
    let dir = tempfile::tempdir().unwrap();
    let png_path = dir.path().join("out.png");
    let jpg_path = dir.path().join("out.jpg");

    let png_out = run_ziv(&[
        "render",
        U8_FIXTURE,
        png_path.to_str().unwrap(),
        "--at",
        "@c=0",
    ]);
    assert!(
        png_out.status.success(),
        "png render failed: {}",
        String::from_utf8_lossy(&png_out.stderr)
    );
    let jpg_out = run_ziv(&[
        "render",
        U8_FIXTURE,
        jpg_path.to_str().unwrap(),
        "--at",
        "@c=0",
    ]);
    assert!(
        jpg_out.status.success(),
        "jpeg render failed: {}",
        String::from_utf8_lossy(&jpg_out.stderr)
    );

    let png_bytes = std::fs::read(&png_path).unwrap();
    let jpg_bytes = std::fs::read(&jpg_path).unwrap();
    assert_eq!(
        &png_bytes[0..8],
        &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n']
    );
    assert_eq!(&jpg_bytes[0..2], &[0xFF, 0xD8]);
    assert_ne!(png_bytes, jpg_bytes, "same bytes despite different formats");

    let (_, png_w, png_h) = decode_png_file(&png_path);
    let (jpg_w, jpg_h) = jpeg_dimensions(&jpg_bytes);
    assert_eq!((png_w, png_h), (16, 16), "sample_u8 is a 16x16 fixture");
    assert_eq!((png_w, png_h), (jpg_w, jpg_h));
}

/// `--format` overrides whatever `<OUT>`'s extension would otherwise infer.
#[test]
fn format_flag_overrides_the_out_extension() {
    let dir = tempfile::tempdir().unwrap();
    let out_path = dir.path().join("out.png"); // extension says png...
    let output = run_ziv(&[
        "render",
        U8_FIXTURE,
        out_path.to_str().unwrap(),
        "--format",
        "jpg", // ...--format says jpg, and wins.
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = std::fs::read(&out_path).unwrap();
    assert_eq!(
        &bytes[0..2],
        &[0xFF, 0xD8],
        "expected JPEG bytes despite the .png extension"
    );
}

/// Neither `<OUT>`'s extension nor `--format` determines a format: a clear, non-zero-exit error,
/// not a silent default.
#[test]
fn ambiguous_format_is_refused_with_a_clear_message() {
    let dir = tempfile::tempdir().unwrap();
    let out_path = dir.path().join("out.bin");
    let output = run_ziv(&["render", U8_FIXTURE, out_path.to_str().unwrap()]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("ziv: error:"), "{stderr}");
    assert!(stderr.contains("--format"), "{stderr}");
    assert!(!out_path.exists());
}

/// A request whose resolved output exceeds `iiif::MAX_WHOLE_IMAGE_PIXELS` is refused before any
/// file is written, non-zero exit, and the message names a size that would actually fit — not
/// just the limits it violated. `sample_over_render_budget.ome.zarr` declares a 19120x13350
/// single level (the design spec's own illustrative over-budget image) with no chunk data: the
/// budget check runs from declared shape alone, so this never attempts to read a pixel.
#[test]
fn over_budget_render_is_refused_with_a_size_suggestion_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let out_path = dir.path().join("out.png");
    let output = run_ziv(&["render", OVER_BUDGET_FIXTURE, out_path.to_str().unwrap()]);

    assert!(
        !output.status.success(),
        "expected a non-zero exit for an over-budget render"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("ziv: error:"), "{stderr}");
    assert!(stderr.contains("19120x13350"), "{stderr}");
    assert!(stderr.contains("64 megapixels"), "{stderr}");
    assert!(
        stderr.contains("--size"),
        "refusal must name a size to ask for instead: {stderr}"
    );
    assert!(
        !out_path.exists(),
        "an over-budget request must not write any output file"
    );
}

/// The differentiator this whole command exists for: `--at ...,overlay=NAME` must actually bake
/// the label's palette into the output.
///
/// `sample_planes_labels.ome.zarr` (see `build_planes_labels_fixture.rs`) fills z-plane 0's
/// `cells` label ENTIRELY with value 1 (never 0/background) and its image plane with a flat grey
/// value — so at native resolution (no resampling) the whole overlay render is one uniform
/// colour: exactly `distinct` palette's colour for label value 1, with no blending, because
/// `overlay=cells` defaults to opacity 1.0 (full replace, not a partial composite). The same
/// projection with no overlay is a flat grey image, so the palette colour cannot appear there by
/// coincidence: this proves the colour comes from the overlay, not from the base image.
#[test]
fn overlay_identifier_bakes_the_label_palette_into_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let overlay_path = dir.path().join("overlay.png");
    let plain_path = dir.path().join("plain.png");

    let overlay_out = run_ziv(&[
        "render",
        PLANES_LABELS_FIXTURE,
        overlay_path.to_str().unwrap(),
        "--at",
        "@z=0,c=0,overlay=cells",
    ]);
    assert!(
        overlay_out.status.success(),
        "{}",
        String::from_utf8_lossy(&overlay_out.stderr)
    );
    let plain_out = run_ziv(&[
        "render",
        PLANES_LABELS_FIXTURE,
        plain_path.to_str().unwrap(),
        "--at",
        "@z=0,c=0",
    ]);
    assert!(
        plain_out.status.success(),
        "{}",
        String::from_utf8_lossy(&plain_out.stderr)
    );

    let (overlay_px, ow, oh) = decode_png_file(&overlay_path);
    let (plain_px, pw, ph) = decode_png_file(&plain_path);
    assert_eq!((ow, oh), (32, 32));
    assert_eq!((pw, ph), (32, 32));

    // Ground truth from the exact same colour function the renderer uses — not a hardcoded RGB
    // guess that could silently drift from `distinct_rgba`'s own definition.
    let info = LabelInfo {
        name: "cells".to_string(),
        colors: vec![],
    };
    let expected = label_rgba(&info, LabelPalette::Distinct, 1);
    let expected_rgb = [expected[0], expected[1], expected[2]];

    let pixel = |px: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let i = ((y * w + x) * 3) as usize;
        [px[i], px[i + 1], px[i + 2]]
    };

    // The label fills the ENTIRE plane and opacity defaults to 1.0 (full replace, no blending),
    // so every pixel of the overlay render must be exactly the palette colour.
    for y in 0..oh {
        for x in 0..ow {
            assert_eq!(
                pixel(&overlay_px, ow, x, y),
                expected_rgb,
                "overlay pixel ({x},{y}) is not the palette colour"
            );
        }
    }

    // The plain render (same projection, no overlay) must never show that colour: proves the
    // colour came from the overlay, not from the base image already happening to have it.
    for y in 0..ph {
        for x in 0..pw {
            assert_ne!(
                pixel(&plain_px, pw, x, y),
                expected_rgb,
                "plain render unexpectedly contains the overlay's palette colour at ({x},{y})"
            );
        }
    }
}
