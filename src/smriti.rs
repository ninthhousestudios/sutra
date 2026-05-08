use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, params};
use tracing::warn;

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct SmritiEvent {
    pub id: i64,
    pub event_type: String,
    pub path: String,
    pub content_hash: String,
    pub previous_hash: Option<String>,
    pub previous_path: Option<String>,
}

#[derive(Debug)]
pub struct EventPage {
    pub cursor_valid: bool,
    pub events: Vec<SmritiEvent>,
    pub next_cursor: i64,
    pub has_more: bool,
}

pub struct SmritiReader {
    conn: Connection,
    cursor_path: PathBuf,
}

fn default_smriti_db() -> PathBuf {
    std::env::var("SUTRA_SMRITI_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".smriti").join("index.db")
        })
}

impl SmritiReader {
    pub fn open(smriti_db: Option<&Path>, cursor_dir: &Path) -> Result<Self> {
        let db_path = match smriti_db {
            Some(p) => p.to_path_buf(),
            None => default_smriti_db(),
        };

        let conn = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "busy_timeout", 5000)?;

        let cursor_path = cursor_dir.join("smriti_cursor");

        Ok(Self { conn, cursor_path })
    }

    #[cfg(test)]
    fn open_conn(conn: Connection, cursor_path: PathBuf) -> Self {
        Self { conn, cursor_path }
    }

    pub fn read_cursor(&self) -> i64 {
        match std::fs::read_to_string(&self.cursor_path) {
            Ok(s) => s.trim().parse().unwrap_or(0),
            Err(_) => 0,
        }
    }

    pub fn write_cursor(&self, cursor: i64) -> Result<()> {
        if let Some(parent) = self.cursor_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.cursor_path, cursor.to_string())?;
        Ok(())
    }

    pub fn events_since(&self, cursor: i64, limit: u32) -> Result<EventPage> {
        let limit = limit.min(1000);

        if cursor > 0 {
            let min_id: Option<i64> =
                self.conn
                    .query_row("SELECT MIN(id) FROM events", [], |r| r.get(0))?;
            match min_id {
                None => {
                    return Ok(EventPage {
                        cursor_valid: true,
                        events: vec![],
                        next_cursor: cursor,
                        has_more: false,
                    });
                }
                Some(min) if min > cursor + 1 => {
                    warn!(
                        cursor,
                        min_id = min,
                        "smriti cursor invalidated — events pruned"
                    );
                    return Ok(EventPage {
                        cursor_valid: false,
                        events: vec![],
                        next_cursor: 0,
                        has_more: false,
                    });
                }
                _ => {}
            }
        }

        let fetch = limit + 1;
        let mut stmt = self.conn.prepare(
            "SELECT id, event_type, path, content_hash, previous_hash, previous_path
             FROM events WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
        )?;

        let mut rows: Vec<SmritiEvent> = stmt
            .query_map(params![cursor, fetch], |row| {
                Ok(SmritiEvent {
                    id: row.get(0)?,
                    event_type: row.get(1)?,
                    path: row.get(2)?,
                    content_hash: row.get(3)?,
                    previous_hash: row.get(4)?,
                    previous_path: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let has_more = rows.len() > limit as usize;
        if has_more {
            rows.truncate(limit as usize);
        }

        let next_cursor = rows.last().map(|e| e.id).unwrap_or(cursor);

        Ok(EventPage {
            cursor_valid: true,
            events: rows,
            next_cursor,
            has_more,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_synthetic_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                event_type  TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                path        TEXT NOT NULL,
                previous_hash TEXT,
                previous_path TEXT,
                timestamp   TIMESTAMP NOT NULL,
                file_extension TEXT,
                mime_type   TEXT,
                scan_id     INTEGER
            );",
        )
        .unwrap();
        conn
    }

    fn insert_event(conn: &Connection, event_type: &str, path: &str) {
        conn.execute(
            "INSERT INTO events (event_type, content_hash, path, timestamp)
             VALUES (?1, ?2, ?3, datetime('now'))",
            params![event_type, "hash_placeholder", path],
        )
        .unwrap();
    }

    #[test]
    fn test_empty_table() {
        let conn = create_synthetic_db();
        let dir = tempfile::tempdir().unwrap();
        let reader = SmritiReader::open_conn(conn, dir.path().join("smriti_cursor"));

        let page = reader.events_since(0, 100).unwrap();
        assert!(page.cursor_valid);
        assert!(page.events.is_empty());
        assert_eq!(page.next_cursor, 0);
        assert!(!page.has_more);
    }

    #[test]
    fn test_normal_pagination() {
        let conn = create_synthetic_db();
        insert_event(&conn, "created", "/tmp/a.rs");
        insert_event(&conn, "modified", "/tmp/b.rs");
        insert_event(&conn, "created", "/tmp/c.rs");

        let dir = tempfile::tempdir().unwrap();
        let reader = SmritiReader::open_conn(conn, dir.path().join("smriti_cursor"));

        let page1 = reader.events_since(0, 2).unwrap();
        assert!(page1.cursor_valid);
        assert_eq!(page1.events.len(), 2);
        assert!(page1.has_more);
        assert_eq!(page1.events[0].path, "/tmp/a.rs");
        assert_eq!(page1.events[1].path, "/tmp/b.rs");

        let page2 = reader.events_since(page1.next_cursor, 2).unwrap();
        assert!(page2.cursor_valid);
        assert_eq!(page2.events.len(), 1);
        assert!(!page2.has_more);
        assert_eq!(page2.events[0].path, "/tmp/c.rs");

        let page3 = reader.events_since(page2.next_cursor, 2).unwrap();
        assert!(page3.cursor_valid);
        assert!(page3.events.is_empty());
        assert!(!page3.has_more);
    }

    #[test]
    fn test_cursor_invalidation() {
        let conn = create_synthetic_db();
        insert_event(&conn, "created", "/tmp/a.rs");
        insert_event(&conn, "created", "/tmp/b.rs");
        insert_event(&conn, "created", "/tmp/c.rs");

        // Simulate reading up to cursor=2
        let dir = tempfile::tempdir().unwrap();
        let reader = SmritiReader::open_conn(conn, dir.path().join("smriti_cursor"));

        let page = reader.events_since(1, 100).unwrap();
        assert!(page.cursor_valid);
        assert_eq!(page.events.len(), 2);

        // Prune events 1 and 2 (simulate smriti pruning)
        reader
            .conn
            .execute("DELETE FROM events WHERE id <= 2", [])
            .unwrap();

        // Cursor 2 is now invalid: min_id (3) > cursor+1 (3) is false, so still valid
        // But cursor 1 would be invalid: min_id (3) > 1+1 (2) is true
        let page = reader.events_since(1, 100).unwrap();
        assert!(!page.cursor_valid);
        assert_eq!(page.next_cursor, 0);
    }

    #[test]
    fn test_cursor_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let cursor_path = dir.path().join("smriti_cursor");

        let conn = create_synthetic_db();
        let reader = SmritiReader::open_conn(conn, cursor_path);

        // Missing file → 0
        assert_eq!(reader.read_cursor(), 0);

        reader.write_cursor(42).unwrap();
        assert_eq!(reader.read_cursor(), 42);

        reader.write_cursor(100).unwrap();
        assert_eq!(reader.read_cursor(), 100);
    }

    #[test]
    fn test_cursor_file_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let cursor_path = dir.path().join("smriti_cursor");
        std::fs::write(&cursor_path, "not-a-number").unwrap();

        let conn = create_synthetic_db();
        let reader = SmritiReader::open_conn(conn, cursor_path);
        assert_eq!(reader.read_cursor(), 0);
    }
}
