// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/image/magick.rs

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use crate::core::constants::MAGICK_PROGRAM;
use crate::image::pipeline::{self, PipelineError, Transfer};

/// True when `program -version` can be spawned and exits successfully.
///
/// Never panics: a missing binary, a non-executable file or a non-zero exit
/// all collapse to `false`, which is exactly the signal a caller needs to
/// choose a fallback. All three std streams are nulled so the probe can
/// neither block on a pipe nor write into the caller's terminal.
pub fn probe(program: &str) -> bool {
    Command::new(program)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Whether ImageMagick's `magick` is usable on this system.
///
/// Probed once per process and cached: the answer only changes if the
/// operator installs or removes the tool, and re-spawning a process for
/// every conversion request would dwarf the cost of small conversions.
pub fn is_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| probe(MAGICK_PROGRAM))
}

/// Streams `input` through `magick <args>` into `output`.
///
/// Deliberately does not consult [`is_available`] first: a missing binary
/// already surfaces as [`PipelineError::Spawn`], which
/// [`PipelineError::is_unavailable`] classifies, and a pre-check would only
/// add a second process spawn plus a check-then-use race. Callers that want
/// to avoid even attempting the call can gate on [`is_available`] themselves.
///
/// On `Err`, `output` may hold partial bytes and must be discarded.
pub fn run<R, W>(args: &[&str], input: R, output: &mut W) -> Result<Transfer, PipelineError>
where
    R: Read + Send,
    W: Write,
{
    pipeline::run_filter(MAGICK_PROGRAM, args, input, output)
}

/// [`run`] under a wall-clock budget: a `magick` that hangs past `timeout`
/// is killed and the call ends with [`PipelineError::Timeout`].
pub fn run_with_timeout<R, W>(args: &[&str], input: R, output: &mut W, timeout: Duration) -> Result<Transfer, PipelineError>
where
    R: Read + Send,
    W: Write,
{
    pipeline::run_filter_timeout(MAGICK_PROGRAM, args, input, output, timeout)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::{Cursor, empty};

    const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

    fn magick_or_skip() -> bool {
        if is_available() {
            true
        } else {
            eprintln!("skipping: `magick` is not available on this system");
            false
        }
    }

    /// Generates a solid-colour PNG of `size`x`size` through the pipeline itself.
    fn solid_png(size: u32) -> Vec<u8> {
        let geometry = format!("{size}x{size}");
        let mut png = Vec::new();
        run(&["-size", &geometry, "xc:red", "png:-"], empty(), &mut png).unwrap();
        png
    }

    #[test]
    fn probe_reports_false_for_a_missing_binary() {
        assert!(!probe("y4p-definitely-not-a-real-binary"));
    }

    #[test]
    fn probe_reports_false_for_a_non_zero_exit() {
        // `false` ignores its arguments and always exits 1.
        assert!(!probe("false"));
    }

    #[test]
    fn probe_reports_true_for_a_working_binary() {
        // `true` ignores its arguments and always exits 0.
        assert!(probe("true"));
    }

    #[test]
    fn identity_conversion_round_trips_a_png() {
        if !magick_or_skip() { return; }
        let source = solid_png(16);
        assert!(source.starts_with(PNG_SIGNATURE));

        let mut converted = Vec::new();
        let transfer = run(&["-", "png:-"], Cursor::new(source.clone()), &mut converted).unwrap();
        assert!(converted.starts_with(PNG_SIGNATURE));
        assert_eq!(transfer.bytes_in, source.len() as u64);
        assert_eq!(transfer.bytes_out, converted.len() as u64);

        // Dimensions survive the round trip.
        let mut info = Vec::new();
        run(&["-", "-format", "%wx%h", "info:"], Cursor::new(converted), &mut info).unwrap();
        assert_eq!(String::from_utf8_lossy(&info).trim(), "16x16");
    }

    #[test]
    fn large_image_exceeding_the_pipe_buffer_converts_without_deadlock() {
        if !magick_or_skip() { return; }
        // Incompressible noise: the PNG alone is far beyond 64KiB, and the
        // BMP it converts to is ~786KiB, so both pipe directions overflow.
        let mut noisy = Vec::new();
        run(&["-size", "512x512", "xc:", "+noise", "Random", "png:-"], empty(), &mut noisy).unwrap();
        assert!(noisy.len() > 64 * 1024);

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut bmp = Vec::new();
            let result = run(&["-", "bmp:-"], Cursor::new(noisy), &mut bmp);
            let _ = tx.send((result, bmp));
        });
        let (result, bmp) = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("magick pipeline deadlocked");
        result.unwrap();
        assert!(bmp.starts_with(b"BM"));
        assert!(bmp.len() > 512 * 512 * 3);
    }

    #[test]
    fn invalid_input_yields_a_failed_error_with_diagnostics() {
        if !magick_or_skip() { return; }
        let mut out = Vec::new();
        let err = run(&["-", "png:-"], Cursor::new(b"this is not an image".to_vec()), &mut out)
            .unwrap_err();
        match err {
            PipelineError::Failed { code, stderr } => {
                assert_ne!(code, Some(0));
                assert!(!stderr.is_empty(), "magick should explain why it rejected the input");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn large_invalid_input_fails_cleanly_instead_of_hanging_on_a_broken_pipe() {
        if !magick_or_skip() { return; }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let result = run(&["-", "png:-"], Cursor::new(vec![0xA5u8; 4 * 1024 * 1024]), &mut out);
            let _ = tx.send(result);
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("magick pipeline hung on early child exit");
        assert!(matches!(result, Err(PipelineError::Failed { .. })));
    }
}
