//! Storage + migration + invalidation + transaction-failure tests for the
//! health evidence layer (sutra/414).

use sutra::db::{Db, HealthFindingRow};
use sutra::health::BiomarkerKind;
use sutra::health::evidence::{
    ConfigStamp, Digest, Generation, GraphStamp, Head, HistoryObservation, HistoryStamp,
    IndexEpoch, InputStamp, ProducerOutcome, PublishRun, RepositoryStamp, StoredFinding,
    StoredOutcome, UtcDay,
};

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    (dir, db)
}

fn d(seed: &str) -> Digest {
    Digest::of(seed.as_bytes())
}

fn sample_inputs(epoch: IndexEpoch, generation: Generation) -> InputStamp {
    let repo = RepositoryStamp {
        identity: d("repo"),
        head: Head::Commit(d("head")),
        history_boundaries: d("bounds"),
    };
    InputStamp {
        graph: GraphStamp {
            epoch,
            generation,
            parser: d("parser"),
            resolver: d("resolver"),
            indexed_paths: d("paths"),
        },
        history: HistoryObservation::Loaded(HistoryStamp {
            repository: repo,
            day: UtcDay(20000),
            window_days: 90,
            indexed_paths: d("paths"),
            ingestion_version: d("ingv1"),
            ingestion_generation: generation,
        }),
        owners: Ok(ConfigStamp::AbsentDefault(d("owners-default"))),
        rollups: Ok(generation),
        analysis_version: d("analysis-v1"),
    }
}

fn sample_run(epoch: IndexEpoch, generation: Generation) -> PublishRun {
    let finding = HealthFindingRow {
        id: 42,
        file_id: 7,
        symbol_id: Some(9),
        biomarker_kind: BiomarkerKind::NestedComplexity.as_str().to_string(),
        severity: "advisory".to_string(),
        confidence: 1.0,
        provenance: "computed".to_string(),
        metric_value: 6.0,
        threshold: 4.0,
        detail: "deep nesting".to_string(),
    };
    PublishRun {
        index_epoch: epoch,
        graph_generation: generation,
        inputs: sample_inputs(epoch, generation),
        outcomes: vec![
            StoredOutcome {
                file_path: "src/a.rs".to_string(),
                producer: BiomarkerKind::NestedComplexity,
                outcome: ProducerOutcome::Complete { finding_count: 1 },
            },
            StoredOutcome {
                file_path: "src/b.rs".to_string(),
                producer: BiomarkerKind::DeadCodeRatio,
                // A successful empty observation is explicit, not absence.
                outcome: ProducerOutcome::Complete { finding_count: 0 },
            },
        ],
        findings: vec![StoredFinding {
            finding,
            file_path: "src/a.rs".to_string(),
            symbol_label: Some("a::deep".to_string()),
        }],
    }
}

#[test]
fn fresh_index_has_no_epoch_and_no_current_run() {
    // Legacy / freshly-migrated index: no run published, epoch not yet minted.
    // Readers must return None (treated as LegacyUnknown), never a fabricated
    // stamp.
    let (_dir, db) = setup_db();
    assert_eq!(db.index_epoch().unwrap(), None);
    assert!(db.load_current_health_run().unwrap().is_none());
}

#[test]
fn ensure_index_epoch_is_stable_across_calls() {
    let (_dir, db) = setup_db();
    let first = db.ensure_index_epoch().unwrap();
    let second = db.ensure_index_epoch().unwrap();
    assert_eq!(first, second);
    assert_eq!(db.index_epoch().unwrap(), Some(first));
}

#[test]
fn publish_and_load_roundtrips() {
    let (_dir, db) = setup_db();
    let epoch = db.ensure_index_epoch().unwrap();
    let generation = Generation(db.get_data_generation().unwrap());
    let run = sample_run(epoch, generation);

    let run_id = db
        .publish_health_run(generation, &run)
        .unwrap()
        .expect("publication should succeed at the current generation");

    let loaded = db
        .load_current_health_run()
        .unwrap()
        .expect("a current run exists after publication");

    assert_eq!(loaded.id, run_id);
    assert_eq!(loaded.index_epoch, epoch);
    assert_eq!(loaded.graph_generation, generation);
    assert_eq!(loaded.inputs, run.inputs);
    assert_eq!(loaded.outcomes, run.outcomes);
    assert_eq!(loaded.findings, run.findings);
    assert!(!loaded.created_at.is_empty());
}

#[test]
fn stale_generation_publication_is_rejected_and_writes_nothing() {
    let (_dir, db) = setup_db();
    let epoch = db.ensure_index_epoch().unwrap();
    let observed = Generation(db.get_data_generation().unwrap());
    // Simulate a concurrent graph mutation: the run was staged against `observed`
    // but the live generation has moved on.
    let stale = Generation(observed.0 + 1);
    let run = sample_run(epoch, stale);

    let result = db.publish_health_run(stale, &run).unwrap();
    assert!(
        result.is_none(),
        "publication must abort when the input generation no longer matches"
    );
    // Nothing was written: no current run, so no mixed-generation stamp leaks.
    assert!(db.load_current_health_run().unwrap().is_none());
}

#[test]
fn current_pointer_advances_and_old_run_is_retained_for_diagnostics() {
    let (_dir, db) = setup_db();
    let epoch = db.ensure_index_epoch().unwrap();
    let generation = Generation(db.get_data_generation().unwrap());

    let first_id = db
        .publish_health_run(generation, &sample_run(epoch, generation))
        .unwrap()
        .unwrap();

    let mut second = sample_run(epoch, generation);
    second.findings.clear(); // a later, distinct run
    let second_id = db.publish_health_run(generation, &second).unwrap().unwrap();
    assert_ne!(first_id, second_id);

    // Current pointer follows the latest publication.
    let current = db.load_current_health_run().unwrap().unwrap();
    assert_eq!(current.id, second_id);
    assert!(current.findings.is_empty());

    // The invalidated old run remains loadable as diagnostic evidence.
    let old = db
        .load_health_run(first_id)
        .unwrap()
        .expect("old run retained");
    assert_eq!(old.id, first_id);
    assert_eq!(old.findings.len(), 1);
}

#[test]
fn publishing_health_does_not_advance_derived_completion() {
    // Health publication has its own sequence; it must never mark the global
    // derived tier complete (that stays the job of full parse / HRR / components).
    let (_dir, db) = setup_db();
    let epoch = db.ensure_index_epoch().unwrap();
    let generation = Generation(db.get_data_generation().unwrap());
    let before = db.get_derived_complete_generation().unwrap();

    db.publish_health_run(generation, &sample_run(epoch, generation))
        .unwrap()
        .unwrap();

    assert_eq!(
        db.get_derived_complete_generation().unwrap(),
        before,
        "publishing health evidence must not touch derived_complete_generation"
    );
}

#[test]
fn reindex_mints_a_fresh_epoch_and_clears_runs() {
    let (_dir, db) = setup_db();
    let epoch = db.ensure_index_epoch().unwrap();
    let generation = Generation(db.get_data_generation().unwrap());
    db.publish_health_run(generation, &sample_run(epoch, generation))
        .unwrap()
        .unwrap();
    assert!(db.load_current_health_run().unwrap().is_some());

    db.reindex().unwrap();

    // New index lifetime: epoch cleared (remints differently), runs gone.
    let new_epoch = db.ensure_index_epoch().unwrap();
    assert_ne!(new_epoch, epoch);
    assert!(db.load_current_health_run().unwrap().is_none());
}

#[test]
fn run_from_a_prior_epoch_cannot_publish_after_reindex() {
    // A run staged before a reindex must never publish, even when the generation
    // counter has counted back up to the same value in the new epoch. The epoch
    // guard — not just the generation guard — rejects it, so a stale staged run
    // can never replace the current pointer under a fresh index identity.
    let (_dir, db) = setup_db();
    let old_epoch = db.ensure_index_epoch().unwrap();

    db.reindex().unwrap();
    let new_epoch = db.ensure_index_epoch().unwrap();
    assert_ne!(new_epoch, old_epoch);
    let live = Generation(db.get_data_generation().unwrap());

    // Stamped with the *old* epoch but at the *current* live generation, so the
    // generation re-check passes and only the epoch guard can reject it.
    let staged = sample_run(old_epoch, live);
    let result = db.publish_health_run(live, &staged).unwrap();
    assert!(
        result.is_none(),
        "a run from a prior epoch must not publish even at a matching generation"
    );
    assert!(db.load_current_health_run().unwrap().is_none());
}

#[test]
fn payload_generation_disagreeing_with_expected_is_rejected() {
    // The run is stamped at a different generation than the one it is published
    // at: a mis-assembled payload. Reject loudly rather than persist evidence
    // mislabeled with an identity it was never computed against.
    let (_dir, db) = setup_db();
    let epoch = db.ensure_index_epoch().unwrap();
    let generation = Generation(db.get_data_generation().unwrap());
    let run = sample_run(epoch, Generation(generation.0 + 1));

    let err = db.publish_health_run(generation, &run).unwrap_err();
    assert!(
        err.to_string()
            .contains("disagrees with expected generation"),
        "unexpected error: {err}"
    );
    assert!(db.load_current_health_run().unwrap().is_none());
}

#[test]
fn payload_epoch_disagreeing_with_run_epoch_is_rejected() {
    // The input stamp's graph epoch drifts from the run's declared epoch — an
    // internally inconsistent payload that must not be stored.
    let (_dir, db) = setup_db();
    let epoch = db.ensure_index_epoch().unwrap();
    let generation = Generation(db.get_data_generation().unwrap());
    let mut run = sample_run(epoch, generation);
    run.inputs.graph.epoch = IndexEpoch(d("some-other-epoch"));

    let err = db.publish_health_run(generation, &run).unwrap_err();
    assert!(
        err.to_string().contains("epoch disagrees"),
        "unexpected error: {err}"
    );
    assert!(db.load_current_health_run().unwrap().is_none());
}
