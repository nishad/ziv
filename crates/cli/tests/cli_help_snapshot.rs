//! Snapshot test over the CLI's `--help` surface.
//!
//! The shell completions and the man page shipped in release archives are generated from the same
//! clap command tree (see `ziv completions` / `ziv man`), so a rename or a reworded help string
//! silently changes those artifacts too. Locking the rendered help means such a change has to be
//! deliberate: the diff shows up here first.
//!
//! Snapshots live in `tests/cmd/*.trycmd`. To regenerate after an intentional CLI change:
//!
//! ```text
//! TRYCMD=overwrite cargo test -p ziv --test cli_help_snapshot
//! ```
#[test]
fn help_surface_matches_snapshot() {
    trycmd::TestCases::new()
        .default_bin_name("ziv")
        .case("tests/cmd/*.trycmd");
}
