use rusqlite::{Connection, params};

use crate::error::{Result, SutraError};

use super::Db;

// (name, sql, ephemeral_only) — ephemeral_only migrations are cleared on reindex.
const MIGRATIONS: &[(&str, &str, bool)] = &[
    (
        "0001_initial",
        include_str!("../../migrations/0001_initial.sql"),
        true,
    ),
    (
        "0002_complexity",
        include_str!("../../migrations/0002_complexity.sql"),
        true,
    ),
    (
        "0003_snapshot_aggregates",
        include_str!("../../migrations/0003_snapshot_aggregates.sql"),
        true,
    ),
    (
        "0004_symbol_flags",
        include_str!("../../migrations/0004_symbol_flags.sql"),
        true,
    ),
    (
        "0005_conventions",
        include_str!("../../migrations/0005_conventions.sql"),
        true,
    ),
    (
        "0006_language_attrs",
        include_str!("../../migrations/0006_language_attrs.sql"),
        true,
    ),
    (
        "0007_convention_overrides",
        include_str!("../../migrations/0007_convention_overrides.sql"),
        true,
    ),
    (
        "0008_components",
        include_str!("../../migrations/0008_components.sql"),
        true,
    ),
    (
        "0009_reconciliation",
        include_str!("../../migrations/0009_reconciliation.sql"),
        false,
    ),
    (
        "0010_clustering_meta",
        include_str!("../../migrations/0010_clustering_meta.sql"),
        false,
    ),
    (
        "0011_anchor_score",
        include_str!("../../migrations/0011_anchor_score.sql"),
        false,
    ),
    (
        "0012_vocabulary_aliases",
        include_str!("../../migrations/0012_vocabulary_aliases.sql"),
        false,
    ),
    (
        "0013_clustering_config_hash",
        include_str!("../../migrations/0013_clustering_config_hash.sql"),
        false,
    ),
];

impl Db {
    pub(crate) fn run_migrations(&self) -> Result<()> {
        let conn = self.conn.lock();

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 name         TEXT    NOT NULL UNIQUE,
                 content_hash TEXT    NOT NULL,
                 applied_at   TEXT    NOT NULL
             )",
        )?;

        let pre_runner = Self::detect_pre_runner_db(&conn)?;
        if pre_runner {
            Self::register_retroactive(&conn)?;
            return Ok(());
        }

        for &(name, sql, _ephemeral_only) in MIGRATIONS {
            let hash = blake3::hash(sql.as_bytes()).to_hex().to_string();

            let existing: Option<String> = conn
                .query_row(
                    "SELECT content_hash FROM schema_migrations WHERE name = ?1",
                    params![name],
                    |row| row.get(0),
                )
                .ok();

            if let Some(stored_hash) = existing {
                if stored_hash != hash {
                    return Err(SutraError::Internal(format!(
                        "migration `{name}` content hash mismatch: \
                         stored={stored_hash}, current={hash}. \
                         Do not modify already-applied migrations."
                    )));
                }
                continue;
            }

            let sp = format!("migration_{name}");
            conn.execute_batch(&format!("SAVEPOINT {sp}"))?;

            match conn.execute_batch(sql) {
                Ok(()) => {
                    conn.execute(
                        "INSERT INTO schema_migrations (name, content_hash, applied_at) \
                         VALUES (?1, ?2, datetime('now'))",
                        params![name, hash],
                    )?;
                    conn.execute_batch(&format!("RELEASE SAVEPOINT {sp}"))?;
                }
                Err(e) => {
                    let _ = conn.execute_batch(&format!("ROLLBACK TO SAVEPOINT {sp}"));
                    let _ = conn.execute_batch(&format!("RELEASE SAVEPOINT {sp}"));
                    return Err(SutraError::Internal(format!(
                        "migration `{name}` failed: {e}"
                    )));
                }
            }
        }
        Ok(())
    }

    fn detect_pre_runner_db(conn: &Connection) -> Result<bool> {
        let has_files: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master \
                 WHERE type='table' AND name='files'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if !has_files {
            return Ok(false);
        }

        let migration_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap_or(0);

        Ok(migration_count == 0)
    }

    pub(crate) fn ephemeral_migration_names() -> Vec<&'static str> {
        MIGRATIONS
            .iter()
            .filter(|(_, _, ephemeral_only)| *ephemeral_only)
            .map(|(name, _, _)| *name)
            .collect()
    }

    fn register_retroactive(conn: &Connection) -> Result<()> {
        for &(name, sql, _ephemeral_only) in MIGRATIONS {
            let hash = blake3::hash(sql.as_bytes()).to_hex().to_string();

            if Self::migration_schema_present(conn, name) {
                conn.execute(
                    "INSERT OR IGNORE INTO schema_migrations (name, content_hash, applied_at) \
                     VALUES (?1, ?2, datetime('now'))",
                    params![name, hash],
                )?;
            } else {
                let sp = format!("migration_{name}");
                conn.execute_batch(&format!("SAVEPOINT {sp}"))?;
                match conn.execute_batch(sql) {
                    Ok(()) => {
                        conn.execute(
                            "INSERT INTO schema_migrations (name, content_hash, applied_at) \
                             VALUES (?1, ?2, datetime('now'))",
                            params![name, hash],
                        )?;
                        conn.execute_batch(&format!("RELEASE SAVEPOINT {sp}"))?;
                    }
                    Err(e) => {
                        let _ = conn.execute_batch(&format!("ROLLBACK TO SAVEPOINT {sp}"));
                        let _ = conn.execute_batch(&format!("RELEASE SAVEPOINT {sp}"));
                        return Err(SutraError::Internal(format!(
                            "retroactive migration `{name}` failed: {e}"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn migration_schema_present(conn: &Connection, name: &str) -> bool {
        match name {
            "0001_initial" => {
                for table in &["files", "symbols", "refs", "imports", "snapshots"] {
                    let exists: bool = conn
                        .query_row(
                            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
                            params![table],
                            |row| row.get(0),
                        )
                        .unwrap_or(false);
                    if !exists {
                        return false;
                    }
                }
                true
            }
            "0002_complexity" => {
                Self::column_exists(conn, "symbols", "cyclomatic")
                    && Self::column_exists(conn, "symbols", "cognitive")
            }
            "0003_snapshot_aggregates" => {
                Self::column_exists(conn, "snapshots", "total_complexity")
                    && Self::column_exists(conn, "snapshots", "health_score")
            }
            "0004_symbol_flags" => Self::column_exists(conn, "symbols", "flags"),
            "0005_conventions" => {
                let exists: bool = conn
                    .query_row(
                        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='conventions'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap_or(false);
                exists
            }
            "0006_language_attrs" => Self::column_exists(conn, "symbols", "language_attrs"),
            "0007_convention_overrides" => {
                let exists: bool = conn
                    .query_row(
                        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='convention_overrides'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap_or(false);
                exists
            }
            "0008_components" => {
                conn.query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='components'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false)
            }
            "0010_clustering_meta" => {
                conn.query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='component_clustering_meta'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false)
            }
            "0011_anchor_score" => Self::column_exists(conn, "semantic_anchors", "score"),
            _ => false,
        }
    }

    fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
        let mut stmt = match conn.prepare(&format!("PRAGMA table_info({table})")) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();
        names.iter().any(|n| n == column)
    }
}
