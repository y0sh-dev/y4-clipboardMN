// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/image/route.rs

//! MIME-aware dispatch for ingestion: decides whether a clipboard payload
//! should be transcoded (privacy-stripped and compressed) and, if so, runs
//! the transcoder with a guaranteed fall-back to the untouched original.
//!
//! The contract is "never lose data, never make things worse": whatever goes
//! wrong, the caller still gets back a usable `(mime, bytes)` pair.
//!
//! Conversions run behind a [`CircuitBreaker`]: a converter that keeps
//! failing is skipped for a cooldown instead of being re-spawned per image.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use std::time::Duration;

use crate::core::constants::{
    IMAGE_BREAKER_COOLDOWN_SECS, IMAGE_BREAKER_THRESHOLD, IMAGE_INGEST_JPEG_QUALITY, LOG_WARN,
    MSG_IMAGE_TOOL_MISSING, log_image_breaker_closed, log_image_breaker_open,
};
use crate::core::utils::detect_image_mime;
use crate::image::breaker::{CircuitBreaker, PermitOutcome};
use crate::image::magick;
use crate::image::pipeline::{PipelineError, Transfer};
use crate::image::transcode::{self, InputFormat, OutputFormat, Quality, Transcode};

/// Bytes inspected when classifying a payload. Binary image signatures fit
/// in 12; the rest is slack for leading whitespace before an SVG prolog.
const SNIFF_HEAD_BYTES: usize = 512;

/// What an ingested image of format `input` is converted to, or `None` to
/// store it untouched.
///
/// - PNG and BMP become lossless WebP: bit-exact, metadata removed, and
///   normally much smaller (BMP is uncompressed).
/// - JPEG becomes lossy WebP at `IMAGE_INGEST_JPEG_QUALITY`: JPEG is where
///   EXIF/GPS from phones and cameras lives, so it must be re-encoded to be
///   stripped, and lossless WebP would make it several times larger.
/// - GIF and WebP are left alone. The transcoder keeps only the first frame
///   (a safety property for untrusted input), which would silently destroy
///   an animation the user copied.
pub fn ingest_target(input: InputFormat) -> Option<OutputFormat> {
    match input {
        InputFormat::Png | InputFormat::Bmp => Some(OutputFormat::WebpLossless),
        InputFormat::Jpeg => Some(OutputFormat::WebpLossy(Quality::new(IMAGE_INGEST_JPEG_QUALITY))),
        InputFormat::Gif | InputFormat::Webp => None,
    }
}

/// Decides which decoder (if any) a payload belongs to. The bytes outrank
/// the label: when the leading bytes identify an image, that identification
/// is final — so an SVG announced as `image/png` is recognised as SVG and
/// bypassed instead of being handed to a raster decoder. Only when the bytes
/// are not recognised (BMP has no short signature; corrupt data has none)
/// does the declared MIME decide. Non-images yield `None` and cost nothing.
fn classify(mime: &str, payload: &[u8]) -> Option<InputFormat> {
    let head = payload.get(..SNIFF_HEAD_BYTES).unwrap_or(payload);
    match detect_image_mime(head) {
        Some(detected) => InputFormat::from_mime(detected),
        None => InputFormat::from_mime(mime),
    }
}

/// A converter must return a plausible file of the promised type; anything
/// else (empty, or the wrong signature) is treated as a failed conversion
/// rather than stored.
fn is_valid_output(target: OutputFormat, converted: &[u8]) -> bool {
    !converted.is_empty() && detect_image_mime(converted) == Some(target.mime())
}

/// Whether a failed conversion says something about the converter's health.
/// Crashes, non-zero exits, deadline overruns and broken pipes do. A missing
/// tool (`Spawn`) is a different, static condition handled by the
/// availability probe, and `Source`/`Sink` errors concern the caller's own
/// streams; counting either would let an unrelated problem open the circuit.
fn is_converter_fault(error: &PipelineError) -> bool {
    matches!(
        error,
        PipelineError::Failed { .. } | PipelineError::Timeout(_) | PipelineError::Child(_)
    )
}

/// Core routing, with the converter injected so every branch — including
/// "no converter was even invoked" — is deterministic to test.
///
/// Returns `(mime, bytes)` to persist: the converted pair on success, the
/// original pair, byte for byte, on bypass or on any failure.
///
/// Breaker accounting happens only around a real conversion attempt, so the
/// counters cannot be polluted by what never reaches the converter: payloads
/// bypassed by classification or policy never touch the breaker, a missing
/// tool is released neutrally, and while the circuit is open the original is
/// returned without calling `convert` at all.
pub fn route_with<F>(mime: String, payload: Vec<u8>, breaker: &CircuitBreaker, convert: F) -> (String, Vec<u8>)
where
    F: FnOnce(Transcode, &[u8], &mut Vec<u8>) -> Result<Transfer, PipelineError>,
{
    // Nothing to convert (ingestion already rejects empty reads; this keeps
    // the function safe on its own): never spawn a process for zero bytes.
    if payload.is_empty() {
        return (mime, payload);
    }
    let Some(input) = classify(&mime, &payload) else { return (mime, payload) };
    let Some(output) = ingest_target(input) else { return (mime, payload) };

    let Some(permit) = breaker.acquire() else { return (mime, payload) };

    let mut converted = Vec::new();
    let (outcome, result_pair) = match convert(Transcode { input, output }, &payload, &mut converted) {
        Ok(_) if is_valid_output(output, &converted) => {
            (PermitOutcome::Success, (output.mime().to_owned(), converted))
        }
        // "Success" that produced no usable file is a converter fault too.
        Ok(_) => (PermitOutcome::Failure, (mime, payload)),
        Err(error) if is_converter_fault(&error) => (PermitOutcome::Failure, (mime, payload)),
        Err(_) => (PermitOutcome::Release, (mime, payload)),
    };

    let (tripped, recovered) = permit.resolve(outcome);
    if recovered {
        eprintln!("{}", log_image_breaker_closed());
    } else if tripped {
        report_failure(true);
    }

    result_pair
}

fn report_failure(tripped: bool) {
    if tripped {
        eprintln!("{}", log_image_breaker_open(IMAGE_BREAKER_THRESHOLD, IMAGE_BREAKER_COOLDOWN_SECS));
    }
}

/// Ingestion entry point: [`route_with`] backed by the real `magick` and the
/// process-wide breaker.
///
/// `magick` is only probed (and only spawned) once a payload has been
/// classified as a convertible raster image, so text, HTML, SVG and URI
/// lists never pay for it. When it is missing, a single warning is printed
/// per process and images are stored unmodified.
pub fn route_for_ingest(mime: String, payload: Vec<u8>) -> (String, Vec<u8>) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    static BREAKER: CircuitBreaker =
        CircuitBreaker::new(IMAGE_BREAKER_THRESHOLD, Duration::from_secs(IMAGE_BREAKER_COOLDOWN_SECS));

    route_with(mime, payload, &BREAKER, |job, input, output| {
        if !magick::is_available() {
            if !WARNED.swap(true, Ordering::Relaxed) {
                eprintln!("{}{}", LOG_WARN, MSG_IMAGE_TOOL_MISSING);
            }
            return Err(PipelineError::Spawn(io::Error::from(io::ErrorKind::NotFound)));
        }
        transcode::transcode(job, input, output)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::empty;
    use crate::image::breaker::BreakerState;

    fn magick_or_skip() -> bool {
        if magick::is_available() {
            true
        } else {
            eprintln!("skipping: `magick` is not available on this system");
            false
        }
    }

    fn generate(args: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        magick::run(args, empty(), &mut out).unwrap();
        out
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn png() -> Vec<u8> {
        generate(&["-size", "32x32", "plasma:fractal", "-depth", "8", "png:-"])
    }

    fn fake_webp() -> Vec<u8> {
        let mut data = b"RIFF".to_vec();
        data.extend_from_slice(&[0, 0, 0, 0]);
        data.extend_from_slice(b"WEBP");
        data.extend_from_slice(b"payload");
        data
    }

    fn fresh_breaker() -> CircuitBreaker {
        CircuitBreaker::new(3, Duration::from_secs(30))
    }

    /// Runs `route_with` on `breaker` with a converter that records how it
    /// was called and answers with `reply`.
    fn route_on(
        breaker: &CircuitBreaker,
        mime: &str,
        payload: Vec<u8>,
        reply: Result<Vec<u8>, PipelineError>,
    ) -> ((String, Vec<u8>), Option<Transcode>) {
        let called = Cell::new(None);
        let routed = route_with(mime.to_owned(), payload, breaker, |job, _, out| {
            called.set(Some(job));
            let bytes = reply?;
            let len = bytes.len() as u64;
            *out = bytes;
            Ok(Transfer { bytes_in: 0, bytes_out: len })
        });
        (routed, called.get())
    }

    /// Same, on a throwaway breaker (for tests that are not about the breaker).
    fn route_recording(
        mime: &str,
        payload: Vec<u8>,
        reply: Result<Vec<u8>, PipelineError>,
    ) -> ((String, Vec<u8>), Option<Transcode>) {
        route_on(&fresh_breaker(), mime, payload, reply)
    }

    fn png_header() -> Vec<u8> {
        vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3]
    }

    fn failed() -> PipelineError {
        PipelineError::Failed { code: Some(1), stderr: "boom".into() }
    }

    // --- bypass: the converter must not even be invoked ---

    #[test]
    fn non_raster_payloads_bypass_without_invoking_the_converter() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec();
        let xml_svg = b"<?xml version=\"1.0\"?><svg/>".to_vec();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("text/plain", b"hello world".to_vec()),
            ("text/plain;charset=utf-8", "日本語のテキスト".as_bytes().to_vec()),
            ("text/html", b"<p>hi</p>".to_vec()),
            ("text/uri-list", b"/home/user/a.png".to_vec()),
            ("application/json", b"{\"a\":1}".to_vec()),
            ("application/octet-stream", vec![0, 1, 2, 3, 4, 5]),
            ("image/svg+xml", svg.clone()),
            ("image/svg+xml", xml_svg),
            // The label lies: bytes that are SVG must still be bypassed.
            ("image/png", svg),
        ];
        for (mime, payload) in cases {
            let (routed, called) = route_recording(mime, payload.clone(), Ok(fake_webp()));
            assert!(called.is_none(), "converter invoked for {mime}");
            assert_eq!(routed, (mime.to_owned(), payload));
        }
    }

    #[test]
    fn avif_is_recognised_as_unsupported_and_bypassed() {
        let mut avif = vec![0, 0, 0, 0x20];
        avif.extend_from_slice(b"ftypavif");
        avif.extend_from_slice(&[0; 16]);
        let (routed, called) = route_recording("image/avif", avif.clone(), Ok(fake_webp()));
        assert!(called.is_none());
        assert_eq!(routed, ("image/avif".to_owned(), avif));
    }

    #[test]
    fn animated_capable_formats_are_left_untouched_by_policy() {
        let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec();
        let mut webp = fake_webp();
        webp.extend_from_slice(&[0; 16]);
        for (mime, payload) in [("image/gif", gif), ("image/webp", webp)] {
            let (routed, called) = route_recording(mime, payload.clone(), Ok(fake_webp()));
            assert!(called.is_none(), "{mime} must not be re-encoded");
            assert_eq!(routed, (mime.to_owned(), payload));
        }
    }

    #[test]
    fn empty_payload_bypasses_even_when_labelled_as_a_raster_image() {
        let (routed, called) = route_recording("image/png", Vec::new(), Ok(fake_webp()));
        assert!(called.is_none(), "no process may be spawned for zero bytes");
        assert_eq!(routed, ("image/png".to_owned(), Vec::new()));
    }

    // --- policy and classification ---

    #[test]
    fn png_is_routed_to_lossless_webp_and_mime_and_bytes_are_replaced() {
        let original = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        let (routed, called) = route_recording("image/png", original, Ok(fake_webp()));
        assert_eq!(called, Some(Transcode { input: InputFormat::Png, output: OutputFormat::WebpLossless }));
        assert_eq!(routed, ("image/webp".to_owned(), fake_webp()));
    }

    #[test]
    fn jpeg_is_routed_to_lossy_webp_at_the_ingest_quality() {
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10];
        let (routed, called) = route_recording("image/jpeg", jpeg, Ok(fake_webp()));
        let expected = OutputFormat::WebpLossy(Quality::new(IMAGE_INGEST_JPEG_QUALITY));
        assert_eq!(called, Some(Transcode { input: InputFormat::Jpeg, output: expected }));
        assert_eq!(routed.0, "image/webp");
    }

    #[test]
    fn bmp_without_a_signature_is_routed_by_its_declared_mime() {
        let bmp = b"BM\x36\x00\x00\x00".to_vec();
        let (routed, called) = route_recording("image/bmp", bmp, Ok(fake_webp()));
        assert_eq!(called, Some(Transcode { input: InputFormat::Bmp, output: OutputFormat::WebpLossless }));
        assert_eq!(routed.0, "image/webp");
    }

    #[test]
    fn recognised_bytes_outrank_a_wrong_label() {
        // JPEG bytes labelled as PNG are decoded as the JPEG they are.
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10];
        let (_, called) = route_recording("image/png", jpeg, Ok(fake_webp()));
        assert_eq!(called.map(|job| job.input), Some(InputFormat::Jpeg));
    }

    // --- fallback: every failure returns the original, byte for byte ---

    #[test]
    fn every_converter_failure_falls_back_to_the_original() {
        let original = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 9, 9, 9];
        let replies: Vec<Result<Vec<u8>, PipelineError>> = vec![
            Err(PipelineError::Spawn(io::Error::from(io::ErrorKind::NotFound))),
            Err(failed()),
            Err(PipelineError::Source(io::Error::other("x"))),
            Err(PipelineError::Sink(io::Error::other("x"))),
            Err(PipelineError::Child(io::Error::other("x"))),
            // "Success" that produced nothing, or the wrong kind of file:
            Ok(Vec::new()),
            Ok(b"not an image at all".to_vec()),
            Ok(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]), // PNG where WebP was promised
        ];
        for reply in replies {
            let (routed, called) = route_recording("image/png", original.clone(), reply);
            assert!(called.is_some());
            assert_eq!(routed, ("image/png".to_owned(), original.clone()));
        }
    }

    // --- end to end with the real magick ---

    #[test]
    fn real_png_is_transcoded_to_a_smaller_valid_webp() {
        if !magick_or_skip() { return; }
        let original = png();
        let (mime, data) = route_for_ingest("image/png".to_owned(), original.clone());
        assert_eq!(mime, "image/webp");
        assert_eq!(&data[..4], b"RIFF");
        assert_eq!(&data[8..12], b"WEBP");
        assert_ne!(data, original);
    }

    #[test]
    fn real_jpeg_loses_its_exif_and_comment_on_the_way_in() {
        if !magick_or_skip() { return; }
        let base = generate(&["-size", "32x32", "plasma:fractal", "jpg:-"]);
        // Splice a COM segment carrying a marker right after SOI.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xFE];
        jpeg.extend_from_slice(&((b"SECRETCOMMENT".len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(b"SECRETCOMMENT");
        jpeg.extend_from_slice(&base[2..]);
        assert!(contains(&jpeg, b"SECRETCOMMENT"));

        let (mime, data) = route_for_ingest("image/jpeg".to_owned(), jpeg);
        assert_eq!(mime, "image/webp");
        assert!(!contains(&data, b"SECRETCOMMENT"));
    }

    #[test]
    fn real_corrupt_images_fall_back_to_the_original_bytes_and_mime() {
        if !magick_or_skip() { return; }
        let good = png();
        let truncated = good[..good.len() / 2].to_vec();
        // Valid signature, garbage body.
        let mut bogus = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bogus.extend_from_slice(&[0xA5; 4096]);

        for original in [truncated, bogus] {
            let (mime, data) = route_for_ingest("image/png".to_owned(), original.clone());
            assert_eq!(mime, "image/png");
            assert_eq!(data, original);
        }
    }

    #[test]
    fn real_text_and_svg_pass_through_identically() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"5\" height=\"5\"/>".to_vec();
        for (mime, payload) in [("text/plain", b"plain".to_vec()), ("image/svg+xml", svg)] {
            let routed = route_for_ingest(mime.to_owned(), payload.clone());
            assert_eq!(routed, (mime.to_owned(), payload));
        }
    }

    // --- circuit breaker integration ---

    fn trip(breaker: &CircuitBreaker) {
        for _ in 0..3 {
            let (routed, called) = route_on(breaker, "image/png", png_header(), Err(failed()));
            assert!(called.is_some());
            assert_eq!(routed.1, png_header());
        }
        assert_eq!(breaker.state(), BreakerState::Open);
    }

    #[test]
    fn three_consecutive_failures_open_the_circuit_and_later_images_skip_the_converter() {
        let breaker = fresh_breaker();
        trip(&breaker);

        // Open: the converter is not even invoked, and the original is kept.
        let (routed, called) = route_on(&breaker, "image/png", png_header(), Ok(fake_webp()));
        assert!(called.is_none(), "no conversion (hence no child process) while open");
        assert_eq!(routed, ("image/png".to_owned(), png_header()));
    }

    #[test]
    fn every_kind_of_converter_fault_counts_toward_the_threshold() {
        let faults: Vec<Result<Vec<u8>, PipelineError>> = vec![
            Err(failed()),
            Err(PipelineError::Failed { code: None, stderr: "killed".into() }),
            Err(PipelineError::Timeout(Duration::from_secs(15))),
            Err(PipelineError::Child(io::Error::from(io::ErrorKind::BrokenPipe))),
            Ok(Vec::new()),
            Ok(b"not an image".to_vec()),
        ];
        for fault in faults {
            let breaker = CircuitBreaker::new(1, Duration::from_secs(30));
            let (routed, _) = route_on(&breaker, "image/png", png_header(), fault);
            assert_eq!(routed, ("image/png".to_owned(), png_header()), "zero data loss");
            assert_eq!(breaker.state(), BreakerState::Open);
        }
    }

    #[test]
    fn bypassed_payloads_neither_advance_nor_reset_the_failure_count() {
        let breaker = fresh_breaker();
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        for _ in 0..20 {
            route_on(&breaker, "text/plain", b"hello".to_vec(), Err(failed()));
            route_on(&breaker, "image/svg+xml", b"<svg/>".to_vec(), Err(failed()));
            route_on(&breaker, "text/uri-list", b"/a".to_vec(), Err(failed()));
            route_on(&breaker, "image/gif", b"GIF89a....".to_vec(), Err(failed()));
        }
        assert_eq!(breaker.state(), BreakerState::Closed, "bypasses must not count as failures");
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        assert_eq!(breaker.state(), BreakerState::Open, "bypasses must not have reset the count either");
    }

    #[test]
    fn a_missing_tool_and_caller_stream_errors_do_not_touch_the_counters() {
        let breaker = fresh_breaker();
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        for _ in 0..20 {
            let missing = PipelineError::Spawn(io::Error::from(io::ErrorKind::NotFound));
            route_on(&breaker, "image/png", png_header(), Err(missing));
            route_on(&breaker, "image/png", png_header(), Err(PipelineError::Source(io::Error::other("x"))));
            route_on(&breaker, "image/png", png_header(), Err(PipelineError::Sink(io::Error::other("x"))));
        }
        assert_eq!(breaker.state(), BreakerState::Closed);
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        assert_eq!(breaker.state(), BreakerState::Open, "third real fault still trips: nothing was reset");
    }

    #[test]
    fn a_missing_tool_alone_can_never_open_the_circuit() {
        let breaker = fresh_breaker();
        for _ in 0..50 {
            let missing = PipelineError::Spawn(io::Error::from(io::ErrorKind::NotFound));
            let (routed, _) = route_on(&breaker, "image/png", png_header(), Err(missing));
            assert_eq!(routed, ("image/png".to_owned(), png_header()));
        }
        assert_eq!(breaker.state(), BreakerState::Closed);
    }

    #[test]
    fn a_successful_conversion_resets_the_consecutive_count() {
        let breaker = fresh_breaker();
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        let (routed, _) = route_on(&breaker, "image/png", png_header(), Ok(fake_webp()));
        assert_eq!(routed.0, "image/webp");
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        assert_eq!(breaker.state(), BreakerState::Closed);
    }

    #[test]
    fn after_the_cooldown_a_successful_canary_conversion_closes_the_circuit() {
        let breaker = CircuitBreaker::new(3, Duration::from_millis(40));
        trip(&breaker);
        std::thread::sleep(Duration::from_millis(60));

        let (routed, called) = route_on(&breaker, "image/png", png_header(), Ok(fake_webp()));
        assert!(called.is_some(), "the canary must actually be attempted");
        assert_eq!(routed, ("image/webp".to_owned(), fake_webp()));
        assert_eq!(breaker.state(), BreakerState::Closed);

        let (routed, called) = route_on(&breaker, "image/png", png_header(), Ok(fake_webp()));
        assert!(called.is_some());
        assert_eq!(routed.0, "image/webp", "normal service resumed");
    }

    #[test]
    fn a_failed_canary_keeps_the_data_and_reopens_the_circuit() {
        let breaker = CircuitBreaker::new(3, Duration::from_millis(40));
        trip(&breaker);
        std::thread::sleep(Duration::from_millis(60));

        let (routed, called) = route_on(&breaker, "image/png", png_header(), Err(failed()));
        assert!(called.is_some());
        assert_eq!(routed, ("image/png".to_owned(), png_header()));
        assert_eq!(breaker.state(), BreakerState::Open);

        let (_, called) = route_on(&breaker, "image/png", png_header(), Ok(fake_webp()));
        assert!(called.is_none(), "back in cooldown");
    }

    #[test]
    fn only_one_image_is_converted_while_the_canary_is_in_flight() {
        let breaker = CircuitBreaker::new(3, Duration::ZERO);
        trip(&breaker);

        let nested_called = Cell::new(false);
        let outer = route_with("image/png".to_owned(), png_header(), &breaker, |_, _, out| {
            // While this canary is running, a second image arrives.
            let (inner, called) = route_on(&breaker, "image/png", png_header(), Ok(fake_webp()));
            nested_called.set(called.is_some());
            assert_eq!(inner, ("image/png".to_owned(), png_header()), "bypassed with the original intact");
            *out = fake_webp();
            Ok(Transfer { bytes_in: 0, bytes_out: out.len() as u64 })
        });
        assert!(!nested_called.get(), "the second image must not start a second probe");
        assert_eq!(outer.0, "image/webp");
        assert_eq!(breaker.state(), BreakerState::Closed);
    }

    // --- real magick behind the breaker ---

    #[test]
    fn real_corrupt_images_trip_the_breaker_without_losing_a_single_byte() {
        if !magick_or_skip() { return; }
        let breaker = fresh_breaker();
        let mut corrupt = png_header();
        corrupt.extend_from_slice(&[0xA5; 2048]);
        for _ in 0..5 {
            let routed = route_with("image/png".to_owned(), corrupt.clone(), &breaker, |job, input, out| {
                crate::image::transcode::transcode(job, input, out)
            });
            assert_eq!(routed, ("image/png".to_owned(), corrupt.clone()));
        }
        assert_eq!(breaker.state(), BreakerState::Open);
    }
}
