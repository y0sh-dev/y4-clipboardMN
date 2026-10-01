// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/storage/db.rs

use super::MetaRow;
use crate::core::constants::{PREVIEW_CHARS, SENSITIVE_MIME_HINTS};
use crate::core::utils::strip_html_tags;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Result, params};
use std::borrow::Cow;
use std::time::{SystemTime, UNIX_EPOCH};

/// Result of `upsert_record`: `real_id == -1` means the payload was
/// deliberately skipped (empty / sensitive MIME) and no rotation ran.
pub struct UpsertOutcome {
    pub real_id: i64,
    pub is_image: bool,
    pub expired_hashes: Vec<String>,
}

/// Pure SQL execution and transaction handling — no filesystem I/O. Binary
/// cache placement is the caller's (facade's) responsibility.
pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// Insert-or-touch a record by hash, then atomically rotate out anything
    /// beyond `max_history`. See `ClipboardDb::insert_with_hash` doc for why
    /// the ID is returned directly instead of via a follow-up query.
    pub fn upsert_record(
        &mut self,
        mime: &str,
        data: &[u8],
        hash: &str,
        max_history: usize,
    ) -> Result<UpsertOutcome, String> {
        if data.is_empty() || SENSITIVE_MIME_HINTS.iter().any(|&hint| mime.contains(hint)) {
            return Ok(UpsertOutcome {
                real_id: -1,
                is_image: false,
                expired_hashes: Vec::new(),
            });
        }

        let is_image = mime.starts_with("image/") || mime.contains("gif");

        // Was `.unwrap()`: a monotonic clock read before UNIX_EPOCH should
        // never happen on a real system, but `panic = "abort"` in the release
        // profile would still turn that near-impossible case into a full
        // process abort instead of degrading gracefully.
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;

        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM clipboard WHERE hash = ?1 LIMIT 1",
                params![hash],
                |row| row.get(0),
            )
            .ok();

        let real_id = if let Some(id) = existing {
            tx.execute(
                "UPDATE clipboard SET timestamp = ?1 WHERE id = ?2",
                params![ts, id],
            )
            .map_err(|e| e.to_string())?;
            id
        } else {
            // "application/xhtml+xml" doesn't contain "text" itself (unlike
            // text/html), so it needs its own check here to get a preview at
            // all rather than falling through to `None`. Also decides the
            // `content` storage class below, so search's `content LIKE ?`
            // (see search_metadata/validate_keywords) always lines up with
            // what actually got stored as TEXT vs BLOB.
            let is_markup = mime == "text/html" || mime.contains("xhtml");
            let is_text_like = mime.contains("text")
                || mime.contains("uri-list")
                || mime.contains("json")
                || is_markup;

            let preview = if is_text_like {
                // Rich markup's raw tags aren't a readable preview — strip
                // them first so the preview column always shows plain,
                // scannable text instead of leaking `<div>`/`<strong>` etc.
                let text_data: Cow<[u8]> = if is_markup {
                    Cow::Owned(strip_html_tags(data))
                } else {
                    Cow::Borrowed(data)
                };
                let s = String::from_utf8_lossy(&text_data);
                Some(
                    s.chars()
                        .take(PREVIEW_CHARS)
                        .collect::<String>()
                        .replace('\n', " "),
                )
            } else {
                None
            };

            // TEXT storage class for textual content: lets `content LIKE ?`
            // match directly, without a per-row `CAST(content AS TEXT)` at
            // query time. Bytes claiming a text MIME but not actually valid
            // UTF-8 fall through to the ordinary BLOB path rather than being
            // rejected or corrupted.
            if is_text_like && let Ok(text) = std::str::from_utf8(data) {
                tx.execute(
                    "INSERT INTO clipboard (timestamp, mime, size, preview, content, hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![ts, mime, data.len() as i64, preview, text, hash],
                ).map_err(|e| e.to_string())?;
            } else {
                let db_content = if is_image { None } else { Some(data) };
                tx.execute(
                    "INSERT INTO clipboard (timestamp, mime, size, preview, content, hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![ts, mime, data.len() as i64, preview, db_content, hash],
                ).map_err(|e| e.to_string())?;
            }

            tx.last_insert_rowid()
        };

        // G-10: rotation only ever counts/evicts unpinned rows, so both
        // queries below filter to `is_pinned = 0` before applying the
        // OFFSET — pinned rows are never candidates for eviction AND never
        // consume the unpinned history quota.
        //
        // PERF: `LIMIT -1 OFFSET ?1` seeks directly to the (max_history+1)th
        // unpinned row in `idx_pinned_ts` and scans only what's actually
        // expiring, instead of the previous `NOT IN (SELECT ... LIMIT ?1)`
        // form — that forced SQLite to build a Bloom filter from the kept
        // set and probe it against every unpinned row (cost scales with
        // total history size, not with how much actually expired).
        let expired_hashes: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT hash FROM clipboard
                 WHERE is_pinned = 0
                 ORDER BY timestamp DESC
                 LIMIT -1 OFFSET ?1",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![max_history as i64], |row| row.get::<_, String>(0))
                .map_err(|e| e.to_string())?;
            rows.filter_map(|r| r.ok()).collect()
        };

        // Same expiring set as above, deleted by rowid (via `id IN (...)`)
        // rather than re-testing `NOT IN` against the kept rows.
        tx.execute(
            "DELETE FROM clipboard
             WHERE id IN (
                 SELECT id FROM clipboard
                 WHERE is_pinned = 0
                 ORDER BY timestamp DESC
                 LIMIT -1 OFFSET ?1
             )",
            params![max_history as i64],
        )
        .map_err(|e| e.to_string())?;

        tx.commit().map_err(|e| e.to_string())?;

        Ok(UpsertOutcome {
            real_id,
            is_image,
            expired_hashes,
        })
    }

    /// See `ClipboardDb::search_metadata` doc: each hit carries its absolute
    /// MRU index, matching `fetch_metadata`'s ordering. Assumes `queries` is
    /// non-empty — the facade returns early otherwise.
    ///
    /// AND-combines one `(preview LIKE ?i OR ...)` clause per keyword; the
    /// WHERE clause is built dynamically (clause count depends on N) but
    /// every value is still bound through a placeholder, never interpolated.
    ///
    /// BUGFIX: the content branch was previously guarded by `preview IS
    /// NULL`, but every text/uri-list record gets a non-null preview (see
    /// `upsert_record`), so that branch was dead and a match past the first
    /// `PREVIEW_CHARS` characters was silently missed. Checking `content`
    /// unconditionally makes the full body actually searchable. No more
    /// `CAST(content AS TEXT)` here either — `upsert_record` now binds
    /// textual content with TEXT storage class directly, so `content` is
    /// already text-comparable for every row this query's mime filter admits.
    ///
    /// PERF: absolute index is a correlated `COUNT(*)` per candidate row
    /// instead of `ROW_NUMBER() OVER (ORDER BY timestamp DESC)`. The window
    /// function forced SQLite to materialize and number every row in the
    /// table before the mime/keyword filter could run at all; this form lets
    /// the filter (and its `idx_ts`/`idx_pinned_ts` index usage) run first,
    /// so the `COUNT(*)` only pays for rows that actually matched. Ties
    /// (identical `timestamp`, effectively never seen at this table's
    /// millisecond granularity) would collapse to the same index rather than
    /// the arbitrary-but-distinct one `ROW_NUMBER()` assigned — an accepted
    /// difference given how the index is only ever a display/lookup key, not
    /// a uniqueness guarantee.
    pub fn search_metadata(&self, queries: &[String], limit: usize) -> Vec<(usize, MetaRow)> {
        let and_clauses: Vec<String> = (1..=queries.len())
            .map(|i| format!("(preview LIKE ?{i} OR content LIKE ?{i})"))
            .collect();
        let limit_idx = queries.len() + 1;

        let sql = format!(
            "SELECT (SELECT COUNT(*) FROM clipboard c2 WHERE c2.timestamp > c1.timestamp) AS abs_idx,
                    id, timestamp, mime, size, preview, is_pinned
             FROM clipboard c1
             WHERE (mime LIKE '%text%' OR mime LIKE '%UTF8%')
               AND {}
             ORDER BY timestamp DESC LIMIT ?{}",
            and_clauses.join(" AND "), limit_idx
        );

        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let wildcarded: Vec<String> = queries.iter().map(|q| format!("%{}%", q)).collect();
        let limit_param = limit as i64;
        let mut bindings: Vec<&dyn rusqlite::ToSql> = wildcarded
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        bindings.push(&limit_param);

        let rows = match stmt.query_map(bindings.as_slice(), |row| {
            let abs_idx: i64 = row.get(0)?;
            Ok((
                abs_idx as usize,
                (
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ),
            ))
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        rows.filter_map(|r| r.ok()).collect()
    }

    /// Pre-sorts keywords into (valid, invalid) by a cheap `LIMIT 1`
    /// existence check per word, so `search_metadata`'s AND query only ever
    /// runs against words actually present. Same text/mime filter as the
    /// real search, minus the absolute-index/ordering machinery it doesn't need.
    pub fn validate_keywords(&self, keywords: &[String]) -> (Vec<String>, Vec<String>) {
        let mut valid = Vec::new();
        let mut invalid = Vec::new();

        for kw in keywords {
            let pattern = format!("%{}%", kw);
            let exists = self
                .conn
                .query_row(
                    "SELECT 1 FROM clipboard
                 WHERE (mime LIKE '%text%' OR mime LIKE '%UTF8%')
                   AND (preview LIKE ?1 OR content LIKE ?1)
                 LIMIT 1",
                    params![pattern],
                    |_| Ok(()),
                )
                .is_ok();

            if exists {
                valid.push(kw.clone());
            } else {
                invalid.push(kw.clone());
            }
        }

        (valid, invalid)
    }

    pub fn fetch_metadata(&self, limit: usize) -> Vec<MetaRow> {
        let mut stmt = match self.conn.prepare(
            "SELECT id, timestamp, mime, size, preview, is_pinned FROM clipboard ORDER BY timestamp DESC LIMIT ?1"
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map(params![limit as i64], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(|r| r.ok()).collect()
    }

    /// Row's inline content (if any) plus its hash, for the facade to fall
    /// back to the file cache when `content` is `NULL`.
    pub fn get_row_content(&self, id: i64) -> Option<(String, Option<Vec<u8>>, String)> {
        self.conn
            .query_row(
                "SELECT mime, content, hash FROM clipboard WHERE id = ?1",
                params![id],
                |row| {
                    // BUGFIX: `content` can now be TEXT storage class (v3
                    // textual rows) or BLOB (binary/legacy rows) — `row.get::<_,
                    // Vec<u8>>` only accepts BLOB and errors on TEXT. Reading
                    // via `ValueRef::as_bytes` accepts either storage class
                    // uniformly as raw bytes.
                    let content = match row.get_ref(1)? {
                        ValueRef::Null => None,
                        v => Some(v.as_bytes()?.to_vec()),
                    };
                    Ok((row.get(0)?, content, row.get(2)?))
                },
            )
            .ok()
    }

    /// Same shape as `get_row_content` but without materializing the BLOB —
    /// backs `ClipboardDb::locate_content`.
    pub fn get_row_location(&self, id: i64) -> Option<(String, bool, String)> {
        self.conn
            .query_row(
                "SELECT mime, content IS NOT NULL, hash FROM clipboard WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok()
    }

    pub fn update_timestamp(&mut self, id: i64) -> Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        self.conn.execute(
            "UPDATE clipboard SET timestamp = ?1 WHERE id = ?2",
            params![ts, id],
        )?;
        Ok(())
    }

    /// Returns whether a row was deleted plus its hash (if it had one), so
    /// the facade can clean up the matching cache file regardless of which
    /// bit fired — mirrors the pre-split behavior exactly.
    pub fn delete_by_id(&mut self, id: i64) -> Result<(bool, Option<String>)> {
        let hash: Option<String> = self
            .conn
            .query_row(
                "SELECT hash FROM clipboard WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .ok();

        let res = self
            .conn
            .execute("DELETE FROM clipboard WHERE id = ?1", params![id])?;
        Ok((res > 0, hash))
    }

    /// Clears rows and reclaims disk space. Cache directory cleanup is the
    /// facade's job (`FileCache::clear`) — kept out of here so this module
    /// never touches the filesystem outside the SQLite file itself.
    pub fn wipe(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM clipboard", [])?;
        let _ = self.conn.execute_batch(
            "PRAGMA journal_mode = DELETE;
             VACUUM;
             PRAGMA journal_mode = WAL;",
        );
        Ok(())
    }

    /// G-10: flips a single record's pin flag by its immutable ID. Returns
    /// whether a row was actually affected, so the CLI can distinguish
    /// "not found" from success rather than reporting a false positive.
    pub fn set_pinned(&mut self, id: i64, is_pinned: bool) -> Result<bool, String> {
        let affected = self
            .conn
            .execute(
                "UPDATE clipboard SET is_pinned = ?1 WHERE id = ?2",
                params![is_pinned, id],
            )
            .map_err(|e| e.to_string())?;
        Ok(affected > 0)
    }

    pub fn get_total_count(&self) -> usize {
        self.conn
            .query_row("SELECT COUNT(*) FROM clipboard", [], |row| {
                row.get::<_, i64>(0).map(|val| val as usize)
            })
            .unwrap_or(0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::storage::schema::SchemaManager;

    fn setup_test_store() -> SqliteStore {
        let mut conn = Connection::open_in_memory().unwrap();
        SchemaManager::initialize(&mut conn, 1000).unwrap();
        SqliteStore::new(conn)
    }

    #[test]
    fn upsert_text_and_fetch_metadata() {
        let mut store = setup_test_store();
        let outcome = store
            .upsert_record("text/plain", b"hello world", "hash1", 10)
            .unwrap();
        assert_eq!(outcome.real_id, 1);
        assert!(!outcome.is_image);

        let rows = store.fetch_metadata(10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, 1); // id
        assert_eq!(rows[0].2, "text/plain"); // mime
        assert_eq!(rows[0].3, 11); // size
        assert_eq!(rows[0].4.as_deref(), Some("hello world")); // preview
        assert!(!rows[0].5); // is_pinned
    }

    #[test]
    fn upsert_image_stores_null_content_and_flags_is_image() {
        let mut store = setup_test_store();
        let img_bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let outcome = store
            .upsert_record("image/png", &img_bytes, "imghash", 10)
            .unwrap();
        assert_eq!(outcome.real_id, 1);
        assert!(outcome.is_image);

        let (mime, content, hash) = store.get_row_content(1).unwrap();
        assert_eq!(mime, "image/png");
        assert!(content.is_none()); // Stored out-of-line in FileCache
        assert_eq!(hash, "imghash");
    }

    #[test]
    fn pin_protection_prevents_rotation_eviction() {
        let mut store = setup_test_store();

        // 1. Insert first record and pin it
        let o1 = store
            .upsert_record("text/plain", b"entry 1", "h1", 2)
            .unwrap();
        assert!(store.set_pinned(o1.real_id, true).unwrap());

        // 2. Insert second record (unpinned, total unpinned = 1)
        std::thread::sleep(std::time::Duration::from_millis(5));
        let o2 = store
            .upsert_record("text/plain", b"entry 2", "h2", 2)
            .unwrap();

        // 3. Insert third record (unpinned, total unpinned = 2, fills quota)
        std::thread::sleep(std::time::Duration::from_millis(5));
        let o3 = store
            .upsert_record("text/plain", b"entry 3", "h3", 2)
            .unwrap();
        assert!(o3.expired_hashes.is_empty());

        // 4. Insert fourth record (unpinned, total unpinned = 3, exceeds quota = 2)
        std::thread::sleep(std::time::Duration::from_millis(5));
        let o4 = store
            .upsert_record("text/plain", b"entry 4", "h4", 2)
            .unwrap();

        // Pinned entry 1 must survive; oldest unpinned entry 2 must be evicted
        assert!(o4.expired_hashes.contains(&"h2".to_string()));
        assert!(!o4.expired_hashes.contains(&"h1".to_string()));

        let rows = store.fetch_metadata(10);
        let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
        assert!(ids.contains(&o1.real_id)); // Pinned row protected!
        assert!(!ids.contains(&o2.real_id)); // Oldest unpinned row evicted!
        assert!(ids.contains(&o3.real_id)); // Surviving unpinned row present!
        assert!(ids.contains(&o4.real_id)); // Newest row present!
    }

    #[test]
    fn search_metadata_finds_matching_text() {
        let mut store = setup_test_store();
        store
            .upsert_record("text/plain", b"Rust language", "h1", 10)
            .unwrap();
        store
            .upsert_record("text/plain", b"Python language", "h2", 10)
            .unwrap();

        let queries = vec!["Rust".to_string()];
        let results = store.search_metadata(&queries, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.4.as_deref(), Some("Rust language"));
    }

    #[test]
    fn delete_and_wipe_operations() {
        let mut store = setup_test_store();
        let o1 = store.upsert_record("text/plain", b"foo", "h1", 10).unwrap();
        let (deleted, hash) = store.delete_by_id(o1.real_id).unwrap();
        assert!(deleted);
        assert_eq!(hash, Some("h1".to_string()));
        assert_eq!(store.get_total_count(), 0);

        store.upsert_record("text/plain", b"bar", "h2", 10).unwrap();
        assert_eq!(store.get_total_count(), 1);
        store.wipe().unwrap();
        assert_eq!(store.get_total_count(), 0);
    }
}
