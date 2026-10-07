// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/image/mod.rs

//! Local Image Pipeline: runs external converters (ImageMagick's `magick`)
//! as filters over `Read`/`Write` streams.
//!
//! Layering: depends only on `std` and `core`; nothing here knows about
//! SQLite, Wayland or the daemon, so any layer may adopt it later.

// Milestones v0.5.1-v0.5.2 land the I/O boundary and the transcoder on
// their own; their consumers (conversion on ingest/egress) arrive in later
// v0.5.x milestones. Until then the public surface is exercised by the
// unit tests only, so `dead_code` is allowed module-wide rather than
// sprinkled per item.
#![allow(dead_code)]

pub mod magick;
pub mod pipeline;
pub mod transcode;
