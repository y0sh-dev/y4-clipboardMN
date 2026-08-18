// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/daemon/mod.rs

use crate::storage::ClipboardDb;
use crate::wayland;
use crate::wayland::state::{WaylandState, ClipboardJob};
use crate::core::constants::*;
use crate::core::SocketGuard;
use std::os::unix::net::{UnixListener, UnixStream};
use std::io::{BufRead, Write};
use std::fs;
use std::os::fd::{AsFd, AsRawFd};
use std::time::Duration;
use std::sync::mpsc;

/// Initialize and run the clipboard daemon with a unified, high-performance event loop.
///
/// Returns `true` if the daemon actually reached its serving state (bound
/// the socket, connected to the compositor, obtained the data-control
/// manager and a seat) at least once before its loop exited. Returns
/// `false` if it failed to start at all, so the caller can report an
/// accurate status instead of the previous unconditional "daemon process
/// terminated." (which was printed even on a startup failure that never
/// served a single request).
pub fn start_daemon(mut db: ClipboardDb, verbose: bool) -> bool {
    let socket_path = crate::core::get_socket_path();

    if let Ok(mut stream) = UnixStream::connect(&socket_path) {
        let _ = stream.write_all(&[IPC_CMD_EXIT]);
        std::thread::sleep(Duration::from_millis(RECONNECT_DELAY_MS));
    }
    
    let _ = fs::remove_file(&socket_path);

    // BUGFIX: was `.expect(...)`, which — combined with `panic = "abort"` in
    // the release profile — turned an ordinary, recoverable failure (e.g.
    // another daemon instance genuinely still holding the socket, or a
    // permissions problem in the runtime directory) into a hard process
    // abort with a generic panic message instead of a clear diagnostic.
    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{}failed to bind IPC socket at {}: {}", LOG_ERROR, socket_path.display(), e);
            return false;
        }
    };
    let _ = listener.set_nonblocking(true);
    let _guard = SocketGuard::new(socket_path);

    // Initialize Database Worker Thread
    let (job_tx, job_rx) = mpsc::channel::<ClipboardJob>();
    let is_verbose = verbose;

    std::thread::spawn(move || {
        // The worker thread owns the mutable reference to the database.
        while let Ok(job) = job_rx.recv() {
            match db.insert_with_hash(&job.mime, &job.data, &job.hash) {
                Ok(_) if is_verbose => println!("{}", log_save(&job.mime, job.data.len())),
                Ok(_) => {}
                Err(e) => eprintln!("{}worker failed to persist data: {}", LOG_ERROR, e),
            }
            #[cfg(target_os = "linux")]
            unsafe { libc::malloc_trim(0); }
        }
    });

    let (conn, mut event_queue) = wayland::create_connection();
    let qh = event_queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    // Initialize state with the job sender and a secondary DB handle for reads.
    let read_db = match ClipboardDb::open() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}failed to open read-side database handle: {}", LOG_ERROR, e);
            return false;
        }
    };
    let mut state = WaylandState::new_daemon(read_db, job_tx, verbose);
    
    // Pre-load last data for deduplication.
    state.last_data = state.db.as_ref().and_then(|d| d.lock().ok()).and_then(|d| d.get_latest_data()).unwrap_or_default();
    state.target_mime = DEFAULT_MIME.to_string();

    if event_queue.roundtrip(&mut state).is_err() {
        eprintln!("{}{}", LOG_ERROR, MSG_WAYLAND_CONN_FAIL);
        return false;
    }
    if !bind_data_device(&mut state, &qh, &conn) {
        eprintln!("{}compositor does not advertise {} and/or {}; cannot serve clipboard.", LOG_ERROR, INTERFACE_MANAGER, INTERFACE_SEAT);
        return false;
    }

    println!("{}{}", LOG_INFO, MSG_DAEMON_READY);

    while !crate::core::is_exiting() {
        let _ = event_queue.dispatch_pending(&mut state);
        let _ = conn.flush();

        // EDGE CASE FIX: if the seat (or, in principle, the data-control
        // manager) was removed by the compositor — e.g. a session
        // suspend/resume or a seat hot-unplug — `wayland/handlers/mod.rs`
        // clears `state.device` on `GlobalRemove`. Previously nothing ever
        // re-created it: the device was only ever bound once, before this
        // loop started. If the seat later reappeared, the daemon would
        // silently sit in a broken state (bound to the socket, but unable
        // to monitor or serve the clipboard) until manually restarted.
        // Retrying this cheap, idempotent check every iteration lets the
        // daemon self-heal instead.
        if state.device.is_none() {
            bind_data_device(&mut state, &qh, &conn);
        }

        let mut poll_fds = [
            libc::pollfd { fd: conn.as_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: listener.as_fd().as_raw_fd(),  events: libc::POLLIN, revents: 0 },
        ];

        if unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, 500) } < 0 { continue; }

        // 3. IPC Ingress Handling
        if poll_fds[1].revents & libc::POLLIN != 0
            && let Ok((stream, _)) = listener.accept() {
                let mut reader = std::io::BufReader::new(stream);
                let mut buf = Vec::new();

                if reader.read_until(IPC_DELIMITER, &mut buf).is_ok()
                    && buf.len() > 1 {
                        let n = buf.len() - 1; 
                        match buf[0] {
                            IPC_CMD_EXIT => crate::core::request_exit(),
                            IPC_CMD_RESTORE => {
                                let id_str = String::from_utf8_lossy(&buf[1..n]);
                                if let Ok(real_id) = id_str.trim().parse::<i64>() {
                                    handle_restore_request(&mut state, &qh, real_id, &conn);
                                }
                            }
                            _ => {}
                        }
                }
        }

        if poll_fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 { break; }
        if poll_fds[0].revents & libc::POLLIN != 0
        && let Some(guard) = event_queue.prepare_read() {
            let _ = guard.read();
        }
    }

    true
}

/// Bind the data-control device from the currently-known manager + seat, if
/// both are available and a device isn't already bound. Returns whether a
/// device is bound after the call (regardless of whether this call itself
/// created it), so both the initial startup path and the per-iteration
/// self-healing check in `start_daemon` can share the same logic.
fn bind_data_device(
    state: &mut WaylandState,
    qh: &wayland_client::QueueHandle<WaylandState>,
    conn: &wayland_client::Connection,
) -> bool {
    if state.device.is_some() { return true; }

    if let (Some(manager), Some(seat)) = (&state.manager, &state.seat) {
        state.device = Some(manager.get_data_device(seat, qh, ()));
        let _ = conn.flush();
        true
    } else {
        false
    }
}

/// Serve a historical record with narrow lock scope and broad MIME compatibility.
fn handle_restore_request(state: &mut WaylandState, qh: &wayland_client::QueueHandle<WaylandState>, real_id: i64, conn: &wayland_client::Connection) {
    let db_payload = {
        if let Some(ref db_mutex) = state.db {
            if let Ok(db) = db_mutex.lock() {
                db.get_content_by_id(real_id)
            } else { None }
        } else { None }
    };

    if let Some((mime, val)) = db_payload
        && let Some(ref manager) = state.manager {
        state.provider_locks += 1;

        let meta = crate::wayland::state::SourceMetadata {
            mime: mime.clone(),
            data: val,
        };

        let source = manager.create_data_source(qh, meta);
        
        // Broadcaster Strategy: Advertise multiple compatible MIMEs
        source.offer(mime.clone());

        if mime.starts_with("image/") {
            let image_alts = ["image/png", "image/jpeg", "image/webp", "image/gif"];
            for alt in image_alts {
                if *alt != mime { source.offer(alt.to_string()); }
            }
        } else if mime.contains("text") || mime == MIME_URI_LIST {
            for alt in TEXT_MIME_ALTS {
                if *alt != mime { source.offer(alt.to_string()); }
            }
            // Ensure URI lists can be consumed by standard text editors
            if mime == MIME_URI_LIST {
                source.offer("text/plain".to_string());
            }
        }

        if let Some(ref device) = state.device {
            device.set_selection(Some(&source));
            let _ = conn.flush();
        }

        state.current_source = Some(source);
        if state.verbose { 
            println!("{}", log_restore(real_id as usize)); 
        }
    }
}
