// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/wayland/handlers/data_control/device.rs

use wayland_client::{Dispatch, Connection, QueueHandle, Proxy};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_offer_v1::ExtDataControlOfferV1,
};
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::{mpsc, Arc, Mutex};
use sha3::{Digest, Sha3_256};
use crate::wayland::state::{WaylandState, OfferData, ClipboardJob};
use crate::core::constants::*;
use super::{make_pipe, is_sensitive};

impl Dispatch<ExtDataControlDeviceV1, ()> for WaylandState {
    fn event(state: &mut Self, _: &ExtDataControlDeviceV1, ev: ext_data_control_device_v1::Event, _: &(), conn: &Connection, _: &QueueHandle<Self>) {
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

            if mimes.is_empty() || is_sensitive(&mimes) { return; }

            // Determine which offered MIME to request: a specific one if
            // `y4p paste-from <mime>` is waiting for it (Action Mode), or the
            // richest/most-reproducible one by `MIME_PRIORITY_ORDER`
            // otherwise (daemon ingestion) — see `select_mime`.
            let drop_rtf = state.config.should_drop_rtf();
            let Some(mime_to_get) = select_mime(&mimes, state.target_mime.as_deref(), drop_rtf) else { return; };

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
                // S-07: uri-list must be fully buffered and normalized before
                // hashing (the fingerprint has to match what's actually
                // persisted), so it can't share the other MIMEs' single-pass
                // hash-while-read below.
                let is_uri_list = mime_to_get == MIME_URI_LIST;

                std::thread::spawn(move || {
                    ingest_and_send(read_file, mime_to_get, is_uri_list, &job_tx_clone);
                });
            } else {
                // Action Mode: Synchronous read for immediate CLI processing
                let mut buf = Vec::new();
                let mut reader = read_file.take(268435456);
                let _ = reader.read_to_end(&mut buf);
                state.rx_buf = buf;
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
pub(crate) fn select_mime(mimes: &[String], target_mime: Option<&str>, drop_rtf: bool) -> Option<String> {
    if let Some(target) = target_mime {
        return mimes.iter().find(|m| crate::core::utils::mime_base_eq(m, target)).cloned();
    }

    MIME_PRIORITY_ORDER.iter()
        .find_map(|&p| mimes.iter().find(|m| crate::core::utils::mime_base_eq(m, p)))
        .cloned()
        .or_else(|| mimes.iter().find(|m| m.to_ascii_lowercase().starts_with("image/")).cloned())
        .or_else(|| mimes.iter().find(|m| m.to_ascii_lowercase().starts_with("text/") && (!drop_rtf || !crate::core::utils::is_rtf_mime(m))).cloned())
        .or_else(|| mimes.iter().find(|m| !drop_rtf || !crate::core::utils::is_rtf_mime(m)).cloned())
}

/// Reads a MIME payload from an already-`offer.receive()`d pipe, hashes it
/// (re-identifying images by magic bytes along the way — see
/// `core::utils::detect_image_mime`), and forwards the finished
/// `ClipboardJob` to the DbWorker.
///
/// Factored out of the Selection handler so it can be called from the
/// dedicated per-selection thread the handler spawns.
fn ingest_and_send(read_file: std::fs::File, mime_to_get: String, is_uri_list: bool, job_tx: &mpsc::Sender<ClipboardJob>) {
    let mut payload = Vec::new();
    let mut reader = read_file.take(268435456);

    if reader.read_to_end(&mut payload).is_err() || payload.is_empty() { return; }
    let mut final_mime = mime_to_get;

    if is_uri_list {
        payload = crate::core::utils::normalize_uri_list(&payload);
        if payload.is_empty() { return; }
    } else if let Some(m) = crate::core::utils::detect_image_mime(&payload) {
        final_mime = m.to_string();
    }

    // SHA3-256 fingerprint of the final normalised payload actually being persisted.
    let mut hasher = Sha3_256::new();
    hasher.update(&payload);
    let hash = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>();

    // Send the completed payload and its SHA3 fingerprint to the persistent worker.
    let _ = job_tx.send(ClipboardJob { mime: final_mime, data: payload, hash });
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
        assert_eq!(select_mime(&offers, Some("text/html"), true), Some("text/html".to_string()));
    }

    #[test]
    fn select_mime_absent_requested_mime_returns_none() {
        let offers = mimes(&["text/plain", "text/html"]);
        assert_eq!(select_mime(&offers, Some("application/pdf"), true), None);
    }

    #[test]
    fn select_mime_requested_match_is_case_and_param_insensitive() {
        let offers = mimes(&["TEXT/Plain; charset=UTF-8"]);
        assert_eq!(select_mime(&offers, Some("text/plain"), true), Some("TEXT/Plain; charset=UTF-8".to_string()));
    }

    #[test]
    fn select_mime_requested_rtf_is_honoured_even_when_drop_rtf_enabled() {
        // An explicit `paste-from text/rtf` request names exactly what the
        // caller wants; `drop_rtf` only governs the daemon's own fallback
        // chain (target_mime = None), never an explicit request.
        let offers = mimes(&["text/rtf"]);
        assert_eq!(select_mime(&offers, Some("text/rtf"), true), Some("text/rtf".to_string()));
    }

    #[test]
    fn select_mime_daemon_fallback_prefers_priority_order() {
        let offers = mimes(&["text/html", "image/png", "text/plain"]);
        assert_eq!(select_mime(&offers, None, true), Some("image/png".to_string()));
    }

    #[test]
    fn select_mime_daemon_fallback_matches_lower_ranked_priority_entry() {
        // "text/html" is still in MIME_PRIORITY_ORDER, just ranked below
        // plain text and images — with nothing higher-ranked on offer, it's
        // still reached via the priority list itself, not the catch-all.
        let offers = mimes(&["text/html"]);
        assert_eq!(select_mime(&offers, None, true), Some("text/html".to_string()));
    }

    #[test]
    fn select_mime_daemon_fallback_catch_all_for_unlisted_mime() {
        // Not in MIME_PRIORITY_ORDER, and neither an "image/" nor "text/"
        // prefix — only the final unconditional catch-all reaches it.
        let offers = mimes(&["application/octet-stream"]);
        assert_eq!(select_mime(&offers, None, true), Some("application/octet-stream".to_string()));
    }

    #[test]
    fn select_mime_drop_rtf_excludes_rtf_from_daemon_fallback() {
        let offers = mimes(&["text/rtf"]);
        assert_eq!(select_mime(&offers, None, true), None);
    }

    #[test]
    fn select_mime_rtf_allowed_in_daemon_fallback_when_drop_rtf_disabled() {
        let offers = mimes(&["text/rtf"]);
        assert_eq!(select_mime(&offers, None, false), Some("text/rtf".to_string()));
    }

    #[test]
    fn select_mime_drop_rtf_skips_rtf_in_favour_of_other_text() {
        let offers = mimes(&["text/rtf", "text/markdown"]);
        assert_eq!(select_mime(&offers, None, true), Some("text/markdown".to_string()));
    }

    #[test]
    fn select_mime_empty_offer_list_returns_none() {
        let offers: Vec<String> = Vec::new();
        assert_eq!(select_mime(&offers, None, true), None);
        assert_eq!(select_mime(&offers, Some("text/plain"), true), None);
    }
}

