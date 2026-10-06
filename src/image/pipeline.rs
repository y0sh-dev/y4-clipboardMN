// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/image/pipeline.rs

use std::fmt;
use std::io::{self, ErrorKind, Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread;

use crate::core::constants::{PIPELINE_CHUNK_BYTES, PIPELINE_STDERR_CAP_BYTES};

/// Why a filter run failed. Every variant is recoverable: nothing in this
/// module panics, so a bad image or a dying child can never take a resident
/// daemon (`panic = "abort"`) down with it.
#[derive(Debug)]
pub enum PipelineError {
    /// The child could not be started (binary missing, not executable, ...).
    Spawn(io::Error),
    /// Reading the caller's input stream failed.
    Source(io::Error),
    /// Writing to the caller's output stream failed.
    Sink(io::Error),
    /// I/O with the child's pipes, or waiting on it, failed.
    Child(io::Error),
    /// The child ran but exited unsuccessfully (non-zero code, or killed by
    /// a signal when `code` is `None`). `stderr` is a capped excerpt.
    Failed { code: Option<i32>, stderr: String },
}

impl PipelineError {
    /// True when the tool itself could not be spawned (absent, wrong architecture,
    /// invalid binary format, permission denied, or OS spawn failure) — the caller's
    /// cue to fall back to raw persistence rather than treating it as input rejection.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Spawn(_))
    }
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "failed to start external process: {e}"),
            Self::Source(e) => write!(f, "failed to read pipeline input: {e}"),
            Self::Sink(e) => write!(f, "failed to write pipeline output: {e}"),
            Self::Child(e) => write!(f, "pipe I/O with external process failed: {e}"),
            Self::Failed { code: Some(code), stderr } => write!(f, "external process exited with status {code}: {stderr}"),
            Self::Failed { code: None, stderr } => write!(f, "external process was terminated by a signal: {stderr}"),
        }
    }
}

impl std::error::Error for PipelineError {}

/// Byte counts of a completed run. `bytes_in` counts what the child
/// accepted on stdin, which is less than the input's length if the child
/// stopped reading early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer {
    pub bytes_in: u64,
    pub bytes_out: u64,
}

enum FeedFailure {
    Source(io::Error),
    Child(io::Error),
}

enum RelayFailure {
    Sink(io::Error),
    Child(io::Error),
}

/// Copies `source` into the child's stdin, then drops it so the child sees EOF.
///
/// A `BrokenPipe` on the write is not an error here: it means the child
/// stopped reading (it finished, or rejected the input and exited), and the
/// exit status — inspected by the caller — is the authoritative verdict.
fn feed<R: Read, W: Write>(mut source: R, mut stdin: W) -> Result<u64, FeedFailure> {
    let mut chunk = vec![0u8; PIPELINE_CHUNK_BYTES];
    let mut total = 0u64;
    loop {
        let n = match source.read(&mut chunk) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(FeedFailure::Source(e)),
        };
        match stdin.write_all(&chunk[..n]) {
            Ok(()) => total += n as u64,
            Err(e) if e.kind() == ErrorKind::BrokenPipe => return Ok(total),
            Err(e) => return Err(FeedFailure::Child(e)),
        }
    }
}

/// Copies the child's stdout into the caller's sink, then flushes it.
fn relay<R: Read, W: Write>(mut from: R, to: &mut W) -> Result<u64, RelayFailure> {
    let mut chunk = vec![0u8; PIPELINE_CHUNK_BYTES];
    let mut total = 0u64;
    loop {
        let n = match from.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(RelayFailure::Child(e)),
        };
        to.write_all(&chunk[..n]).map_err(RelayFailure::Sink)?;
        total += n as u64;
    }
    to.flush().map_err(RelayFailure::Sink)?;
    Ok(total)
}

/// Reads `from` to EOF but keeps only the first `cap` bytes. Draining to EOF
/// is the point: a child that fills the stderr pipe while nobody reads it
/// blocks forever, which would stall the whole pipeline.
fn drain_capped<R: Read>(mut from: R, cap: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match from.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    kept
}

/// Best-effort termination plus reaping, so an abandoned child never lingers
/// as a zombie or keeps a pipe end open.
fn reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn join_worker<T>(handle: thread::ScopedJoinHandle<'_, T>) -> Result<T, PipelineError> {
    handle
        .join()
        .map_err(|_| PipelineError::Child(io::Error::other("pipeline worker thread panicked")))
}

/// Runs `program args...` as a filter: `input` is streamed into its stdin
/// and its stdout is streamed into `output`, neither side ever buffered
/// whole in memory (at most one `PIPELINE_CHUNK_BYTES` chunk per direction).
///
/// # Deadlock freedom
///
/// A pipe holds roughly 64KiB. If the parent wrote all of `input` before
/// reading any output, a filter that emits output while it is still
/// consuming input would fill its stdout pipe and block; the parent would
/// in turn block on the full stdin pipe, and neither could ever proceed.
/// To rule that out structurally, three activities always run concurrently:
///
/// 1. a scoped worker feeds `input` into the child's stdin,
/// 2. a scoped worker drains the child's stderr (capped excerpt kept),
/// 3. the calling thread relays the child's stdout into `output`.
///
/// Each pipe therefore always has a live peer, whatever order the child
/// reads and writes in. The `Read`/`Write` boundary is also the seam where a
/// future zero-copy implementation (`splice(2)` between raw descriptors) can
/// replace the chunked copy without changing any caller.
///
/// # Failure semantics
///
/// On `Err`, `output` may already hold partial bytes and must be discarded.
/// When several things go wrong at once, the most causal one is reported:
/// sink and source failures (the caller's own streams), then child pipe
/// failures, then the child's exit status. If the sink fails the child is
/// killed, because nobody will read its output any more.
pub fn run_filter<R, W>(program: &str, args: &[&str], input: R, output: &mut W) -> Result<Transfer, PipelineError>
where
    R: Read + Send,
    W: Write,
{
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(PipelineError::Spawn)?;

    let (Some(stdin), Some(mut stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        reap(&mut child);
        return Err(PipelineError::Child(io::Error::other("child pipe handle missing")));
    };

    thread::scope(|scope| {
        // `Builder::spawn_scoped` instead of `Scope::spawn`: the latter
        // panics when the OS refuses a thread, which this module must not do.
        let feeder = match thread::Builder::new()
            .name("y4p-pipe-stdin".into())
            .spawn_scoped(scope, move || feed(input, stdin))
        {
            Ok(handle) => handle,
            Err(e) => {
                reap(&mut child);
                return Err(PipelineError::Child(e));
            }
        };
        let drainer = match thread::Builder::new()
            .name("y4p-pipe-stderr".into())
            .spawn_scoped(scope, move || drain_capped(stderr, PIPELINE_STDERR_CAP_BYTES))
        {
            Ok(handle) => handle,
            Err(e) => {
                // Killing the child breaks the feeder's pipe, so the scope's
                // implicit join cannot hang on it.
                reap(&mut child);
                return Err(PipelineError::Child(e));
            }
        };

        let relayed = relay(&mut stdout, output);
        if relayed.is_err() {
            let _ = child.kill();
        }
        let status = child.wait();
        let fed = join_worker(feeder);
        let captured = join_worker(drainer);

        let bytes_out = match relayed {
            Ok(n) => n,
            Err(RelayFailure::Sink(e)) => return Err(PipelineError::Sink(e)),
            Err(RelayFailure::Child(e)) => return Err(PipelineError::Child(e)),
        };
        let bytes_in = match fed? {
            Ok(n) => n,
            Err(FeedFailure::Source(e)) => return Err(PipelineError::Source(e)),
            Err(FeedFailure::Child(e)) => return Err(PipelineError::Child(e)),
        };
        let status = status.map_err(PipelineError::Child)?;
        if !status.success() {
            let stderr = String::from_utf8_lossy(&captured?).trim().to_owned();
            return Err(PipelineError::Failed { code: status.code(), stderr });
        }
        Ok(Transfer { bytes_in, bytes_out })
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Deterministic, non-repeating-looking payload so any reordering,
    /// truncation or duplication shows up as an inequality.
    fn patterned(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8)) as u8).collect()
    }

    /// Runs `f` on a helper thread and fails the test, rather than hanging
    /// the whole suite, if it has not finished in time — a regression back
    /// to a write-all-then-read design would deadlock, not fail an assert.
    fn within_deadline<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(30))
            .expect("pipeline deadlocked: no completion within 30s")
    }

    fn run_sh(script: &'static str, input: Vec<u8>) -> (Result<Transfer, PipelineError>, Vec<u8>) {
        within_deadline(move || {
            let mut out = Vec::new();
            let result = run_filter("sh", &["-c", script], Cursor::new(input), &mut out);
            (result, out)
        })
    }

    #[test]
    fn small_block_echo_of_256kib_does_not_deadlock() {
        // 256KiB is 4x the 64KiB pipe capacity in each direction. `dd
        // bs=1024` echoes in 1KiB pieces, so it can buffer almost nothing
        // itself: a parent that wrote all input before reading any output
        // would block on a full stdin pipe while dd blocks on a full stdout
        // pipe, forever. (`cat` is deliberately not used here: its own
        // 128KiB buffer hides this deadlock at 256KiB.)
        let payload = patterned(256 * 1024);
        let expected = payload.clone();
        let (result, out) = within_deadline(move || {
            let mut out = Vec::new();
            let result = run_filter("dd", &["bs=1024"], Cursor::new(payload), &mut out);
            (result, out)
        });
        let transfer = result.unwrap();
        assert_eq!(out, expected);
        assert_eq!(transfer, Transfer { bytes_in: 256 * 1024, bytes_out: 256 * 1024 });
    }

    #[test]
    fn multi_megabyte_payload_round_trips_intact() {
        // Well past what pipes plus `cat`'s own buffer can absorb (~256KiB),
        // so a sequential write-then-read parent deadlocks here too.
        let payload = patterned(8 * 1024 * 1024 + 17);
        let expected = payload.clone();
        let (result, out) = within_deadline(move || {
            let mut out = Vec::new();
            let result = run_filter("cat", &[], Cursor::new(payload), &mut out);
            (result, out)
        });
        result.unwrap();
        assert!(out == expected, "output differs from input");
    }

    #[test]
    fn input_that_is_not_a_multiple_of_the_chunk_size_is_not_truncated() {
        let payload = patterned(PIPELINE_CHUNK_BYTES * 3 + 1);
        let expected = payload.clone();
        let (result, out) = within_deadline(move || {
            let mut out = Vec::new();
            let result = run_filter("cat", &[], Cursor::new(payload), &mut out);
            (result, out)
        });
        result.unwrap();
        assert_eq!(out, expected);
    }

    #[test]
    fn empty_input_completes_with_empty_output() {
        let (result, out) = run_sh("cat", Vec::new());
        assert_eq!(result.unwrap(), Transfer { bytes_in: 0, bytes_out: 0 });
        assert!(out.is_empty());
    }

    #[test]
    fn child_flooding_stdout_and_stderr_before_reading_stdin_does_not_deadlock() {
        // The child emits 300KB on each output pipe *before* touching stdin,
        // while the parent is simultaneously pushing 300KB in: all three
        // pipes overflow at once. Only three-way concurrency survives this.
        let (result, out) = run_sh(
            "head -c 300000 /dev/zero; head -c 300000 /dev/zero >&2; cat >/dev/null",
            patterned(300_000),
        );
        let transfer = result.unwrap();
        assert_eq!(out.len(), 300_000);
        assert_eq!(transfer.bytes_in, 300_000);
        assert_eq!(transfer.bytes_out, 300_000);
    }

    #[test]
    fn early_exit_with_unread_large_input_reports_failure_not_a_hang() {
        // The child never reads stdin and exits at once; the parent's
        // 4MiB write hits a broken pipe, which must not be mistaken for the
        // verdict — the exit status is.
        let (result, _) = run_sh("echo rejected >&2; exit 3", patterned(4 * 1024 * 1024));
        match result {
            Err(PipelineError::Failed { code, stderr }) => {
                assert_eq!(code, Some(3));
                assert_eq!(stderr, "rejected");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn child_killed_by_a_signal_is_reported_with_no_exit_code() {
        let (result, _) = run_sh("kill -9 $$", Vec::new());
        assert!(matches!(result, Err(PipelineError::Failed { code: None, .. })));
    }

    #[test]
    fn retained_stderr_is_capped_but_fully_drained() {
        // 1MiB of stderr: far past both the pipe buffer and the retained cap.
        let (result, _) = run_sh("head -c 1048576 /dev/zero | tr '\\0' 'e' >&2; exit 1", Vec::new());
        match result {
            Err(PipelineError::Failed { stderr, .. }) => assert_eq!(stderr.len(), PIPELINE_STDERR_CAP_BYTES),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn missing_program_is_classified_as_unavailable() {
        let mut out = Vec::new();
        let err = run_filter("y4p-definitely-not-a-real-binary", &[], Cursor::new(Vec::new()), &mut out)
            .unwrap_err();
        assert!(matches!(err, PipelineError::Spawn(_)));
        assert!(err.is_unavailable());
    }

    #[test]
    fn rejected_input_is_not_classified_as_unavailable() {
        let (result, _) = run_sh("exit 1", Vec::new());
        assert!(!result.unwrap_err().is_unavailable());
    }

    /// Sink that accepts `limit` bytes and then fails, like a closed socket.
    struct FailingSink {
        limit: usize,
        written: usize,
    }

    impl Write for FailingSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.written >= self.limit {
                return Err(io::Error::new(ErrorKind::BrokenPipe, "sink closed"));
            }
            self.written += buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn failing_sink_stops_the_child_and_reports_a_sink_error() {
        // `yes` would write forever; the run can only end because the sink
        // failure makes the pipeline kill the child.
        let result = within_deadline(|| {
            let mut sink = FailingSink { limit: 100_000, written: 0 };
            run_filter("yes", &[], Cursor::new(Vec::new()), &mut sink)
        });
        assert!(matches!(result, Err(PipelineError::Sink(_))));
    }

    /// Source that yields `good` bytes and then fails, like a dropped file.
    struct FailingSource {
        good: usize,
    }

    impl Read for FailingSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.good == 0 {
                return Err(io::Error::other("source went away"));
            }
            let n = buf.len().min(self.good);
            buf[..n].fill(b'x');
            self.good -= n;
            Ok(n)
        }
    }

    #[test]
    fn failing_source_is_reported_as_a_source_error() {
        let result = within_deadline(|| {
            let mut out = Vec::new();
            run_filter("cat", &[], FailingSource { good: 200_000 }, &mut out)
        });
        assert!(matches!(result, Err(PipelineError::Source(_))));
    }

    #[test]
    fn feed_treats_broken_pipe_as_a_clean_stop() {
        struct ClosedPipe;
        impl Write for ClosedPipe {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let fed = feed(Cursor::new(patterned(1000)), ClosedPipe);
        assert!(matches!(fed, Ok(0)));
    }

    #[test]
    fn drain_capped_keeps_only_the_prefix() {
        let kept = drain_capped(Cursor::new(patterned(10_000)), 100);
        assert_eq!(kept, patterned(100));
    }

    #[test]
    fn display_distinguishes_exit_codes_from_signals() {
        let coded = PipelineError::Failed { code: Some(2), stderr: "bad".into() }.to_string();
        let signalled = PipelineError::Failed { code: None, stderr: "bad".into() }.to_string();
        assert!(coded.contains("status 2"));
        assert!(signalled.contains("signal"));
    }

    #[test]
    fn is_unavailable_returns_true_for_all_spawn_error_kinds() {
        assert!(PipelineError::Spawn(io::Error::new(io::ErrorKind::NotFound, "not found")).is_unavailable());
        assert!(PipelineError::Spawn(io::Error::new(io::ErrorKind::PermissionDenied, "denied")).is_unavailable());
        assert!(PipelineError::Spawn(io::Error::new(io::ErrorKind::InvalidData, "exec format error")).is_unavailable());
        assert!(PipelineError::Spawn(io::Error::other("dynamic linker fault")).is_unavailable());

        // Non-spawn failures must NOT be reported as unavailable
        assert!(!PipelineError::Source(io::Error::other("read error")).is_unavailable());
        assert!(!PipelineError::Sink(io::Error::other("write error")).is_unavailable());
        assert!(!PipelineError::Child(io::Error::other("pipe broken")).is_unavailable());
        assert!(!PipelineError::Failed { code: Some(1), stderr: "syntax error".into() }.is_unavailable());
    }
}
