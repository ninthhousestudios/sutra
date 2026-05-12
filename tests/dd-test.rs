use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use sutra::dd::{DdDelta, DdEngine, DdFacts};
use sutra::rules::ForbiddenDep;

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
    engine.ingest(facts).unwrap();
    let first = engine.query_cycles().unwrap();
    assert!(engine.is_warm());

    // Wait for idle timeout
    std::thread::sleep(Duration::from_millis(10));
    assert!(engine.evict_if_idle());
    assert!(!engine.is_warm());
    assert!(engine.is_loaded());

    // Query again without re-ingesting — auto-warms from stored facts
    let second = engine.query_cycles().unwrap();
    assert!(engine.is_warm());
    assert_eq!(first, second);
}

#[test]
fn test_query_on_cold_engine_errors() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    let result = engine.query_cycles();
    assert!(result.is_err());
}

#[test]
fn test_lazy_population() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    let facts = DdFacts {
        import_edges: vec![(1, 2), (2, 3), (3, 1)],
    };
    engine.ingest(facts).unwrap();
    // After ingest, engine is loaded but not warm (no worker spawned)
    assert!(!engine.is_warm());
    assert!(engine.is_loaded());

    // First query auto-warms
    let cycles = engine.query_cycles().unwrap();
    assert!(engine.is_warm());
    assert_eq!(cycles.len(), 1);
}

#[test]
fn test_update_on_loaded_state() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    let facts = DdFacts {
        import_edges: vec![(1, 2), (2, 3)],
    };
    engine.ingest(facts).unwrap();
    assert!(!engine.is_warm());

    // Update while loaded (before any query)
    engine
        .update(DdDelta {
            added_edges: vec![(3, 1)],
            removed_edges: vec![],
        })
        .unwrap();

    // Query should reflect the update
    let cycles = engine.query_cycles().unwrap();
    assert_eq!(cycles.len(), 1);
    assert_eq!(cycles[0].file_ids, vec![1, 2, 3]);
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

#[test]
fn test_blast_radius_simple_chain() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    // 1→2→3: 1 imports 2, 2 imports 3
    let facts = DdFacts {
        import_edges: vec![(1, 2), (2, 3)],
    };
    engine.ingest(facts).unwrap();
    // Node 3: reachable from 1 and 2 → blast_radius = 2
    assert_eq!(engine.query_blast_radius(3).unwrap(), 2);
    // Node 2: reachable from 1 → blast_radius = 1
    assert_eq!(engine.query_blast_radius(2).unwrap(), 1);
    // Node 1: not reachable from anyone → blast_radius = 0
    assert_eq!(engine.query_blast_radius(1).unwrap(), 0);
}

#[test]
fn test_blast_radius_all() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    let facts = DdFacts {
        import_edges: vec![(1, 2), (2, 3), (1, 3)],
    };
    engine.ingest(facts).unwrap();
    let all = engine.query_blast_radius_all().unwrap();
    assert_eq!(all.get(&3).copied().unwrap_or(0), 2); // 1 and 2 reach 3
    assert_eq!(all.get(&2).copied().unwrap_or(0), 1); // only 1 reaches 2
    assert_eq!(all.get(&1).copied().unwrap_or(0), 0); // nothing reaches 1
}

#[test]
fn test_blast_radius_delta_update() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    let facts = DdFacts {
        import_edges: vec![(1, 2), (2, 3)],
    };
    engine.ingest(facts).unwrap();
    assert_eq!(engine.query_blast_radius(3).unwrap(), 2);

    // Add edge 3→4: now 1,2,3 all transitively reach 4
    engine
        .update(DdDelta {
            added_edges: vec![(3, 4)],
            removed_edges: vec![],
        })
        .unwrap();
    assert_eq!(engine.query_blast_radius(4).unwrap(), 3);

    // Remove edge 1→2: now only 2→3→4 path exists
    engine
        .update(DdDelta {
            added_edges: vec![],
            removed_edges: vec![(1, 2)],
        })
        .unwrap();
    assert_eq!(engine.query_blast_radius(3).unwrap(), 1); // only 2
    assert_eq!(engine.query_blast_radius(4).unwrap(), 2); // 2 and 3
}

#[test]
fn test_blast_radius_matches_imperative() {
    fn imperative_blast_radius(edges: &[(i64, i64)]) -> HashMap<i64, usize> {
        let mut fan_in: HashMap<i64, HashSet<i64>> = HashMap::new();
        let mut all_nodes: HashSet<i64> = HashSet::new();
        for &(src, dst) in edges {
            fan_in.entry(dst).or_default().insert(src);
            all_nodes.insert(src);
            all_nodes.insert(dst);
        }

        let mut result = HashMap::new();
        for &node in &all_nodes {
            let mut visited = HashSet::new();
            visited.insert(node);
            let mut queue = VecDeque::new();
            if let Some(deps) = fan_in.get(&node) {
                for &dep in deps {
                    if visited.insert(dep) {
                        queue.push_back(dep);
                    }
                }
            }
            while let Some(current) = queue.pop_front() {
                if let Some(deps) = fan_in.get(&current) {
                    for &dep in deps {
                        if visited.insert(dep) {
                            queue.push_back(dep);
                        }
                    }
                }
            }
            let blast = visited.len() - 1;
            if blast > 0 {
                result.insert(node, blast);
            }
        }
        result
    }

    let edges = vec![(1, 2), (2, 3), (1, 3), (3, 4), (4, 5), (2, 5)];
    let expected = imperative_blast_radius(&edges);

    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: edges,
        })
        .unwrap();
    let actual = engine.query_blast_radius_all().unwrap();

    for node in 1..=5i64 {
        assert_eq!(
            actual.get(&node).copied().unwrap_or(0),
            expected.get(&node).copied().unwrap_or(0),
            "blast radius mismatch for node {node}"
        );
    }
}

#[test]
fn test_forbidden_deps_detects_violation() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    // Edge 1→2 where 1="src/tools/foo.rs", 2="src/daemon.rs"
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2), (2, 3)],
        })
        .unwrap();

    let paths: HashMap<i64, String> = [
        (1, "src/tools/foo.rs".into()),
        (2, "src/daemon.rs".into()),
        (3, "src/lib.rs".into()),
    ]
    .into();

    let rules = vec![ForbiddenDep {
        from: "src/tools/*".into(),
        to: "src/daemon.rs".into(),
    }];

    let violations = engine.query_forbidden_deps(&rules, &paths).unwrap();
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].from_id, 1);
    assert_eq!(violations[0].to_id, 2);
    assert_eq!(violations[0].rule_from, "src/tools/*");
    assert_eq!(violations[0].rule_to, "src/daemon.rs");
}

#[test]
fn test_forbidden_deps_clean_graph() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2), (2, 3)],
        })
        .unwrap();

    let paths: HashMap<i64, String> = [
        (1, "src/models/user.rs".into()),
        (2, "src/db.rs".into()),
        (3, "src/lib.rs".into()),
    ]
    .into();

    let rules = vec![ForbiddenDep {
        from: "src/tools/*".into(),
        to: "src/daemon.rs".into(),
    }];

    let violations = engine.query_forbidden_deps(&rules, &paths).unwrap();
    assert!(violations.is_empty());
}

#[test]
fn test_forbidden_deps_delta_update() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2)],
        })
        .unwrap();

    let paths: HashMap<i64, String> = [
        (1, "src/models/user.rs".into()),
        (2, "src/db.rs".into()),
        (3, "src/tools/parse.rs".into()),
        (4, "src/daemon.rs".into()),
    ]
    .into();

    let rules = vec![ForbiddenDep {
        from: "src/tools/*".into(),
        to: "src/daemon.rs".into(),
    }];

    // Initially no violations
    assert!(engine.query_forbidden_deps(&rules, &paths).unwrap().is_empty());

    // Add forbidden edge
    engine
        .update(DdDelta {
            added_edges: vec![(3, 4)],
            removed_edges: vec![],
        })
        .unwrap();
    let violations = engine.query_forbidden_deps(&rules, &paths).unwrap();
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].from_id, 3);
    assert_eq!(violations[0].to_id, 4);

    // Remove the forbidden edge
    engine
        .update(DdDelta {
            added_edges: vec![],
            removed_edges: vec![(3, 4)],
        })
        .unwrap();
    assert!(engine.query_forbidden_deps(&rules, &paths).unwrap().is_empty());
}

#[test]
fn test_forbidden_deps_glob_patterns() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2), (3, 2), (4, 2)],
        })
        .unwrap();

    let paths: HashMap<i64, String> = [
        (1, "src/tools/parse.rs".into()),
        (2, "src/internal/secret.rs".into()),
        (3, "src/tools/sub/deep.rs".into()),
        (4, "tests/helper.rs".into()),
    ]
    .into();

    // Single * doesn't match path separators
    let rules = vec![ForbiddenDep {
        from: "src/tools/*".into(),
        to: "src/internal/*".into(),
    }];
    let violations = engine.query_forbidden_deps(&rules, &paths).unwrap();
    // Only file 1 matches "src/tools/*" (single level), not file 3 (nested)
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].from_id, 1);

    // ** matches nested paths
    let rules = vec![ForbiddenDep {
        from: "src/tools/**".into(),
        to: "src/internal/*".into(),
    }];
    let violations = engine.query_forbidden_deps(&rules, &paths).unwrap();
    assert_eq!(violations.len(), 2);
}
