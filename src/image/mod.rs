// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/image/mod.rs

//! Local Image Pipeline: runs external converters (ImageMagick's `magick`)
//! as filters over `Read`/`Write` streams.
//!
//! Layering: depends only on `std` and `core`; nothing here knows about
//! SQLite, Wayland or the daemon, so any layer may adopt it later.

// Only ingestion (`route::route_for_ingest`) consumes this module so far;
// parts of the public surface (e.g. PNG output, egress-side conversion) are
// exercised by the unit tests only until later v0.5.x milestones adopt them,
// so `dead_code` is allowed module-wide rather than sprinkled per item.
#![allow(dead_code)]

pub mod breaker;
pub mod magick;
pub mod pipeline;
pub mod route;
pub mod throttle;
pub mod transcode;
