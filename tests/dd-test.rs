use std::time::Duration;

use sutra::dd::{DdDelta, DdEngine, DdFacts};

#[test]
fn test_cycle_detection_finds_known_cycle() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    let facts = DdFacts {
        import_edges: vec![(1, 2), (2, 3), (3, 1)],
    };
    engine.ingest(facts).unwrap();
    let cycles = engine.query_cycles().unwrap();
    assert_eq!(cycles.len(), 1);
    assert_eq!(cycles[0].file_ids, vec![1, 2, 3]);
}

#[test]
fn test_no_cycles_in_dag() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    let facts = DdFacts {
        import_edges: vec![(1, 2), (2, 3), (1, 3)],
    };
    engine.ingest(facts).unwrap();
    let cycles = engine.query_cycles().unwrap();
    assert!(cycles.is_empty());
}

#[test]
fn test_delta_update_adds_cycle() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    // Start as DAG
    let facts = DdFacts {
        import_edges: vec![(1, 2), (2, 3)],
    };
    engine.ingest(facts).unwrap();
    assert!(engine.query_cycles().unwrap().is_empty());

    // Add back-edge to create cycle
    engine
        .update(DdDelta {
            added_edges: vec![(3, 1)],
            removed_edges: vec![],
        })
        .unwrap();
    let cycles = engine.query_cycles().unwrap();
    assert_eq!(cycles.len(), 1);
    assert_eq!(cycles[0].file_ids, vec![1, 2, 3]);
}

#[test]
fn test_delta_update_removes_cycle() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    let facts = DdFacts {
        import_edges: vec![(1, 2), (2, 3), (3, 1)],
    };
    engine.ingest(facts).unwrap();
    assert_eq!(engine.query_cycles().unwrap().len(), 1);

    // Remove edge to break cycle
    engine
        .update(DdDelta {
            added_edges: vec![],
            removed_edges: vec![(3, 1)],
        })
        .unwrap();
    assert!(engine.query_cycles().unwrap().is_empty());
}

#[test]
fn test_eviction_round_trip() {
    let engine = DdEngine::new(Duration::from_millis(1));
    let facts = DdFacts {
        import_edges: vec![(1, 2), (2, 3), (3, 1)],
    };
    engine.ingest(facts.clone()).unwrap();
    assert!(engine.is_warm());

    let first = engine.query_cycles().unwrap();

    // Wait for idle timeout
    std::thread::sleep(Duration::from_millis(10));
    assert!(engine.evict_if_idle());
    assert!(!engine.is_warm());

    // Reingest and query again — same results
    engine.ingest(facts).unwrap();
    assert!(engine.is_warm());
    let second = engine.query_cycles().unwrap();
    assert_eq!(first, second);
}

#[test]
fn test_query_on_cold_engine_errors() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    let result = engine.query_cycles();
    assert!(result.is_err());
}

#[test]
fn test_multiple_sccs() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    // Two independent cycles: 1→2→1 and 3→4→3
    let facts = DdFacts {
        import_edges: vec![(1, 2), (2, 1), (3, 4), (4, 3)],
    };
    engine.ingest(facts).unwrap();
    let mut cycles = engine.query_cycles().unwrap();
    cycles.sort_by(|a, b| a.file_ids.cmp(&b.file_ids));
    assert_eq!(cycles.len(), 2);
    assert_eq!(cycles[0].file_ids, vec![1, 2]);
    assert_eq!(cycles[1].file_ids, vec![3, 4]);
}
