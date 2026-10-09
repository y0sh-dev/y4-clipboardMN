# 06 — Local Image Pipeline & Defense-in-Depth Hardening

This document details the architecture, security sandboxing, and resource governance of `y4p`'s local image processing pipeline introduced across the v0.5.x cycle.

The [Overview](00_overview.md) explains how daemon and CLI communicate. This document focuses on the execution boundaries that protect the daemon process while ingesting, transcoding, and caching visual payloads.

---

## 1. Architectural Motivation

Wayland clipboards routinely handle high-resolution screenshots, raw bitmapped framebuffers, and multi-megabyte visual assets alongside plain UTF-8 text.

Treating image data identically to text creates acute hazards:
1. **Memory Exhaustion (OOM)**: Storing multi-megabyte image binaries directly inline within SQLite degrades B-tree query locality and bloats the write-ahead log (WAL).
2. **Decompression Bombs**: Malicious or malformed image headers (e.g. dimensions advertising 100,000 x 100,000 pixels) can induce severe CPU exhaustion or memory starvation during decoding.
3. **Privilege & Multi-User Hazards**: Image decoders occasionally spill working state onto disk; writing to shared public directories invites symlink traversal and data leakage.

To address these concerns, `y4p` routes image assets through an isolated pipeline (`src/image/`) before committing metadata to SQLite and binary blobs to the deduplicated filesystem cache (`~/.cache/y4p/blobs/`).

---

## 2. Ingestion Boundaries & Format Sniffing

MIME types advertised by Wayland source applications cannot be trusted unconditionally. An application advertising `image/png` may supply corrupted stream bytes or an executable polyglot.

Before allocating decompression buffers or spawning external transcode helpers, `y4p` validates payload headers via magic byte sniffing:

```text
[ Incoming Byte Stream ]
           |
           v
 [ Magic Byte Sniffer ]
           |-- 89 50 4E 47 0D 0A 1A 0A -> ImageFormat::Png
           |-- FF D8 FF                -> ImageFormat::Jpeg
           |-- 47 49 46 38 [37|39] 61  -> ImageFormat::Gif
           |-- 52 49 46 46 .... 57 45 42 50 -> ImageFormat::Webp
           |
           +-- Non-matching -> Rejected early without memory allocation
```

If the leading bytes fail to match a supported graphical container, ingestion is aborted immediately. The daemon avoids invoking heavier parsing stages or allocating working memory for unrecognised streams.

---

## 3. Sandboxing & Workspace Isolation (`ScratchDir`)

When transforming or normalising image streams (e.g. converting uncompressed pixel buffers into deduplicated PNG formats), `y4p` invokes ImageMagick utilities with strict sandboxing invariants.

### Disk Spillage and the Shared `/tmp` Vulnerability
Under memory-constrained workloads, ImageMagick can spill intermediate raster allocations onto disk. Defaulting to `/tmp` on multi-user systems introduces serious risks:
- Other local users can observe temporary file naming conventions.
- Unprivileged users can stage symlink races targeting predictable paths.
- Shared temporary directories frequently lack restrictive umask controls.

### RAII Scratch Directory (`src/image/sandbox.rs`)
`y4p` encapsulates temporary working directories inside `ScratchDir`, an RAII guard guaranteeing POSIX mode `0700` (`rwx------`):

```rust
pub struct ScratchDir {
    path: PathBuf,
}
```

1. **Resolution Priority**: The directory resolver attempts to anchor scratch storage inside `$XDG_RUNTIME_DIR/y4p/scratch/` (mounted on tmpfs with user-only permissions by default). If `$XDG_RUNTIME_DIR` is unavailable, it falls back to a securely permissions-checked directory under `std::env::temp_dir()`.
2. **Permission Hardening**: Directory creation strictly verifies or sets POSIX mode `0700`.
3. **Deterministic Cleanup**: The `Drop` implementation removes the directory tree completely upon normal return, error return, or unwinding.

### Child Environment Whitelist (`child_env`)
Spawning child processes with full inherited environments exposes execution to unintended variables (such as `MAGICK_CONFIGURE_PATH`, `LD_PRELOAD`, or proxy variables).

`y4p` executes child helpers with a positive environment whitelist:
- `PATH`: Retained for resolving standard system binaries.
- `MAGICK_TEMPORARY_PATH`: Explicitly pinned to the active `ScratchDir`.
- `TMPDIR`: Explicitly pinned to the active `ScratchDir`.

All other parent environment variables are stripped.

---

## 4. Defense-in-Depth Resource Controls

`y4p` enforces multiple concentric safety boundaries to ensure that rogue images cannot stall or destabilise the clipboard daemon.

```text
+-----------------------------------------------------------------+
| Parent Watchdog (IMAGE_TRANSCODE_TIMEOUT_SECS = 15s)            |
| kill(-pgid, SIGKILL) terminates stalled process trees           |
|                                                                 |
|   +-----------------------------------------------------------+ |
|   | Child Limit (IMAGE_LIMIT_TIME_SECS = 10s)                 | |
|   | -limit time 10 triggers controlled internal exit          | |
|   |                                                           | |
|   |   +-----------------------------------------------------+ | |
|   |   | Memory Quotas                                       | | |
|   |   | -limit memory 256MiB -limit map 512MiB              | | |
|   |   | -limit disk 1GiB                                    | | |
|   |   +-----------------------------------------------------+ | |
|   +-----------------------------------------------------------+ |
+-----------------------------------------------------------------+
```

### Compile-Time Timeout Assertion
To guarantee that the child process's internal CPU limit fires before the parent's external SIGKILL watchdog, the codebase enforces an invariant at compile time:

```rust
const _: () = assert!(
    IMAGE_LIMIT_TIME_SECS < IMAGE_TRANSCODE_TIMEOUT_SECS,
    "internal ImageMagick CPU time limit must be strictly shorter than outer watchdog timeout"
);
```

This ensures that CPU-intensive operations yield structured diagnostic errors from the child helper before the parent is forced to tear down the process group via `SIGKILL`.

---

## 5. Concurrency Throttling & Self-Healing Circuit Breaker

### Bounded Concurrency
Clipboard bursts (such as rapid repeated copy operations or automated clipboard flooding) can trigger concurrent transcoding processes. `y4p` bounds active transcode jobs using a bounded concurrency controller (`src/image/throttle.rs`), queuing or dropping excess transcode requests to prevent thread starvation and load spikes.

### Self-Healing Circuit Breaker (`src/image/breaker.rs`)
To protect against persistent system-level transcode failures (e.g. missing external dependencies, corrupted helper libraries, or persistent pipeline bugs), the pipeline incorporates a three-state circuit breaker:

```text
     +-----------------[ Success / Normal ]-----------------+
     |                                                      |
     v                                                      |
+--------+         Consecutive Failures >= Threshold     +------+
| Closed | --------------------------------------------> | Open |
+--------+                                               +------+
     ^                                                      |
     |                     Cooldown Elapsed                 |
     |                       (Probe Call)                   |
     |                                                      v
     +------------------ [ Success ] ----------------- +-----------+
                                                       | Half-Open |
                                                       +-----------+
                                                            |
                                        [ Failure ] --------+
```

- **Closed**: Normal operations. Transcodes are dispatched as standard.
- **Open**: Consecutive transcode failures exceeded the threshold. Incoming transcode requests fail fast without spawning processes, protecting system resources.
- **Half-Open**: Following a configurable cooldown window, a single probe transcode is permitted. If the probe succeeds, the breaker resets to `Closed`. If it fails, the cooldown timer restarts in `Open`.
- **Fault-Isolation Exception**: Environmental errors originating from directory isolation (`ScratchDir` creation failure) are classified separately from transcode execution failures. Environmental faults do not penalise the circuit breaker's transcode health counter, eliminating false-positive circuit trips.

---

## 6. Zero-Copy Trajectory & Future Milestones

Across the v0.5.x cycle, the image pipeline eliminated unnecessary reallocations across the transcode hot path:
- Slice-based magic sniffing operates over borrowed immutable buffers ($O(1)$ allocation).
- Intermediate scratch files are piped directly through OS file descriptors without redundant user-space vector copying.

This design establishes the architectural foundation for subsequent milestones:
- **v0.6.x (The Great Modularization)**: Physical crate decoupling of `y4p-core`, `y4p-wayland`, `y4p-storage`, and `y4p-image`.
- **v0.7.x (The Zero-Copy Zenith)**: Transitioning pipe and file staging directly to Linux `splice(2)` and `memfd_create(2)` primitives, eliminating user-space buffering entirely.
