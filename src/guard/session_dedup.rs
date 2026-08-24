//! Session-scoped record of which lesson ids the guard has already surfaced.
//!
//! The guard is a fresh process on every edit, so "have I already shown this
//! lesson this session?" cannot live in memory. This is a tiny sqlite file —
//! deliberately separate from `lessons.db` — that answers exactly that, keyed
//! by `(session_id, lesson_id)`.
//!
//! Two invariants earn it its own file rather than a table in `lessons.db`:
//!
//! - **No lock contention.** The guard's `lessons.db` handle is opened
//!   `query_only` inside a blocking PreToolUse hook (see
//!   [`crate::lessons::LessonsDb::open_existing`]); a write against that same
//!   file could stall on its lock. This store is the sole writer of its own
//!   file, so it never contends for that handle.
//! - **Fail-open.** Every operation is best-effort. An open, read, or write
//!   error degrades to "not seen", which at worst re-emits a lesson the agent
//!   has already seen — never a blocked or slowed edit. Callers discard the
//!   `Err`; they never let it reach the hook's decision.
//!
//! Unlike the `lessons.db` handle this store *may* create state — it is the
//! guard's own bookkeeping, not a shared asset — but only ever off a path that
//! already found a lesson to show, so a zero-match edit never touches it.

use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

/// Rows older than this are swept on open. Comfortably longer than any single
/// coding session, so dedup holds for a session's entire life while rows left
/// by long-dead sessions don't accumulate.
const TTL_SECS: i64 = 7 * 86_400;

/// `busy_timeout` for the store. It shares the PreToolUse budget with the
/// lessons lookup, so it must never block for long; as the sole writer of its
/// own file it should never actually have to wait.
const BUSY_TIMEOUT_MS: u32 = 250;

const DB_FILE: &str = "guard_session_dedup.db";

/// A per-session ledger of surfaced lesson ids. See the module docs.
pub(crate) struct SessionDedup {
    conn: Connection,
}

impl SessionDedup {
    /// Open (creating if absent) the dedup store, and opportunistically sweep
    /// rows past [`TTL_SECS`]. A sweep failure is ignored — it only affects
    /// table size, never correctness.
    pub(crate) fn open(db_dir: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_dir.join(DB_FILE))?;
        conn.execute_batch(&format!(
            "PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS surfaced_lessons (
                 session_id TEXT    NOT NULL,
                 lesson_id  TEXT    NOT NULL,
                 ts         INTEGER NOT NULL,
                 PRIMARY KEY (session_id, lesson_id)
             ) WITHOUT ROWID;"
        ))?;
        let cutoff = now_secs() - TTL_SECS;
        let _ = conn.execute(
            "DELETE FROM surfaced_lessons WHERE ts < ?1",
            params![cutoff],
        );
        Ok(Self { conn })
    }

    /// Every lesson id already surfaced in `session_id`. A per-session row count
    /// is small (bounded by what one session's edits have shown), so returning
    /// the whole set and intersecting in Rust beats binding a candidate list.
    pub(crate) fn surfaced_in_session(
        &self,
        session_id: &str,
    ) -> rusqlite::Result<HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT lesson_id FROM surfaced_lessons WHERE session_id = ?1")?;
        stmt.query_map(params![session_id], |row| row.get::<_, String>(0))?
            .collect()
    }

    /// Record that `ids` were surfaced in `session_id`. Idempotent — a repeat of
    /// the same `(session, lesson)` keeps the original timestamp via `OR IGNORE`.
    pub(crate) fn record(&self, session_id: &str, ids: &[&str]) -> rusqlite::Result<()> {
        let now = now_secs();
        for id in ids {
            self.conn.execute(
                "INSERT OR IGNORE INTO surfaced_lessons (session_id, lesson_id, ts) \
                 VALUES (?1, ?2, ?3)",
                params![session_id, id, now],
            )?;
        }
        Ok(())
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SessionDedup) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionDedup::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn records_and_reports_within_a_session() {
        let (_dir, store) = store();
        store.record("s1", &["a", "b"]).unwrap();
        let seen = store.surfaced_in_session("s1").unwrap();
        assert!(seen.contains("a") && seen.contains("b"));
        assert!(!seen.contains("c"));
    }

    #[test]
    fn sessions_are_isolated() {
        let (_dir, store) = store();
        store.record("s1", &["a"]).unwrap();
        assert!(store.surfaced_in_session("s2").unwrap().is_empty());
    }

    #[test]
    fn recording_is_idempotent() {
        let (_dir, store) = store();
        store.record("s1", &["a"]).unwrap();
        store.record("s1", &["a", "a"]).unwrap();
        assert_eq!(store.surfaced_in_session("s1").unwrap().len(), 1);
    }

    #[test]
    fn expired_rows_are_swept_on_open() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = SessionDedup::open(dir.path()).unwrap();
            store.record("s1", &["a"]).unwrap();
            // Backdate the row well past the TTL.
            let stale = now_secs() - TTL_SECS - 1;
            store
                .conn
                .execute("UPDATE surfaced_lessons SET ts = ?1", params![stale])
                .unwrap();
        }
        // Reopening runs the sweep.
        let store = SessionDedup::open(dir.path()).unwrap();
        assert!(store.surfaced_in_session("s1").unwrap().is_empty());
    }
}
