// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/wayland/handlers/data_control/device.rs

use super::{is_sensitive, make_pipe};
use crate::core::constants::*;
use crate::wayland::state::{ClipboardJob, OfferData, WaylandState};
use sha3::{Digest, Sha3_256};
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::{Arc, Mutex, mpsc};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_offer_v1::ExtDataControlOfferV1,
};

impl Dispatch<ExtDataControlDeviceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        ev: ext_data_control_device_v1::Event,
        _: &(),
        conn: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_device_v1::Event::Selection { id } = ev {
            // Update synchronization status
            state.selection_received = true;

            // Prevent self-ingestion by checking active provider locks
            if state.provider_locks > 0 {
                state.provider_locks -= 1;
                return;
            }

            // Private mode: bypass ingestion entirely (no offer.receive, no DB Worker job).
            if state.paused {
                return;
            }

            let Some(offer) = id else { return };

            // Extract all available MIME types for this specific offer
            let mimes: Vec<String> = offer
                .data::<OfferData>()
                .and_then(|d| d.mimes.lock().ok())
                .map(|g| g.clone())
                .unwrap_or_default();

            if mimes.is_empty() || is_sensitive(&mimes) {
                return;
            }

            // Determine which offered MIME to request: a specific one if
            // `y4p paste-from <mime>` is waiting for it (Action Mode), or the
            // richest/most-reproducible one by `MIME_PRIORITY_ORDER`
            // otherwise (daemon ingestion) — see `select_mime`.
            let drop_rtf = state.config.should_drop_rtf();
            let Some(mime_to_get) = select_mime(&mimes, state.target_mime.as_deref(), drop_rtf)
            else {
                return;
            };

            // Initialize data transfer pipe
            let (read_file, write_fd) = match make_pipe() {
                Some(p) => p,
                None => return,
            };

            // Request data transmission from the compositor
            offer.receive(mime_to_get.clone(), write_fd.as_fd());
            drop(write_fd);
            let _ = conn.flush();

            // Offload ingestion and persistence to the worker thread
            if let Some(ref tx) = state.job_tx {
                let job_tx_clone = tx.clone();

                std::thread::spawn(move || {
                    ingest_and_send(read_file, mime_to_get, &job_tx_clone);
                });
            } else {
                // Action Mode: Synchronous read for immediate CLI processing.
                // Guarded against truncation and unified with daemon normalisation.
                if let Ok(Some(buf)) = read_bounded_payload(read_file, MAX_PAYLOAD_SIZE)
                    && let Some((_, normalised)) = normalise_payload(mime_to_get, buf)
                {
                    state.rx_buf = normalised;
                }
            }
        }
    }

    // Bind OfferData to each new DataOffer instance for isolated MIME tracking
    wayland_client::event_created_child!(WaylandState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, OfferData { mimes: Arc::new(Mutex::new(Vec::new())) })
    ]);
}

/// Chooses which of an offer's `mimes` to request from the compositor.
///
/// `target_mime` is `Some` only in Action Mode (`y4p paste-from <mime>`
/// waits for one specific MIME): the matching offer is found via
/// `core::utils::mime_base_eq` (case- and whitespace-insensitive, parameters
/// ignored), so a sender announcing e.g. "TEXT/Plain" or
/// "text/plain; charset=utf-8" still satisfies a request for "text/plain".
/// `drop_rtf` is not consulted here — an explicit request names exactly the
/// MIME the caller wants, RTF included, and that request is honoured as-is.
///
/// `target_mime` is `None` in daemon ingestion, where no single MIME is
/// being awaited: the richest/most-reproducible offer is chosen via
/// `MIME_PRIORITY_ORDER`, falling back through images, then plain text,
/// then anything at all. `drop_rtf` (`[mime] drop_rtf` in `y4p.toml`) is
/// applied at every step of this fallback chain, so RTF is never selected
/// when it's enabled, even as the last-resort catch-all.
pub(crate) fn select_mime(
    mimes: &[String],
    target_mime: Option<&str>,
    drop_rtf: bool,
) -> Option<String> {
    if let Some(target) = target_mime {
        return mimes
            .iter()
            .find(|m| crate::core::utils::mime_base_eq(m, target))
            .cloned();
    }

    MIME_PRIORITY_ORDER
        .iter()
        .find_map(|&p| {
            mimes
                .iter()
                .find(|m| crate::core::utils::mime_base_eq(m, p))
        })
        .cloned()
        .or_else(|| {
            mimes
                .iter()
                .find(|m| crate::core::utils::starts_with_ignore_ascii_case(m, "image/"))
                .cloned()
        })
        .or_else(|| {
            mimes
                .iter()
                .find(|m| {
                    crate::core::utils::starts_with_ignore_ascii_case(m, "text/")
                        && (!drop_rtf || !crate::core::utils::is_rtf_mime(m))
                })
                .cloned()
        })
        .or_else(|| {
            mimes
                .iter()
                .find(|m| !drop_rtf || !crate::core::utils::is_rtf_mime(m))
                .cloned()
        })
}

/// Normalises a freshly-read payload and re-detects its true MIME where the
/// offered label can't be trusted outright (images, in particular — see
/// `core::utils::detect_image_mime`), or where the wire format needs
/// reshaping before it's fit to persist (`text/uri-list`'s percent-encoded,
/// `file://`-prefixed lines — see `core::utils::normalize_uri_list`).
///
/// Deliberately decoupled from both the I/O read that produces `payload`
/// and the hashing/dispatch that follows in `ingest_and_send`: this is the
/// single, clearly-bounded hook later normalisation steps (e.g. v0.5.0
/// image compression) extend, without having to touch the read loop or the
/// SHA3/worker-dispatch logic either side of it.
///
/// `mime_to_get` is matched via `core::utils::mime_base_eq` so a `; charset=`
/// or similar parameter on the offered `text/uri-list` label doesn't defeat
/// the comparison. Returns `None` when normalisation leaves nothing worth
/// persisting (e.g. a uri-list that decoded to no valid paths).
fn normalise_payload(mime_to_get: String, mut payload: Vec<u8>) -> Option<(String, Vec<u8>)> {
    let mut final_mime = mime_to_get;

    if crate::core::utils::mime_base_eq(&final_mime, MIME_URI_LIST) {
        payload = crate::core::utils::normalize_uri_list(&payload);
        if payload.is_empty() {
            return None;
        }
    } else if let Some(m) = crate::core::utils::detect_image_mime(&payload) {
        final_mime = m.to_string();
    }

    Some((final_mime, payload))
}

/// Reads up to `max_size` bytes from `reader`. Returns `Ok(None)` if the
/// input is empty or strictly exceeds `max_size` (preventing truncated,
/// corrupted records from being accepted), and `Ok(Some(payload))` on a clean
/// complete transfer.
pub(crate) fn read_bounded_payload<R: Read>(
    mut reader: R,
    max_size: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut payload = Vec::new();
    let mut bounded_reader = (&mut reader).take((max_size + 1) as u64);
    bounded_reader.read_to_end(&mut payload)?;

    if payload.is_empty() || payload.len() > max_size {
        Ok(None)
    } else {
        Ok(Some(payload))
    }
}

/// Reads a MIME payload from an already-`offer.receive()`d pipe, normalises
/// it, hashes it, and forwards the finished `ClipboardJob` to the DbWorker.
///
/// Factored out of the Selection handler so it can be called from the
/// dedicated per-selection thread the handler spawns.
fn ingest_and_send(
    read_file: std::fs::File,
    mime_to_get: String,
    job_tx: &mpsc::Sender<ClipboardJob>,
) {
    let payload = match read_bounded_payload(read_file, MAX_PAYLOAD_SIZE) {
        Ok(Some(p)) => p,
        Ok(None) => return,
        Err(_) => return,
    };

    let Some((final_mime, payload)) = normalise_payload(mime_to_get, payload) else {
        return;
    };

    // SHA3-256 fingerprint of the final normalised payload actually being persisted.
    let mut hasher = Sha3_256::new();
    hasher.update(&payload);
    let hash = hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    // Send the completed payload and its SHA3 fingerprint to the persistent worker.
    let _ = job_tx.send(ClipboardJob {
        mime: final_mime,
        data: payload,
        hash,
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn mimes(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn select_mime_requested_match_is_returned_verbatim() {
        let offers = mimes(&["text/plain", "text/html"]);
        assert_eq!(
            select_mime(&offers, Some("text/html"), true),
            Some("text/html".to_string())
        );
    }

    #[test]
    fn select_mime_absent_requested_mime_returns_none() {
        let offers = mimes(&["text/plain", "text/html"]);
        assert_eq!(select_mime(&offers, Some("application/pdf"), true), None);
    }

    #[test]
    fn select_mime_requested_match_is_case_and_param_insensitive() {
        let offers = mimes(&["TEXT/Plain; charset=UTF-8"]);
        assert_eq!(
            select_mime(&offers, Some("text/plain"), true),
            Some("TEXT/Plain; charset=UTF-8".to_string())
        );
    }

    #[test]
    fn select_mime_requested_rtf_is_honoured_even_when_drop_rtf_enabled() {
        // An explicit `paste-from text/rtf` request names exactly what the
        // caller wants; `drop_rtf` only governs the daemon's own fallback
        // chain (target_mime = None), never an explicit request.
        let offers = mimes(&["text/rtf"]);
        assert_eq!(
            select_mime(&offers, Some("text/rtf"), true),
            Some("text/rtf".to_string())
        );
    }

    #[test]
    fn select_mime_daemon_fallback_prefers_priority_order() {
        let offers = mimes(&["text/html", "image/png", "text/plain"]);
        assert_eq!(
            select_mime(&offers, None, true),
            Some("image/png".to_string())
        );
    }

    #[test]
    fn select_mime_daemon_fallback_matches_lower_ranked_priority_entry() {
        // "text/html" is still in MIME_PRIORITY_ORDER, just ranked below
        // plain text and images — with nothing higher-ranked on offer, it's
        // still reached via the priority list itself, not the catch-all.
        let offers = mimes(&["text/html"]);
        assert_eq!(
            select_mime(&offers, None, true),
            Some("text/html".to_string())
        );
    }

    #[test]
    fn select_mime_daemon_fallback_catch_all_for_unlisted_mime() {
        // Not in MIME_PRIORITY_ORDER, and neither an "image/" nor "text/"
        // prefix — only the final unconditional catch-all reaches it.
        let offers = mimes(&["application/octet-stream"]);
        assert_eq!(
            select_mime(&offers, None, true),
            Some("application/octet-stream".to_string())
        );
    }

    #[test]
    fn select_mime_drop_rtf_excludes_rtf_from_daemon_fallback() {
        let offers = mimes(&["text/rtf"]);
        assert_eq!(select_mime(&offers, None, true), None);
    }

    #[test]
    fn select_mime_rtf_allowed_in_daemon_fallback_when_drop_rtf_disabled() {
        let offers = mimes(&["text/rtf"]);
        assert_eq!(
            select_mime(&offers, None, false),
            Some("text/rtf".to_string())
        );
    }

    #[test]
    fn select_mime_drop_rtf_skips_rtf_in_favour_of_other_text() {
        let offers = mimes(&["text/rtf", "text/markdown"]);
        assert_eq!(
            select_mime(&offers, None, true),
            Some("text/markdown".to_string())
        );
    }

    #[test]
    fn select_mime_empty_offer_list_returns_none() {
        let offers: Vec<String> = Vec::new();
        assert_eq!(select_mime(&offers, None, true), None);
        assert_eq!(select_mime(&offers, Some("text/plain"), true), None);
    }

    // --- normalise_payload ---

    #[test]
    fn normalise_payload_plain_text_passes_through_unchanged() {
        let result = normalise_payload("text/plain".to_string(), b"hello".to_vec());
        assert_eq!(result, Some(("text/plain".to_string(), b"hello".to_vec())));
    }

    #[test]
    fn normalise_payload_uri_list_is_normalised() {
        let result = normalise_payload(
            MIME_URI_LIST.to_string(),
            b"file:///a.txt\nfile:///b.txt".to_vec(),
        );
        assert_eq!(
            result,
            Some((MIME_URI_LIST.to_string(), b"/a.txt\n/b.txt".to_vec()))
        );
    }

    #[test]
    fn normalise_payload_uri_list_mime_with_parameters_is_still_recognised() {
        // `mime_base_eq` must see through a trailing parameter on the
        // offered label, exactly as it does for every other MIME comparison.
        let mime = format!("{};charset=utf-8", MIME_URI_LIST);
        let result = normalise_payload(mime, b"file:///a.txt".to_vec());
        assert_eq!(result.map(|(_, data)| data), Some(b"/a.txt".to_vec()));
    }

    #[test]
    fn normalise_payload_uri_list_with_no_valid_paths_returns_none() {
        let result = normalise_payload(MIME_URI_LIST.to_string(), b"# just a comment\n".to_vec());
        assert_eq!(result, None);
    }

    #[test]
    fn normalise_payload_reidentifies_image_by_magic_bytes() {
        // Offered as a generic label, but the bytes are unmistakably PNG —
        // the detected MIME must win over the offered one.
        let png_bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let result = normalise_payload("application/octet-stream".to_string(), png_bytes.clone());
        assert_eq!(result, Some(("image/png".to_string(), png_bytes)));
    }

    #[test]
    fn normalise_payload_non_image_non_uri_list_mime_is_untouched() {
        let result = normalise_payload("text/html".to_string(), b"<p>hi</p>".to_vec());
        assert_eq!(
            result,
            Some(("text/html".to_string(), b"<p>hi</p>".to_vec()))
        );
    }

    // --- read_bounded_payload ---

    #[test]
    fn read_bounded_payload_under_limit_succeeds() {
        let input = b"short text payload";
        let result = read_bounded_payload(&input[..], 1024).unwrap();
        assert_eq!(result, Some(input.to_vec()));
    }

    #[test]
    fn read_bounded_payload_exact_limit_succeeds() {
        let input = [0x42; 64];
        let result = read_bounded_payload(&input[..], 64).unwrap();
        assert_eq!(result, Some(input.to_vec()));
    }

    #[test]
    fn read_bounded_payload_exceeding_limit_returns_none() {
        // 65 bytes against a 64-byte bound: strictly exceeds, must return None
        // to guard against saving a truncated partial payload.
        let input = [0x42; 65];
        let result = read_bounded_payload(&input[..], 64).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn read_bounded_payload_empty_input_returns_none() {
        let input: &[u8] = b"";
        let result = read_bounded_payload(input, 1024).unwrap();
        assert_eq!(result, None);
    }
}
