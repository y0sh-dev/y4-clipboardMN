// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/image/transcode.rs

//! Privacy-stripping, resource-bounded image transcoding on top of the
//! streaming `magick` runner.
//!
//! Every argument handed to `magick` is derived from the closed enums below
//! and from compile-time constants, never from caller-supplied text, so
//! there is no way to smuggle an option, a `coder:` prefix or a filename
//! through clipboard-controlled data.

use std::io::{Read, Write};
use std::time::Duration;

use crate::core::constants::{
    IMAGE_LIMIT_DISK, IMAGE_LIMIT_HEIGHT_PX, IMAGE_LIMIT_MAP, IMAGE_LIMIT_MEMORY,
    IMAGE_LIMIT_TIME_SECS, IMAGE_LIMIT_WIDTH_PX, IMAGE_QUALITY_DEFAULT, IMAGE_QUALITY_MAX,
    IMAGE_QUALITY_MIN, IMAGE_TRANSCODE_TIMEOUT_SECS,
};
use crate::core::utils::{detect_image_mime, mime_base};
use crate::image::magick;
use crate::image::pipeline::{PipelineError, Transfer};

/// Raster formats accepted as *input*. The decoder is pinned explicitly
/// (`png:-`) instead of letting ImageMagick sniff the stream: auto-detection
/// can route attacker-chosen bytes into any of its ~100 coders, including
/// scriptable ones (SVG, MVG, MSL, ...) with a long history of
/// vulnerabilities. A pinned coder makes a mislabelled or disguised payload
/// fail to decode rather than reach a different parser. Vector and
/// scriptable formats are deliberately absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
}

impl InputFormat {
    fn coder(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Bmp => "bmp",
        }
    }

    /// Maps a clipboard MIME type (parameters and case ignored) to a
    /// supported input format; `None` for anything else, SVG included.
    pub fn from_mime(mime: &str) -> Option<Self> {
        let base = mime_base(mime);
        [
            ("image/png", Self::Png),
            ("image/jpeg", Self::Jpeg),
            ("image/jpg", Self::Jpeg),
            ("image/gif", Self::Gif),
            ("image/webp", Self::Webp),
            ("image/bmp", Self::Bmp),
            ("image/x-ms-bmp", Self::Bmp),
        ]
        .into_iter()
        .find(|(name, _)| base.eq_ignore_ascii_case(name))
        .map(|(_, format)| format)
    }

    /// Identifies the format from a payload's leading bytes (the same
    /// magic-byte check ingestion already trusts over sender labels).
    /// Unsupported formats, BMP (no reliable short signature) and
    /// unrecognised data yield `None`.
    pub fn sniff(header: &[u8]) -> Option<Self> {
        detect_image_mime(header).and_then(Self::from_mime)
    }
}

/// Encoder quality, always within `IMAGE_QUALITY_MIN..=IMAGE_QUALITY_MAX`.
/// Construction clamps instead of failing: a caller passing 0 or 255 gets
/// the nearest valid value, so no input can make `magick` reject the option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quality(u8);

impl Quality {
    pub fn new(raw: u8) -> Self {
        Self(raw.clamp(IMAGE_QUALITY_MIN, IMAGE_QUALITY_MAX))
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

impl Default for Quality {
    fn default() -> Self {
        Self::new(IMAGE_QUALITY_DEFAULT)
    }
}

/// Target encodings. Quality exists only where it means something: the two
/// lossless variants carry none, so "lossless at quality 40" is not a state
/// the type can express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Lossless PNG at maximum zlib effort: the "keep it exactly" choice.
    Png,
    /// Lossless WebP: bit-exact pixels, typically smaller than PNG on
    /// screenshots. Always encoded at ImageMagick quality 100, because in
    /// lossless mode ImageMagick maps any lower quality onto libwebp's
    /// near-lossless pre-processing, which *does* alter pixels (measured:
    /// 59 of 16384 pixels differ at quality 75, 118 at 50). Only 100 is exact.
    WebpLossless,
    /// Lossy WebP: the "small and good enough" choice, tuned by `Quality`.
    WebpLossy(Quality),
}

impl OutputFormat {
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::WebpLossless | Self::WebpLossy(_) => "image/webp",
        }
    }
}

/// One transcoding job: which decoder to pin and which encoding to produce.
/// Metadata stripping is not a switch — every transcode removes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transcode {
    pub input: InputFormat,
    pub output: OutputFormat,
}

impl Transcode {
    /// The complete `magick` argument vector, in the one order that is safe:
    ///
    /// 1. `-limit ...` first, so the ceilings are active before any byte is
    ///    decoded (a bomb is rejected at the header, not after allocation).
    /// 2. The pinned-decoder input `fmt:-[0]`. `[0]` selects the first frame,
    ///    so an animated GIF/WebP cannot expand into a frame sequence.
    /// 3. `-auto-orient` *before* `-strip`: orientation lives in EXIF, and
    ///    stripping it first would silently turn a rotated photo sideways.
    ///    The rotation is baked into the pixels, then the tag is dropped.
    /// 4. `-strip` removes every profile and comment (EXIF incl. GPS and
    ///    camera/timestamp tags, XMP, IPTC, ICC, text chunks).
    /// 5. Encoder settings, then the output `fmt:-`.
    fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        for (name, value) in [
            ("width", IMAGE_LIMIT_WIDTH_PX),
            ("height", IMAGE_LIMIT_HEIGHT_PX),
            ("memory", IMAGE_LIMIT_MEMORY),
            ("map", IMAGE_LIMIT_MAP),
            ("disk", IMAGE_LIMIT_DISK),
            ("time", IMAGE_LIMIT_TIME_SECS),
        ] {
            args.extend(["-limit".to_owned(), name.to_owned(), value.to_owned()]);
        }
        args.push(format!("{}:-[0]", self.input.coder()));
        args.push("-auto-orient".to_owned());
        args.push("-strip".to_owned());
        match self.output {
            OutputFormat::Png => {
                args.extend(["-define".to_owned(), "png:compression-level=9".to_owned()]);
                args.push("png:-".to_owned());
            }
            OutputFormat::WebpLossless => {
                args.extend(["-define".to_owned(), "webp:lossless=true".to_owned()]);
                args.extend(["-quality".to_owned(), IMAGE_QUALITY_MAX.to_string()]);
                args.push("webp:-".to_owned());
            }
            OutputFormat::WebpLossy(quality) => {
                args.extend(["-define".to_owned(), "webp:lossless=false".to_owned()]);
                args.extend(["-quality".to_owned(), quality.get().to_string()]);
                args.push("webp:-".to_owned());
            }
        }
        args
    }
}

/// Streams `input` through `magick` according to `job` into `output`,
/// stripping all metadata. Nothing is buffered whole: see
/// [`crate::image::pipeline::run_filter`] for the chunked, deadlock-free
/// relay, and for the rule that `output` must be discarded on `Err`.
///
/// A payload that does not decode as `job.input` (corrupt, truncated,
/// mislabelled, or over the resource limits) ends as
/// [`PipelineError::Failed`]; a `magick` that hangs is killed after
/// `IMAGE_TRANSCODE_TIMEOUT_SECS` of wall-clock time and ends as
/// [`PipelineError::Timeout`]; a missing `magick` ends as
/// [`PipelineError::Spawn`] (`is_unavailable()`), the cue to keep the
/// original bytes untouched instead.
pub fn transcode<R, W>(job: Transcode, input: R, output: &mut W) -> Result<Transfer, PipelineError>
where
    R: Read + Send,
    W: Write,
{
    let args = job.args();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    magick::run_with_timeout(&refs, input, output, Duration::from_secs(IMAGE_TRANSCODE_TIMEOUT_SECS))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::{Cursor, empty};
    use std::sync::mpsc;
    use std::time::Duration;

    const MARKER_CAMERA: &[u8] = b"SECRETCAM";
    const MARKER_COMMENT: &[u8] = b"SECRETCOMMENT";
    const MARKER_AUTHOR: &[u8] = b"SECRETAUTHOR";

    fn magick_or_skip() -> bool {
        if magick::is_available() {
            true
        } else {
            eprintln!("skipping: `magick` is not available on this system");
            false
        }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn lossy(quality: u8) -> OutputFormat {
        OutputFormat::WebpLossy(Quality::new(quality))
    }

    /// Runs `magick` directly (no stripping, no pinned decoder) to build
    /// fixtures and to take measurements; the code under test is `transcode`.
    fn raw(args: &[&str], input: Vec<u8>) -> Vec<u8> {
        let mut out = Vec::new();
        magick::run(args, Cursor::new(input), &mut out).unwrap();
        out
    }

    fn generate(args: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        magick::run(args, empty(), &mut out).unwrap();
        out
    }

    fn dimensions(image: Vec<u8>) -> String {
        String::from_utf8(raw(&["-", "-format", "%wx%h", "info:"], image)).unwrap().trim().to_owned()
    }

    fn pixel_signature(image: Vec<u8>) -> String {
        String::from_utf8(raw(&["-", "-format", "%#", "info:"], image)).unwrap().trim().to_owned()
    }

    /// Completes within `secs` or fails the test instead of hanging the suite.
    fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(secs)).expect("transcode did not finish in time")
    }

    fn run_job(job: Transcode, input: Vec<u8>) -> (Result<Transfer, PipelineError>, Vec<u8>) {
        within(60, move || {
            let mut out = Vec::new();
            let result = transcode(job, Cursor::new(input), &mut out);
            (result, out)
        })
    }

    /// A PNG carrying plain-text metadata chunks (tEXt: comment + author).
    fn png_with_text_metadata() -> Vec<u8> {
        generate(&[
            "-size", "64x64", "plasma:fractal",
            "-set", "comment", "SECRETCOMMENT",
            "-set", "Author", "SECRETAUTHOR",
            "png:-",
        ])
    }

    /// Minimal little-endian EXIF (TIFF) blob: Make = "SECRETCAM" plus an
    /// optional Orientation tag.
    fn exif_blob(orientation: Option<u16>) -> Vec<u8> {
        let make = b"SECRETCAM\0";
        let entries: u16 = if orientation.is_some() { 2 } else { 1 };
        let data_off = 8 + 2 + 12 * u32::from(entries) + 4;
        let mut tiff = b"II*\0".to_vec();
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&entries.to_le_bytes());
        // IFD entries must be sorted by tag: Make (0x010F) < Orientation (0x0112).
        tiff.extend_from_slice(&0x010Fu16.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&(make.len() as u32).to_le_bytes());
        tiff.extend_from_slice(&data_off.to_le_bytes());
        if let Some(value) = orientation {
            tiff.extend_from_slice(&0x0112u16.to_le_bytes());
            tiff.extend_from_slice(&3u16.to_le_bytes());
            tiff.extend_from_slice(&1u32.to_le_bytes());
            tiff.extend_from_slice(&value.to_le_bytes());
            tiff.extend_from_slice(&[0, 0]);
        }
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(make);
        tiff
    }

    /// A JPEG of `size` with an EXIF APP1 segment and a COM comment spliced
    /// in directly after SOI (pure byte surgery, no external helper).
    fn jpeg_with_metadata(size: &str, orientation: Option<u16>) -> Vec<u8> {
        let base = generate(&["-size", size, "plasma:fractal", "jpg:-"]);
        assert_eq!(&base[..2], &[0xFF, 0xD8]);
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&exif_blob(orientation));
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&exif);
        jpeg.extend_from_slice(&[0xFF, 0xFE]);
        jpeg.extend_from_slice(&((MARKER_COMMENT.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(MARKER_COMMENT);
        jpeg.extend_from_slice(&base[2..]);
        jpeg
    }

    // --- argument construction (needs no magick) ---

    #[test]
    fn args_apply_limits_before_the_pinned_input_then_strip_then_encode() {
        let job = Transcode { input: InputFormat::Jpeg, output: lossy(70) };
        let args = job.args();
        let pos = |needle: &str| args.iter().position(|a| a == needle).unwrap();

        for name in ["width", "height", "memory", "map", "disk", "time"] {
            let at = args.iter().position(|a| a == name).unwrap();
            assert_eq!(args[at - 1], "-limit");
            assert!(at < pos("jpeg:-[0]"), "{name} limit must precede the input");
        }
        assert!(pos("jpeg:-[0]") < pos("-auto-orient"));
        assert!(pos("-auto-orient") < pos("-strip"), "orientation must be applied before EXIF is stripped");
        assert!(pos("-strip") < pos("webp:lossless=false"));
        assert_eq!(args.last().map(String::as_str), Some("webp:-"));
        assert_eq!(args[pos("-quality") + 1], "70");
    }

    #[test]
    fn args_for_png_output_pin_no_webp_settings() {
        let args = Transcode { input: InputFormat::Png, output: OutputFormat::Png }.args();
        assert_eq!(args.last().map(String::as_str), Some("png:-"));
        assert!(args.iter().all(|a| !a.contains("webp")));
        assert!(args.iter().any(|a| a == "-strip"));
    }

    #[test]
    fn lossless_webp_args_ignore_quality_and_pin_the_exact_setting() {
        let args = Transcode { input: InputFormat::Png, output: OutputFormat::WebpLossless }.args();
        assert!(args.windows(2).any(|w| w == ["-define", "webp:lossless=true"]));
        assert!(args.windows(2).any(|w| w == ["-quality", "100"]), "only quality 100 is bit-exact");
        assert_eq!(args.last().map(String::as_str), Some("webp:-"));
    }

    #[test]
    fn quality_is_clamped_into_range_at_both_ends() {
        assert_eq!(Quality::new(0).get(), IMAGE_QUALITY_MIN);
        assert_eq!(Quality::new(1).get(), 1);
        assert_eq!(Quality::new(100).get(), 100);
        assert_eq!(Quality::new(101).get(), IMAGE_QUALITY_MAX);
        assert_eq!(Quality::new(255).get(), IMAGE_QUALITY_MAX);
        assert_eq!(Quality::default().get(), IMAGE_QUALITY_DEFAULT);
    }

    #[test]
    fn extreme_quality_values_reach_magick_already_clamped() {
        let low = Transcode { input: InputFormat::Png, output: lossy(0) }.args();
        let high = Transcode { input: InputFormat::Png, output: lossy(255) }.args();
        assert!(low.windows(2).any(|w| w == ["-quality", "1"]));
        assert!(high.windows(2).any(|w| w == ["-quality", "100"]));
    }

    #[test]
    fn from_mime_ignores_case_and_parameters_and_rejects_vector_formats() {
        assert_eq!(InputFormat::from_mime("image/png"), Some(InputFormat::Png));
        assert_eq!(InputFormat::from_mime("IMAGE/JPEG; q=1"), Some(InputFormat::Jpeg));
        assert_eq!(InputFormat::from_mime("image/jpg"), Some(InputFormat::Jpeg));
        assert_eq!(InputFormat::from_mime("image/x-ms-bmp"), Some(InputFormat::Bmp));
        assert_eq!(InputFormat::from_mime("image/svg+xml"), None);
        assert_eq!(InputFormat::from_mime("application/pdf"), None);
        assert_eq!(InputFormat::from_mime("text/plain"), None);
        assert_eq!(InputFormat::from_mime(""), None);
    }

    #[test]
    fn sniff_trusts_bytes_and_refuses_svg_and_junk() {
        assert_eq!(InputFormat::sniff(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A]), Some(InputFormat::Png));
        assert_eq!(InputFormat::sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(InputFormat::Jpeg));
        assert_eq!(InputFormat::sniff(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"), None);
        assert_eq!(InputFormat::sniff(b"plain text"), None);
        assert_eq!(InputFormat::sniff(b""), None);
    }

    #[test]
    fn output_mime_matches_the_encoding() {
        assert_eq!(OutputFormat::Png.mime(), "image/png");
        assert_eq!(OutputFormat::WebpLossless.mime(), "image/webp");
    }

    // --- privacy stripping ---

    #[test]
    fn png_text_chunks_are_removed_for_both_output_formats() {
        if !magick_or_skip() { return; }
        let source = png_with_text_metadata();
        assert!(contains(&source, MARKER_COMMENT) && contains(&source, MARKER_AUTHOR), "fixture must carry the metadata");

        for output in [OutputFormat::Png, lossy(80), OutputFormat::WebpLossless] {
            let (result, out) = run_job(Transcode { input: InputFormat::Png, output }, source.clone());
            result.unwrap();
            assert!(!contains(&out, MARKER_COMMENT), "{output:?} kept the comment");
            assert!(!contains(&out, MARKER_AUTHOR), "{output:?} kept the author");
        }
    }

    #[test]
    fn exif_and_jpeg_comments_are_removed_and_the_control_proves_the_test_can_fail() {
        if !magick_or_skip() { return; }
        let source = jpeg_with_metadata("32x32", None);
        assert!(contains(&source, MARKER_CAMERA) && contains(&source, MARKER_COMMENT));

        // Control: the same conversion WITHOUT `-strip` keeps the EXIF, so
        // the absence asserted below is caused by the stripping, not by the
        // fixture or the encoder happening to drop it.
        let unstripped_webp = raw(&["jpeg:-", "-quality", "80", "webp:-"], source.clone());
        let unstripped_png = raw(&["jpeg:-", "png:-"], source.clone());
        assert!(contains(&unstripped_webp, MARKER_CAMERA), "control: webp should carry EXIF without -strip");
        assert!(contains(&unstripped_png, MARKER_CAMERA), "control: png should carry EXIF without -strip");
        assert!(contains(&unstripped_png, MARKER_COMMENT), "control: png should carry the comment without -strip");

        for output in [OutputFormat::Png, lossy(80), OutputFormat::WebpLossless] {
            let (result, out) = run_job(Transcode { input: InputFormat::Jpeg, output }, source.clone());
            result.unwrap();
            assert!(!contains(&out, MARKER_CAMERA), "{output:?} kept EXIF");
            assert!(!contains(&out, MARKER_COMMENT), "{output:?} kept the comment");
        }
    }

    #[test]
    fn exif_orientation_is_baked_into_the_pixels_before_it_is_stripped() {
        if !magick_or_skip() { return; }
        // Stored 8x4 with Orientation=6 (rotate 90° CW to display): shown as 4x8.
        let source = jpeg_with_metadata("8x4", Some(6));
        assert_eq!(dimensions(source.clone()), "8x4");
        let (result, out) = run_job(Transcode { input: InputFormat::Jpeg, output: OutputFormat::Png }, source);
        result.unwrap();
        assert_eq!(dimensions(out), "4x8");
    }

    // --- encoding ---

    #[test]
    fn webp_output_has_a_riff_webp_signature_and_is_smaller_than_the_png() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "512x512", "plasma:fractal", "-depth", "8", "png:-"]);
        let (result, out) = run_job(Transcode { input: InputFormat::Png, output: lossy(80) }, png.clone());
        let transfer = result.unwrap();
        assert_eq!(&out[..4], b"RIFF");
        assert_eq!(&out[8..12], b"WEBP");
        assert_eq!(transfer.bytes_in, png.len() as u64);
        assert_eq!(transfer.bytes_out, out.len() as u64);
        assert!(out.len() < png.len(), "lossy webp ({}) should be smaller than png ({})", out.len(), png.len());
        assert_eq!(dimensions(out), "512x512");
    }

    #[test]
    fn lower_quality_yields_a_smaller_lossy_file() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "256x256", "plasma:fractal", "-depth", "8", "png:-"]);
        let (low, low_out) = run_job(Transcode { input: InputFormat::Png, output: lossy(10) }, png.clone());
        let (high, high_out) = run_job(Transcode { input: InputFormat::Png, output: lossy(95) }, png);
        low.unwrap();
        high.unwrap();
        assert!(low_out.len() < high_out.len());
    }

    #[test]
    fn lossless_webp_and_png_preserve_every_pixel() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "128x128", "plasma:fractal", "-depth", "8", "png:-"]);
        let original = pixel_signature(png.clone());
        for output in [OutputFormat::Png, OutputFormat::WebpLossless] {
            let (result, out) = run_job(Transcode { input: InputFormat::Png, output }, png.clone());
            result.unwrap();
            assert_eq!(pixel_signature(out), original, "{output:?} altered pixel data");
        }
    }

    #[test]
    fn lossy_webp_is_not_pixel_identical_which_distinguishes_the_two_modes() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "128x128", "plasma:fractal", "-depth", "8", "png:-"]);
        let original = pixel_signature(png.clone());
        let (result, out) = run_job(Transcode { input: InputFormat::Png, output: lossy(30) }, png);
        result.unwrap();
        assert_ne!(pixel_signature(out), original);
    }

    #[test]
    fn extreme_quality_inputs_still_produce_valid_webp() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "32x32", "plasma:fractal", "-depth", "8", "png:-"]);
        for q in [0u8, 255] {
            let (result, out) = run_job(Transcode { input: InputFormat::Png, output: lossy(q) }, png.clone());
            result.unwrap();
            assert_eq!(&out[8..12], b"WEBP");
        }
    }

    #[test]
    fn animated_input_yields_a_single_static_frame() {
        if !magick_or_skip() { return; }
        let gif = generate(&["-size", "8x4", "xc:red", "xc:blue", "xc:green", "-set", "delay", "10", "gif:-"]);
        let frames = raw(&["-", "-format", "%n,", "info:"], gif.clone());
        assert!(String::from_utf8_lossy(&frames).starts_with('3'), "fixture should have 3 frames");
        let (result, out) = run_job(Transcode { input: InputFormat::Gif, output: OutputFormat::Png }, gif);
        result.unwrap();
        let out_frames = raw(&["-", "-format", "%n,", "info:"], out);
        assert!(String::from_utf8_lossy(&out_frames).starts_with('1'));
    }

    // --- hostile and malformed input ---

    #[test]
    fn declared_format_is_enforced_rather_than_auto_detected() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "8x8", "xc:red", "png:-"]);
        let (result, out) = run_job(Transcode { input: InputFormat::Jpeg, output: OutputFormat::Png }, png);
        assert!(matches!(result, Err(PipelineError::Failed { .. })), "a PNG declared as JPEG must not decode");
        assert!(out.is_empty());
    }

    #[test]
    fn svg_cannot_be_smuggled_in_under_a_raster_label() {
        if !magick_or_skip() { return; }
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"5\" height=\"5\"><rect width=\"5\" height=\"5\"/></svg>".to_vec();
        for input in [InputFormat::Png, InputFormat::Jpeg, InputFormat::Gif, InputFormat::Webp, InputFormat::Bmp] {
            let (result, out) = run_job(Transcode { input, output: OutputFormat::Png }, svg.clone());
            assert!(matches!(result, Err(PipelineError::Failed { .. })), "{input:?} accepted SVG");
            assert!(out.is_empty());
        }
    }

    #[test]
    fn image_wider_than_the_resource_limit_is_rejected() {
        if !magick_or_skip() { return; }
        // 17000 > 16384 pixels wide, yet only a few hundred bytes of PNG.
        let wide = generate(&["-size", "17000x1", "xc:red", "png:-"]);
        assert!(wide.len() < 4096);
        let (result, out) = run_job(Transcode { input: InputFormat::Png, output: OutputFormat::Png }, wide);
        match result {
            Err(PipelineError::Failed { stderr, .. }) => assert!(stderr.contains("limit"), "stderr: {stderr}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(out.is_empty());
    }

    #[test]
    fn image_at_the_width_limit_is_still_accepted() {
        if !magick_or_skip() { return; }
        let edge = generate(&["-size", "16384x1", "xc:red", "png:-"]);
        let (result, out) = run_job(Transcode { input: InputFormat::Png, output: OutputFormat::Png }, edge);
        result.unwrap();
        assert_eq!(dimensions(out), "16384x1");
    }

    #[test]
    fn garbage_empty_and_truncated_inputs_fail_without_panicking_or_hanging() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "128x128", "plasma:fractal", "-depth", "8", "png:-"]);
        let truncated = png[..png.len() / 2].to_vec();
        for input in [Vec::new(), b"definitely not an image".to_vec(), truncated, vec![0xA5u8; 4 * 1024 * 1024]] {
            let (result, _) = run_job(Transcode { input: InputFormat::Png, output: lossy(80) }, input);
            assert!(matches!(result, Err(PipelineError::Failed { .. })));
        }
    }
}
