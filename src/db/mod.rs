//! SQLite database layer for sutra.
//!
//! All timestamps are stored as ISO-8601 strings (TIMESTAMP columns). Access
//! is serialised through a `parking_lot::Mutex<Connection>` — single-writer
//! model, correct for one server with short-lived transactions.

mod components;
mod constraints;
mod conventions;
pub mod entity_changes;
mod graph;
pub(crate) use graph::MAX_COCHANGE_COMMIT_FANOUT;
mod health;
mod health_evidence;
mod migrations;
mod similarity;

pub use constraints::{
    AckProjection, ConstraintInstanceAckRow, ConstraintRatchetRow, ConstraintWaiverRow,
    WaiverProjection,
};
pub(crate) use constraints::{accepted_sync_marker_from_conn, active_ratchets_from_conn};
pub use conventions::ConventionRow;
pub use health::{HealthFindingRow, HealthWaiverRow, NestingExceedRow};
pub use similarity::{
    HrrSymbolRow, PatternFamily, PatternFamilyMember, PatternFamilyRow, SymbolSummary,
};

use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::{Connection, params};

use crate::error::{Result, SutraError};
use crate::lexical_tokenize::tokenize;
use crate::workspace::{self, WorkspaceEntry};

/// The `symbols_fts.lex_tokens` content for one symbol: the shared lexical
/// tokenizer applied to every searchable field, space-joined. This is the
/// index-time half of the single-tokenizer invariant (sutra/371) — the query
/// side (`explore`) calls the same `tokenize`. Unlike the raw `short_name` /
/// `qualified_name` / `docstring` / `signature` columns (which SQLite's
/// `unicode61` tokenizer stores without splitting camelCase), pre-splitting here
/// lets a prefix query for an interior component like `context` reach
/// `RequestContext` (sutra/394).
fn lex_tokens_blob(
    short_name: &str,
    qualified_name: &str,
    docstring: Option<&str>,
    signature: Option<&str>,
) -> String {
    let mut toks = tokenize(short_name);
    toks.extend(tokenize(qualified_name));
    if let Some(d) = docstring {
        toks.extend(tokenize(d));
    }
    if let Some(s) = signature {
        toks.extend(tokenize(s));
    }
    toks.join(" ")
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SearchTier {
    Exact,
    Fts,
}

impl SearchTier {
    pub fn confidence(&self) -> f64 {
        match self {
            SearchTier::Exact => 1.0,
            SearchTier::Fts => 0.6,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            SearchTier::Exact => "exact",
            SearchTier::Fts => "fts",
        }
    }

    pub fn confidence_json(&self) -> serde_json::Value {
        serde_json::json!({
            "score": self.confidence(),
            "tier": self.label(),
            "formula": "exact short_name match = 1.0, FTS5 prefix match = 0.6",
        })
    }
}

// ---------------------------------------------------------------------------
// Table partition metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TablePartition {
    Ephemeral,
    Durable,
}

pub struct TableMeta {
    pub name: &'static str,
    pub partition: TablePartition,
    pub is_virtual: bool,
}

pub const TABLE_REGISTRY: &[TableMeta] = &[
    TableMeta {
        name: "files",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "symbols",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "symbols_fts",
        partition: TablePartition::Ephemeral,
        is_virtual: true,
    },
    TableMeta {
        name: "refs",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "imports",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "snapshots",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "conventions",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "components",
        partition: TablePartition::Durable,
        is_virtual: false,
    },
    TableMeta {
        name: "semantic_anchors",
        partition: TablePartition::Durable,
        is_virtual: false,
    },
    TableMeta {
        name: "aliases",
        partition: TablePartition::Durable,
        is_virtual: false,
    },
    TableMeta {
        name: "component_events",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "component_membership",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "component_clustering_meta",
        partition: TablePartition::Durable,
        is_virtual: false,
    },
    TableMeta {
        name: "constraint_waivers",
        partition: TablePartition::Durable,
        is_virtual: false,
    },
    TableMeta {
        name: "constraint_ratchets",
        partition: TablePartition::Durable,
        is_virtual: false,
    },
    TableMeta {
        name: "commits",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "commit_files",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "health_findings",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "health_coverage",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "health_waivers",
        partition: TablePartition::Durable,
        is_virtual: false,
    },
    TableMeta {
        name: "hrr_vectors",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "hrr_file_hashes",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "pattern_families",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "pattern_family_members",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "health_snapshot_files",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "health_snapshot_components",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "health_runs",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "health_current",
        partition: TablePartition::Ephemeral,
        is_virtual: false,
    },
    TableMeta {
        name: "index_meta",
        partition: TablePartition::Durable,
        is_virtual: false,
    },
    TableMeta {
        name: "entity_commits",
        partition: TablePartition::Durable,
        is_virtual: false,
    },
    TableMeta {
        name: "entity_changes",
        partition: TablePartition::Durable,
        is_virtual: false,
    },
];

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: i64,
    pub path: Arc<str>,
    pub language: String,
    pub content_hash: String,
    pub line_count: i64,
    pub parsed_ok: bool,
    pub last_parsed: String,
    pub fan_in_files: i64,
    pub blast_radius: i64,
    pub pagerank: Option<f64>,
    /// File mtime (ns since epoch) captured at last parse, or `None` for rows
    /// predating the mtime baseline (sutra/324). Used only to skip the read+hash
    /// of unchanged files on a frozen-workspace reparse.
    pub mtime_ns: Option<i64>,
    /// File size in bytes captured at last parse, or `None` for rows predating
    /// the size baseline (sutra/362). Paired with `mtime_ns` as the (size, mtime)
    /// fast path of the freshness drift probe.
    pub size_bytes: Option<i64>,
}

/// The subset of a file's indexed state the freshness drift probe compares
/// against disk: the `(size_bytes, mtime_ns)` fast-path baseline and the
/// `content_hash` confirm (sutra/362).
#[derive(Debug, Clone)]
pub struct FileFingerprint {
    pub path: String,
    pub size_bytes: Option<i64>,
    pub mtime_ns: Option<i64>,
    pub content_hash: String,
}

/// An import row the resolver could not point at a workspace file — the input
/// for external-crate constraint checking. `is_test` lets a rule skip a crate
/// reached only from `#[cfg(test)]` code unless it sets `include_tests`
/// (sutra/294).
#[derive(Debug, Clone)]
pub struct UnresolvedImport {
    pub file_id: i64,
    pub path: String,
    pub language: String,
    pub imported_path: String,
    pub is_test: bool,
}

#[derive(Debug, Clone)]
pub struct SymbolEntry {
    pub id: i64,
    pub qualified_name: String,
    pub short_name: String,
    pub kind: String,
    pub parent_symbol_id: Option<i64>,
    pub file_id: i64,
}

#[derive(Debug, Clone)]
pub struct SymbolRow {
    pub id: i64,
    pub file_id: i64,
    pub qualified_name: Arc<str>,
    pub short_name: Arc<str>,
    pub kind: Arc<str>,
    pub signature: Option<String>,
    pub signature_hash: Option<String>,
    pub structural_hash: Option<String>,
    pub visibility: Option<String>,
    pub start_line: i64,
    pub start_col: i64,
    pub end_line: i64,
    pub end_col: i64,
    pub parent_symbol_id: Option<i64>,
    pub docstring: Option<String>,
    pub pagerank: Option<f64>,
    pub cyclomatic: Option<i64>,
    pub cognitive: Option<i64>,
    pub max_nesting: Option<i64>,
    pub flags: i64,
    pub language_attrs: Option<String>,
}

pub struct InsertSymbolParams<'a> {
    pub file_id: i64,
    pub qualified_name: &'a str,
    pub short_name: &'a str,
    pub kind: &'a str,
    pub signature: Option<&'a str>,
    pub signature_hash: Option<&'a str>,
    pub structural_hash: Option<&'a str>,
    pub visibility: Option<&'a str>,
    pub start_line: i64,
    pub start_col: i64,
    pub end_line: i64,
    pub end_col: i64,
    pub parent_symbol_id: Option<i64>,
    pub docstring: Option<&'a str>,
    pub cyclomatic: Option<i64>,
    pub cognitive: Option<i64>,
    pub max_nesting: Option<i64>,
    pub flags: i64,
    pub language_attrs: Option<&'a str>,
}

pub struct InsertImportParams<'a> {
    pub imported_path: &'a str,
    pub line: i64,
    pub kind: &'a str,
    pub alias: Option<&'a str>,
    pub is_test: bool,
}

pub struct InsertRefParams<'a> {
    pub unresolved_name: Option<&'a str>,
    pub line: i64,
    pub col: i64,
    pub context_kind: &'a str,
    pub resolved_local_target: Option<&'a str>,
    pub receiver: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct RefRow {
    pub id: i64,
    pub file_id: i64,
    pub target_symbol_id: Option<i64>,
    pub unresolved_name: Option<String>,
    pub line: i64,
    pub col: i64,
    pub context_kind: String,
    pub resolved_local_target: Option<String>,
    pub receiver: Option<String>,
}

pub struct ResolvedRefRow<'a> {
    pub target_symbol_id: Option<i64>,
    pub unresolved_name: Option<&'a str>,
    pub line: i64,
    pub col: i64,
    pub context_kind: &'a str,
    pub resolution_method: Option<&'a str>,
    pub resolved_local_target: Option<&'a str>,
    pub receiver: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct ImportRow {
    pub id: i64,
    pub file_id: i64,
    pub imported_path: String,
    pub resolved_file_id: Option<i64>,
    pub line: i64,
    pub kind: String,
    pub alias: Option<String>,
    /// Import sits in test-only code (Rust `#[cfg(test)]`).
    pub is_test: bool,
}

#[derive(Debug, Clone)]
pub struct ComponentRow {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
    pub dissolved_at: Option<String>,
    pub prior_paths: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AnchorRow {
    pub id: String,
    pub component_id: String,
    pub symbol_name: String,
    pub score: Option<f64>,
    pub rationale: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AliasRow {
    pub id: String,
    pub term: String,
    /// Trailing segment after '/' for namespaced terms, else `None`.
    pub short_name: Option<String>,
    pub target_kind: String,
    pub target_ref: String,
}

#[derive(Debug, Clone)]
pub struct SnapshotRow {
    pub id: i64,
    pub timestamp: String,
    pub files_parsed: i64,
    pub symbols_extracted: i64,
    pub refs_extracted: i64,
    pub parse_errors: i64,
    pub duration_ms: i64,
    pub total_complexity: i64,
    pub dead_symbol_count: i64,
    pub hotspot_count: i64,
    pub health_score: f64,
    pub pattern_family_count: i64,
    /// The health run this checkpoint's health rows were scored from
    /// (sutra/416). `None` on legacy checkpoints.
    pub health_run_id: Option<i64>,
}

pub struct CommitRow {
    pub hash: String,
    pub committed_at: i64,
    pub author: String,
}

#[derive(Default)]
pub struct SnapshotParams {
    pub files_parsed: i64,
    pub symbols_extracted: i64,
    pub refs_extracted: i64,
    pub parse_errors: i64,
    pub duration_ms: i64,
    pub total_complexity: i64,
    pub dead_symbol_count: i64,
    pub hotspot_count: i64,
    pub health_score: f64,
    pub pattern_family_count: i64,
    pub head_commit: Option<String>,
    /// Pre-parse timestamp for freshness watermark. When set, used instead
    /// of insert-time so edits during parsing aren't hidden.
    pub timestamp: Option<String>,
    /// Provenance: the health run the checkpoint was scored from.
    pub health_run_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct SnapshotFileRow {
    pub file_id: i64,
    pub file_path: String,
    pub score: f64,
    pub category_scores: String,
    /// Whether this score was computed from complete analysis (sutra/408), or
    /// whether that is unknown because the row predates recording it (sutra/418).
    /// Lets trend tell partial analysis apart from real degradation.
    pub completeness: SnapshotCompleteness,
    /// Names of the missing producers behind a `Partial` score.
    pub missing_biomarkers: Vec<String>,
    /// Optimistic bound of a `Partial` score (`score` is the conservative lower
    /// bound). `None` when measured, or on rows predating sutra/416.
    pub score_upper: Option<f64>,
    /// Hex [`crate::health::scoring::file_score_basis`] digest. `None` = Unknown
    /// basis (every row before sutra/416): never matches, so never measured.
    pub score_basis: Option<String>,
}

/// Persisted completeness of one per-file snapshot score.
///
/// Stored as `partial` + `completeness_recorded` on `health_snapshot_files`.
/// `Unknown` is a first-class state, not a synonym for `Complete`: rows written
/// before completeness was recorded carry a defaulted `partial = 0` that proves
/// nothing (health-evidence-contract.md § Migration), so they read as `Unknown`
/// and are never backfilled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotCompleteness {
    /// Every file-scored biomarker was measured.
    Complete,
    /// Some biomarker was worst-cased (see `missing_biomarkers`).
    Partial,
    /// Completeness was never recorded for this row (legacy observation).
    Unknown,
}

impl SnapshotCompleteness {
    /// Wire name used in tool output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Unknown => "unknown",
        }
    }

    /// `(partial, completeness_recorded)` column values.
    fn to_columns(self) -> (i64, i64) {
        match self {
            Self::Complete => (0, 1),
            Self::Partial => (1, 1),
            Self::Unknown => (0, 0),
        }
    }

    fn from_columns(partial: i64, recorded: i64) -> Self {
        match (recorded != 0, partial != 0) {
            (false, _) => Self::Unknown,
            (true, true) => Self::Partial,
            (true, false) => Self::Complete,
        }
    }
}

/// One entry of a file's snapshot history, newest-first (`Db::file_health_history`).
#[derive(Debug, Clone)]
pub struct FileHealthHistoryRow {
    pub timestamp: String,
    pub score: f64,
    pub category_scores: String,
    pub completeness: SnapshotCompleteness,
    pub missing_biomarkers: Vec<String>,
    pub score_upper: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct SnapshotComponentRow {
    pub component_id: String,
    pub component_name: String,
    /// Conservative (lower-bound) score when `Partial`.
    pub score: f64,
    pub member_count: i64,
    pub total_nloc: i64,
    /// `Complete` only when every member file was measured (sutra/416);
    /// `Unknown` on rows that never recorded it.
    pub completeness: SnapshotCompleteness,
    /// Digest over membership, member bases and the aggregation rule. `None` =
    /// Unknown (legacy): a component delta is never measured against it.
    pub score_basis: Option<String>,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ResolveResult {
    Unique(SymbolRow),
    Ambiguous(Vec<SymbolRow>),
    NotFound,
}

// ---------------------------------------------------------------------------
// Db
// ---------------------------------------------------------------------------

pub struct Db {
    conn: Mutex<Connection>,
    workspace_id: String,
    /// Cached query-time symbol wiring graph (sutra/372), tagged with the
    /// `data_generation` it was built at. Rebuilt lazily when the generation
    /// moves — `replace_file_data` bumps it on every write (full or
    /// incremental), so an edit that changes symbols/refs invalidates the graph
    /// without a snapshot (incremental reparse records none; see
    /// docs/freshness-map.md).
    symbol_graph: Mutex<Option<(i64, Arc<crate::graph::SymbolGraph>)>>,
}

impl Db {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    /// Open the SQLite database for a registered workspace after validating
    /// that the configured DB directory cannot place the index inside the
    /// workspace root.
    pub fn open_for_workspace(workspace: &WorkspaceEntry, db_dir: &Path) -> Result<Self> {
        workspace::validate_db_dir_for_workspace(db_dir, workspace)?;
        Self::open_unchecked(&workspace.id, db_dir)
    }

    /// Open the SQLite database at `db_dir/<workspace_id>/index.db` without
    /// workspace-root placement validation.
    ///
    /// Production code should use `open_for_workspace` so unsafe DB placement
    /// is rejected before SQLite creates files.
    #[doc(hidden)]
    pub fn open_unchecked(workspace_id: &str, db_dir: &Path) -> Result<Self> {
        let dir = db_dir.join(workspace_id);
        std::fs::create_dir_all(&dir).map_err(|e| {
            SutraError::Internal(format!(
                "could not create database directory {}: {e}",
                dir.display()
            ))
        })?;

        let db_path = dir.join("index.db");
        let conn = Connection::open(&db_path)?;

        // PRAGMAs — WAL first (must precede others on a fresh connection).
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA synchronous = NORMAL;\
             PRAGMA foreign_keys = ON;\
             PRAGMA busy_timeout = 5000;",
        )?;

        let db = Self {
            conn: Mutex::new(conn),
            workspace_id: workspace_id.to_string(),
            symbol_graph: Mutex::new(None),
        };
        db.run_migrations()?;
        let healed = db.heal_duplicate_symbols()?;
        if healed > 0 {
            tracing::warn!(
                workspace_id,
                healed,
                "healed duplicate symbol rows at startup"
            );
        }
        let rebuilt = db.rebuild_symbols_fts_if_stale()?;
        if rebuilt > 0 {
            tracing::info!(
                workspace_id,
                rebuilt,
                "repopulated symbols_fts (lex_tokens) after schema upgrade"
            );
        }
        Ok(db)
    }

    /// After a schema migration recreates the (ephemeral) `symbols_fts` table
    /// empty — as 0071 does to add the `lex_tokens` column — repopulate it from
    /// the durable `symbols` rows, including the Rust-tokenized `lex_tokens`
    /// blob that pure SQL can't compute (sutra/394). Gated on a row-count
    /// mismatch so it fires exactly once after the upgrade and is a no-op on a
    /// healthy index or a fresh/reindexed one (where symbols is empty at open
    /// time and the parse-time insert sites fill every column). This is strictly
    /// cheaper than the `content_hash=''` forced reparse of 0054/0055: it reads
    /// only the symbols table — no file I/O, no re-parse — because every
    /// `lex_tokens` input already lives durably in `symbols`.
    fn rebuild_symbols_fts_if_stale(&self) -> Result<usize> {
        let conn = self.conn.lock();
        let symbol_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))?;
        let fts_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM symbols_fts", [], |row| row.get(0))?;
        if symbol_count == fts_count {
            return Ok(0);
        }

        let tx = conn.unchecked_transaction()?;
        conn.execute("DELETE FROM symbols_fts", [])?;
        {
            let mut select = conn.prepare(
                "SELECT id, short_name, qualified_name, docstring, signature FROM symbols",
            )?;
            let mut insert = conn.prepare(
                "INSERT INTO symbols_fts \
                 (symbol_id, short_name, qualified_name, docstring, signature, lex_tokens) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            let rows = select.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })?;
            for row in rows {
                let (id, short_name, qualified_name, docstring, signature) = row?;
                let blob = lex_tokens_blob(
                    &short_name,
                    &qualified_name,
                    docstring.as_deref(),
                    signature.as_deref(),
                );
                insert.execute(params![
                    id,
                    short_name,
                    qualified_name,
                    docstring,
                    signature,
                    blob
                ])?;
            }
        }
        tx.commit()?;
        Ok((symbol_count - fts_count).max(0) as usize)
    }

    fn heal_duplicate_symbols(&self) -> Result<usize> {
        let conn = self.conn.lock();
        let dup_count: usize = conn.query_row(
            "SELECT COUNT(*) FROM symbols s
             WHERE EXISTS (
                 SELECT 1 FROM symbols s2
                 WHERE s2.file_id = s.file_id
                   AND s2.qualified_name = s.qualified_name
                   AND s2.start_line = s.start_line
                   AND s2.id < s.id
             )",
            [],
            |row| row.get(0),
        )?;
        if dup_count == 0 {
            return Ok(0);
        }
        let tx = conn.unchecked_transaction()?;
        conn.execute(
            "DELETE FROM symbols_fts WHERE symbol_id IN (
                 SELECT s.id FROM symbols s
                 WHERE EXISTS (
                     SELECT 1 FROM symbols s2
                     WHERE s2.file_id = s.file_id
                       AND s2.qualified_name = s.qualified_name
                       AND s2.start_line = s.start_line
                       AND s2.id < s.id
                 )
             )",
            [],
        )?;
        conn.execute(
            "DELETE FROM symbols WHERE id IN (
                 SELECT s.id FROM symbols s
                 WHERE EXISTS (
                     SELECT 1 FROM symbols s2
                     WHERE s2.file_id = s.file_id
                       AND s2.qualified_name = s.qualified_name
                       AND s2.start_line = s.start_line
                       AND s2.id < s.id
                 )
             )",
            [],
        )?;
        tx.commit()?;
        Ok(dup_count)
    }

    // -----------------------------------------------------------------------
    // Public accessor
    // -----------------------------------------------------------------------

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    #[doc(hidden)]
    pub fn conn_for_test(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    // -----------------------------------------------------------------------
    // Reindex
    // -----------------------------------------------------------------------

    pub fn reindex(&self) -> Result<Vec<&'static str>> {
        let conn = self.conn.lock();

        let ephemeral: Vec<&TableMeta> = TABLE_REGISTRY
            .iter()
            .filter(|t| t.partition == TablePartition::Ephemeral)
            .collect();

        for meta in ephemeral.iter().rev() {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {}", meta.name))?;
        }

        for name in Self::ephemeral_migration_names() {
            conn.execute(
                "DELETE FROM schema_migrations WHERE name = ?1",
                params![name],
            )?;
        }

        drop(conn);

        self.run_migrations()?;

        // Reset generation counters — all ephemeral data was wiped. Also clear
        // the index epoch (sutra/414): a full reindex is a new index lifetime, so
        // the next Db::ensure_index_epoch mints a fresh epoch and prior retained
        // evidence (already dropped with health_runs) is never re-resolved against
        // the replacement extraction.
        self.conn.lock().execute(
            "UPDATE index_meta
             SET data_generation = 0, derived_complete_generation = 0, index_epoch = NULL
             WHERE id = 1",
            [],
        )?;

        let names: Vec<&'static str> = ephemeral.iter().map(|t| t.name).collect();
        Ok(names)
    }

    // -----------------------------------------------------------------------
    // files
    // -----------------------------------------------------------------------

    /// Upsert a file row. Returns the id of the inserted/replaced row.
    pub fn upsert_file(
        &self,
        path: &str,
        language: &str,
        content_hash: &str,
        line_count: i64,
        parsed_ok: bool,
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO files (path, language, content_hash, line_count, parsed_ok, last_parsed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET
                language     = excluded.language,
                content_hash = excluded.content_hash,
                line_count   = excluded.line_count,
                parsed_ok    = excluded.parsed_ok,
                last_parsed  = excluded.last_parsed",
            params![
                path,
                language,
                content_hash,
                line_count,
                parsed_ok as i64,
                now
            ],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![path],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Fetch a single file row by id.
    pub fn file_by_id(&self, id: i64) -> Result<Option<FileRow>> {
        let conn = self.conn.lock();
        match conn.query_row(
            "SELECT id, path, language, content_hash, line_count, parsed_ok,
                    last_parsed, fan_in_files, blast_radius, pagerank, mtime_ns, size_bytes
             FROM files WHERE id = ?1",
            params![id],
            map_file_row,
        ) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(SutraError::Db(e)),
        }
    }

    /// Bulk-fetch file rows for a set of ids in a single query, keyed by id.
    /// Ids with no matching row are absent from the map. Lets callers hydrate a
    /// bounded candidate set in one round trip instead of a per-node `file_by_id`
    /// fan-out (sutra/395).
    pub fn files_by_ids(&self, ids: &[i64]) -> Result<std::collections::HashMap<i64, FileRow>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let conn = self.conn.lock();
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, path, language, content_hash, line_count, parsed_ok,
                    last_parsed, fan_in_files, blast_radius, pagerank, mtime_ns, size_bytes
             FROM files WHERE id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), map_file_row)?;
        let mut out = std::collections::HashMap::with_capacity(ids.len());
        for r in rows {
            let f = r?;
            out.insert(f.id, f);
        }
        Ok(out)
    }

    /// Fetch a single file row by path.
    pub fn file_by_path(&self, path: &str) -> Result<Option<FileRow>> {
        let conn = self.conn.lock();
        match conn.query_row(
            "SELECT id, path, language, content_hash, line_count, parsed_ok,
                    last_parsed, fan_in_files, blast_radius, pagerank, mtime_ns, size_bytes
             FROM files WHERE path = ?1",
            params![path],
            map_file_row,
        ) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(SutraError::Db(e)),
        }
    }

    /// Return all file rows ordered by blast_radius DESC (a rough proxy for
    /// symbol count / importance until we have real PageRank).
    pub fn all_files(&self) -> Result<Vec<FileRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, path, language, content_hash, line_count, parsed_ok,
                    last_parsed, fan_in_files, blast_radius, pagerank, mtime_ns, size_bytes
             FROM files
             ORDER BY blast_radius DESC",
        )?;
        let rows: rusqlite::Result<Vec<FileRow>> = stmt.query_map([], map_file_row)?.collect();
        Ok(rows?)
    }

    /// Delete a file row. Foreign-key cascades remove symbols and refs.
    /// FTS5 rows must be deleted manually first.
    pub fn delete_file_cascade(&self, file_id: i64) -> Result<()> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        {
            // Manual FTS5 sync: delete every symbol FTS row for this file.
            let symbol_ids: Vec<i64> = {
                let mut stmt = conn.prepare("SELECT id FROM symbols WHERE file_id = ?1")?;
                let ids: rusqlite::Result<Vec<i64>> = stmt
                    .query_map(params![file_id], |row| row.get(0))?
                    .collect();
                ids?
            };

            for sid in &symbol_ids {
                conn.execute("DELETE FROM symbols_fts WHERE symbol_id = ?1", params![sid])?;
            }

            // Mark dependent files before cascade deletes destroy the evidence.
            conn.execute(
                "UPDATE files SET needs_resolution = 1
                 WHERE id != ?1 AND id IN (
                     SELECT DISTINCT file_id FROM refs
                     WHERE target_symbol_id IN (SELECT id FROM symbols WHERE file_id = ?1)
                 )",
                params![file_id],
            )?;

            // Clear resolved_file_id on imports that target this file,
            // so the FK on resolved_file_id doesn't block the delete.
            conn.execute(
                "UPDATE imports SET resolved_file_id = NULL WHERE resolved_file_id = ?1",
                params![file_id],
            )?;

            // Delete the file; FK cascades handle symbols and refs.
            conn.execute("DELETE FROM files WHERE id = ?1", params![file_id])?;

            conn.execute(
                "UPDATE index_meta SET data_generation = data_generation + 1 WHERE id = 1",
                [],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Atomically replace all data for a file in a single transaction: keep the
    /// existing `files` row (stable id) when the path is already indexed,
    /// replace its extraction children (symbols/imports/refs/FTS), invalidate
    /// its extraction-derived analysis, and re-insert the new symbols/imports/
    /// refs. Raw history (`commit_files`) and immutable snapshots are preserved
    /// across a content edit; only `delete_file_cascade` removes a file. See the
    /// child-table lifecycle audit inline below (sutra/413).
    /// Returns `(file_id, symbols_inserted)`.
    #[allow(clippy::too_many_arguments)]
    pub fn replace_file_data(
        &self,
        path: &str,
        language: &str,
        content_hash: &str,
        line_count: i64,
        parsed_ok: bool,
        mtime_ns: Option<i64>,
        size_bytes: Option<i64>,
        symbols: &[InsertSymbolParams<'_>],
        parent_indices: &[Option<usize>],
        imports: &[InsertImportParams<'_>],
        refs: &[InsertRefParams<'_>],
    ) -> Result<(i64, i64)> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        let tx = batch_aware_transaction(&conn)?;

        // Delete old file data if it exists.
        let old_file_id: Option<i64> = match conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![path],
            |row| row.get(0),
        ) {
            Ok(id) => Some(id),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(SutraError::Db(e)),
        };
        if let Some(old_id) = old_file_id {
            // Content replacement, NOT deletion: keep this `files` row (and its
            // stable id) so raw history and other file-keyed immutable evidence
            // survive the edit. Replace the extraction children, invalidate the
            // extraction-derived analysis, and leave `commit_files` untouched.
            // Deleting the row instead cascaded away `commit_files` (raw git
            // history) and churned the id, orphaning history and derived
            // evidence on every incremental reparse (sutra/413). Actual file
            // removal stays in `delete_file_cascade`.
            //
            // Child-table lifecycle audit (every table FK'd to files/symbols):
            //   REPLACE (extraction):  symbols, refs (outgoing), imports,
            //                          symbols_fts.
            //   INVALIDATE (derived):  health_findings, health_coverage,
            //                          hrr_file_hashes, plus hrr_vectors and
            //                          pattern_family_members (cascade off the
            //                          symbols delete).
            //   PRESERVE (raw/history): commit_files, and health_snapshot_files
            //                          (no FK; immutable path-keyed snapshots).
            //   PRESERVE (global partition): component_membership. It groups
            //                          stable file ids by the whole graph; its
            //                          freshness is the clustering gate's
            //                          edge-drift threshold, like any neighbour
            //                          whose edges moved. Deleting the row made no
            //                          grouping stale — the gate still read
            //                          current, so the edited file silently left
            //                          its component (sutra/439).
            //   RE-RESOLVE (resolution): inbound refs from other files (detached
            //                          below); inbound import edges keep their
            //                          resolved_file_id — path identity is stable
            //                          so those edges remain correct.

            // Mark dependent files before the symbol replace destroys the evidence.
            conn.execute(
                "UPDATE files SET needs_resolution = 1
                 WHERE id != ?1 AND id IN (
                     SELECT DISTINCT file_id FROM refs
                     WHERE target_symbol_id IN (SELECT id FROM symbols WHERE file_id = ?1)
                 )",
                params![old_id],
            )?;

            // Detach inbound references from OTHER files before deleting this
            // file's symbols. A resolved ref stores no call-site name
            // (unresolved_name is cleared on resolution), so recover it from the
            // target symbol's short_name; nulling the target keeps the row alive
            // through `refs.target_symbol_id ON DELETE CASCADE` so post-parse
            // resolution can re-link it to this file's new symbols. Without this
            // an edit to this file silently drops resolved inbound edges from
            // unchanged caller files (sutra/378). SQLite evaluates every SET
            // expression against the pre-update row, so the COALESCE subquery
            // still sees the old target_symbol_id.
            conn.execute(
                "UPDATE refs
                    SET unresolved_name = COALESCE(
                            unresolved_name,
                            (SELECT short_name FROM symbols WHERE symbols.id = refs.target_symbol_id)
                        ),
                        target_symbol_id = NULL
                  WHERE file_id != ?1
                    AND target_symbol_id IN (SELECT id FROM symbols WHERE file_id = ?1)",
                params![old_id],
            )?;

            // Manual FTS5 sync: drop this file's symbol FTS rows before the
            // symbols themselves go (symbols_fts has no FK to cascade).
            let symbol_ids: Vec<i64> = {
                let mut stmt = conn.prepare("SELECT id FROM symbols WHERE file_id = ?1")?;
                let ids: rusqlite::Result<Vec<i64>> =
                    stmt.query_map(params![old_id], |row| row.get(0))?.collect();
                ids?
            };
            for sid in &symbol_ids {
                conn.execute("DELETE FROM symbols_fts WHERE symbol_id = ?1", params![sid])?;
            }

            // Replace extraction children. Deleting the symbols cascades their
            // dependent derived rows (hrr_vectors, pattern_family_members, and
            // symbol-level health_findings). Refs and imports are keyed by
            // file_id and replaced directly.
            conn.execute("DELETE FROM symbols WHERE file_id = ?1", params![old_id])?;
            conn.execute("DELETE FROM refs WHERE file_id = ?1", params![old_id])?;
            conn.execute("DELETE FROM imports WHERE file_id = ?1", params![old_id])?;

            // Invalidate extraction-derived analysis that is keyed by file_id and
            // therefore survives the symbol cascade. Preserving the file id must
            // not let stale findings/coverage/similarity claim they reflect the
            // new content (health-evidence contract, sutra/412).
            conn.execute(
                "DELETE FROM health_findings WHERE file_id = ?1",
                params![old_id],
            )?;
            conn.execute(
                "DELETE FROM health_coverage WHERE file_id = ?1",
                params![old_id],
            )?;
            conn.execute(
                "DELETE FROM hrr_file_hashes WHERE file_id = ?1",
                params![old_id],
            )?;
        }

        // Upsert the file row (marks needs_resolution for post-parse ref resolution).
        conn.execute(
            "INSERT INTO files (path, language, content_hash, line_count, parsed_ok, last_parsed, mtime_ns, size_bytes, needs_resolution)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1)
             ON CONFLICT(path) DO UPDATE SET
                language         = excluded.language,
                content_hash     = excluded.content_hash,
                line_count       = excluded.line_count,
                parsed_ok        = excluded.parsed_ok,
                last_parsed      = excluded.last_parsed,
                mtime_ns         = excluded.mtime_ns,
                size_bytes       = excluded.size_bytes,
                needs_resolution = 1",
            params![
                path,
                language,
                content_hash,
                line_count,
                parsed_ok as i64,
                now,
                mtime_ns,
                size_bytes
            ],
        )?;
        let file_id: i64 = conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![path],
            |row| row.get(0),
        )?;

        // Insert symbols, tracking generated IDs for parent mapping.
        let mut symbol_ids: Vec<i64> = Vec::with_capacity(symbols.len());
        for (i, p) in symbols.iter().enumerate() {
            let parent_symbol_id = parent_indices[i].map(|pi| symbol_ids[pi]);
            let id: i64 = conn.prepare_cached(
                "INSERT INTO symbols (
                    file_id, qualified_name, short_name, kind,
                    signature, signature_hash, structural_hash, visibility,
                    start_line, start_col, end_line, end_col,
                    parent_symbol_id, docstring, cyclomatic, cognitive, max_nesting, flags,
                    language_attrs
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
                 ON CONFLICT(file_id, qualified_name, start_line) DO UPDATE SET
                    short_name = excluded.short_name,
                    kind = excluded.kind,
                    signature = excluded.signature,
                    signature_hash = excluded.signature_hash,
                    structural_hash = excluded.structural_hash,
                    visibility = excluded.visibility,
                    start_col = excluded.start_col,
                    end_line = excluded.end_line,
                    end_col = excluded.end_col,
                    parent_symbol_id = excluded.parent_symbol_id,
                    docstring = excluded.docstring,
                    cyclomatic = excluded.cyclomatic,
                    cognitive = excluded.cognitive,
                    max_nesting = excluded.max_nesting,
                    flags = excluded.flags,
                    language_attrs = excluded.language_attrs
                 RETURNING id",
            )?.query_row(
                params![
                    file_id,
                    p.qualified_name,
                    p.short_name,
                    p.kind,
                    p.signature,
                    p.signature_hash,
                    p.structural_hash,
                    p.visibility,
                    p.start_line,
                    p.start_col,
                    p.end_line,
                    p.end_col,
                    parent_symbol_id,
                    p.docstring,
                    p.cyclomatic,
                    p.cognitive,
                    p.max_nesting,
                    p.flags,
                    p.language_attrs,
                ],
                |row| row.get(0),
            )?;
            symbol_ids.push(id);

            conn.prepare_cached("DELETE FROM symbols_fts WHERE symbol_id = ?1")?
                .execute(params![id])?;
            conn.prepare_cached(
                "INSERT INTO symbols_fts (symbol_id, short_name, qualified_name, docstring, signature, lex_tokens)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                id,
                p.short_name,
                p.qualified_name,
                p.docstring,
                p.signature,
                lex_tokens_blob(p.short_name, p.qualified_name, p.docstring, p.signature),
            ])?;
        }

        // Insert imports.
        for imp in imports {
            conn.prepare_cached(
                "INSERT INTO imports (file_id, imported_path, resolved_file_id, line, kind, alias, is_test)
                 VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                file_id,
                imp.imported_path,
                imp.line,
                imp.kind,
                imp.alias,
                imp.is_test
            ])?;
        }

        // Insert refs.
        for rf in refs {
            conn.prepare_cached(
                "INSERT INTO refs (file_id, target_symbol_id, unresolved_name, line, col, context_kind, resolved_local_target, receiver)
                 VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                file_id,
                rf.unresolved_name,
                rf.line,
                rf.col,
                rf.context_kind,
                rf.resolved_local_target,
                rf.receiver
            ])?;
        }

        conn.execute(
            "UPDATE index_meta SET data_generation = data_generation + 1 WHERE id = 1",
            [],
        )?;

        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok((file_id, symbol_ids.len() as i64))
    }

    // -----------------------------------------------------------------------
    // symbols
    // -----------------------------------------------------------------------

    /// Test-only fixture primitive (no live callers): live symbol writes go
    /// through `replace_file_data`, so this deliberately does not bump
    /// `data_generation` (the `symbol_graph` cache token, sutra/396). Wiring it
    /// into a live path would need that bump added — in a transaction.
    pub fn insert_symbol(&self, p: &InsertSymbolParams<'_>) -> Result<i64> {
        let conn = self.conn.lock();
        let id: i64 = conn.query_row(
            "INSERT INTO symbols (
                file_id, qualified_name, short_name, kind,
                signature, signature_hash, structural_hash, visibility,
                start_line, start_col, end_line, end_col,
                parent_symbol_id, docstring, cyclomatic, cognitive, max_nesting, flags,
                language_attrs
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
             ON CONFLICT(file_id, qualified_name, start_line) DO UPDATE SET
                short_name = excluded.short_name,
                kind = excluded.kind,
                signature = excluded.signature,
                signature_hash = excluded.signature_hash,
                structural_hash = excluded.structural_hash,
                visibility = excluded.visibility,
                start_col = excluded.start_col,
                end_line = excluded.end_line,
                end_col = excluded.end_col,
                parent_symbol_id = excluded.parent_symbol_id,
                docstring = excluded.docstring,
                cyclomatic = excluded.cyclomatic,
                cognitive = excluded.cognitive,
                max_nesting = excluded.max_nesting,
                flags = excluded.flags,
                language_attrs = excluded.language_attrs
             RETURNING id",
            params![
                p.file_id,
                p.qualified_name,
                p.short_name,
                p.kind,
                p.signature,
                p.signature_hash,
                p.structural_hash,
                p.visibility,
                p.start_line,
                p.start_col,
                p.end_line,
                p.end_col,
                p.parent_symbol_id,
                p.docstring,
                p.cyclomatic,
                p.cognitive,
                p.max_nesting,
                p.flags,
                p.language_attrs,
            ],
            |row| row.get(0),
        )?;

        conn.execute("DELETE FROM symbols_fts WHERE symbol_id = ?1", params![id])?;
        conn.execute(
            "INSERT INTO symbols_fts (symbol_id, short_name, qualified_name, docstring, signature, lex_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                p.short_name,
                p.qualified_name,
                p.docstring,
                p.signature,
                lex_tokens_blob(p.short_name, p.qualified_name, p.docstring, p.signature),
            ],
        )?;

        Ok(id)
    }

    /// Fetch a symbol by its id.
    pub fn symbol_by_id(&self, id: i64) -> Result<Option<SymbolRow>> {
        let conn = self.conn.lock();
        match conn.query_row(
            "SELECT id, file_id, qualified_name, short_name, kind,
                    signature, signature_hash, structural_hash, visibility,
                    start_line, start_col, end_line, end_col,
                    parent_symbol_id, docstring, pagerank,
                    cyclomatic, cognitive, max_nesting, flags, language_attrs
             FROM symbols WHERE id = ?1",
            params![id],
            map_symbol_row,
        ) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(SutraError::Db(e)),
        }
    }

    /// Bulk-fetch full symbol rows for a set of ids in a single query. Ids with
    /// no matching row are omitted; result order is unspecified. Lets callers
    /// hydrate a bounded candidate set in one round trip instead of a per-node
    /// `symbol_by_id` fan-out (sutra/395). Distinct from `symbols_by_ids`, which
    /// returns lightweight `SymbolSummary` rows for the similarity path.
    pub fn symbol_rows_by_ids(&self, ids: &[i64]) -> Result<Vec<SymbolRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock();
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, file_id, qualified_name, short_name, kind,
                    signature, signature_hash, structural_hash, visibility,
                    start_line, start_col, end_line, end_col,
                    parent_symbol_id, docstring, pagerank,
                    cyclomatic, cognitive, max_nesting, flags, language_attrs
             FROM symbols WHERE id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(ids.iter()), map_symbol_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Fetch a symbol by its fully qualified name.
    pub fn symbol_by_qualified_name(&self, name: &str) -> Result<Option<SymbolRow>> {
        let conn = self.conn.lock();
        match conn.query_row(
            "SELECT id, file_id, qualified_name, short_name, kind,
                    signature, signature_hash, structural_hash, visibility,
                    start_line, start_col, end_line, end_col,
                    parent_symbol_id, docstring, pagerank,
                    cyclomatic, cognitive, max_nesting, flags, language_attrs
             FROM symbols WHERE qualified_name = ?1",
            params![name],
            map_symbol_row,
        ) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(SutraError::Db(e)),
        }
    }

    /// Find symbols by name. Tries an exact match on `short_name` first;
    /// falls back to an FTS5 fuzzy match. Optionally filters by `kind`.
    /// Returns at most `limit` results.
    pub fn find_symbols_by_name(
        &self,
        name: &str,
        kind_filter: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SymbolRow>> {
        self.find_symbols_by_name_tiered(name, kind_filter, limit)
            .map(|(rows, _tier)| rows)
    }

    /// Like `find_symbols_by_name` but also returns which search tier matched.
    pub fn find_symbols_by_name_tiered(
        &self,
        name: &str,
        kind_filter: Option<&str>,
        limit: i64,
    ) -> Result<(Vec<SymbolRow>, SearchTier)> {
        let conn = self.conn.lock();

        // Exact short_name match.
        let exact: Vec<SymbolRow> = {
            let mut stmt = match kind_filter {
                Some(_) => conn.prepare(
                    "SELECT id, file_id, qualified_name, short_name, kind,
                            signature, signature_hash, structural_hash, visibility,
                            start_line, start_col, end_line, end_col,
                            parent_symbol_id, docstring, pagerank,
                            cyclomatic, cognitive, max_nesting, flags, language_attrs
                     FROM symbols
                     WHERE short_name = ?1 AND kind = ?2
                     LIMIT ?3",
                )?,
                None => conn.prepare(
                    "SELECT id, file_id, qualified_name, short_name, kind,
                            signature, signature_hash, structural_hash, visibility,
                            start_line, start_col, end_line, end_col,
                            parent_symbol_id, docstring, pagerank,
                            cyclomatic, cognitive, max_nesting, flags, language_attrs
                     FROM symbols
                     WHERE short_name = ?1
                     LIMIT ?2",
                )?,
            };
            let rows: rusqlite::Result<Vec<SymbolRow>> = match kind_filter {
                Some(k) => stmt
                    .query_map(params![name, k, limit], map_symbol_row)?
                    .collect(),
                None => stmt
                    .query_map(params![name, limit], map_symbol_row)?
                    .collect(),
            };
            rows?
        };

        if !exact.is_empty() {
            return Ok((exact, SearchTier::Exact));
        }

        let escaped = name.replace('"', "\"\"");
        // Column-scope to the name/docstring columns this tier has always
        // searched. symbols_fts also indexes `signature` as of 0069, but that
        // column feeds explore's lexical stage only — an unscoped MATCH would
        // silently broaden find/lookup, turning a NAME search into one that
        // matches parameter types (sutra/371 refactor-contract guard).
        let fts_query = format!("{{short_name qualified_name docstring}} : \"{escaped}\"*");
        let ids: Vec<i64> = {
            let mut stmt = conn.prepare(
                "SELECT symbol_id FROM symbols_fts
                 WHERE symbols_fts MATCH ?1
                 LIMIT ?2",
            )?;
            let ids: rusqlite::Result<Vec<i64>> = stmt
                .query_map(params![fts_query, limit], |row| row.get(0))?
                .collect();
            ids?
        };

        if ids.is_empty() {
            return Ok((vec![], SearchTier::Fts));
        }

        // Fetch full rows for matched ids, respecting kind filter.
        let mut results = Vec::with_capacity(ids.len());
        for sid in ids {
            if let Some(sym) = {
                match conn.query_row(
                    "SELECT id, file_id, qualified_name, short_name, kind,
                            signature, signature_hash, structural_hash, visibility,
                            start_line, start_col, end_line, end_col,
                            parent_symbol_id, docstring, pagerank,
                            cyclomatic, cognitive, max_nesting, flags, language_attrs
                     FROM symbols WHERE id = ?1",
                    params![sid],
                    map_symbol_row,
                ) {
                    Ok(row) => Ok(Some(row)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(SutraError::Db(e)),
                }?
            } && kind_filter.is_none_or(|k| &*sym.kind == k)
            {
                results.push(sym);
            }
        }
        Ok((results, SearchTier::Fts))
    }

    /// Total indexed symbol count — the corpus size `N` for explore's IDF
    /// (sutra/371).
    pub fn symbol_count(&self) -> Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT count(*) FROM symbols", [], |row| row.get(0))?)
    }

    /// Document frequency of one lexical token: how many symbols carry a token
    /// with this prefix in any indexed FTS column (short_name / qualified_name /
    /// docstring / signature). Drives IDF in explore's lexical stage (sutra/371)
    /// — a token in every symbol weighs ~0, a rare one weighs heavily. Matched
    /// as an FTS5 prefix (`"tok"*`), the same rule the candidate retrieval and
    /// the in-process scorer use, so a query token's df reflects the tokens it
    /// will actually score against (e.g. `import` counts `imports`). The caller
    /// passes an already-tokenized word.
    pub fn fts_doc_frequency(&self, token: &str) -> Result<i64> {
        let conn = self.conn.lock();
        let escaped = token.replace('"', "\"\"");
        // Scope to lex_tokens — the camelCase/snake-split field explore retrieves
        // from — so the df matches what fts_candidates actually finds. Counting
        // the raw unicode61 columns would understate df for an interior token
        // (e.g. `context` in `RequestContext`), inflating its IDF (sutra/394).
        let match_expr = format!("{{lex_tokens}} : \"{escaped}\"*");
        Ok(conn.query_row(
            "SELECT count(*) FROM symbols_fts WHERE symbols_fts MATCH ?1",
            params![match_expr],
            |row| row.get(0),
        )?)
    }

    /// Full symbol rows for an arbitrary FTS5 MATCH expression, ordered by the
    /// FTS bm25 `rank` (best first) and capped at `limit`. explore's lexical
    /// candidate retrieval (sutra/371): the in-process scorer re-ranks these, so
    /// this is a recall net, not the final order.
    pub fn fts_candidates(&self, match_expr: &str, limit: i64) -> Result<Vec<SymbolRow>> {
        // Fetch ids under the lock, then release it before per-id fetches:
        // symbol_by_id locks the same non-reentrant mutex.
        let ids: Vec<i64> = {
            let conn = self.conn.lock();
            let mut stmt = conn.prepare(
                "SELECT symbol_id FROM symbols_fts
                 WHERE symbols_fts MATCH ?1
                 ORDER BY rank
                 LIMIT ?2",
            )?;
            let ids: rusqlite::Result<Vec<i64>> = stmt
                .query_map(params![match_expr, limit], |row| row.get(0))?
                .collect();
            ids?
        };
        let mut results = Vec::with_capacity(ids.len());
        for sid in ids {
            if let Some(sym) = self.symbol_by_id(sid)? {
                results.push(sym);
            }
        }
        Ok(results)
    }

    /// Return all symbols in a file ordered by start_line.
    pub fn find_symbols_by_file(&self, file_id: i64) -> Result<Vec<SymbolRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, file_id, qualified_name, short_name, kind,
                    signature, signature_hash, structural_hash, visibility,
                    start_line, start_col, end_line, end_col,
                    parent_symbol_id, docstring, pagerank,
                    cyclomatic, cognitive, max_nesting, flags, language_attrs
             FROM symbols
             WHERE file_id = ?1
             ORDER BY start_line",
        )?;
        let rows: rusqlite::Result<Vec<SymbolRow>> =
            stmt.query_map(params![file_id], map_symbol_row)?.collect();
        Ok(rows?)
    }

    pub fn all_symbols_by_file(&self) -> Result<std::collections::HashMap<i64, Vec<SymbolRow>>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, file_id, qualified_name, short_name, kind,
                    signature, signature_hash, structural_hash, visibility,
                    start_line, start_col, end_line, end_col,
                    parent_symbol_id, docstring, pagerank,
                    cyclomatic, cognitive, max_nesting, flags, language_attrs
             FROM symbols ORDER BY file_id, start_line",
        )?;
        let rows: rusqlite::Result<Vec<SymbolRow>> = stmt.query_map([], map_symbol_row)?.collect();
        let mut grouped: std::collections::HashMap<i64, Vec<SymbolRow>> =
            std::collections::HashMap::new();
        for sym in rows? {
            grouped.entry(sym.file_id).or_default().push(sym);
        }
        Ok(grouped)
    }

    /// Return (file_id, symbol_count) for all files in a single query.
    pub fn symbol_counts_by_file(&self) -> Result<std::collections::HashMap<i64, i64>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT file_id, COUNT(*) FROM symbols GROUP BY file_id")?;
        let rows: rusqlite::Result<Vec<(i64, i64)>> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect();
        Ok(rows?.into_iter().collect())
    }

    /// Return (file_id, (max_cognitive, avg_cognitive)) for files with complexity data.
    pub fn complexity_by_file(&self) -> Result<std::collections::HashMap<i64, (i64, f64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT file_id, MAX(cognitive), AVG(cognitive)
             FROM symbols
             WHERE cognitive IS NOT NULL
             GROUP BY file_id",
        )?;
        let rows: rusqlite::Result<Vec<(i64, i64, f64)>> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect();
        Ok(rows?
            .into_iter()
            .map(|(fid, max_c, avg_c)| (fid, (max_c, avg_c)))
            .collect())
    }

    /// Find symbols with zero inbound references (potential dead code).
    /// Returns (qualified_name, file_path, kind, start_line, visibility).
    #[allow(clippy::type_complexity)]
    pub fn find_dead_symbols(
        &self,
        include_pub: bool,
        path_prefix: Option<&str>,
    ) -> Result<Vec<(String, String, String, i64, Option<String>)>> {
        let conn = self.conn.lock();
        let like_pattern = path_prefix.map(|p| format!("{p}%"));
        let mut stmt = conn.prepare(
            "SELECT s.qualified_name, f.path, s.kind, s.start_line, s.visibility
             FROM symbols s
             JOIN files f ON s.file_id = f.id
             LEFT JOIN refs r ON r.target_symbol_id = s.id
             WHERE r.id IS NULL
               AND s.kind IN ('function','method','struct','enum','trait',
                              'type_alias','class','mixin','const','static')
               AND s.short_name != 'main'
               AND (s.flags & 7) = 0
               AND f.path NOT LIKE 'tests/%'
               AND (?1 = 1 OR s.visibility IS NULL OR s.visibility NOT IN ('pub','public'))
               AND (?2 IS NULL OR f.path LIKE ?2)
             ORDER BY f.path, s.start_line",
        )?;
        let rows: rusqlite::Result<Vec<_>> = stmt
            .query_map(params![include_pub as i32, like_pattern], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect();
        Ok(rows?)
    }

    /// Find files with zero fan-in that are not root files.
    /// Returns (path, line_count).
    pub fn find_unreachable_files(&self, path_prefix: Option<&str>) -> Result<Vec<(String, i64)>> {
        let conn = self.conn.lock();
        let like_pattern = path_prefix.map(|p| format!("{p}%"));
        let mut stmt = conn.prepare(
            "SELECT path, line_count FROM files
             WHERE fan_in_files = 0
               AND path NOT LIKE '%/lib.rs'
               AND path NOT LIKE '%/main.rs'
               AND path NOT LIKE '%/mod.rs'
               AND path NOT LIKE 'src/bin/%'
               AND path NOT LIKE 'lib/%'
               AND path NOT LIKE 'tests/%'
               AND path NOT LIKE 'test/%'
               AND path NOT LIKE '%/test/%'
               AND path NOT LIKE 'integration\\_test/%' ESCAPE '\\'
               AND path NOT LIKE '%/integration\\_test/%' ESCAPE '\\'
               AND path NOT LIKE '%\\_test.dart' ESCAPE '\\'
               AND (?1 IS NULL OR path LIKE ?1)
             ORDER BY path",
        )?;
        let rows: rusqlite::Result<Vec<(String, i64)>> = stmt
            .query_map(params![like_pattern], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect();
        Ok(rows?)
    }

    /// Per-file dead-code ratio: (file_id, dead_count, total_count) for files
    /// with at least one dead symbol. "Dead" and the scorable-symbol set mirror
    /// `find_dead_symbols` (non-pub, non-test, `main` excluded, generated/impl
    /// flags cleared) so the ratio biomarker and the `sutra_dead` tool agree on
    /// what counts. A symbol is dead when no ref targets it (EXISTS avoids the
    /// row multiplication a LEFT JOIN would cause for multiply-referenced symbols).
    pub fn dead_code_ratio_by_file(&self) -> Result<Vec<(i64, i64, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT s.file_id,
                    SUM(CASE WHEN NOT EXISTS (
                        SELECT 1 FROM refs r WHERE r.target_symbol_id = s.id
                    ) THEN 1 ELSE 0 END) AS dead,
                    COUNT(*) AS total
             FROM symbols s
             JOIN files f ON s.file_id = f.id
             WHERE s.kind IN ('function','method','struct','enum','trait',
                              'type_alias','class','mixin','const','static')
               AND s.short_name != 'main'
               AND (s.flags & 7) = 0
               AND f.path NOT LIKE 'tests/%'
               AND (s.visibility IS NULL OR s.visibility NOT IN ('pub','public'))
             GROUP BY s.file_id
             HAVING dead > 0",
        )?;
        let rows: rusqlite::Result<Vec<(i64, i64, i64)>> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect();
        Ok(rows?)
    }

    /// Load summary of every symbol: id, names, kind, parent, and file_id.
    ///
    /// Ordered by file path, then id (extraction order within a file). The
    /// resolver's tie-breaks take the first candidate in this order, so it must
    /// not depend on rowids — re-extracting a file reassigns its ids, which
    /// would flip which of two equal candidates wins (sutra/431).
    pub fn all_symbols_summary(&self) -> Result<Vec<SymbolEntry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.qualified_name, s.short_name, s.kind, s.parent_symbol_id, s.file_id
             FROM symbols s JOIN files f ON f.id = s.file_id
             ORDER BY f.path, s.id",
        )?;
        let rows: rusqlite::Result<Vec<SymbolEntry>> = stmt
            .query_map([], |row| {
                Ok(SymbolEntry {
                    id: row.get(0)?,
                    qualified_name: row.get(1)?,
                    short_name: row.get(2)?,
                    kind: row.get(3)?,
                    parent_symbol_id: row.get(4)?,
                    file_id: row.get(5)?,
                })
            })?
            .collect();
        Ok(rows?)
    }

    /// Resolve a symbol by name: try qualified_name first, then short_name lookup.
    pub fn resolve_symbol(&self, name: &str, kind: Option<&str>) -> Result<Option<SymbolRow>> {
        if let Some(sym) = self.symbol_by_qualified_name(name)? {
            return Ok(Some(sym));
        }
        let mut results = self.find_symbols_by_name(name, kind, 1)?;
        Ok(if results.is_empty() {
            None
        } else {
            Some(results.swap_remove(0))
        })
    }

    /// All distinct files that define a symbol carrying `qualified_name`.
    ///
    /// A single logical entity is routinely split across files — a Rust struct
    /// and its `impl` blocks, for instance, all share the qualified name of the
    /// type. `resolve_symbol` collapses that set to one arbitrary row, so a
    /// caller acting on the resolved symbol's file (the modification guard's
    /// impact ack) would only cover one of the definition sites. This returns
    /// every file so the ack can span the whole entity (sutra/329).
    pub fn symbol_definition_files(&self, qualified_name: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT f.path FROM symbols s \
             JOIN files f ON s.file_id = f.id \
             WHERE s.qualified_name = ?1 ORDER BY f.path",
        )?;
        let paths = stmt
            .query_map(params![qualified_name], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(paths)
    }

    pub fn resolve_symbol_diagnostic(
        &self,
        name: &str,
        kind: Option<&str>,
    ) -> Result<ResolveResult> {
        if let Some(sym) = self.symbol_by_qualified_name(name)? {
            return Ok(ResolveResult::Unique(sym));
        }
        let results = self.find_symbols_by_name(name, kind, 10)?;
        Ok(match results.len() {
            0 => ResolveResult::NotFound,
            1 => ResolveResult::Unique(results.into_iter().next().unwrap()),
            _ => ResolveResult::Ambiguous(results),
        })
    }

    pub fn distinct_symbol_kinds(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT DISTINCT kind FROM symbols ORDER BY kind")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        let mut kinds = Vec::new();
        for r in rows {
            kinds.push(r?);
        }
        Ok(kinds)
    }

    pub fn distinct_languages(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT DISTINCT language FROM files ORDER BY language")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        let mut langs = Vec::new();
        for r in rows {
            langs.push(r?);
        }
        Ok(langs)
    }

    /// Find the narrowest symbol enclosing the given line in a file.
    pub fn find_enclosing_symbol(&self, file_id: i64, line: i64) -> Result<Option<SymbolRow>> {
        let symbols = self.find_symbols_by_file(file_id)?;
        let mut best: Option<&SymbolRow> = None;
        for s in &symbols {
            if s.start_line <= line && line <= s.end_line {
                match best {
                    None => best = Some(s),
                    Some(prev)
                        if (s.end_line - s.start_line) < (prev.end_line - prev.start_line) =>
                    {
                        best = Some(s);
                    }
                    _ => {}
                }
            }
        }
        Ok(best.cloned())
    }

    // -----------------------------------------------------------------------
    // refs
    // -----------------------------------------------------------------------

    /// Insert a reference row. Returns the new row id.
    ///
    /// Test-only fixture primitive (no live callers): live ref writes go through
    /// `replace_file_data` / `replace_refs_and_clear_resolution`, so this does
    /// not bump `data_generation` (the `symbol_graph` cache token, sutra/396).
    pub fn insert_ref(
        &self,
        file_id: i64,
        target_symbol_id: Option<i64>,
        unresolved_name: Option<&str>,
        line: i64,
        col: i64,
        context_kind: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO refs (file_id, target_symbol_id, unresolved_name, line, col, context_kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                file_id,
                target_symbol_id,
                unresolved_name,
                line,
                col,
                context_kind
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Return all refs that target a given symbol.
    pub fn find_refs_to_symbol(&self, symbol_id: i64) -> Result<Vec<RefRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, file_id, target_symbol_id, unresolved_name, line, col, context_kind, resolved_local_target, receiver
             FROM refs WHERE target_symbol_id = ?1",
        )?;
        let rows: rusqlite::Result<Vec<RefRow>> =
            stmt.query_map(params![symbol_id], map_ref_row)?.collect();
        Ok(rows?)
    }

    /// Return all refs contained in a given file.
    pub fn find_refs_in_file(&self, file_id: i64) -> Result<Vec<RefRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, file_id, target_symbol_id, unresolved_name, line, col, context_kind, resolved_local_target, receiver
             FROM refs WHERE file_id = ?1",
        )?;
        let rows: rusqlite::Result<Vec<RefRow>> =
            stmt.query_map(params![file_id], map_ref_row)?.collect();
        Ok(rows?)
    }

    /// Delete all refs belonging to a given file.
    ///
    /// Test-only fixture primitive (no live callers): the live delete path is
    /// `delete_file_cascade`, which bumps `data_generation`. This does not (it
    /// would need to, in a transaction, if ever promoted to a live path — sutra/396).
    pub fn delete_refs_by_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM refs WHERE file_id = ?1", params![file_id])?;
        Ok(())
    }

    /// Begin a multi-file bulk batch. While a batch is open,
    /// `replace_file_data` and `replace_refs_and_clear_resolution` join it
    /// instead of opening their own transactions, so many files share one
    /// commit (and one WAL fsync).
    ///
    /// Not `pub`: unbalanced begin/commit leaves the connection in
    /// non-autocommit state, after which `batch_aware_transaction` silently
    /// turns every per-file transaction into a no-op batch-join. `BulkBatch`
    /// (pipeline.rs) is the only intended caller and balances these via RAII.
    pub(crate) fn begin_batch(&self) -> Result<()> {
        self.conn.lock().execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    pub(crate) fn commit_batch(&self) -> Result<()> {
        self.conn.lock().execute_batch("COMMIT")?;
        Ok(())
    }

    pub(crate) fn rollback_batch(&self) -> Result<()> {
        self.conn.lock().execute_batch("ROLLBACK")?;
        Ok(())
    }

    /// Checkpoint the WAL and truncate it. Best-effort: a concurrent reader
    /// can legitimately block a full checkpoint, so busy errors are not fatal.
    pub(crate) fn wal_checkpoint_truncate(&self) -> Result<()> {
        self.conn
            .lock()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// Atomically replace all refs for a file and clear its needs_resolution flag.
    pub fn replace_refs_and_clear_resolution(
        &self,
        file_id: i64,
        refs: &[ResolvedRefRow<'_>],
    ) -> Result<()> {
        let conn = self.conn.lock();
        let tx = batch_aware_transaction(&conn)?;
        conn.execute("DELETE FROM refs WHERE file_id = ?1", params![file_id])?;
        for r in refs {
            conn.execute(
                "INSERT INTO refs (file_id, target_symbol_id, unresolved_name, line, col, context_kind, resolution_method, resolved_local_target, receiver)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    file_id,
                    r.target_symbol_id,
                    r.unresolved_name,
                    r.line,
                    r.col,
                    r.context_kind,
                    r.resolution_method,
                    r.resolved_local_target,
                    r.receiver
                ],
            )?;
        }
        conn.execute(
            "UPDATE files SET needs_resolution = 0 WHERE id = ?1",
            params![file_id],
        )?;
        // Resolution rewrites the resolved ref edges the symbol wiring graph
        // (sutra/372) is built from, so it must move the staleness token that
        // `symbol_graph`'s cache keys on. `replace_file_data` inserts refs
        // UNRESOLVED (target_symbol_id = NULL); the edges only appear here.
        // Without this bump, a graph cached during the resolve window is served
        // stale until an unrelated parse advances the counter (sutra/396).
        conn.execute(
            "UPDATE index_meta SET data_generation = data_generation + 1 WHERE id = 1",
            [],
        )?;
        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok(())
    }

    /// Mark files as needing ref resolution (e.g. after their referenced symbols were deleted).
    ///
    /// Only flips the `needs_resolution` flag; it does not touch symbols or
    /// resolved ref edges, so it correctly leaves `data_generation` unchanged —
    /// the subsequent `replace_refs_and_clear_resolution` bumps it (sutra/396).
    pub fn mark_needs_resolution(&self, file_ids: &[i64]) -> Result<()> {
        if file_ids.is_empty() {
            return Ok(());
        }
        let placeholders: String = file_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("UPDATE files SET needs_resolution = 1 WHERE id IN ({placeholders})");
        let conn = self.conn.lock();
        conn.execute(&sql, rusqlite::params_from_iter(file_ids.iter()))?;
        Ok(())
    }

    /// Return IDs of all files that need ref resolution.
    pub fn files_needing_resolution(&self) -> Result<Vec<i64>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id FROM files WHERE needs_resolution = 1")?;
        let ids: rusqlite::Result<Vec<i64>> = stmt.query_map([], |row| row.get(0))?.collect();
        Ok(ids?)
    }

    /// Check whether any post-parse work remains (unresolved files or stale derived data).
    pub fn has_pending_work(&self) -> Result<bool> {
        let conn = self.conn.lock();
        let pending: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE needs_resolution = 1)
                OR (SELECT data_generation != derived_complete_generation FROM index_meta WHERE id = 1)",
            [],
            |row| row.get(0),
        )?;
        Ok(pending)
    }

    /// Return the current data_generation from index_meta.
    pub fn get_data_generation(&self) -> Result<i64> {
        let conn = self.conn.lock();
        let data_gen: i64 = conn.query_row(
            "SELECT data_generation FROM index_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(data_gen)
    }

    /// Return the generation up to which the derived tier is marked complete.
    /// Health publication has its own sequence and must never advance this
    /// (health evidence contract, sutra/412) — the symmetric partner to
    /// [`Db::set_derived_complete`].
    pub fn get_derived_complete_generation(&self) -> Result<i64> {
        let conn = self.conn.lock();
        let generation: i64 = conn.query_row(
            "SELECT derived_complete_generation FROM index_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(generation)
    }

    /// The query-time symbol wiring graph (sutra/372), built once per workspace
    /// and cached on the handle. Rebuilt when `data_generation` moves — the same
    /// counter `replace_file_data` bumps on every write, so an incremental
    /// reparse (which records no snapshot) still invalidates it. The build is
    /// two lean queries plus enclosing-symbol attribution; the cache keeps the
    /// per-explore-query cost to the PageRank walk alone.
    pub fn symbol_graph(&self) -> Result<Arc<crate::graph::SymbolGraph>> {
        let generation = self.get_data_generation()?;
        {
            let cache = self.symbol_graph.lock();
            if let Some((cached_gen, graph)) = cache.as_ref()
                && *cached_gen == generation
            {
                return Ok(Arc::clone(graph));
            }
        }

        let spans = self.symbol_spans_by_file()?;
        let ref_edges = self.resolved_ref_edges()?;
        let graph = Arc::new(crate::graph::SymbolGraph::build(
            &spans,
            &ref_edges,
            &crate::graph::SymbolGraph::default_walk_kinds(),
        ));

        // spans and edges are read under separate lock acquisitions, so a write
        // committing mid-build could have advanced the generation between them.
        // Only publish to the cache if the generation still matches what we
        // built against — otherwise this handle would serve a superseded (and
        // possibly mixed-generation) snapshot until the next bump. On a race we
        // return the freshly built graph but leave the cache for the next call
        // to rebuild from a consistent generation (sutra/396).
        if self.get_data_generation()? == generation {
            let mut cache = self.symbol_graph.lock();
            *cache = Some((generation, Arc::clone(&graph)));
        }
        Ok(graph)
    }

    /// The extractor-identity stamp recorded at the last full parse, or `None`
    /// on an index built before sutra/364 (column defaults NULL). A `None` reads
    /// as "unknown extractor" — the caller must force a re-extraction.
    pub fn parser_stamp(&self) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let stamp: Option<String> = conn.query_row(
            "SELECT parser_stamp FROM index_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(stamp)
    }

    /// Record the extractor identity that produced the current index. Called
    /// after a full parse re-extracts under the active extractor.
    pub fn set_parser_stamp(&self, stamp: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE index_meta SET parser_stamp = ?1 WHERE id = 1",
            params![stamp],
        )?;
        Ok(())
    }

    /// Mark derived data as complete up to the given generation.
    pub fn set_derived_complete(&self, generation: i64) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE index_meta SET derived_complete_generation = ?1 WHERE id = 1",
            params![generation],
        )?;
        Ok(())
    }

    /// Return the distinct set of file_ids that contain a reference to any of
    /// the given symbol_ids.
    pub fn find_files_referencing_symbols(&self, symbol_ids: &[i64]) -> Result<Vec<i64>> {
        if symbol_ids.is_empty() {
            return Ok(vec![]);
        }

        // Build a parameterised IN clause at runtime.
        let placeholders: String = symbol_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");

        let sql =
            format!("SELECT DISTINCT file_id FROM refs WHERE target_symbol_id IN ({placeholders})");

        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let ids: rusqlite::Result<Vec<i64>> = stmt
            .query_map(rusqlite::params_from_iter(symbol_ids.iter()), |row| {
                row.get(0)
            })?
            .collect();
        Ok(ids?)
    }

    // -----------------------------------------------------------------------
    // imports
    // -----------------------------------------------------------------------

    /// Insert a production-scope import row. Returns the new row id.
    pub fn insert_import(
        &self,
        file_id: i64,
        imported_path: &str,
        resolved_file_id: Option<i64>,
        line: i64,
        kind: &str,
        alias: Option<&str>,
    ) -> Result<i64> {
        self.insert_import_with_scope(
            file_id,
            imported_path,
            resolved_file_id,
            line,
            kind,
            alias,
            false,
        )
    }

    /// Insert an import row, stating whether it sits in test-only code.
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the imports table columns; a params struct here would only wrap the same flat row"
    )]
    pub fn insert_import_with_scope(
        &self,
        file_id: i64,
        imported_path: &str,
        resolved_file_id: Option<i64>,
        line: i64,
        kind: &str,
        alias: Option<&str>,
        is_test: bool,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO imports (file_id, imported_path, resolved_file_id, line, kind, alias, is_test)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![file_id, imported_path, resolved_file_id, line, kind, alias, is_test],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Count distinct files per import root crate across the workspace.
    pub fn import_root_file_counts(&self) -> Result<std::collections::HashMap<String, usize>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT file_id, imported_path FROM imports")?;
        let mut sets: std::collections::HashMap<String, std::collections::HashSet<i64>> =
            std::collections::HashMap::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let file_id: i64 = row.get(0)?;
            let path: String = row.get(1)?;
            let root = path
                .strip_prefix("package:")
                .map(|r| r.split('/').next().unwrap_or(""))
                .unwrap_or_else(|| {
                    if path.starts_with("dart:") || path.starts_with("pub use ") {
                        return "";
                    }
                    let r = path.split("::").next().unwrap_or("");
                    if r.starts_with('.') || r.starts_with('/') {
                        return "";
                    }
                    match r {
                        "crate" | "self" | "super" | "std" => "",
                        _ => r,
                    }
                });
            if !root.is_empty() {
                sets.entry(root.to_string()).or_default().insert(file_id);
            }
        }
        Ok(sets.into_iter().map(|(k, v)| (k, v.len())).collect())
    }

    /// Return all import rows for a file.
    pub fn imports_for_file(&self, file_id: i64) -> Result<Vec<ImportRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, file_id, imported_path, resolved_file_id, line, kind, alias, is_test
             FROM imports WHERE file_id = ?1",
        )?;
        let rows: rusqlite::Result<Vec<ImportRow>> =
            stmt.query_map(params![file_id], map_import_row)?.collect();
        Ok(rows?)
    }

    /// Return all unresolved Dart imports (package: and relative .dart paths).
    pub fn unresolved_dart_imports(&self) -> Result<Vec<(i64, i64, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT i.id, i.file_id, i.imported_path FROM imports i
             JOIN files f ON f.id = i.file_id
             WHERE i.resolved_file_id IS NULL
             AND f.language = 'dart'",
        )?;
        let rows: rusqlite::Result<Vec<(i64, i64, String)>> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect();
        Ok(rows?)
    }

    /// Return all unresolved Rust imports.
    pub fn unresolved_rust_imports(&self) -> Result<Vec<(i64, i64, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT i.id, i.file_id, i.imported_path FROM imports i
             JOIN files f ON f.id = i.file_id
             WHERE i.resolved_file_id IS NULL
             AND f.language = 'rust'",
        )?;
        let rows: rusqlite::Result<Vec<(i64, i64, String)>> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect();
        Ok(rows?)
    }

    /// Return all unresolved Python imports.
    pub fn unresolved_python_imports(&self) -> Result<Vec<(i64, i64, String, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT i.id, i.file_id, i.imported_path, i.kind FROM imports i
             JOIN files f ON f.id = i.file_id
             WHERE i.resolved_file_id IS NULL
             AND f.language = 'python'",
        )?;
        let rows: rusqlite::Result<Vec<(i64, i64, String, String)>> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect();
        Ok(rows?)
    }

    /// Return all unresolved JS/TS imports.
    pub fn unresolved_js_ts_imports(&self) -> Result<Vec<(i64, i64, String, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT i.id, i.file_id, i.imported_path, i.kind FROM imports i
             JOIN files f ON f.id = i.file_id
             WHERE i.resolved_file_id IS NULL
             AND f.language IN ('javascript', 'typescript')",
        )?;
        let rows: rusqlite::Result<Vec<(i64, i64, String, String)>> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect();
        Ok(rows?)
    }

    /// Return all unresolved C imports.
    pub fn unresolved_c_imports(&self) -> Result<Vec<(i64, i64, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT i.id, i.file_id, i.imported_path FROM imports i
             JOIN files f ON f.id = i.file_id
             WHERE i.resolved_file_id IS NULL
             AND f.language = 'c'",
        )?;
        let rows: rusqlite::Result<Vec<(i64, i64, String)>> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect();
        Ok(rows?)
    }

    /// Unresolved imports joined with file path + language — the input for
    /// external-crate constraint checking.
    pub fn unresolved_imports_with_files(&self) -> Result<Vec<UnresolvedImport>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT i.file_id, f.path, f.language, i.imported_path, i.is_test FROM imports i
             JOIN files f ON f.id = i.file_id
             WHERE i.resolved_file_id IS NULL",
        )?;
        let rows: rusqlite::Result<Vec<UnresolvedImport>> = stmt
            .query_map([], |row| {
                Ok(UnresolvedImport {
                    file_id: row.get(0)?,
                    path: row.get(1)?,
                    language: row.get(2)?,
                    imported_path: row.get(3)?,
                    is_test: row.get(4)?,
                })
            })?
            .collect();
        Ok(rows?)
    }

    /// Batch-set resolved_file_id on import rows in a single transaction.
    pub fn batch_update_import_resolved_file_ids(&self, updates: &[(i64, i64)]) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut stmt = conn.prepare("UPDATE imports SET resolved_file_id = ?1 WHERE id = ?2")?;
        for &(import_id, resolved_file_id) in updates {
            stmt.execute(params![resolved_file_id, import_id])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // snapshots
    // -----------------------------------------------------------------------

    /// Insert a snapshot record. Returns the new row id.
    pub fn insert_snapshot(&self, p: &SnapshotParams) -> Result<i64> {
        let ts = p
            .timestamp
            .clone()
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO snapshots (timestamp, files_parsed, symbols_extracted,
                                    refs_extracted, parse_errors, duration_ms,
                                    total_complexity, dead_symbol_count,
                                    hotspot_count, health_score,
                                    pattern_family_count, head_commit, health_run_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                ts,
                p.files_parsed,
                p.symbols_extracted,
                p.refs_extracted,
                p.parse_errors,
                p.duration_ms,
                p.total_complexity,
                p.dead_symbol_count,
                p.hotspot_count,
                p.health_score,
                p.pattern_family_count,
                p.head_commit,
                p.health_run_id,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Insert snapshot + file/component details atomically in one transaction.
    pub fn insert_snapshot_atomic(
        &self,
        p: &SnapshotParams,
        files: &[SnapshotFileRow],
        components: &[SnapshotComponentRow],
    ) -> Result<i64> {
        let ts = p
            .timestamp
            .clone()
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        let conn = self.conn.lock();
        conn.execute_batch("BEGIN")?;

        let result = (|| -> Result<i64> {
            conn.execute(
                "INSERT INTO snapshots (timestamp, files_parsed, symbols_extracted,
                                        refs_extracted, parse_errors, duration_ms,
                                        total_complexity, dead_symbol_count,
                                        hotspot_count, health_score,
                                        pattern_family_count, head_commit, health_run_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    ts,
                    p.files_parsed,
                    p.symbols_extracted,
                    p.refs_extracted,
                    p.parse_errors,
                    p.duration_ms,
                    p.total_complexity,
                    p.dead_symbol_count,
                    p.hotspot_count,
                    p.health_score,
                    p.pattern_family_count,
                    p.head_commit,
                    p.health_run_id,
                ],
            )?;
            let snapshot_id = conn.last_insert_rowid();

            insert_snapshot_file_rows(&conn, snapshot_id, files)?;

            insert_snapshot_component_rows(&conn, snapshot_id, components)?;

            Ok(snapshot_id)
        })();

        match result {
            Ok(id) => {
                conn.execute_batch("COMMIT")?;
                drop(conn);
                self.prune_snapshots(Self::MAX_SNAPSHOTS)?;
                Ok(id)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    const MAX_SNAPSHOTS: usize = 30;

    /// Delete snapshots (and their child rows) beyond the most recent `keep`.
    /// Returns the number of snapshots deleted.
    pub fn prune_snapshots(&self, keep: usize) -> Result<usize> {
        let conn = self.conn.lock();
        let stale_ids: Vec<i64> = conn
            .prepare("SELECT id FROM snapshots ORDER BY id DESC LIMIT -1 OFFSET ?1")?
            .query_map(params![keep as i64], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(SutraError::Db)?;

        if stale_ids.is_empty() {
            return Ok(0);
        }

        let placeholders: String = stale_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql_files =
            format!("DELETE FROM health_snapshot_files WHERE snapshot_id IN ({placeholders})");
        let sql_components =
            format!("DELETE FROM health_snapshot_components WHERE snapshot_id IN ({placeholders})");
        let sql_snapshots = format!("DELETE FROM snapshots WHERE id IN ({placeholders})");

        let id_params: Vec<Box<dyn rusqlite::types::ToSql>> =
            stale_ids.iter().map(|id| Box::new(*id) as _).collect();
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            id_params.iter().map(|b| b.as_ref()).collect();

        conn.execute(&sql_files, param_refs.as_slice())?;
        conn.execute(&sql_components, param_refs.as_slice())?;
        conn.execute(&sql_snapshots, param_refs.as_slice())?;
        health_evidence::prune_unreferenced_health_runs(&conn)?;

        Ok(stale_ids.len())
    }

    /// Return the timestamp of the most recent snapshot, or `None` if no
    /// snapshot has been recorded yet.
    pub fn last_parse_time(&self) -> Result<Option<String>> {
        let conn = self.conn.lock();
        match conn.query_row("SELECT MAX(timestamp) FROM snapshots", [], |row| {
            row.get::<_, Option<String>>(0)
        }) {
            Ok(ts) => Ok(ts),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(SutraError::Db(e)),
        }
    }

    /// Return (timestamp, head_commit) from the most recent snapshot.
    pub fn last_parse_info(&self) -> Result<Option<(String, Option<String>)>> {
        let conn = self.conn.lock();
        match conn.query_row(
            "SELECT timestamp, head_commit FROM snapshots ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        ) {
            Ok(info) => Ok(Some(info)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(SutraError::Db(e)),
        }
    }

    /// Return the freshness fingerprint of every indexed file: path plus the
    /// `(size, mtime)` fast-path baseline and the `content_hash` confirm. Drives
    /// the drift probe (sutra/362), so it selects only those four columns rather
    /// than materializing full [`FileRow`]s.
    pub fn all_file_fingerprints(&self) -> Result<Vec<FileFingerprint>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT path, size_bytes, mtime_ns, content_hash FROM files")?;
        let rows = stmt.query_map([], |row| {
            Ok(FileFingerprint {
                path: row.get(0)?,
                size_bytes: row.get(1)?,
                mtime_ns: row.get(2)?,
                content_hash: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(SutraError::Db)?);
        }
        Ok(out)
    }

    /// Return the N most recent snapshots, ordered newest-first.
    pub fn latest_snapshots(&self, limit: i64) -> Result<Vec<SnapshotRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, timestamp, files_parsed, symbols_extracted,
                    refs_extracted, parse_errors, duration_ms,
                    total_complexity, dead_symbol_count,
                    hotspot_count, health_score, pattern_family_count, health_run_id
             FROM snapshots ORDER BY timestamp DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], map_snapshot_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The health run a checkpoint was scored from (`None` for legacy
    /// checkpoints or an unknown id).
    pub fn snapshot_health_run_id(&self, snapshot_id: i64) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        let run: Option<Option<i64>> = conn
            .query_row(
                "SELECT health_run_id FROM snapshots WHERE id = ?1",
                params![snapshot_id],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(run.flatten())
    }

    /// Return all snapshots whose timestamp falls within [from, to], ordered oldest-first.
    pub fn snapshots_between(&self, from: &str, to: &str) -> Result<Vec<SnapshotRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, timestamp, files_parsed, symbols_extracted,
                    refs_extracted, parse_errors, duration_ms,
                    total_complexity, dead_symbol_count,
                    hotspot_count, health_score, pattern_family_count, health_run_id
             FROM snapshots WHERE timestamp >= ?1 AND timestamp <= ?2
             ORDER BY timestamp ASC",
        )?;
        let rows = stmt
            .query_map(params![from, to], map_snapshot_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // health snapshot details
    // -----------------------------------------------------------------------

    pub fn insert_snapshot_files(&self, snapshot_id: i64, files: &[SnapshotFileRow]) -> Result<()> {
        let conn = self.conn.lock();
        insert_snapshot_file_rows(&conn, snapshot_id, files)
    }

    pub fn insert_snapshot_components(
        &self,
        snapshot_id: i64,
        components: &[SnapshotComponentRow],
    ) -> Result<()> {
        let conn = self.conn.lock();
        insert_snapshot_component_rows(&conn, snapshot_id, components)
    }

    pub fn snapshot_file_scores(&self, snapshot_id: i64) -> Result<Vec<SnapshotFileRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT file_id, file_path, score, category_scores,
                    partial, completeness_recorded, missing_biomarkers,
                    score_upper, score_basis
             FROM health_snapshot_files WHERE snapshot_id = ?1",
        )?;
        let rows = stmt
            .query_map(params![snapshot_id], |row| {
                let (completeness, missing_biomarkers) = map_snapshot_completeness(row, 4)?;
                Ok(SnapshotFileRow {
                    file_id: row.get(0)?,
                    file_path: row.get(1)?,
                    score: row.get(2)?,
                    category_scores: row.get(3)?,
                    completeness,
                    missing_biomarkers,
                    score_upper: row.get(7)?,
                    score_basis: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn snapshot_component_scores(&self, snapshot_id: i64) -> Result<Vec<SnapshotComponentRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT component_id, component_name, score, member_count, total_nloc,
                    partial, completeness_recorded, score_basis
             FROM health_snapshot_components WHERE snapshot_id = ?1",
        )?;
        let rows = stmt
            .query_map(params![snapshot_id], |row| {
                Ok(SnapshotComponentRow {
                    component_id: row.get(0)?,
                    component_name: row.get(1)?,
                    score: row.get(2)?,
                    member_count: row.get(3)?,
                    total_nloc: row.get(4)?,
                    completeness: SnapshotCompleteness::from_columns(row.get(5)?, row.get(6)?),
                    score_basis: row.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn file_health_history(
        &self,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<FileHealthHistoryRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT s.timestamp, hsf.score, hsf.category_scores,
                    hsf.partial, hsf.completeness_recorded, hsf.missing_biomarkers,
                    hsf.score_upper
             FROM health_snapshot_files hsf
             JOIN snapshots s ON s.id = hsf.snapshot_id
             WHERE hsf.file_path = ?1
             ORDER BY s.timestamp DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![file_path, limit as i64], |row| {
                let (completeness, missing_biomarkers) = map_snapshot_completeness(row, 3)?;
                Ok(FileHealthHistoryRow {
                    timestamp: row.get(0)?,
                    score: row.get(1)?,
                    category_scores: row.get(2)?,
                    completeness,
                    missing_biomarkers,
                    score_upper: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// Row mappers
// ---------------------------------------------------------------------------

/// Open a transaction unless the connection is already inside one (an open
/// bulk batch — see `Db::begin_batch`), in which case the caller's statements
/// join the batch and `None` is returned. SQLite cannot nest BEGIN.
fn batch_aware_transaction(conn: &Connection) -> Result<Option<rusqlite::Transaction<'_>>> {
    if conn.is_autocommit() {
        Ok(Some(conn.unchecked_transaction()?))
    } else {
        Ok(None)
    }
}

/// The single writer for `health_snapshot_files` rows. Both snapshot insert
/// paths go through it so completeness cannot be dropped by one of them again
/// (sutra/418: the atomic production writer omitted `partial` and
/// `missing_biomarkers` while the standalone one stored them).
fn insert_snapshot_file_rows(
    conn: &Connection,
    snapshot_id: i64,
    files: &[SnapshotFileRow],
) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO health_snapshot_files
         (snapshot_id, file_id, file_path, score, category_scores,
          partial, completeness_recorded, missing_biomarkers, score_upper, score_basis)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    for f in files {
        let (partial, recorded) = f.completeness.to_columns();
        let missing_json =
            serde_json::to_string(&f.missing_biomarkers).unwrap_or_else(|_| "[]".to_string());
        stmt.execute(params![
            snapshot_id,
            f.file_id,
            f.file_path,
            f.score,
            f.category_scores,
            partial,
            recorded,
            missing_json,
            f.score_upper,
            f.score_basis,
        ])?;
    }
    Ok(())
}

/// The single writer for `health_snapshot_components` rows, shared by both
/// snapshot insert paths for the same reason as [`insert_snapshot_file_rows`].
fn insert_snapshot_component_rows(
    conn: &Connection,
    snapshot_id: i64,
    components: &[SnapshotComponentRow],
) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO health_snapshot_components
         (snapshot_id, component_id, component_name, score, member_count, total_nloc,
          partial, completeness_recorded, score_basis)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for c in components {
        let (partial, recorded) = c.completeness.to_columns();
        stmt.execute(params![
            snapshot_id,
            c.component_id,
            c.component_name,
            c.score,
            c.member_count,
            c.total_nloc,
            partial,
            recorded,
            c.score_basis,
        ])?;
    }
    Ok(())
}

/// Read `(partial, completeness_recorded, missing_biomarkers)` starting at
/// column `first`.
fn map_snapshot_completeness(
    row: &rusqlite::Row<'_>,
    first: usize,
) -> rusqlite::Result<(SnapshotCompleteness, Vec<String>)> {
    let completeness = SnapshotCompleteness::from_columns(row.get(first)?, row.get(first + 1)?);
    let missing_json: String = row.get(first + 2)?;
    let missing_biomarkers: Vec<String> = serde_json::from_str(&missing_json).unwrap_or_default();
    Ok((completeness, missing_biomarkers))
}

fn map_file_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRow> {
    let parsed_ok_int: i64 = row.get(5)?;
    Ok(FileRow {
        id: row.get(0)?,
        path: Arc::from(row.get::<_, String>(1)?),
        language: row.get(2)?,
        content_hash: row.get(3)?,
        line_count: row.get(4)?,
        parsed_ok: parsed_ok_int != 0,
        last_parsed: row.get(6)?,
        fan_in_files: row.get(7)?,
        blast_radius: row.get(8)?,
        pagerank: row.get(9)?,
        mtime_ns: row.get(10)?,
        size_bytes: row.get(11)?,
    })
}

fn map_symbol_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SymbolRow> {
    Ok(SymbolRow {
        id: row.get(0)?,
        file_id: row.get(1)?,
        qualified_name: Arc::from(row.get::<_, String>(2)?),
        short_name: Arc::from(row.get::<_, String>(3)?),
        kind: Arc::from(row.get::<_, String>(4)?),
        signature: row.get(5)?,
        signature_hash: row.get(6)?,
        structural_hash: row.get(7)?,
        visibility: row.get(8)?,
        start_line: row.get(9)?,
        start_col: row.get(10)?,
        end_line: row.get(11)?,
        end_col: row.get(12)?,
        parent_symbol_id: row.get(13)?,
        docstring: row.get(14)?,
        pagerank: row.get(15)?,
        cyclomatic: row.get(16)?,
        cognitive: row.get(17)?,
        max_nesting: row.get(18)?,
        flags: row.get(19)?,
        language_attrs: row.get(20)?,
    })
}

fn map_ref_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RefRow> {
    Ok(RefRow {
        id: row.get(0)?,
        file_id: row.get(1)?,
        target_symbol_id: row.get(2)?,
        unresolved_name: row.get(3)?,
        line: row.get(4)?,
        col: row.get(5)?,
        context_kind: row.get(6)?,
        resolved_local_target: row.get(7)?,
        receiver: row.get(8)?,
    })
}

fn map_import_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImportRow> {
    Ok(ImportRow {
        id: row.get(0)?,
        file_id: row.get(1)?,
        imported_path: row.get(2)?,
        resolved_file_id: row.get(3)?,
        line: row.get(4)?,
        kind: row.get(5)?,
        alias: row.get(6)?,
        is_test: row.get(7)?,
    })
}

fn map_snapshot_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotRow> {
    Ok(SnapshotRow {
        id: row.get(0)?,
        timestamp: row.get(1)?,
        files_parsed: row.get(2)?,
        symbols_extracted: row.get(3)?,
        refs_extracted: row.get(4)?,
        parse_errors: row.get(5)?,
        duration_ms: row.get(6)?,
        total_complexity: row.get(7)?,
        dead_symbol_count: row.get(8)?,
        hotspot_count: row.get(9)?,
        health_score: row.get(10)?,
        pattern_family_count: row.get(11)?,
        health_run_id: row.get(12)?,
    })
}
