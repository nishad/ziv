//! The `ziv` binary. Everything (the `Cli`/`Command` definitions, `run()`, error reporting) lives
//! in `lib.rs` so `crates/cli/tests/` can link against it directly — in particular so
//! `dist_assets_are_current.rs` can derive the set of user-facing subcommands from clap's own
//! command tree (`ziv::Cli::command()`) rather than a hardcoded list that a new subcommand could
//! silently bypass.

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match ziv::run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            ziv::report_error(&*err);
            std::process::ExitCode::FAILURE
        }
    }
}
