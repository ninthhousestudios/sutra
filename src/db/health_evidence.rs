//! Immutable health-evidence storage (sutra/414): the index epoch, atomic
//! publication of one health run + its current pointer, and coherent reads.
//!
//! The run's `InputStamp`, per-producer outcomes and retained findings are stored
//! as JSON blobs (see `0075_health_evidence_runs.sql` for why the contract wants
//! them FK-free). This layer returns `crate::error::SutraError`; the future
//! health module (sutra/415) wraps the generation-race signal into
//! `HealthError::InputsChanged`.

use rusqlite::{OptionalExtension, params};

use super::Db;
use crate::error::{Result, SutraError};
use crate::health::evidence::{
    Digest, Generation, IndexEpoch, PublishRun, RunId, StoredFinding, StoredOutcome, StoredRun,
};

fn invalid(context: &str, e: impl std::fmt::Display) -> SutraError {
    SutraError::Internal(format!(
        "invalid persisted health evidence ({context}): {e}"
    ))
}

impl Db {
    /// Read the persisted index epoch, or `None` when none has been minted yet
    /// (a legacy or freshly-reindexed index). A present-but-unparseable value is
    /// an error, not a silent `None` — a corrupt stamp must never masquerade as
    /// "not yet minted".
    pub fn index_epoch(&self) -> Result<Option<IndexEpoch>> {
        let conn = self.conn.lock();
        let hex: Option<String> =
            conn.query_row("SELECT index_epoch FROM index_meta WHERE id = 1", [], |r| {
                r.get(0)
            })?;
        match hex {
            None => Ok(None),
            Some(h) => Digest::from_hex(&h)
                .map(|d| Some(IndexEpoch(d)))
                .ok_or_else(|| {
                    invalid("index_epoch", format!("`{h}` is not a 32-byte hex digest"))
                }),
        }
    }

    /// Return the index epoch, minting one if absent. Idempotent under
    /// concurrency: the mint only writes when the column is still NULL, then
    /// re-reads the winner.
    pub fn ensure_index_epoch(&self) -> Result<IndexEpoch> {
        if let Some(epoch) = self.index_epoch()? {
            return Ok(epoch);
        }
        // Uniqueness per index lifetime: blake3 of the current wall-clock nanos.
        // Reindexes are seconds apart, so distinct nanos give distinct epochs.
        let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let epoch = IndexEpoch(Digest::of(&nanos.to_le_bytes()));
        {
            let conn = self.conn.lock();
            conn.execute(
                "UPDATE index_meta SET index_epoch = ?1 WHERE id = 1 AND index_epoch IS NULL",
                params![epoch.0.to_hex()],
            )?;
        }
        self.index_epoch()?
            .ok_or_else(|| SutraError::Internal("failed to mint index epoch".into()))
    }

    /// Publish one immutable health run and advance the current pointer in a
    /// single transaction — but only if `data_generation` still equals
    /// `expected_generation`, the value observed when the inputs were staged. A
    /// concurrent mutation aborts publication with `Ok(None)` and writes nothing;
    /// the caller maps that to `HealthError::InputsChanged`. This is the storage
    /// guard against a run publishing a complete stamp over mixed-generation
    /// inputs.
    pub fn publish_health_run(
        &self,
        expected_generation: Generation,
        run: &PublishRun,
    ) -> Result<Option<RunId>> {
        let input_stamp =
            serde_json::to_string(&run.inputs).map_err(|e| invalid("serialize inputs", e))?;
        let outcomes =
            serde_json::to_string(&run.outcomes).map_err(|e| invalid("serialize outcomes", e))?;
        let findings =
            serde_json::to_string(&run.findings).map_err(|e| invalid("serialize findings", e))?;
        let created_at = chrono::Utc::now().to_rfc3339();
        let epoch_hex = run.index_epoch.0.to_hex();

        // Payload self-consistency: the run must be assembled at a single index
        // identity. A stamp whose declared generation or epoch disagrees with the
        // generation being published is a caller bug — reject loudly rather than
        // persist evidence mislabeled with an identity it wasn't computed against.
        if run.graph_generation != expected_generation {
            return Err(invalid(
                "publish_health_run",
                format!(
                    "run graph_generation {} disagrees with expected generation {}",
                    run.graph_generation.0, expected_generation.0
                ),
            ));
        }
        if run.inputs.graph.generation != expected_generation {
            return Err(invalid(
                "publish_health_run",
                format!(
                    "input-stamp graph generation {} disagrees with expected generation {}",
                    run.inputs.graph.generation.0, expected_generation.0
                ),
            ));
        }
        if run.inputs.graph.epoch != run.index_epoch {
            return Err(invalid(
                "publish_health_run",
                "input-stamp graph epoch disagrees with the run's index_epoch",
            ));
        }

        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;

        // Re-check the live index identity under the write lock. A reindex resets
        // the generation counter AND remints the epoch, so both must still match:
        // a run staged before a reindex must never publish just because the
        // counter counted back up to the same value in a new epoch. If either
        // moved, drop the transaction (rollback) and report the race — no run row,
        // no pointer move, so a reader can never observe a mixed-generation or
        // mixed-epoch complete stamp.
        let (current_generation, current_epoch): (i64, Option<String>) = conn.query_row(
            "SELECT data_generation, index_epoch FROM index_meta WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if current_generation != expected_generation.0 {
            return Ok(None);
        }
        if current_epoch.as_deref() != Some(epoch_hex.as_str()) {
            return Ok(None);
        }

        conn.execute(
            "INSERT INTO health_runs
             (index_epoch, graph_generation, created_at, input_stamp, outcomes, findings)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                epoch_hex,
                run.graph_generation.0,
                created_at,
                input_stamp,
                outcomes,
                findings
            ],
        )?;
        let run_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO health_current (id, run_id, index_epoch) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET run_id = ?1, index_epoch = ?2",
            params![run_id, epoch_hex],
        )?;

        tx.commit()?;
        Ok(Some(RunId(run_id)))
    }

    /// Load the current health run — pointer, outcomes and findings — in one read
    /// transaction, or `None` when no run has been published (legacy/unknown).
    pub fn load_current_health_run(&self) -> Result<Option<StoredRun>> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let row = conn
            .query_row(
                "SELECT r.run_id, r.index_epoch, r.graph_generation, r.created_at,
                        r.input_stamp, r.outcomes, r.findings
                 FROM health_current c
                 JOIN health_runs r ON r.run_id = c.run_id
                 WHERE c.id = 1",
                [],
                map_run_row,
            )
            .optional()?;
        tx.commit()?;
        row.map(decode_run).transpose()
    }

    /// Load a specific run by id, for diagnostic / retained-evidence display. An
    /// invalidated old run remains loadable here even after the current pointer
    /// has advanced past it.
    pub fn load_health_run(&self, run_id: RunId) -> Result<Option<StoredRun>> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT run_id, index_epoch, graph_generation, created_at,
                        input_stamp, outcomes, findings
                 FROM health_runs WHERE run_id = ?1",
                params![run_id.0],
                map_run_row,
            )
            .optional()?;
        row.map(decode_run).transpose()
    }
}

/// Raw column tuple for a `health_runs` row.
type RawRun = (i64, String, i64, String, String, String, String);

fn map_run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRun> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}

fn decode_run(raw: RawRun) -> Result<StoredRun> {
    let (run_id, epoch_hex, graph_generation, created_at, input_stamp, outcomes, findings) = raw;
    let index_epoch = Digest::from_hex(&epoch_hex)
        .map(IndexEpoch)
        .ok_or_else(|| invalid("run index_epoch", format!("`{epoch_hex}` is not hex")))?;
    let inputs = serde_json::from_str(&input_stamp).map_err(|e| invalid("input_stamp", e))?;
    let outcomes: Vec<StoredOutcome> =
        serde_json::from_str(&outcomes).map_err(|e| invalid("outcomes", e))?;
    let findings: Vec<StoredFinding> =
        serde_json::from_str(&findings).map_err(|e| invalid("findings", e))?;
    Ok(StoredRun {
        id: RunId(run_id),
        index_epoch,
        graph_generation: Generation(graph_generation),
        inputs,
        outcomes,
        findings,
        created_at,
    })
}
