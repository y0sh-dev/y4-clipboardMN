# Wayland Protocol & Streaming I/O

`y4p` talks to the compositor through `ext-data-control-v1`.

This protocol hands you clipboard data one file descriptor at a time. Every decision in this document comes back to the same constraint: a clipboard manager is background infrastructure. It has to move that data — sometimes 70MB+ of lossless image — without becoming the reason the desktop feels slow.

---

## Why one thread watches both Wayland and IPC

The daemon has two independent sources of work.

Wayland protocol events: a new clipboard offer just arrived. IPC commands: someone ran `y4p copy-to 3` on the socket.

The obvious design spawns a thread per source. `y4p` doesn't. Both are watched on the *same* thread, with a single `libc::poll` call:

```rust
// src/daemon/mod.rs — the daemon's main loop
let mut poll_fds = [
    libc::pollfd { fd: conn.as_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
    libc::pollfd { fd: listener.as_fd().as_raw_fd(),  events: libc::POLLIN, revents: 0 },
];

if unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, 500) } < 0 { continue; }
```

Why does this matter?

A thread that only exists to block on `recv()` still costs something. A kernel stack. A scheduler entry. A context switch, every time it wakes up. That's pure overhead for a process that's idle almost all the time.

`poll(2)` puts the kernel in charge of watching both descriptors at once. It only wakes the process when one of them actually has data.

That's what lets the daemon sit at genuinely 0% CPU between clipboard events — instead of spending cycles just to discover there's nothing to do.

The 500ms timeout isn't a busy-loop interval, either. It's a watchdog. It makes sure `is_exiting()`, and the seat-rebind self-heal check in `bind_data_device`, still get re-evaluated periodically — even with zero I/O activity.

One tradeoff this buys: no locking between "Wayland event" and "IPC command" handling. They can never run concurrently with each other, because they're the same thread. Anything that *does* need real concurrency — disk I/O, hashing — is pushed off this thread entirely instead. More on that in [03](03_concurrency_and_memory.md).

---

## How ingestion reads, normalises, then hashes

Turning a compositor's pipe into a stored record is three distinct decisions — how much to read, how to interpret it, and when to fingerprint it — and `y4p` keeps them in that fixed order rather than interleaving them.

First, the whole payload is read in one bounded call, `read_bounded_payload`, capped by `MAX_PAYLOAD_BYTES` (256MiB, `src/core/constants.rs`):

```rust
// src/wayland/handlers/data_control/device.rs
fn read_bounded_payload<R: Read>(reader: R, limit: u64) -> Option<Vec<u8>> {
    let mut payload = Vec::new();
    let mut bounded = reader.take(limit + 1); // one probe byte beyond the cap
    if bounded.read_to_end(&mut payload).is_err() { return None; }
    if payload.is_empty() || payload.len() as u64 > limit { return None; }
    Some(payload)
}

// ingest_and_send, and Action Mode's synchronous read
let Some(payload) = read_bounded_payload(read_file, MAX_PAYLOAD_BYTES) else { return; };
```

The cap means a misbehaving clipboard source can't turn one ingestion into an unbounded allocation. The `limit + 1` is the subtle part. `Read::take` stops *silently* at its cap: no error, no flag, just a clean `Ok(_)`. If the reader were capped at exactly `limit`, a stream that is truly larger would be cut off mid-transfer and look identical to one that happened to be exactly `limit` bytes long. The truncated bytes would then be hashed and persisted as if they were the whole clipboard entry: silent corruption, and a corrupt entry that restores "successfully".

Reading one extra byte removes the ambiguity. If more than `limit` bytes come back, the stream was oversized, so the whole payload is discarded (`None`) rather than kept as a partial. A stream of exactly `limit` bytes still passes. An I/O error or an empty read also yields `None`, so every failure mode ends the same way: nothing is stored.

`read_to_end` is the standard library's own fully-buffered read; there's no bespoke chunk loop or page-aligned buffer to maintain here, because nothing on this path needs one — ingestion already runs off the daemon's main `poll` loop entirely, on its own per-selection spawned thread (see above), so a straightforward buffered read costs nothing the design cares about.

Second, the buffered payload is normalised, and — where the offered label can't be trusted outright — re-identified from its own bytes. `normalise_payload` re-detects images by their magic bytes (`core::utils::detect_image_mime`) and rewrites `text/uri-list` payloads into plain, percent-decoded paths (`core::utils::normalize_uri_list`):

```rust
// src/wayland/handlers/data_control/device.rs
let Some((final_mime, payload)) = normalise_payload(mime_to_get, payload) else { return; };
```

Only then is the result hashed, exactly once, over the bytes that are actually about to be persisted:

```rust
let mut hasher = Sha3_256::new();
hasher.update(&payload);
let hash = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>();
```

Hashing after normalisation, not before, is what makes deduplication correct: two offers that normalise to the same `text/uri-list` paths — different `file://` escaping, say — collapse into the same fingerprint, because the hash is computed over what they both became, not over how each one happened to arrive.

---

## Why `provider_locks` exists

`ext-data-control-v1` can't tell "the user copied something new" apart from "the compositor is re-offering content the daemon itself just set."

Without a way to distinguish those, restoring a history entry (`copy-to`) would immediately trigger the daemon's own ingestion handler. It would re-hash and re-store the exact content it just placed on the clipboard. A feedback loop — one that does nothing useful, and just touches a timestamp on every restore.

`y4p` closes that loop with a plain counter.

Right before handing data to the compositor, it increments the lock:

```rust
// src/daemon/mod.rs — handle_restore_request
state.provider_locks += 1;
```

And the ingestion handler checks it first, before doing any work at all:

```rust
// src/wayland/handlers/data_control/device.rs
if state.provider_locks > 0 {
    state.provider_locks -= 1;
    return;
}
```

This works cleanly because both sides run on the same single-threaded event loop described above. There's no race between "increment" and "check" to guard against — so a plain `usize` is enough. No atomics. No mutex.

That's the point, really. Use the simplest tool that actually fits the concurrency model you have. Adding synchronization the design doesn't need is just more surface area to get wrong later.

---

## Zero-copy egress for cached payloads

Restoring a *text* entry is cheap either way — it's already a small `Vec<u8>` sitting in the process.

Restoring an *image* from `~/.cache/y4p/<hash>.cache` is a different story. Naively: `read()` the whole file into a `Vec<u8>`, then `write()` that `Vec` back out to the requesting client's pipe. The full payload crosses the userspace boundary twice, for a transfer that never needed to touch this process's heap in the first place.

`storage::ContentLocation` exists to route around that.

A record's payload is either `InlineBlob` — small, lives in the SQLite row — or `CacheFile` — large, lives on disk. Egress branches on which one it is. A `CacheFile` payload goes to `sendfile(2)`, not a read/write pair:

```rust
// src/wayland/handlers/data_control/source.rs
SourcePayload::File(path) => {
    let path = path.clone();
    std::thread::spawn(move || {
        send_via_sendfile(&path, fd);
    });
}
```

`sendfile(2)` copies file-to-pipe entirely inside the kernel. This process's heap never holds the image — not even briefly.

The transfer runs on its own spawned thread, same as the owned-payload path already does. That way, a slow reader on the other end can never stall the daemon's main poll loop.

A userspace copy loop still exists as a fallback (`fallback_copy`). It only kicks in if `sendfile(2)` itself reports it can't be used against this destination — `EINVAL` or `ENOSYS`. Rare, for a plain pipe. But cheap insurance against an exotic kernel silently producing a truncated paste instead of a working one.
