//! `ziv completions bash | head` and `ziv man | head` must exit quietly when the reader closes the
//! pipe early. That is the reader saying it has what it wants, not a failure, and every standard
//! command-line tool treats it that way. In 0.1.0 `completions` panicked
//! (`clap_complete` expects its write to succeed) and `man` printed "Broken pipe (os error 32)".
//!
//! The read end is closed before the child writes anything. Closing it after reading would not be
//! deterministic: the whole completion script fits in a pipe's buffer, so the child could finish
//! writing before the close and never see the error at all.
use std::process::{Command, Stdio};

fn run_with_closed_stdout(args: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ziv"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ziv");
    // Drop our only handle to the read end, so every write the child makes fails with EPIPE.
    drop(child.stdout.take());
    child.wait_with_output().expect("wait for ziv")
}

fn assert_quiet_exit(args: &[&str]) {
    let out = run_with_closed_stdout(args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked"),
        "`ziv {}` panicked on a closed pipe:\n{stderr}",
        args.join(" ")
    );
    assert!(
        out.status.success(),
        "`ziv {}` exited {:?} on a closed pipe, with stderr:\n{stderr}",
        args.join(" "),
        out.status.code()
    );
    assert!(
        stderr.is_empty(),
        "`ziv {}` wrote to stderr on a closed pipe:\n{stderr}",
        args.join(" ")
    );
}

#[test]
fn completions_exit_quietly_when_the_reader_closes_the_pipe() {
    for shell in ["bash", "zsh", "fish"] {
        assert_quiet_exit(&["completions", shell]);
    }
}

#[test]
fn man_exits_quietly_when_the_reader_closes_the_pipe() {
    assert_quiet_exit(&["man"]);
    assert_quiet_exit(&["man", "render"]);
}
