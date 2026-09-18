//! Signal-injection proof of graceful shutdown: spawns the REAL `ziv` binary, puts a request in
//! flight, sends it a genuine SIGTERM, and asserts the in-flight response still arrives complete
//! and the process exits cleanly.
//!
//! This replaces what was previously a self-declared manual-verification item. The in-process
//! tests in `server` can only prove the drain future is wired up and that the loop returns; they
//! cannot prove the binary installs a SIGTERM handler at all, which is the part an operator
//! actually depends on when a container runtime or systemd stops the service. A process that
//! ignores SIGTERM looks identical in every in-process test and then gets SIGKILLed after the
//! runtime's grace period, truncating whatever it was serving.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Kills the child on drop so a failed assertion never leaks a serving process onto the port.
struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_until_serving(addr: &str, deadline: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < deadline {
        if TcpStream::connect(addr).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn sigterm_drains_an_in_flight_request_and_exits_cleanly() {
    // Port 0 would be ideal, but the child prints its address rather than reporting it back, so a
    // fixed high port is used and the readiness probe below tolerates it being briefly unbound.
    let addr = "127.0.0.1:3094";
    let mut child = ServerProcess(
        Command::new(env!("CARGO_BIN_EXE_ziv"))
            .args([
                "serve",
                "../../tests/fixtures/sample_multi_tile.ome.zarr",
                "--addr",
                addr,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the ziv binary"),
    );

    assert!(
        wait_until_serving(addr, Duration::from_secs(60)),
        "ziv did not start serving on {addr}"
    );

    // Put a real request in flight: a full-image render at level 0 of the multi-tile fixture, so
    // the server has actual work to do between accepting the request and writing the response.
    let mut sock = TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    sock.write_all(
        b"GET /iiif/default/full/max/0/default.jpg HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    sock.flush().unwrap();

    // SIGTERM while that request is being served. If the binary ignored SIGTERM the process would
    // still be alive at the end; if it shut down abruptly instead of draining, the response below
    // would be truncated or the connection reset.
    std::thread::sleep(Duration::from_millis(5));
    let pid = child.0.id();
    let killed = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("failed to run kill");
    assert!(killed.success(), "kill -TERM failed for pid {pid}");

    // The in-flight response must arrive COMPLETE. Reading to EOF and checking the body length
    // against Content-Length is what distinguishes a real drain from a mid-write teardown.
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw)
        .expect("in-flight response was not readable after SIGTERM");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("no header/body boundary in the response");
    let headers = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = &raw[split + 4..];

    assert!(
        headers.starts_with("HTTP/1.1 200 OK"),
        "expected 200 for the in-flight request, got: {}",
        headers.lines().next().unwrap_or("<empty>")
    );
    let content_length: usize = headers
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .expect("response had no parseable Content-Length");
    assert_eq!(
        body.len(),
        content_length,
        "response body was TRUNCATED by shutdown: got {} bytes, Content-Length said {content_length}",
        body.len()
    );
    assert_eq!(&body[0..2], &[0xFF, 0xD8], "body is not a JPEG");

    // And the process must actually exit on its own, cleanly, without needing SIGKILL.
    let started = Instant::now();
    let status = loop {
        match child.0.try_wait().unwrap() {
            Some(status) => break status,
            None if started.elapsed() > Duration::from_secs(30) => {
                panic!("ziv did not exit within 30s of SIGTERM — is the signal handler installed?")
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    assert!(
        status.success(),
        "ziv exited non-zero after SIGTERM: {status:?}"
    );

    // The graceful path logs before draining; its absence means the process died some other way.
    let mut stdout = String::new();
    if let Some(out) = child.0.stdout.take() {
        let mut reader = BufReader::new(out);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            stdout.push_str(&line);
            line.clear();
        }
    }
    assert!(
        stdout.contains("shutdown signal received"),
        "expected the graceful-shutdown log line; got: {stdout}"
    );
}
