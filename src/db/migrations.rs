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
    (
        "0014_convention_state",
        include_str!("../../migrations/0014_convention_state.sql"),
        false,
    ),
    (
        "0015_convention_component_id",
        include_str!("../../migrations/0015_convention_component_id.sql"),
        true,
    ),
    (
        "0016_convention_history",
        include_str!("../../migrations/0016_convention_history.sql"),
        true,
    ),
    (
        "0017_convention_proposals",
        include_str!("../../migrations/0017_convention_proposals.sql"),
        false,
    ),
    (
        "0018_convention_waivers",
        include_str!("../../migrations/0018_convention_waivers.sql"),
        false,
    ),
    (
        "0019_drop_cascade_proposals",
        include_str!("../../migrations/0019_drop_cascade_proposals.sql"),
        false,
    ),
    (
        "0020_convention_snapshots",
        include_str!("../../migrations/0020_convention_snapshots.sql"),
        true,
    ),
    (
        "0021_component_lifecycle",
        include_str!("../../migrations/0021_component_lifecycle.sql"),
        false,
    ),
    (
        "0022_convention_templates",
        include_str!("../../migrations/0022_convention_templates.sql"),
        true,
    ),
    (
        "0023_constraint_waivers",
        include_str!("../../migrations/0023_constraint_waivers.sql"),
        false,
    ),
    (
        "0024_commit_files",
        include_str!("../../migrations/0024_commit_files.sql"),
        false,
    ),
    (
        "0025_commit_tables",
        include_str!("../../migrations/0025_commit_tables.sql"),
        true,
    ),
    (
        "0026_clustering_commit_timestamp",
        include_str!("../../migrations/0026_clustering_commit_timestamp.sql"),
        false,
    ),
    (
        "0027_health_findings",
        include_str!("../../migrations/0027_health_findings.sql"),
        true,
    ),
    (
        "0028_health_waivers",
        include_str!("../../migrations/0028_health_waivers.sql"),
        false,
    ),
    (
        "0029_hrr_codebook",
        include_str!("../../migrations/0029_hrr_codebook.sql"),
        false,
    ),
    (
        "0030_hrr_vectors",
        include_str!("../../migrations/0030_hrr_vectors.sql"),
        true,
    ),
    (
        "0031_pattern_families",
        include_str!("../../migrations/0031_pattern_families.sql"),
        true,
    ),
    (
        "0032_snapshot_family_count",
        include_str!("../../migrations/0032_snapshot_family_count.sql"),
        true,
    ),
    (
        "0033_health_snapshot_details",
        include_str!("../../migrations/0033_health_snapshot_details.sql"),
        true,
    ),
    (
        "0034_drift_metrics",
        include_str!("../../migrations/0034_drift_metrics.sql"),
        true,
    ),
    (
        "0035_dedup_symbols",
        include_str!("../../migrations/0035_dedup_symbols.sql"),
        true,
    ),
    (
        "0036_drift_alerts",
        include_str!("../../migrations/0036_drift_alerts.sql"),
        true,
    ),
    (
        "0037_completion_tracking",
        include_str!("../../migrations/0037_completion_tracking.sql"),
        true,
    ),
    (
        "0038_hrr_file_hashes",
        include_str!("../../migrations/0038_hrr_file_hashes.sql"),
        true,
    ),
    (
        "0039_idx_imports_file",
        include_str!("../../migrations/0039_idx_imports_file.sql"),
        true,
    ),
    (
        "0040_fca_cache",
        include_str!("../../migrations/0040_fca_cache.sql"),
        true,
    ),
    (
        "0041_snapshot_head_commit",
        include_str!("../../migrations/0041_snapshot_head_commit.sql"),
        true,
    ),
    (
        "0042_import_kind",
        include_str!("../../migrations/0042_import_kind.sql"),
        true,
    ),
    (
        "0043_drop_other_refs",
        include_str!("../../migrations/0043_drop_other_refs.sql"),
        false,
    ),
    (
        "0044_drop_lifecycle",
        include_str!("../../migrations/0044_drop_lifecycle.sql"),
        false,
    ),
    (
        "0045_drop_convention_drift",
        include_str!("../../migrations/0045_drop_convention_drift.sql"),
        false,
    ),
    (
        "0046_constraint_ratchets",
        include_str!("../../migrations/0046_constraint_ratchets.sql"),
        false,
    ),
    (
        "0047_structural_hash",
        include_str!("../../migrations/0047_structural_hash.sql"),
        true,
    ),
    (
        "0048_entity_changes",
        include_str!("../../migrations/0048_entity_changes.sql"),
        false,
    ),
    (
        "0049_resolution_method",
        include_str!("../../migrations/0049_resolution_method.sql"),
        true,
    ),
    (
        "0050_resolved_local_target",
        include_str!("../../migrations/0050_resolved_local_target.sql"),
        true,
    ),
    (
        "0051_ref_receiver",
        include_str!("../../migrations/0051_ref_receiver.sql"),
        true,
    ),
    (
        "0052_import_alias",
        include_str!("../../migrations/0052_import_alias.sql"),
        true,
    ),
    (
        "0053_import_is_test",
        include_str!("../../migrations/0053_import_is_test.sql"),
        true,
    ),
    (
        "0054_reparse_rust_for_is_test",
        include_str!("../../migrations/0054_reparse_rust_for_is_test.sql"),
        true,
    ),
    (
        "0055_reparse_for_test_path_is_test",
        include_str!("../../migrations/0055_reparse_for_test_path_is_test.sql"),
        true,
    ),
    (
        "0056_reparse_for_test_path_remaining_languages",
        include_str!("../../migrations/0056_reparse_for_test_path_remaining_languages.sql"),
        true,
    ),
];

impl Db {
    pub fn migration_count() -> usize {
        MIGRATIONS.len()
    }

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
        // Tables dropped by 0044/0045 — skip their CREATE migrations during
        // reindex so we don't recreate tables we just demolished.
        const SUPERSEDED: &[&str] = &[
            "0016_convention_history",
            "0020_convention_snapshots",
            "0022_convention_templates",
            "0034_drift_metrics",
            "0036_drift_alerts",
        ];
        MIGRATIONS
            .iter()
            .filter(|(name, _, ephemeral_only)| *ephemeral_only && !SUPERSEDED.contains(name))
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
            "0046_constraint_ratchets" => {
                conn.query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='constraint_ratchets'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false)
            }
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
