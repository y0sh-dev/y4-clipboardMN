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
use crate::core::config::Config;
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

            // Determine optimal MIME type based on MIME_PRIORITY_ORDER
            // (richest/most-reproducible first). Matching is case- and
            // whitespace-insensitive on the base type (see
            // core::utils::mime_base_eq), so a sender announcing e.g.
            // "TEXT/Plain" or "text/plain; charset=utf-8" (space after ';')
            // still hits its intended priority entry instead of falling
            // through to a generic category fallback. `.cloned()` always
            // takes the string as the sender actually offered it — the
            // compositor request below needs that exact original form, not
            // the normalised one.
            // Check whether RTF is excluded from selection. When enabled via
            // `[mime] drop_rtf`, RTF is ignored in favour of other alternatives.
            let drop_rtf = state.config.should_drop_rtf();

            let mime_to_get = MIME_PRIORITY_ORDER.iter()
                .find_map(|&p| mimes.iter().find(|m| crate::core::utils::mime_base_eq(m, p)))
                .cloned()
                .or_else(|| mimes.iter().find(|m| m.to_ascii_lowercase().starts_with("image/")).cloned())
                .or_else(|| mimes.iter().find(|m| m.to_ascii_lowercase().starts_with("text/") && (!drop_rtf || !crate::core::utils::is_rtf_mime(m))).cloned())
                .or_else(|| mimes.iter().find(|m| !drop_rtf || !crate::core::utils::is_rtf_mime(m)).cloned());

            let Some(mime_to_get) = mime_to_get else { return; };
            if drop_rtf && crate::core::utils::is_rtf_mime(&mime_to_get) { return; }

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
                let config = state.config.clone();

                std::thread::spawn(move || {
                    ingest_and_send(read_file, mime_to_get, is_uri_list, &job_tx_clone, &config);
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

/// Reads a MIME payload from an already-`offer.receive()`d pipe, hashes it
/// (re-identifying images by magic bytes along the way — see
/// `core::utils::detect_image_mime`), and forwards the finished
/// `ClipboardJob` to the DbWorker.
///
/// Factored out of the Selection handler so it can be called from the
/// dedicated per-selection thread the handler spawns.
fn ingest_and_send(read_file: std::fs::File, mime_to_get: String, is_uri_list: bool, job_tx: &mpsc::Sender<ClipboardJob>, config: &Config) {
    let mut payload = Vec::new();
    let mut reader = read_file.take(268435456);

    if reader.read_to_end(&mut payload).is_err() || payload.is_empty() { return; }
    let mut final_mime = mime_to_get;

    if is_uri_list {
        payload = crate::core::utils::normalize_uri_list(&payload);
        if payload.is_empty() { return; }
    } else if let Some(m) = crate::core::utils::detect_image_mime(&payload) {
        final_mime = m.to_string();
    } else if crate::core::utils::is_html_mime(&final_mime) && config.should_downgrade_html() {
        // Strip HTML tags when no plain-text alternative was offered alongside markup.
        payload = crate::core::utils::strip_html_tags(&payload);
        if payload.is_empty() { return; }
        final_mime = DEFAULT_MIME.to_string();
    }

    // SHA3-256 fingerprint of the final normalised payload actually being persisted.
    let mut hasher = Sha3_256::new();
    hasher.update(&payload);
    let hash = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>();

    // Send the completed payload and its SHA3 fingerprint to the persistent worker.
    let _ = job_tx.send(ClipboardJob { mime: final_mime, data: payload, hash });
}

