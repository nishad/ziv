//! Shared drift-check helper for the `tests/build_*fixture*.rs` binaries scattered across this
//! workspace's crates.
//!
//! Every one of those binaries used to build its fixture by deleting and rewriting, in place, the
//! directory committed under the workspace's top-level `tests/fixtures/`, every time it ran.
//! Under `cargo nextest run --workspace` (or any parallel `cargo test`), a different test binary
//! that merely READS that same committed path could observe it mid-rewrite: gone, half-written, or
//! momentarily missing a file, depending purely on scheduling. That is a genuine, timing-dependent
//! test failure, not a hypothetical one: it is exactly what made
//! `a_broken_label_is_reported_by_name_and_reason` fail on CI while
//! `build_broken_label_fixture`'s own `builds_fixture` test ran concurrently and deleted the tree
//! it reads.
//!
//! [`check_or_regenerate`] replaces that pattern. It builds the fixture into a private, unique
//! temporary directory and then either:
//!  - compares it byte-for-byte (and file-SET-for-file-set, so an extra or missing file is also a
//!    failure) against the committed copy and panics naming the differences, or
//!  - if `ZIV_REGENERATE_FIXTURES` is set in the environment, skips the comparison and overwrites
//!    the committed copy with the freshly built tree instead.
//!
//! No test that uses this helper ever writes to a committed fixture path during a normal run,
//! which removes the race by construction rather than by timing luck, and the committed fixtures
//! gain a guarantee they did not have before: that the checked-in bytes are exactly what the
//! generator that claims to build them actually produces today, not merely what it produced the
//! last time someone happened to run it locally.
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The only environment variable that causes a write under `tests/fixtures/`. Set to any value
/// (checked with `var_os`, so even `ZIV_REGENERATE_FIXTURES=` counts) to regenerate rather than
/// check.
pub const REGENERATE_ENV_VAR: &str = "ZIV_REGENERATE_FIXTURES";

/// Builds a fixture with `build` into a fresh, private temporary directory, then checks it
/// against the committed copy at `committed_root`.
///
/// - Normally: panics if the two trees differ in file set or in any file's bytes, naming every
///   difference found, and printing `regen_hint` — the exact command the caller should re-run
///   with [`REGENERATE_ENV_VAR`] set to update the committed copy.
/// - With [`REGENERATE_ENV_VAR`] set in the environment: skips the comparison and overwrites
///   `committed_root` with the freshly built tree. This is the ONLY code path, anywhere in this
///   helper, that writes to `committed_root`.
///
/// `build` receives the temporary directory's path and must write the fixture tree at exactly
/// that path. It must never touch `committed_root` (or anything else under `tests/fixtures/`)
/// itself — that is what makes this race-free under parallel test execution.
pub fn check_or_regenerate(committed_root: &Path, regen_hint: &str, build: impl FnOnce(&Path)) {
    let tmp = tempfile::tempdir().expect("create a temporary directory for the fixture build");
    build(tmp.path());

    if std::env::var_os(REGENERATE_ENV_VAR).is_some() {
        regenerate(tmp.path(), committed_root);
        return;
    }

    let diffs = differences(tmp.path(), committed_root);
    if !diffs.is_empty() {
        panic!(
            "fixture drift detected: the committed fixture at {committed} no longer matches \
             what this test's generator produces.\n\n  {joined}\n\nIf this generator change is \
             intentional, regenerate the committed fixture with:\n\n  {regen_hint}\n\n(no test \
             writes to the committed fixture directly; that env var is the only way)\n",
            committed = committed_root.display(),
            joined = diffs.join("\n  "),
        );
    }
}

/// Recursively lists every regular file under `root`, as paths relative to `root`. A missing
/// `root` is treated as an empty tree (the natural state of a fixture that has never been
/// committed yet), not an error.
fn collect_files(root: &Path) -> BTreeSet<PathBuf> {
    let mut files = BTreeSet::new();
    collect_files_into(root, root, &mut files);
    files
}

fn collect_files_into(root: &Path, dir: &Path, files: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let entry = entry.expect("read fixture directory entry");
        let path = entry.path();
        let file_type = entry.file_type().expect("read fixture entry file type");
        if file_type.is_dir() {
            collect_files_into(root, &path, files);
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(root)
                .expect("walked path is under its own root")
                .to_path_buf();
            files.insert(rel);
        }
        // Fixtures never contain symlinks; anything else is deliberately ignored rather than
        // silently followed.
    }
}

/// Every difference between the freshly built tree at `built` and the committed tree at
/// `committed`: files present in only one of the two, and files present in both whose bytes
/// differ. Empty means the two trees are byte-for-byte identical, including their file sets.
fn differences(built: &Path, committed: &Path) -> Vec<String> {
    let built_files = collect_files(built);
    let committed_files = collect_files(committed);
    let mut diffs = Vec::new();

    for extra in built_files.difference(&committed_files) {
        diffs.push(format!(
            "{} is produced by the generator but is not part of the committed fixture",
            extra.display()
        ));
    }
    for missing in committed_files.difference(&built_files) {
        diffs.push(format!(
            "{} is part of the committed fixture but the generator no longer writes it",
            missing.display()
        ));
    }
    for rel in built_files.intersection(&committed_files) {
        let built_bytes = fs::read(built.join(rel)).expect("read freshly built fixture file");
        let committed_bytes = fs::read(committed.join(rel)).expect("read committed fixture file");
        if built_bytes != committed_bytes {
            diffs.push(format!(
                "{} differs: the generator now produces {} bytes, the committed fixture has {} \
                 bytes",
                rel.display(),
                built_bytes.len(),
                committed_bytes.len()
            ));
        }
    }
    diffs
}

/// Overwrites `committed_root` with the tree built at `built`. The only caller is
/// [`check_or_regenerate`], and only when [`REGENERATE_ENV_VAR`] is set.
fn regenerate(built: &Path, committed_root: &Path) {
    if committed_root.exists() {
        fs::remove_dir_all(committed_root).expect("remove previous committed fixture");
    }
    copy_dir_all(built, committed_root);
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create fixture directory");
    for entry in fs::read_dir(src).expect("read freshly built fixture directory") {
        let entry = entry.expect("read fixture directory entry");
        let path = entry.path();
        let file_type = entry.file_type().expect("read fixture entry file type");
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&path, &dst_path);
        } else if file_type.is_file() {
            fs::copy(&path, &dst_path).expect("copy fixture file");
        }
    }
}
