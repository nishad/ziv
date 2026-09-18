//! `main`'s error path must print the way a CLI user reads errors: `Display` text, never `Debug`.
//!
//! Before this test existed, `main` returned `Result<(), Box<dyn std::error::Error>>` and let the
//! Rust runtime print a returned `Err` itself — which prints with `Debug`. A refused export (see
//! `exporter::ExportError::UnsupportedPyramid`) showed a raw struct literal,
//! `Error: UnsupportedPyramid { scale_factors: [1, 3, 9], levels: 3 }`, instead of the carefully
//! written prose that tells the operator to use `ziv serve` instead. Refusing an export is a
//! normal, expected outcome now (see the `feat(exporter)` commit that introduced the refusal), so
//! that Debug dump was a production defect: this spawns the real binary (same style as
//! `export_progress.rs`) and checks stderr, exit code, and the untouched output directory.

use std::process::Command;

/// `sample_unpinnable.ome.zarr`'s pyramid scale factors don't match the tile layout OpenSeadragon
/// expects, so the exporter refuses rather than writing a tree whose own viewer would 404. The
/// refusal must reach the operator as prose, prefixed `ziv: error:`, not as a Debug struct.
#[test]
fn refused_export_prints_the_display_message_not_a_debug_struct() {
    let out_dir = tempfile::tempdir().unwrap();
    let out_path = out_dir.path().join("out");
    let output = Command::new(env!("CARGO_BIN_EXE_ziv"))
        .args([
            "export",
            "../../tests/fixtures/sample_unpinnable.ome.zarr",
            out_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run the ziv binary");

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit status 1; stderr was:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ziv: error: cannot export this image as a static IIIF level 0 tree"),
        "stderr missing the ziv: error: prefix + Display message; stderr was:\n{stderr}"
    );
    assert!(
        stderr.contains("ziv serve"),
        "stderr missing the `ziv serve` fallback advice; stderr was:\n{stderr}"
    );
    assert!(
        !stderr.contains("UnsupportedPyramid {"),
        "stderr still contains the raw Debug struct literal; stderr was:\n{stderr}"
    );

    assert!(
        !out_path.exists(),
        "a refused export must not create the output directory, but {} exists",
        out_path.display()
    );
}

/// A second, unrelated error path through `main` (a source path that does not exist): confirms
/// the `ziv: error:` prefix and exit code 1 aren't specific to the exporter's refusal, but apply
/// to every error `main` can return.
#[test]
fn export_of_a_missing_path_prints_the_error_prefix_and_exits_1() {
    let out_dir = tempfile::tempdir().unwrap();
    let out_path = out_dir.path().join("out");
    let output = Command::new(env!("CARGO_BIN_EXE_ziv"))
        .args([
            "export",
            "/nonexistent/path/does-not-exist.ome.zarr",
            out_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run the ziv binary");

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit status 1; stderr was:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("ziv: error: "),
        "stderr missing the ziv: error: prefix; stderr was:\n{stderr}"
    );

    assert!(
        !out_path.exists(),
        "a failed export must not create the output directory, but {} exists",
        out_path.display()
    );
}
