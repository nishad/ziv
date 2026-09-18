//! Nothing else checks that the committed `dist-assets/` (shell completions + man pages, listed
//! in the root `Cargo.toml`'s `[[workspace.metadata.dist.extra-artifacts]]` and shipped in
//! release archives) still match what the binary actually generates. A stale `serve` description
//! already shipped unnoticed once. This runs the real binary the same way an operator
//! regenerating these files would, and asserts each output is byte-identical to the committed
//! file — in `cargo test`, so in CI, with no separate workflow step.

use std::path::Path;
use std::process::Command;

/// Every shipped dist-asset: its path, the `ziv` arguments that generate it, and the command an
/// operator runs to regenerate it. The single table the per-file tests below read, and the one
/// `every_shipped_dist_asset_is_guarded` checks against the root `Cargo.toml`, so a file added to
/// the release list without a row here fails `cargo test` instead of shipping unguarded.
const ASSETS: &[(&str, &[&str], &str)] = &[
    (
        "dist-assets/completions/ziv.bash",
        &["completions", "bash"],
        "cargo run -p ziv -- completions bash > dist-assets/completions/ziv.bash",
    ),
    (
        "dist-assets/completions/_ziv",
        &["completions", "zsh"],
        "cargo run -p ziv -- completions zsh > dist-assets/completions/_ziv",
    ),
    (
        "dist-assets/completions/ziv.fish",
        &["completions", "fish"],
        "cargo run -p ziv -- completions fish > dist-assets/completions/ziv.fish",
    ),
    (
        "dist-assets/man/ziv.1",
        &["man"],
        "cargo run -p ziv -- man > dist-assets/man/ziv.1",
    ),
    (
        "dist-assets/man/ziv-serve.1",
        &["man", "serve"],
        "cargo run -p ziv -- man serve > dist-assets/man/ziv-serve.1",
    ),
    (
        "dist-assets/man/ziv-export.1",
        &["man", "export"],
        "cargo run -p ziv -- man export > dist-assets/man/ziv-export.1",
    ),
    (
        "dist-assets/man/ziv-render.1",
        &["man", "render"],
        "cargo run -p ziv -- man render > dist-assets/man/ziv-render.1",
    ),
];

/// Generates the asset at `relative_path` from its `ASSETS` row and compares it with the
/// committed file.
fn check(relative_path: &str) {
    let (_, args, regen) = ASSETS
        .iter()
        .find(|(path, _, _)| *path == relative_path)
        .unwrap_or_else(|| panic!("{relative_path} has no row in ASSETS"));
    assert_matches_committed(relative_path, &ziv_stdout(args), regen);
}

/// The paths listed in the root `Cargo.toml`'s `[[workspace.metadata.dist.extra-artifacts]]`
/// `artifacts` array. Read with a plain text scan rather than a TOML parser, to avoid a
/// dependency for one array of quoted paths; it fails loudly if the shape it expects is missing.
fn shipped_artifacts() -> Vec<String> {
    let manifest =
        std::fs::read_to_string(Path::new("../../Cargo.toml")).expect("read the root Cargo.toml");
    let table = manifest
        .find("[[workspace.metadata.dist.extra-artifacts]]")
        .expect("root Cargo.toml has no [[workspace.metadata.dist.extra-artifacts]] table");
    let rest = &manifest[table..];
    let open = rest
        .find("artifacts = [")
        .expect("extra-artifacts table has no `artifacts = [` array");
    let close = rest[open..]
        .find(']')
        .expect("extra-artifacts `artifacts` array is not closed");
    rest[open..open + close]
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// Runs the real `ziv` binary with `args` and returns stdout. Panics (naming `args` and stderr)
/// on a non-zero exit, since comparing a committed file against a failed command's empty stdout
/// would fail with a misleading diff instead of the real problem.
fn ziv_stdout(args: &[&str]) -> Vec<u8> {
    let output = Command::new(env!("CARGO_BIN_EXE_ziv"))
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run the ziv binary with {args:?}: {e}"));
    assert!(
        output.status.success(),
        "ziv {args:?} exited non-zero; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// Asserts `actual` is byte-identical to the committed file at `relative_path` (relative to the
/// repo root; these tests run with cwd = `crates/cli`, so `../../` reaches it, matching the style
/// `export_progress.rs` uses for fixture paths). On mismatch, the failure names the stale file and
/// gives the exact command that regenerates it, so fixing it is a copy-paste, never a guess.
///
/// Both sides are normalised from CRLF to LF before comparing. `ziv`'s own stdout is always LF
/// (its own line endings never depend on the host), but a `.gitattributes` line-ending policy is
/// enforced only from the moment it is committed and only by tooling that reads it —
/// `actions/checkout` does not override Git for Windows' `core.autocrlf`, so a Windows CI runner
/// can still hand this test a CRLF-checked-out `committed` even with `dist-assets/** text eol=lf`
/// in place (a stale local clone predating that file, a checkout step that skips attributes,
/// etc). Normalising here means a genuinely stale asset is still caught (its LF-normalised
/// content still differs), while a checkout-only line-ending difference is not mistaken for one.
fn assert_matches_committed(relative_path: &str, actual: &[u8], regen_command: &str) {
    let path = Path::new("../..").join(relative_path);
    let committed = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("failed to read committed file {}: {e}", path.display()));
    let normalise = |bytes: &[u8]| -> Vec<u8> {
        // `\r\n` -> `\n`; a lone `\r` (old Mac style) is not part of this normalisation, since
        // neither `ziv`'s output nor a `core.autocrlf` checkout of an `eol=lf` file produces one.
        let mut out = Vec::with_capacity(bytes.len());
        let mut iter = bytes.iter().copied().peekable();
        while let Some(b) = iter.next() {
            if b == b'\r' && iter.peek() == Some(&b'\n') {
                continue;
            }
            out.push(b);
        }
        out
    };
    assert!(
        normalise(actual) == normalise(&committed),
        "{relative_path} is stale: it no longer matches what `ziv` generates.\n\
         Regenerate it with:\n  {regen_command}"
    );
}

#[test]
fn bash_completions_match_the_binary() {
    check("dist-assets/completions/ziv.bash");
}

#[test]
fn zsh_completions_match_the_binary() {
    check("dist-assets/completions/_ziv");
}

#[test]
fn fish_completions_match_the_binary() {
    check("dist-assets/completions/ziv.fish");
}

#[test]
fn top_level_man_page_matches_the_binary() {
    check("dist-assets/man/ziv.1");
}

#[test]
fn serve_man_page_matches_the_binary() {
    check("dist-assets/man/ziv-serve.1");
}

#[test]
fn export_man_page_matches_the_binary() {
    check("dist-assets/man/ziv-export.1");
}

#[test]
fn render_man_page_matches_the_binary() {
    check("dist-assets/man/ziv-render.1");
}

/// Guards the guard: the release list in `Cargo.toml` and `ASSETS` must name exactly the same
/// files. Without this, a man page added to the release for a new subcommand would ship with no
/// drift check at all, and every test here would stay green.
#[test]
fn every_shipped_dist_asset_is_guarded() {
    let mut shipped = shipped_artifacts();
    shipped.sort();
    let mut guarded: Vec<String> = ASSETS.iter().map(|(path, _, _)| path.to_string()).collect();
    guarded.sort();
    assert_eq!(
        shipped, guarded,
        "the release list in Cargo.toml (left) and ASSETS in this file (right) disagree; \
         add a row to ASSETS for every shipped dist-asset"
    );
}

/// The subcommand names clap itself considers user-facing: not `#[command(hide = true)]`
/// (`completions`/`man` carry that, and ship no man page of their own by design) and not clap's
/// own implicit `help` subcommand. Derived from the real `Cli` command tree — via `ziv::Cli`,
/// which `crates/cli/src/lib.rs` exists specifically so this test binary can link against — so a
/// subcommand `src/lib.rs` adds is picked up here automatically rather than needing someone to
/// remember to also update a hardcoded list.
fn user_facing_subcommand_names() -> Vec<String> {
    let mut cmd = <ziv::Cli as clap::CommandFactory>::command();
    cmd.build();
    cmd.get_subcommands()
        .filter(|sc| !sc.is_hide_set() && sc.get_name() != "help")
        .map(|sc| sc.get_name().to_string())
        .collect()
}

/// The gap this closes: before this test existed, nothing asserted that a new subcommand ships a
/// man page at all — `ASSETS` was a hardcoded table nobody was forced to extend, so a subcommand
/// added with no `dist-assets/man/ziv-{name}.1` row (and so no committed page, and no drift check
/// on it ever) left every other test in this file green. This derives the expected set of pages
/// from clap's own subcommand list (`user_facing_subcommand_names`, not a second hardcoded table)
/// and fails the moment one has no corresponding `ASSETS` row.
#[test]
fn every_user_facing_subcommand_has_a_man_page() {
    let names = user_facing_subcommand_names();
    assert!(
        !names.is_empty(),
        "found no user-facing subcommands at all; the `Cli` command tree lookup is broken"
    );
    for name in names {
        let expected = format!("dist-assets/man/ziv-{name}.1");
        assert!(
            ASSETS.iter().any(|(path, _, _)| *path == expected),
            "subcommand `{name}` has no man page row in ASSETS ({expected}); generate one with \
             `cargo run -p ziv -- man {name} > {expected}`, commit it, and add a row to ASSETS \
             (and to the release list in the root Cargo.toml) for it"
        );
    }
}
