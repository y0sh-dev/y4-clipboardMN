# Filter Pipeline & Dynamic Egress

A clipboard manager cannot simply be a passive byte sink.

In modern desktop environments, clipboard payloads arrive in arbitrary formats, bloated by redundant rich-text duplicates. Furthermore, sensitive applications (such as password managers) demand exclusion, while command-line utilities and API debuggers require raw, unmodified input.

This document explores how `y4p` filters and transforms clipboard data without introducing external dependencies, compromising memory safety, stalling the Wayland event loop, or performing any network communication — a clipboard manager stores and returns exactly what the compositor gave it.

---

## 1. The Multi-Stage Ingress Pipeline

When a Wayland application sets a new clipboard selection, the compositor emits an `offer` event containing the list of available MIME types. `y4p` processes this offer through a strictly ordered, multi-stage pipeline:

```text
 Compositor Offer (MIME list)
             |
             v
 [Stage 1: Sensitive MIME Gate]  ---> Contains password hints? ---> Discard silently
             |
             v
 [Stage 2: MIME Priority & Filtering]
             |-- Drop RTF if [mime.drop_rtf] enabled
             |-- Select richest format (MIME_PRIORITY_ORDER)
             |
             v
 [Stage 3: Payload Acquisition]
             \-- Bounded direct read: read_to_end, capped at 256 MiB
             |
             v
 [Stage 4: Normalisation]
             \-- URI-List: normalise percent-encoding & file:// schemes
             |
             v
 [Stage 5: SHA3-256 Fingerprint & Worker Dispatch]
             \---> mpsc::Sender<ClipboardJob> ---> DbWorker (SQLite WAL)
```

### Why order matters

1. **Discard before allocation**: Checking sensitive hints occurs before opening pipes or allocating memory buffers. If an offer originates from a password manager, `y4p` drops it immediately at zero I/O cost.
2. **MIME priority over raw offers**: Electron and Chromium applications routinely offer `text/html` alongside `text/plain`, but only synthesise HTML on demand. Requesting `text/html` first can yield an empty transfer. `y4p` prioritises high-fidelity images, standard text, and structured fallbacks in a deterministic sequence (`MIME_PRIORITY_ORDER`).
3. **Never mutate user data on ingress**: SHA3-256 hashing happens *after* URI-list normalisation (a lossless encoding fix, not a content change), so the persisted hash always matches the persisted bytes. Beyond that, `y4p` stores exactly what it received — no repair, no rewriting, no re-encoding, and critically, no markup stripping: a `text/html` offer is persisted byte-for-byte. `strip_html_tags` is reserved strictly for preview generation and plain-text egress fallback (see `wayland::handlers::data_control::source`), never applied at ingress.

---

## 2. Dynamic Egress

`y4p` never fetches or transcodes data over the network or through an external converter — clipboard managers must not perform network communication, full stop. Whatever bytes the compositor hands `y4p` on ingestion are what get stored and, ultimately, pasted back out, unmodified.

Large binary payloads (images, GIFs, ...) are stored *exactly as received* in a deduplicated filesystem cache (`~/.cache/y4p/`), and served back out via the Linux `sendfile(2)` system call — a zero-copy kernel-side transfer that never routes the payload's bytes through this process's own userspace heap:

```text
 Target App requests the stored MIME type
              |
              v
 sendfile(2): cache file -> destination pipe, zero-copy
```

A bounded userspace copy loop is the only fallback, and only if `sendfile(2)` itself reports it can't be used for this particular destination (`EINVAL`/`ENOSYS`) — rare for a plain pipe, but this keeps a `copy-to` of a large payload from silently producing empty/partial clipboard content on an exotic kernel/target instead of just failing outright.

---

## 3. Zero-Dependency Configuration (`y4p.toml`)

### Lenient Standard-Library TOML Parser

To adhere to the project's zero-dependency invariant, `y4p` avoids heavy parsing frameworks (`serde`, `toml`). Instead, `core::config::Config` implements a dedicated streaming parser using standard library slice operations alone.

The parser is designed to be forgiving:
- **Resilience**: Stray syntax errors, comments (`#`), and unrecognised sections are ignored.
- **Fail-safe defaults**: Any unparseable line leaves the corresponding field initialised to its documented default value. A malformed config file will never crash the daemon or prevent it from monitoring the clipboard.

### Configuration Precedence

History limits and MIME-handling behaviour resolve through a deterministic hierarchy:

```text
 1. Command-Line Arguments / Environment Variables (e.g. Y4P_MAX_HISTORY)
                         |
                         v
 2. User Configuration File ($XDG_CONFIG_HOME/y4p/y4p.toml)
                         |
                         v
 3. Hard-Coded Built-In Defaults (DEFAULT_MAX_HISTORY = 100, drop_rtf = true, etc.)
```

This ensures full operational capability in minimal containerised environments where no configuration file exists, while granting desktop users control over history retention and MIME handling.
