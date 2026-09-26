//! The shared firing log for write-side mechanisms (sutra/467, design rule 9 in
//! `docs/sutra-purpose.md`).
//!
//! Every mechanism that flags a site (sibling pattern, swallow, dup-exists,
//! orphans) records one row per flagged site here, so the acted-on rate can be
//! measured later (sutra/485): did the flagged code change after the firing?
//! Rows are durable. Recording the same diff twice is a no-op, keyed by
//! `diff_fingerprint`.

use rusqlite::params;

use crate::error::Result;

use super::Db;

/// Which review of which change produced a batch of firings.
pub struct FiringContext<'a> {
    /// `review` (MCP `sutra_review`), `check` (`sutra check --diff`) or `guard`.
    pub surface: &'a str,
    /// The diff spec as requested: `branch`, `staged`, `unstaged`, a commit spec.
    pub diff_spec: &'a str,
    pub base_rev: Option<&'a str>,
    /// `None` when the head side is the worktree.
    pub head_rev: Option<&'a str>,
    /// The HEAD commit at firing time. The acted-on window starts here.
    pub anchor_commit: Option<&'a str>,
    /// Identity of the reviewed change: the same content on both sides gives
    /// the same fingerprint, so a repeated review does not re-count.
    pub diff_fingerprint: &'a str,
}

/// One flagged site.
pub struct FiringRecord<'a> {
    /// The mechanism: `sibling_pattern`, `swallow`, `dup_exists`, `orphan`.
    pub mechanism: &'a str,
    /// The rule id or finding kind within the mechanism.
    pub finding_kind: &'a str,
    /// What was flagged, stable across reviews (for the sibling check, the idiom).
    pub finding_key: &'a str,
    pub file_path: &'a str,
    pub line: Option<i64>,
    /// The enclosing symbol, when known.
    pub symbol: Option<&'a str>,
    /// The flagged line's text, trimmed. The acted-on check looks for it later.
    pub snippet: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct FiringRow {
    pub id: i64,
    pub mechanism: String,
    pub finding_kind: String,
    pub finding_key: String,
    pub file_path: String,
    pub line: Option<i64>,
    pub symbol: Option<String>,
    pub snippet: Option<String>,
    pub surface: String,
    pub diff_spec: String,
    pub base_rev: Option<String>,
    pub head_rev: Option<String>,
    pub anchor_commit: Option<String>,
    pub diff_fingerprint: String,
    pub fired_at: String,
}

impl Db {
    /// Record a batch of firings from one review. Returns how many rows were
    /// new; a site already recorded for the same diff is skipped.
    pub fn record_firings(
        &self,
        ctx: &FiringContext<'_>,
        records: &[FiringRecord<'_>],
    ) -> Result<usize> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut inserted = 0;
        {
            let mut stmt = conn.prepare_cached(
                "INSERT OR IGNORE INTO mechanism_firings \
                 (mechanism, finding_kind, finding_key, file_path, line, symbol, snippet, \
                  surface, diff_spec, base_rev, head_rev, anchor_commit, diff_fingerprint) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            for r in records {
                inserted += stmt.execute(params![
                    r.mechanism,
                    r.finding_kind,
                    r.finding_key,
                    r.file_path,
                    r.line,
                    r.symbol,
                    r.snippet,
                    ctx.surface,
                    ctx.diff_spec,
                    ctx.base_rev,
                    ctx.head_rev,
                    ctx.anchor_commit,
                    ctx.diff_fingerprint,
                ])?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Firings, oldest first, optionally narrowed to one mechanism and to rows
    /// fired at or after `since` (an ISO-8601 prefix such as `2026-10-01`).
    pub fn firings(&self, mechanism: Option<&str>, since: Option<&str>) -> Result<Vec<FiringRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, mechanism, finding_kind, finding_key, file_path, line, symbol, snippet, \
                    surface, diff_spec, base_rev, head_rev, anchor_commit, diff_fingerprint, \
                    fired_at \
             FROM mechanism_firings \
             WHERE (?1 IS NULL OR mechanism = ?1) AND (?2 IS NULL OR fired_at >= ?2) \
             ORDER BY fired_at, id",
        )?;
        let rows = stmt.query_map(params![mechanism, since], |row| {
            Ok(FiringRow {
                id: row.get(0)?,
                mechanism: row.get(1)?,
                finding_kind: row.get(2)?,
                finding_key: row.get(3)?,
                file_path: row.get(4)?,
                line: row.get(5)?,
                symbol: row.get(6)?,
                snippet: row.get(7)?,
                surface: row.get(8)?,
                diff_spec: row.get(9)?,
                base_rev: row.get(10)?,
                head_rev: row.get(11)?,
                anchor_commit: row.get(12)?,
                diff_fingerprint: row.get(13)?,
                fired_at: row.get(14)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
