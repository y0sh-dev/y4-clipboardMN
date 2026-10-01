// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/unpin.rs

use super::pin;
use crate::storage::ClipboardDb;

/// Clear a history record's pinned flag, returning it to normal automatic
/// rotation eviction. Argument parsing and target resolution are identical
/// to `pin::run` — see `pin::set_pin_state`.
pub fn run(args: &[String], db: &mut ClipboardDb) {
    pin::set_pin_state(args, db, false, "unpin");
}
