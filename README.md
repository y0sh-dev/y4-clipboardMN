
<div align="center">

# y4p

**Unified Wayland Clipboard Infrastructure.**

[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://rust-lang.org)
[![License](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Wayland-lightgerm.svg)](https://wayland.freedesktop.org)
[![Version](https://img.shields.io/badge/version-0.6.0-green.svg)](https://github.com/y0sh-dev/y4p/releases/latest)

`y4p` is a standalone clipboard manager, built natively for Wayland.

It runs as a single daemon — no separate monitor process, no separate provider process — that watches the clipboard, serves history back out, and persists everything to disk.
Engineered with strict zero-loss integrity and defense-in-depth resource controls, it seamlessly handles text streams, binary payloads, and high-resolution images with deterministic performance.

</div>

---

## Targets

`y4p` targets Wayland compositors that implement the `ext-data-control-v1` protocol natively:
- **Supported**: wlroots-based compositors (Sway, Hyprland, river, and derivatives).
- **Unsupported**: Compositors without `ext-data-control-v1` support (e.g. standard GNOME/Mutter, KWin).

---

## Features

- **Unified Daemon**: Single-process architecture managing clipboard monitoring, IPC serving, and disk persistence without external daemon orchestration.
- **Hybrid Storage**: Fast, searchable SQLite engine for text metadata paired with content-addressed filesystem caching for binary blobs.
- **Script-First CLI**: Deterministic command interface with immutable entry IDs and strict argument parsing for shell pipeline integration (`fzf`, `rofi`).
- **Zero-Loss Integrity**: Pinned entries are strictly immune to ring buffer rotation; zero schema corruption across migrations.
- **Local Image Pipeline**: Bounded concurrency transcode engine with isolated scratch directories, timeout watchdogs, and memory limits.
- **Robust & Dependency-Free**: Panic-free execution path with zero third-party dependencies outside the Rust standard library and SQLite.

---

## Command List

A quick look at what's available. Full flags and examples live in `y4p help`.

| Command | What it does |
| :--- | :--- |
| `daemon` | Start the background monitor and IPC listener. |
| `list` / `search` | Browse or search clipboard history. |
| `copy-to` | Restore a history entry to the live clipboard. |
| `show` | Inspect a record's content directly. |
| `store` / `paste-from` | Manually ingest stdin, or read the OS clipboard directly. |
| `pin` / `unpin` | Protect or release a record from automatic rotation. |
| `delete` / `wipe` | Remove one record, or clear all history. |
| `status` / `pause` / `resume` | Check or control the daemon's monitoring state. |

---

## Get Started

### Quick Digest

```bash
# Clone repository
git clone https://github.com/y0sh-dev/y4p.git
cd y4p

# Build and install binary
cargo build --release
sudo cp target/release/y4p /usr/local/bin/

# Start background daemon
y4p daemon &

# Verify status and store clipboard content
y4p status
echo "Wayland native clipboard" | y4p store
y4p list
```

Enable Zsh completions (optional). Add the completions directory to your `fpath` before `compinit`:

```zsh
fpath+=(/path/to/y4p/completions)
```

For every command, its flags, and detailed examples, run:

```bash
y4p help
```

<details>
<summary><strong>Verification & Tests</strong></summary>

```bash
# Run unit and integration tests
cargo test

# Strict linter checks
cargo clippy --all-targets -- -D warnings

# Full automated verification suite
bash scripts/check.sh
```

</details>

---

## Configuration

`y4p` works out of the box with sensible defaults and zero mandatory configuration.

To customise behaviour — such as history limits or MIME filtering — place a `y4p.toml` file at `$XDG_CONFIG_HOME/y4p/y4p.toml` (defaulting to `~/.config/y4p/y4p.toml`).

A documented reference template is provided in [`y4p.toml.example`](y4p.toml.example):

```bash
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/y4p"
cp y4p.toml.example "${XDG_CONFIG_HOME:-$HOME/.config}/y4p/y4p.toml"
```

---

## Documentation

- [`docs/00_overview.md`](docs/00_overview.md) — Architectural overview, single-threaded I/O multiplexing, and ring buffer invariants.
- [`docs/AI_POLICY.md`](docs/AI_POLICY.md) — Shared policies and guidelines on Generative AI (LLMs) usage.

---

## License

GPL-3.0-or-later

Copyright (c) 2026 yosana (y0sh-dev)
