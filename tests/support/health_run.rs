//! Shared test support: publish seeded health findings as a current run.
//!
//! Scoring reads only published runs (sutra/416), so tests that seed findings
//! directly must also publish the run a full parse would have staged.

use sutra::db::Db;
use sutra::health::BiomarkerKind;
use sutra::health::assess::RunVerdict;
use sutra::health::evidence::{ProducerOutcome, UnsupportedReason, Validity};
use sutra::health::scoring::PERSISTENT_PRODUCERS;

/// Publish the live `health_findings` as a current run in which every persistent
/// producer is `Complete` for every file (coverage gradient unsupported), unless
/// `override_outcome` says otherwise — what a successful full parse would stage.
pub fn publish_run_with(
    db: &Db,
    override_outcome: impl Fn(&str, BiomarkerKind) -> Option<ProducerOutcome>,
) {
    use sutra::health::evidence::{
        ConfigStamp, Digest, Generation, GraphStamp, HistoryObservation, InputStamp, PublishRun,
        StoredFinding, StoredOutcome,
    };
    let files = db.all_files().unwrap();
    let rows = db.get_health_findings(None, None).unwrap();
    let path_of = |id: i64| {
        files
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.path.to_string())
            .unwrap()
    };
    let ids: Vec<i64> = rows.iter().filter_map(|r| r.symbol_id).collect();
    let labels = db.symbol_labels(&ids).unwrap();
    let mut outcomes = Vec::new();
    for f in &files {
        for kind in PERSISTENT_PRODUCERS {
            let count = rows
                .iter()
                .filter(|r| r.file_id == f.id && r.biomarker_kind == kind.as_str())
                .count();
            let default = if kind == BiomarkerKind::CoverageGradient {
                ProducerOutcome::Unsupported(UnsupportedReason::NoCoverageIngestion)
            } else {
                ProducerOutcome::Complete {
                    finding_count: count,
                }
            };
            outcomes.push(StoredOutcome {
                file_path: f.path.to_string(),
                producer: kind,
                outcome: override_outcome(&f.path, kind).unwrap_or(default),
            });
        }
    }
    let findings = rows
        .into_iter()
        .map(|r| StoredFinding {
            file_path: path_of(r.file_id),
            symbol_label: r.symbol_id.and_then(|s| labels.get(&s).cloned()),
            finding: r,
        })
        .collect();
    let epoch = db.ensure_index_epoch().unwrap();
    let generation = Generation(db.get_data_generation().unwrap());
    let d = |s: &str| Digest::of(s.as_bytes());
    let run = PublishRun {
        index_epoch: epoch,
        graph_generation: generation,
        inputs: InputStamp {
            graph: GraphStamp {
                epoch,
                generation,
                parser: d("parser"),
                resolver: d("resolver"),
                indexed_paths: d("paths"),
            },
            history: HistoryObservation::Unsupported {
                absence_probe: d("none"),
            },
            owners: Ok(ConfigStamp::AbsentDefault(d("owners"))),
            rollups: Ok(generation),
            analysis_version: d("analysis"),
        },
        outcomes,
        findings,
    };
    db.publish_health_run(generation, &run)
        .unwrap()
        .expect("seeded run publishes at the current generation");
}

pub fn publish_seeded_run(db: &Db) {
    publish_run_with(db, |_, _| None);
}

/// A `Current` verdict for whichever run is current now — what a successful
/// refresh would vouch for.
pub fn current_verdict(db: &Db) -> RunVerdict {
    RunVerdict {
        run: db.load_current_health_run().unwrap().map(|r| r.id),
        validity: Validity::Current,
    }
}
