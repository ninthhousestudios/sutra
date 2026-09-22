-- Persist the scoring basis and interval bounds of health snapshots (sutra/416).
--
-- A temporal health delta is a measured change only between two complete
-- observations scored under the same rules (health-evidence-contract.md
-- § Comparison and scoring). Snapshots recorded no rules, so adding a waiver (or
-- changing weights/applicability) between two otherwise identical parses read as
-- a measured improvement.
--
-- health_snapshot_files:
--   score_upper  optimistic bound of a partial score (NULL when measured or
--                legacy); `score` holds the conservative lower bound.
--   score_basis  hex digest of scoring::file_score_basis (scoring version,
--                producer versions/weights/caps, applicability, waiver policy).
--                NULL on every row written before sutra/416: Unknown basis, never
--                matching.
-- health_snapshot_components:
--   partial / completeness_recorded  same Complete/Partial/Unknown encoding as
--                the file rows (0076); a component is complete only when every
--                member file is.
--   score_basis  digest over membership, member bases and the aggregation rule.
--                NULL = Unknown (legacy), so a component delta is never measured
--                against a row that recorded no membership.
-- Ephemeral ALTERs (both tables are dropped and recreated on reindex by 0033) —
-- replay after 0033 to re-add the columns.
ALTER TABLE health_snapshot_files ADD COLUMN score_upper REAL;
ALTER TABLE health_snapshot_files ADD COLUMN score_basis TEXT;
ALTER TABLE health_snapshot_components ADD COLUMN partial INTEGER NOT NULL DEFAULT 0;
ALTER TABLE health_snapshot_components ADD COLUMN completeness_recorded INTEGER NOT NULL DEFAULT 0;
ALTER TABLE health_snapshot_components ADD COLUMN score_basis TEXT;
