// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/wayland/handlers/data_control/mod.rs

pub mod device;
pub mod source;

use wayland_client::{Dispatch, Connection, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
};
use std::os::fd::{FromRawFd, OwnedFd, AsRawFd};
use crate::wayland::state::{WaylandState, OfferData};
use crate::core::constants::*;

/// Evaluates if the requested MIME type is compatible with the target type.
/// Supports category-level matching for text and image groups. Both sides
/// are compared via their normalized base type (case- and
/// whitespace-insensitive, parameters stripped — see
/// `core::utils::parse_mime`), so `TEXT/PLAIN`, `text/plain; charset=utf-8`
/// and `text/plain;charset=UTF-8` are all treated identically.
fn mime_is_compatible(requested: &str, target: &str) -> bool {
    let req_base = crate::core::utils::parse_mime(requested).0;
    let tgt_base = crate::core::utils::parse_mime(target).0;

    if req_base == tgt_base { return true; }
    if req_base.starts_with("text/") && tgt_base.starts_with("text/") { return true; }
    if req_base.starts_with("image/") && tgt_base.starts_with("image/") { return true; }

    // Already-lowercased base forms — the old list's charset-suffixed
    // variants are redundant now that params are stripped before comparing.
    const TEXT_ALIASES: &[&str] = &[
        "text/plain",
        "utf8_string",
        "string",
        "text",
        "compound_text",
    ];
    // text/html already matches via the text/* rule above; application/xhtml+xml
    // is HTML in an XML wrapper and needs the same "requestable as plain text" treatment.
    let req_is_text_alias = TEXT_ALIASES.contains(&req_base.as_str());
    let tgt_is_text_alias = TEXT_ALIASES.contains(&tgt_base.as_str())
        || tgt_base.starts_with("text/")
        || tgt_base == "application/xhtml+xml";

    req_is_text_alias && tgt_is_text_alias
}

/// Checks if any offered MIME type matches the sensitive hints blacklist.
///
/// BUGFIX: both the haystack (offered MIME) and the hint now go through
/// `to_ascii_lowercase()`. Previously only the MIME was lowercased while
/// `SENSITIVE_MIME_HINTS` mixes case (`x-kde-passwordManagerHint`) — a
/// lowercased haystack can never contain that hint's own uppercase letters
/// verbatim, so this specific hint could never actually match anything,
/// silently defeating the KDE password-manager filter it exists for.
pub(crate) fn is_sensitive<S: AsRef<str>>(mimes: &[S]) -> bool {
    SENSITIVE_MIME_HINTS.iter().any(|&hint| {
        let hint_lower = hint.to_ascii_lowercase();
        mimes.iter().any(|m| m.as_ref().to_ascii_lowercase().contains(&hint_lower))
    })
}

/// Creates a Unix pipe and configures it for safe, high-throughput I/O.
/// Implements immediate RAII wrapping to prevent FD leaks during setup failures.
fn make_pipe() -> Option<(std::fs::File, OwnedFd)> {
    let mut fds = [0i32; 2];

    // SAFETY: `fds` is a valid, correctly-sized `&mut [c_int; 2]` for
    // `pipe(2)` to write its two returned descriptors into.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return None;
    }

    // RAII Safety: Immediately wrap raw FDs.
    // If the function returns early after this point, FDs are automatically closed.
    // SAFETY: `fds[0]` is a freshly-created, open, unique descriptor `pipe(2)`
    // just returned above, wrapped here exactly once.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: same as `read_fd` above, for the pipe's other (write) end.
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    // SAFETY: every `libc` call below only reads/sets flags on the two FDs
    // owned above (still valid — neither `OwnedFd` has been dropped yet).
    unsafe {
        for fd in &[read_fd.as_raw_fd(), write_fd.as_raw_fd()] {
            let flags = libc::fcntl(*fd, libc::F_GETFL, 0);
            if flags >= 0 {
                // Force blocking mode to ensure complete data transfer for large buffers
                libc::fcntl(*fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
        }
    }

    // Convert read side to File for standard I/O compatibility
    Some((std::fs::File::from(read_fd), write_fd))
}

// --- ExtDataControlManagerV1 ---

impl Dispatch<ExtDataControlManagerV1, ()> for WaylandState {
    fn event(_: &mut Self, _: &ExtDataControlManagerV1, _: ext_data_control_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

// --- ExtDataControlOfferV1 ---

impl Dispatch<ExtDataControlOfferV1, OfferData> for WaylandState {
    fn event(_: &mut Self, _: &ExtDataControlOfferV1, ev: ext_data_control_offer_v1::Event, data: &OfferData, _: &Connection, _: &QueueHandle<Self>) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = ev
            && let Ok(mut mimes) = data.mimes.lock()
            && !mimes.contains(&mime_type) {
            mimes.push(mime_type);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn is_sensitive_detects_all_configured_hints() {
        assert!(is_sensitive(&["x-kde-passwordManagerHint"]));
        assert!(is_sensitive(&["X-KDE-PASSWORDMANAGERHINT"]));
        assert!(is_sensitive(&["application/x-keepassxc-selection"]));
        assert!(is_sensitive(&["application/x-vnd.1password"]));
        assert!(is_sensitive(&["application/x-bitwarden"]));
        assert!(is_sensitive(&["x-gnome-cliptrace"]));
        assert!(is_sensitive(&["org.nspasteboard.ConcealedType"]));
        assert!(is_sensitive(&["custom/secret-payload"]));
        assert!(is_sensitive(&["text/plain", "application/x-keepassxc-selection"]));
    }

    #[test]
    fn is_sensitive_clean_mimes_pass() {
        assert!(!is_sensitive(&["text/plain"]));
        assert!(!is_sensitive(&["text/plain;charset=utf-8"]));
        assert!(!is_sensitive(&["text/html"]));
        assert!(!is_sensitive(&["image/png"]));
        assert!(!is_sensitive(&["text/uri-list"]));
    }

    #[test]
    fn is_sensitive_empty_list_returns_false() {
        let empty: &[&str] = &[];
        assert!(!is_sensitive(empty));
    }
}
