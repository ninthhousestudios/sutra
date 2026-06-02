use crate::db::{ConventionHistoryRow, Db};
use crate::error::Result;

const TREND_WINDOW: usize = 3;
const MIN_PROMOTE_SUPPORT: i64 = 3;
const MIN_PROMOTE_CONFIDENCE: f64 = 0.9;

#[derive(Debug, PartialEq)]
pub enum Signal {
    PromoteToPreferred {
        convention_id: String,
        rationale: String,
    },
    Deprecate {
        convention_id: String,
        rationale: String,
    },
    ProposeDelete {
        convention_id: String,
        rationale: String,
    },
}

impl Signal {
    pub fn convention_id(&self) -> &str {
        match self {
            Signal::PromoteToPreferred { convention_id, .. } => convention_id,
            Signal::Deprecate { convention_id, .. } => convention_id,
            Signal::ProposeDelete { convention_id, .. } => convention_id,
        }
    }

    pub fn direction(&self) -> &str {
        match self {
            Signal::PromoteToPreferred { .. } => "promote",
            Signal::Deprecate { .. } | Signal::ProposeDelete { .. } => "demote",
        }
    }

    pub fn transition(&self) -> &str {
        match self {
            Signal::PromoteToPreferred { .. } => "descriptive\u{2192}preferred",
            Signal::Deprecate { .. } => "preferred\u{2192}deprecated",
            Signal::ProposeDelete { .. } => "deprecated\u{2192}deleted",
        }
    }

    pub fn rationale(&self) -> &str {
        match self {
            Signal::PromoteToPreferred { rationale, .. } => rationale,
            Signal::Deprecate { rationale, .. } => rationale,
            Signal::ProposeDelete { rationale, .. } => rationale,
        }
    }
}

pub fn detect_signals(db: &Db) -> Result<Vec<Signal>> {
    let conventions = db.all_conventions_merged()?;
    let mut signals = Vec::new();

    for conv in &conventions {
        let history = db.convention_history(&conv.id, TREND_WINDOW * 2)?;
        let recent = dedup_by_snapshot(&history, TREND_WINDOW);
        if recent.len() < TREND_WINDOW {
            continue;
        }

        let lifecycle = conv.lifecycle_state.as_deref().unwrap_or("descriptive");

        match lifecycle {
            "descriptive" => {
                if recent
                    .iter()
                    .all(|h| h.support >= MIN_PROMOTE_SUPPORT && h.confidence >= MIN_PROMOTE_CONFIDENCE)
                {
                    signals.push(Signal::PromoteToPreferred {
                        convention_id: conv.id.clone(),
                        rationale: format!(
                            "stable high support ({}) and confidence ({:.0}%) over {} snapshots",
                            recent[0].support,
                            recent[0].confidence * 100.0,
                            TREND_WINDOW,
                        ),
                    });
                }
            }
            "preferred" => {
                if is_declining_support(&recent) {
                    signals.push(Signal::Deprecate {
                        convention_id: conv.id.clone(),
                        rationale: format!(
                            "support declining: {} \u{2192} {} over {} snapshots",
                            recent.last().unwrap().support,
                            recent[0].support,
                            TREND_WINDOW,
                        ),
                    });
                }
            }
            "deprecated" => {
                if recent.iter().all(|h| h.support == 0) {
                    signals.push(Signal::ProposeDelete {
                        convention_id: conv.id.clone(),
                        rationale: format!(
                            "zero matching symbols for {} consecutive snapshots",
                            TREND_WINDOW,
                        ),
                    });
                }
            }
            _ => {}
        }
    }

    Ok(signals)
}

pub fn generate_proposals(db: &Db, signals: Vec<Signal>) -> Result<Vec<i64>> {
    let mut created = Vec::new();
    for signal in &signals {
        let conv_id = signal.convention_id();
        let direction = signal.direction();

        if db.has_pending_proposal(conv_id)? {
            continue;
        }

        if db.dismissed_proposal_for_convention(conv_id, direction)? {
            continue;
        }

        let id = db.create_proposal(
            conv_id,
            signal.transition(),
            signal.rationale(),
            direction,
        )?;
        created.push(id);
    }
    Ok(created)
}

fn dedup_by_snapshot<'a>(
    history: &'a [ConventionHistoryRow],
    limit: usize,
) -> Vec<&'a ConventionHistoryRow> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for h in history {
        if seen.insert(&h.snapshot_id) {
            result.push(h);
            if result.len() >= limit {
                break;
            }
        }
    }
    result
}

fn is_declining_support(snapshots: &[&ConventionHistoryRow]) -> bool {
    if snapshots.len() < 2 {
        return false;
    }
    // snapshots are most-recent-first; declining means each older one has more support
    for i in 0..snapshots.len() - 1 {
        if snapshots[i].support >= snapshots[i + 1].support {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn setup_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_unchecked("lifecycle_test", dir.path()).unwrap();
        (db, dir)
    }

    fn insert_convention(db: &Db, id: &str) {
        db.upsert_convention(id, "kind:function", "has_return_type", 5, 0.95, None)
            .unwrap();
    }

    fn record_snapshots(db: &Db, convention_id: &str, entries: &[(i64, f64)]) {
        for (i, (support, confidence)) in entries.iter().enumerate() {
            let snapshot_id = format!("snap-{i}");
            db.record_convention_history(convention_id, *support, *confidence, &snapshot_id)
                .unwrap();
        }
    }

    #[test]
    fn stable_high_support_proposes_promotion() {
        let (db, _dir) = setup_db();
        insert_convention(&db, "conv-1");
        record_snapshots(&db, "conv-1", &[(5, 0.95), (6, 0.92), (5, 0.91)]);

        let signals = detect_signals(&db).unwrap();
        assert_eq!(signals.len(), 1);
        assert!(matches!(&signals[0], Signal::PromoteToPreferred { convention_id, .. } if convention_id == "conv-1"));
    }

    #[test]
    fn insufficient_snapshots_no_proposal() {
        let (db, _dir) = setup_db();
        insert_convention(&db, "conv-1");
        record_snapshots(&db, "conv-1", &[(5, 0.95), (6, 0.92)]);

        let signals = detect_signals(&db).unwrap();
        assert!(signals.is_empty());
    }

    #[test]
    fn low_support_no_promotion() {
        let (db, _dir) = setup_db();
        insert_convention(&db, "conv-1");
        record_snapshots(&db, "conv-1", &[(2, 0.95), (2, 0.92), (2, 0.91)]);

        let signals = detect_signals(&db).unwrap();
        assert!(signals.is_empty());
    }

    #[test]
    fn low_confidence_no_promotion() {
        let (db, _dir) = setup_db();
        insert_convention(&db, "conv-1");
        record_snapshots(&db, "conv-1", &[(5, 0.85), (6, 0.88), (5, 0.80)]);

        let signals = detect_signals(&db).unwrap();
        assert!(signals.is_empty());
    }

    #[test]
    fn declining_support_on_preferred_proposes_deprecation() {
        let (db, _dir) = setup_db();
        insert_convention(&db, "conv-1");
        db.set_convention_lifecycle("conv-1", "preferred", None)
            .unwrap();
        // Inserted chronologically: oldest first → newest last
        // DB returns by recorded_at DESC (or id DESC for same timestamp)
        // So snap-2 (newest, support=2) comes first in query results
        // Declining: newest support < older support
        record_snapshots(&db, "conv-1", &[(6, 0.95), (4, 0.95), (2, 0.95)]);

        let signals = detect_signals(&db).unwrap();
        assert_eq!(signals.len(), 1);
        assert!(matches!(&signals[0], Signal::Deprecate { convention_id, .. } if convention_id == "conv-1"));
    }

    #[test]
    fn flat_support_on_preferred_no_deprecation() {
        let (db, _dir) = setup_db();
        insert_convention(&db, "conv-1");
        db.set_convention_lifecycle("conv-1", "preferred", None)
            .unwrap();
        record_snapshots(&db, "conv-1", &[(5, 0.95), (5, 0.95), (5, 0.95)]);

        let signals = detect_signals(&db).unwrap();
        assert!(signals.is_empty());
    }

    #[test]
    fn zero_support_on_deprecated_proposes_deletion() {
        let (db, _dir) = setup_db();
        insert_convention(&db, "conv-1");
        db.set_convention_lifecycle("conv-1", "deprecated", None)
            .unwrap();
        record_snapshots(&db, "conv-1", &[(0, 0.0), (0, 0.0), (0, 0.0)]);

        let signals = detect_signals(&db).unwrap();
        assert_eq!(signals.len(), 1);
        assert!(matches!(&signals[0], Signal::ProposeDelete { convention_id, .. } if convention_id == "conv-1"));
    }

    #[test]
    fn nonzero_support_on_deprecated_no_deletion() {
        let (db, _dir) = setup_db();
        insert_convention(&db, "conv-1");
        db.set_convention_lifecycle("conv-1", "deprecated", None)
            .unwrap();
        record_snapshots(&db, "conv-1", &[(0, 0.0), (1, 0.5), (0, 0.0)]);

        let signals = detect_signals(&db).unwrap();
        assert!(signals.is_empty());
    }

    #[test]
    fn dismissed_proposal_blocks_same_direction() {
        let (db, _dir) = setup_db();
        insert_convention(&db, "conv-1");
        record_snapshots(&db, "conv-1", &[(5, 0.95), (6, 0.92), (5, 0.91)]);

        let signals = detect_signals(&db).unwrap();
        assert_eq!(signals.len(), 1);

        let created = generate_proposals(&db, signals).unwrap();
        assert_eq!(created.len(), 1);

        // Dismiss the proposal
        db.resolve_proposal(created[0], "dismissed").unwrap();

        // Re-detect — same signal still fires
        let signals = detect_signals(&db).unwrap();
        assert_eq!(signals.len(), 1);

        // But generate_proposals skips it
        let created = generate_proposals(&db, signals).unwrap();
        assert!(created.is_empty());
    }

    #[test]
    fn pending_proposal_blocks_duplicate() {
        let (db, _dir) = setup_db();
        insert_convention(&db, "conv-1");
        record_snapshots(&db, "conv-1", &[(5, 0.95), (6, 0.92), (5, 0.91)]);

        let signals = detect_signals(&db).unwrap();
        let created = generate_proposals(&db, signals).unwrap();
        assert_eq!(created.len(), 1);

        // Re-detect and try again
        let signals = detect_signals(&db).unwrap();
        let created = generate_proposals(&db, signals).unwrap();
        assert!(created.is_empty());
    }

    #[test]
    fn accept_proposal_applies_lifecycle_transition() {
        let (db, _dir) = setup_db();
        insert_convention(&db, "conv-1");
        record_snapshots(&db, "conv-1", &[(5, 0.95), (6, 0.92), (5, 0.91)]);

        let signals = detect_signals(&db).unwrap();
        let created = generate_proposals(&db, signals).unwrap();
        let proposal_id = created[0];

        let proposal = db.get_proposal(proposal_id).unwrap().unwrap();
        assert_eq!(proposal.status, "pending");

        // Accept: apply the transition
        db.set_convention_lifecycle("conv-1", "preferred", Some("accepted proposal"))
            .unwrap();
        db.resolve_proposal(proposal_id, "accepted").unwrap();

        let proposal = db.get_proposal(proposal_id).unwrap().unwrap();
        assert_eq!(proposal.status, "accepted");
        assert!(proposal.resolved_at.is_some());

        let state = db.get_convention_state("conv-1").unwrap().unwrap();
        assert_eq!(state.lifecycle_state, "preferred");
    }

    #[test]
    fn dedup_by_snapshot_takes_first_per_snapshot() {
        let entries = vec![
            ConventionHistoryRow {
                id: 1, convention_id: "c".into(), support: 5, confidence: 0.9,
                snapshot_id: "s1".into(), recorded_at: "2026-01-03".into(),
            },
            ConventionHistoryRow {
                id: 2, convention_id: "c".into(), support: 3, confidence: 0.8,
                snapshot_id: "s1".into(), recorded_at: "2026-01-03".into(),
            },
            ConventionHistoryRow {
                id: 3, convention_id: "c".into(), support: 4, confidence: 0.85,
                snapshot_id: "s2".into(), recorded_at: "2026-01-02".into(),
            },
        ];

        let deduped = dedup_by_snapshot(&entries, 5);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].snapshot_id, "s1");
        assert_eq!(deduped[1].snapshot_id, "s2");
    }

    #[test]
    fn is_declining_detects_strictly_decreasing() {
        let make = |support| ConventionHistoryRow {
            id: 0, convention_id: "c".into(), support, confidence: 0.9,
            snapshot_id: "s".into(), recorded_at: "".into(),
        };
        let h1 = make(2);
        let h2 = make(4);
        let h3 = make(6);
        // newest first: 2, 4, 6 — each older one is larger → declining
        assert!(is_declining_support(&[&h1, &h2, &h3]));
        // flat: not declining
        let h4 = make(4);
        assert!(!is_declining_support(&[&h4, &h2, &h3]));
        // increasing: not declining
        assert!(!is_declining_support(&[&h3, &h2, &h1]));
    }
}
