//! The shared firing log for write-side mechanisms (sutra/467, design rule 9 in
//! `docs/sutra-purpose.md`).
//!
//! Every mechanism that flags a site (sibling pattern, swallow, dup-exists,
//! orphans) records one row per flagged site here, so the acted-on rate can be
//! measured later (sutra/485): did the flagged code change after the firing?
//! Rows are durable.
//!
//! Two identities (sutra/491). A review event is one reviewed change: its
//! `patch_id` (the removed and added lines, no line numbers) plus an `epoch`
//! bumped when the change was reverted and is reviewed again. A site is
//! (mechanism, kind, key, file, enclosing symbol, line text, occurrence) within
//! an event. Recording the same site for the same event twice is a no-op.

use rusqlite::{OptionalExtension, params};

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
}

/// A stored review event.
#[derive(Debug, Clone)]
pub struct ReviewEvent {
    pub id: i64,
    pub patch_id: String,
    pub epoch: i64,
    pub anchor_commit: Option<String>,
    pub fired_at: String,
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
    /// Display only; not part of the site's identity.
    pub line: Option<i64>,
    /// The enclosing symbol, when known.
    pub symbol: Option<&'a str>,
    /// The flagged line's text, trimmed. The acted-on check looks for it later.
    pub snippet: Option<&'a str>,
    /// The site's ordinal among identical lines in its symbol (in the file
    /// when there is no symbol), so duplicate lines are distinct sites.
    pub occurrence: i64,
}

#[derive(Debug, Clone)]
pub struct FiringRow {
    pub id: i64,
    pub event_id: i64,
    pub mechanism: String,
    pub finding_kind: String,
    pub finding_key: String,
    pub file_path: String,
    pub line: Option<i64>,
    pub symbol: Option<String>,
    pub snippet: Option<String>,
    pub occurrence: i64,
    pub surface: String,
    pub diff_spec: String,
    pub base_rev: Option<String>,
    pub head_rev: Option<String>,
    pub anchor_commit: Option<String>,
    pub patch_id: String,
    pub epoch: i64,
    pub fired_at: String,
}

impl Db {
    /// The latest epoch recorded for `patch_id`, if any.
    pub fn latest_review_event(&self, patch_id: &str) -> Result<Option<ReviewEvent>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT id, patch_id, epoch, anchor_commit, fired_at FROM review_events \
                 WHERE patch_id = ?1 ORDER BY epoch DESC LIMIT 1",
                params![patch_id],
                |row| {
                    Ok(ReviewEvent {
                        id: row.get(0)?,
                        patch_id: row.get(1)?,
                        epoch: row.get(2)?,
                        anchor_commit: row.get(3)?,
                        fired_at: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// The id of event `(patch_id, epoch)`, created from `ctx` if new. A
    /// concurrent writer that created it first wins; its row is returned.
    pub fn ensure_review_event(
        &self,
        ctx: &FiringContext<'_>,
        patch_id: &str,
        epoch: i64,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO review_events \
             (patch_id, epoch, surface, diff_spec, base_rev, head_rev, anchor_commit) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                patch_id,
                epoch,
                ctx.surface,
                ctx.diff_spec,
                ctx.base_rev,
                ctx.head_rev,
                ctx.anchor_commit,
            ],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM review_events WHERE patch_id = ?1 AND epoch = ?2",
            params![patch_id, epoch],
            |row| row.get(0),
        )?)
    }

    /// Record a batch of firings under one review event. Returns how many rows
    /// were new; a site already recorded for the event is skipped.
    pub fn record_firings(&self, event_id: i64, records: &[FiringRecord<'_>]) -> Result<usize> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut inserted = 0;
        {
            let mut stmt = conn.prepare_cached(
                "INSERT OR IGNORE INTO mechanism_firings \
                 (event_id, mechanism, finding_kind, finding_key, file_path, symbol, snippet, \
                  occurrence, line) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for r in records {
                inserted += stmt.execute(params![
                    event_id,
                    r.mechanism,
                    r.finding_kind,
                    r.finding_key,
                    r.file_path,
                    r.symbol,
                    r.snippet,
                    r.occurrence,
                    r.line,
                ])?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Firings with their review event, oldest first, optionally narrowed to
    /// one mechanism and to rows fired at or after `since` (an ISO-8601 prefix
    /// such as `2026-10-01`).
    pub fn firings(&self, mechanism: Option<&str>, since: Option<&str>) -> Result<Vec<FiringRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT f.id, f.event_id, f.mechanism, f.finding_kind, f.finding_key, f.file_path, \
                    f.line, f.symbol, f.snippet, f.occurrence, e.surface, e.diff_spec, \
                    e.base_rev, e.head_rev, e.anchor_commit, e.patch_id, e.epoch, f.fired_at \
             FROM mechanism_firings f JOIN review_events e ON e.id = f.event_id \
             WHERE (?1 IS NULL OR f.mechanism = ?1) AND (?2 IS NULL OR f.fired_at >= ?2) \
             ORDER BY f.fired_at, f.id",
        )?;
        let rows = stmt.query_map(params![mechanism, since], |row| {
            Ok(FiringRow {
                id: row.get(0)?,
                event_id: row.get(1)?,
                mechanism: row.get(2)?,
                finding_kind: row.get(3)?,
                finding_key: row.get(4)?,
                file_path: row.get(5)?,
                line: row.get(6)?,
                symbol: row.get(7)?,
                snippet: row.get(8)?,
                occurrence: row.get(9)?,
                surface: row.get(10)?,
                diff_spec: row.get(11)?,
                base_rev: row.get(12)?,
                head_rev: row.get(13)?,
                anchor_commit: row.get(14)?,
                patch_id: row.get(15)?,
                epoch: row.get(16)?,
                fired_at: row.get(17)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    /// 0091 carries 0089 rows over: one legacy event per old fingerprint,
    /// duplicate lines in one symbol kept apart by occurrence.
    #[test]
    fn migration_carries_0089_rows_into_events() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(include_str!("../../migrations/0089_mechanism_firings.sql"))
            .expect("0089 applies");
        conn.execute_batch(
            "INSERT INTO mechanism_firings \
               (mechanism, finding_kind, finding_key, file_path, line, symbol, snippet, \
                surface, diff_spec, anchor_commit, diff_fingerprint, fired_at) VALUES \
               ('sp', 'chain', 'k', 'b.rs', 4, 'f', 'x()', 'check', 'HEAD', 'c1', 'fp1', 't1'), \
               ('sp', 'chain', 'k', 'b.rs', 2, 'f', 'x()', 'check', 'HEAD', 'c1', 'fp1', 't1'), \
               ('sp', 'chain', 'k', 'b.rs', 9, 'f', 'x()', 'review', 'staged', 'c2', 'fp2', 't2');",
        )
        .expect("legacy rows");
        conn.execute_batch(include_str!("../../migrations/0091_firing_events.sql"))
            .expect("0091 applies");

        let events: Vec<(String, i64, String)> = conn
            .prepare("SELECT patch_id, epoch, anchor_commit FROM review_events ORDER BY id")
            .and_then(|mut s| {
                s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect()
            })
            .expect("events");
        assert_eq!(
            events,
            vec![
                ("legacy:fp1".to_string(), 0, "c1".to_string()),
                ("legacy:fp2".to_string(), 0, "c2".to_string()),
            ]
        );
        let sites: Vec<(String, i64, i64)> = conn
            .prepare(
                "SELECT e.patch_id, f.line, f.occurrence FROM mechanism_firings f \
                 JOIN review_events e ON e.id = f.event_id ORDER BY e.id, f.line",
            )
            .and_then(|mut s| {
                s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect()
            })
            .expect("sites");
        assert_eq!(
            sites,
            vec![
                ("legacy:fp1".to_string(), 2, 0),
                ("legacy:fp1".to_string(), 4, 1),
                ("legacy:fp2".to_string(), 9, 0),
            ]
        );
    }
}
