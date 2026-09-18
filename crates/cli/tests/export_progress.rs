//! `ziv export`'s progress and warning output is otherwise uncovered: `crates/cli/tests/` only
//! ever exercises `serve` (the e2e and graceful-shutdown tests) and `--help` snapshots, so nothing
//! runs the real binary through `export`. That gap let three silent regressions compile clean and
//! leave the whole suite green: swallowing `ExportEvent::Warning`, swallowing `TreeWritten`, and
//! replacing the summary line's text — each is either a compiling no-op or a string change, so no
//! existing unit test (which only checks clap parsing) or exporter-crate test (which never spawns
//! the binary) notices. Concretely: `ziv export --labels` on an image with no label images would
//! silently export one view with no explanation, and CI would stay green.
//!
//! This spawns the real `ziv` binary against `sample_u8.ome.zarr` (no label images) with
//! `--labels`, and checks all three progress/warning surfaces land in the stream `main.rs` intends
//! them for: the warning and per-tree progress on stderr, the final summary on stdout. A second
//! test covers the same gap for the level0 cost warning (`sample_no_pyramid.ome.zarr`), which has
//! its own path from `iiif::level0_sizes` through `ExportEvent::Warning` and could just as easily
//! be silently dropped on the way to stderr.

use std::process::Command;

#[test]
fn export_with_labels_on_an_unlabeled_image_reports_progress_and_warns() {
    let out_dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ziv"))
        .args([
            "export",
            "../../tests/fixtures/sample_u8.ome.zarr",
            out_dir.path().to_str().unwrap(),
            "--labels",
        ])
        .output()
        .expect("failed to run the ziv binary");

    assert!(
        output.status.success(),
        "ziv export exited non-zero; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The image has no label images, so `--labels` adds nothing: the exporter reports this as a
    // warning rather than an error (the export still proceeds), and the CLI must not swallow it.
    assert!(
        stderr
            .contains("ziv export: warning: --labels adds nothing: the image has no label images"),
        "stderr missing the --labels warning; stderr was:\n{stderr}"
    );
    // At least one `[index/total]` per-tree progress line, so a long export shows it is moving.
    assert!(
        stderr.lines().any(|line| line.contains("[1/1]")),
        "stderr missing a [i/n] progress line; stderr was:\n{stderr}"
    );
    // The final summary lands on stdout, not stderr, so `ziv export ... 2>/dev/null` still shows
    // it and a script capturing stdout alone gets a parseable result.
    assert!(
        stdout.contains("ziv export: wrote") && stdout.contains("info.json and index.html to"),
        "stdout missing the summary line; stdout was:\n{stdout}"
    );
}

/// Task 5's level0 cost warning must reach the operator through the real binary, not merely
/// `ExportSummary::warnings`: `main.rs`'s `print_export_event` only ever prints
/// `ExportEvent::Warning`, never the summary, so a build that dropped the event while still
/// populating the summary would leave an operator with no warning on stderr at all, and every
/// exporter-crate unit test (which checks `summary.warnings` directly) would stay green.
/// `sample_no_pyramid.ome.zarr` (single, untrimmed level bigger than the tile size) is exactly the
/// shape that pays the extra full-resolution whole-image cost this warning exists to name.
#[test]
fn export_of_a_no_pyramid_image_warns_about_the_full_resolution_cost_on_stderr() {
    let out_dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ziv"))
        .args([
            "export",
            "../../tests/fixtures/sample_no_pyramid.ome.zarr",
            out_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("failed to run the ziv binary");

    assert!(
        output.status.success(),
        "ziv export exited non-zero; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ziv export: warning:") && stderr.contains("full resolution"),
        "stderr missing the level0 full-resolution cost warning; stderr was:\n{stderr}"
    );
}
