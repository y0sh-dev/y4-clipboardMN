# Concurrency & Memory Reclamation

The daemon's poll loop, described in [01](01_wayland_streaming_io.md), has to stay responsive to new Wayland events at all times.

Disk writes and hashing are exactly the kind of work that would stall it, if they ran inline. This document covers how `y4p` keeps that work off the critical path — and how it keeps a long-lived process from quietly accumulating memory it no longer needs.

---

## Why every database write goes through one worker thread

SQLite allows exactly one writer at a time.

A second connection that tries to write while another transaction is open gets `SQLITE_BUSY` — "database is locked." A naive retry-with-backoff strategy for that error becomes a source of latency spikes, and under enough contention, outright write failures.

`y4p` avoids the problem structurally, instead of defensively. Only one thread in the entire process ever holds a write connection:

```rust
// src/daemon/worker.rs
pub fn spawn(mut db: ClipboardDb, metrics: Arc<DaemonMetrics>, verbose: bool, max_history: usize) -> Self {
    let (tx, rx) = mpsc::channel::<ClipboardJob>();

    std::thread::spawn(move || {
        while let Ok(job) = rx.recv() {
            match db.insert_with_hash(&job.mime, &job.data, &job.hash, max_history) {
                Ok(_) => metrics.record_ingress(),
                Err(e) => eprintln!("worker failed to persist data: {}", e),
            }
        }
    });

    Self { tx }
}
```

`ClipboardDb` moves *into* the spawned thread. It's never shared.

Rust's ownership model makes "only this thread can write" a compile-time fact — not a convention someone has to remember at every call site. Every other part of the daemon that wants something persisted (the Wayland ingestion handler, mainly) doesn't touch the database at all. It builds a `ClipboardJob`, and sends it down an `mpsc::Sender` clone.

The channel is the only coupling between "something happened on the clipboard" and "something got written to disk." And it's a coupling that can never produce a lock conflict — because structurally, there is nothing on the other side of that lock to conflict with.

This is also why the daemon opens a *second*, read-only `ClipboardDb` handle for reads served from within the daemon process itself — see `read_db` in `daemon::start_daemon` — rather than sharing the writer's connection. SQLite's WAL mode is specifically designed to let readers proceed concurrently with a writer, without blocking either side. Splitting the handle this way costs nothing, and it means a slow read query could never, even in principle, block clipboard ingestion.

```text
   Wayland event
        |
        v
   ClipboardJob --> mpsc channel --> DbWorker thread --> SQLite
        |                                                (single writer)
        |                                                     ^
        |                                                     | WAL mode:
        +-- main poll loop keeps running,                     | readers never
            never waits on this send                          | block the writer
                                                                |
                                              read_db (list / search) ---+
```

---

## Why `open_read_only` exists — from convention to enforcement

Everything above describes a *design*: only the worker thread's `ClipboardDb` handle ever writes, and `read_db` exists purely to read. But until v0.2.5, that guarantee lived entirely in the Rust code's structure — every `ClipboardDb`, `read_db` included, was opened through the same `Connection::open`, which hands back an ordinary read-write connection.

Nothing was actually wrong with that. Rust's ownership rules already make it impossible for two threads to *share* the writer's handle, and `read_db`'s own code never calls anything but `SELECT`. The single-writer property held — but it held because every call site happened to behave, not because SQLite itself would refuse to let it do otherwise.

That distinction matters for exactly the kind of bug code review can't catch by inspection alone: a future contributor adding one read-oriented feature to `read_db`'s call path, who reaches for `UPDATE` instead of `SELECT` because it's the fastest way to make a bug go away, and nothing stops the connection from accepting it.

`ClipboardDb::open_read_only()` closes that gap at the layer beneath Rust's own type system:

```rust
// src/storage/mod.rs
Connection::open_with_flags(
    &db_path,
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
)
```

`SQLITE_OPEN_READ_ONLY` isn't a Rust-level promise. It's instruction to the SQLite engine itself: any `INSERT`, `UPDATE`, or `DELETE` sent down this particular connection is rejected by SQLite before it ever touches a page on disk, regardless of what the calling Rust code intended to do. The single-writer rule stops being "true because nobody has broken it yet," and becomes "true because the database itself won't allow it to be broken." `SQLITE_OPEN_NO_MUTEX` is a smaller, complementary optimization riding along on the same call — it skips SQLite's own internal connection locking, which a connection that (by construction) only one thread will ever touch doesn't need to pay for.

```text
   Before (v0.2.0)                         After (open_read_only, v0.2.5)

   read_db: Connection::open(...)          read_db: Connection::open_with_flags(
     (read-write connection —                 ..., SQLITE_OPEN_READ_ONLY)
      "please only SELECT" is a
      convention, not a rule)                 a stray UPDATE/DELETE/INSERT
                                               is rejected by SQLite itself
```

As of v0.2.5, this constructor exists but is not yet the one `read_db` or any CLI read path actually calls — today they still open through the ordinary read-write `open()`, exactly as before. Wiring `read_db` and the read-only CLI commands (`list`, `search`, `show`) over to `open_read_only` is tracked for v0.3.0 (see the `TODO(v0.3.0)` marker in `storage/mod.rs`). The v0.2.5 change is deliberately scoped to landing the enforcement primitive on its own — a constructor whose only job is opening a connection SQLite itself will refuse to let write, ready for the call sites that will adopt it next.
