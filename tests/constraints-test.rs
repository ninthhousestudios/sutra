#![allow(deprecated)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use sutra::constraints::{DdDelta, DdEngine, DdFacts};
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
fn test_blast_radius_matches_unbounded_reachability() {
    // DD computes unbounded transitive reachability — the true blast radius.
    // This differs from bfs_blast_radius() in graph.rs which caps at depth 3.
    fn unbounded_reachability(edges: &[(i64, i64)]) -> HashMap<i64, usize> {
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
    let expected = unbounded_reachability(&edges);

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
fn test_blast_radius_deep_chain() {
    // Chain deeper than 3 hops: DD should count all transitive dependents,
    // unlike the depth-3-bounded bfs_blast_radius in graph.rs.
    let engine = DdEngine::new(Duration::from_secs(1800));
    // 1→2→3→4→5→6
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2), (2, 3), (3, 4), (4, 5), (5, 6)],
        })
        .unwrap();

    // Node 6: all 5 predecessors reach it transitively
    assert_eq!(engine.query_blast_radius(6).unwrap(), 5);
    // Node 5: 4 predecessors (1,2,3,4)
    assert_eq!(engine.query_blast_radius(5).unwrap(), 4);
    // Node 1: nothing reaches it
    assert_eq!(engine.query_blast_radius(1).unwrap(), 0);
}

#[test]
fn test_forbidden_deps_detects_violation() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    // Edge 1→2 where 1="src/tools/foo.rs", 2="src/server.rs"
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2), (2, 3)],
        })
        .unwrap();

    let paths: HashMap<i64, String> = [
        (1, "src/tools/foo.rs".into()),
        (2, "src/server.rs".into()),
        (3, "src/lib.rs".into()),
    ]
    .into();

    let rules = vec![ForbiddenDep {
        from: "src/tools/*".into(),
        to: "src/server.rs".into(),
    }];

    let violations = engine.query_forbidden_deps(&rules, &paths).unwrap();
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].from_id, 1);
    assert_eq!(violations[0].to_id, 2);
    assert_eq!(violations[0].rule_from, "src/tools/*");
    assert_eq!(violations[0].rule_to, "src/server.rs");
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
        to: "src/server.rs".into(),
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
        (4, "src/server.rs".into()),
    ]
    .into();

    let rules = vec![ForbiddenDep {
        from: "src/tools/*".into(),
        to: "src/server.rs".into(),
    }];

    // Initially no violations
    assert!(
        engine
            .query_forbidden_deps(&rules, &paths)
            .unwrap()
            .is_empty()
    );

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
    assert!(
        engine
            .query_forbidden_deps(&rules, &paths)
            .unwrap()
            .is_empty()
    );
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

#[test]
fn test_forbidden_deps_invalid_glob_errors() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2)],
        })
        .unwrap();

    let paths: HashMap<i64, String> = [(1, "src/a.rs".into()), (2, "src/b.rs".into())].into();

    let rules = vec![ForbiddenDep {
        from: "src/[invalid".into(),
        to: "src/*".into(),
    }];
    let result = engine.query_forbidden_deps(&rules, &paths);
    assert!(result.is_err(), "invalid glob should produce an error");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("invalid glob"),
        "error should identify the problem: {msg}"
    );
}

// --- Maintained view tests (set_forbidden_pairs + query_violations) ---

#[test]
fn test_violations_matching_edge() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2), (2, 3)],
        })
        .unwrap();
    engine.set_forbidden_pairs(vec![(1, 2)]).unwrap();
    let violations = engine.query_violations().unwrap();
    assert_eq!(violations, vec![(1, 2)]);
}

#[test]
fn test_violations_no_match() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2)],
        })
        .unwrap();
    engine.set_forbidden_pairs(vec![(3, 4)]).unwrap();
    let violations = engine.query_violations().unwrap();
    assert!(violations.is_empty());
}

#[test]
fn test_violations_remove_edge_clears() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2), (2, 3)],
        })
        .unwrap();
    engine.set_forbidden_pairs(vec![(1, 2)]).unwrap();
    assert_eq!(engine.query_violations().unwrap(), vec![(1, 2)]);

    engine
        .update(DdDelta {
            added_edges: vec![],
            removed_edges: vec![(1, 2)],
        })
        .unwrap();
    assert!(engine.query_violations().unwrap().is_empty());
}

#[test]
fn test_violations_change_forbidden_pairs() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2), (3, 4)],
        })
        .unwrap();
    engine.set_forbidden_pairs(vec![(1, 2)]).unwrap();
    assert_eq!(engine.query_violations().unwrap(), vec![(1, 2)]);

    engine.set_forbidden_pairs(vec![(3, 4)]).unwrap();
    assert_eq!(engine.query_violations().unwrap(), vec![(3, 4)]);
}

#[test]
fn test_violations_survive_eviction_and_rewarm() {
    let engine = DdEngine::new(Duration::from_millis(1));
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2), (2, 3)],
        })
        .unwrap();
    engine.set_forbidden_pairs(vec![(1, 2)]).unwrap();
    assert_eq!(engine.query_violations().unwrap(), vec![(1, 2)]);

    std::thread::sleep(Duration::from_millis(10));
    assert!(engine.evict_if_idle());
    assert!(!engine.is_warm());

    let violations = engine.query_violations().unwrap();
    assert!(engine.is_warm());
    assert_eq!(violations, vec![(1, 2)]);
}

#[test]
fn test_violations_set_before_warm() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2), (2, 3)],
        })
        .unwrap();
    assert!(!engine.is_warm());

    engine.set_forbidden_pairs(vec![(1, 2)]).unwrap();
    assert!(!engine.is_warm());

    let violations = engine.query_violations().unwrap();
    assert!(engine.is_warm());
    assert_eq!(violations, vec![(1, 2)]);
}

#[test]
fn test_violations_add_edge_creates_violation() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2)],
        })
        .unwrap();
    engine.set_forbidden_pairs(vec![(3, 4)]).unwrap();
    assert!(engine.query_violations().unwrap().is_empty());

    engine
        .update(DdDelta {
            added_edges: vec![(3, 4)],
            removed_edges: vec![],
        })
        .unwrap();
    assert_eq!(engine.query_violations().unwrap(), vec![(3, 4)]);
}

#[test]
fn test_cycles_and_blast_radius_unchanged_with_forbidden_pairs() {
    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2), (2, 3), (3, 1)],
        })
        .unwrap();
    engine.set_forbidden_pairs(vec![(1, 2)]).unwrap();

    let cycles = engine.query_cycles().unwrap();
    assert_eq!(cycles.len(), 1);
    assert_eq!(cycles[0].file_ids, vec![1, 2, 3]);

    assert_eq!(engine.query_blast_radius(3).unwrap(), 2);
}

#[test]
fn cfg_test_only_cycle_not_reported() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "no_cycles"
name = "no-module-cycles"
"#,
    )
    .unwrap();

    db.upsert_file("src/arc.rs", "rust", "h1", 10, true)
        .unwrap();
    db.upsert_file("src/task.rs", "rust", "h2", 10, true)
        .unwrap();
    let fa = db.file_by_path("src/arc.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/task.rs").unwrap().unwrap();

    // Both legs of the cycle live inside `#[cfg(test)] mod tests` — the shape
    // that made yojana report a blocking cycle that no release build has.
    db.insert_import_with_scope(fa.id, "src/task.rs", Some(fb.id), 197, "use", None, true)
        .unwrap();
    db.insert_import_with_scope(fb.id, "src/arc.rs", Some(fa.id), 432, "use", None, true)
        .unwrap();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: None,
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let cycle_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "no_cycles")
        .collect();
    assert!(
        cycle_findings.is_empty(),
        "test-only cycle should not be reported, got: {:?}",
        cycle_findings.iter().map(|f| &f.detail).collect::<Vec<_>>()
    );
}

#[test]
fn production_cycle_still_reported_alongside_test_edges() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "no_cycles"
name = "no-module-cycles"
"#,
    )
    .unwrap();

    for (path, hash) in [("src/a.rs", "h1"), ("src/b.rs", "h2")] {
        db.upsert_file(path, "rust", hash, 10, true).unwrap();
    }
    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();

    db.insert_import(fa.id, "src/b.rs", Some(fb.id), 1, "use", None)
        .unwrap();
    db.insert_import(fb.id, "src/a.rs", Some(fa.id), 1, "use", None)
        .unwrap();
    // A redundant test-scope import must not mask the genuine cycle.
    db.insert_import_with_scope(fa.id, "src/b.rs", Some(fb.id), 90, "use", None, true)
        .unwrap();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: None,
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let cycle_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "no_cycles")
        .collect();
    assert_eq!(cycle_findings.len(), 1);
    assert!(cycle_findings[0].detail.contains("src/a.rs"));
    assert!(cycle_findings[0].detail.contains("src/b.rs"));
}

#[test]
fn include_tests_opt_in_restores_test_only_cycle() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "no_cycles"
name = "no-module-cycles"
include_tests = true
"#,
    )
    .unwrap();

    db.upsert_file("src/a.rs", "rust", "h1", 10, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "h2", 10, true).unwrap();
    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();

    db.insert_import_with_scope(fa.id, "src/b.rs", Some(fb.id), 1, "use", None, true)
        .unwrap();
    db.insert_import_with_scope(fb.id, "src/a.rs", Some(fa.id), 1, "use", None, true)
        .unwrap();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: None,
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let cycle_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "no_cycles")
        .collect();
    assert_eq!(cycle_findings.len(), 1);
}

#[test]
fn cfg_test_only_forbidden_dep_not_reported() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/tools/*"
to = "src/daemon.rs"
name = "no-tool-daemon"
"#,
    )
    .unwrap();

    for (path, hash) in [
        ("src/tools/thing.rs", "h1"),
        ("src/daemon.rs", "h2"),
        ("src/other.rs", "h3"),
    ] {
        db.upsert_file(path, "rust", hash, 10, true).unwrap();
    }
    let tool = db.file_by_path("src/tools/thing.rs").unwrap().unwrap();
    let daemon = db.file_by_path("src/daemon.rs").unwrap().unwrap();
    let other = db.file_by_path("src/other.rs").unwrap().unwrap();

    db.insert_import_with_scope(
        tool.id,
        "src/daemon.rs",
        Some(daemon.id),
        200,
        "use",
        None,
        true,
    )
    .unwrap();
    // Unrelated production edge so the edge set isn't empty.
    db.insert_import(tool.id, "src/other.rs", Some(other.id), 1, "use", None)
        .unwrap();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: None,
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let dep_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "forbidden_dep")
        .collect();
    assert!(
        dep_findings.is_empty(),
        "test-only dependency should not violate a production rule, got: {:?}",
        dep_findings.iter().map(|f| &f.detail).collect::<Vec<_>>()
    );
}

/// Run a `forbidden_external` rule against a single unresolved import of `axum`
/// at the given scope, end to end through the index. Covers the plumbing —
/// `is_test` reaching the external check from the imports table (sutra/294).
fn external_findings_for_scope(import_is_test: bool, rules: &str) -> Vec<String> {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(rules_dir.join("rules.toml"), rules).unwrap();

    db.upsert_file("report/src/lib.rs", "rust", "h1", 10, true)
        .unwrap();
    let f = db.file_by_path("report/src/lib.rs").unwrap().unwrap();
    db.insert_import_with_scope(f.id, "axum::Router", None, 1, "use", None, import_is_test)
        .unwrap();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: None,
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "forbidden_external")
        .map(|f| f.detail.clone())
        .collect()
}

const EXTERNAL_RULE: &str = r#"
[[constraint]]
kind = "forbidden_external"
from = "report/**"
crates = ["axum"]
name = "report-stays-pure"
"#;

#[test]
fn cfg_test_only_external_crate_not_reported() {
    let details = external_findings_for_scope(true, EXTERNAL_RULE);
    assert!(
        details.is_empty(),
        "a crate used only under #[cfg(test)] is not a production dependency, got: {details:?}"
    );
}

#[test]
fn production_external_crate_still_reported() {
    let details = external_findings_for_scope(false, EXTERNAL_RULE);
    assert_eq!(details.len(), 1, "got: {details:?}");
}

#[test]
fn include_tests_opt_in_restores_external_finding() {
    let rules = format!("{EXTERNAL_RULE}include_tests = true\n");
    let details = external_findings_for_scope(true, &rules);
    assert_eq!(details.len(), 1, "got: {details:?}");
}

/// Build a one-file workspace whose only import is a self-import, and return
/// the no_cycles findings. A self-edge reaches `no_cycles` as a single-node SCC,
/// which the production-edge narrowing must not discard wholesale (sutra/294).
fn self_loop_cycle_details(import_is_test: bool) -> Vec<String> {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "no_cycles"
name = "no-module-cycles"
"#,
    )
    .unwrap();

    db.upsert_file("src/solo.rs", "rust", "h1", 10, true)
        .unwrap();
    let solo = db.file_by_path("src/solo.rs").unwrap().unwrap();
    db.insert_import_with_scope(
        solo.id,
        "src/solo.rs",
        Some(solo.id),
        1,
        "use",
        None,
        import_is_test,
    )
    .unwrap();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: None,
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "no_cycles")
        .map(|f| f.detail.clone())
        .collect()
}

#[test]
fn production_self_loop_cycle_survives_narrowing() {
    let details = self_loop_cycle_details(false);
    assert_eq!(
        details.len(),
        1,
        "a production self-import is a genuine one-node cycle, got: {details:?}"
    );
    assert!(details[0].contains("src/solo.rs"));
}

#[test]
fn test_only_self_loop_cycle_suppressed() {
    let details = self_loop_cycle_details(true);
    assert!(
        details.is_empty(),
        "self-import from test scope is not a production cycle, got: {details:?}"
    );
}

#[test]
fn stale_engine_cycle_ids_not_in_path_map_skipped() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "no_cycles"
name = "no-module-cycles"
"#,
    )
    .unwrap();

    // DB files with IDs that differ from the engine's stale cycle IDs.
    db.upsert_file("src/a.rs", "rust", "h1", 10, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "h2", 10, true).unwrap();
    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();
    // Non-cyclic edge so edges aren't empty (would short-circuit before cycle check).
    db.insert_import(fa.id, "src/b.rs", Some(fb.id), 1, "use", None)
        .unwrap();

    // Engine loaded with stale edges forming a cycle on IDs 1,2,3
    // (which don't exist in the DB's files table).
    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(1, 2), (2, 3), (3, 1)],
        })
        .unwrap();
    assert_eq!(engine.query_cycles().unwrap().len(), 1);

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: Some(&engine),
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let cycle_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "no_cycles")
        .collect();
    assert!(
        cycle_findings.is_empty(),
        "stale cycle with unresolvable file_ids should be skipped, got: {:?}",
        cycle_findings.iter().map(|f| &f.detail).collect::<Vec<_>>()
    );
}

#[test]
fn stale_engine_cycle_resolvable_ids_but_no_backing_edges_skipped() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "no_cycles"
name = "no-module-cycles"
"#,
    )
    .unwrap();

    db.upsert_file("src/a.rs", "rust", "h1", 10, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "h2", 10, true).unwrap();
    db.upsert_file("src/c.rs", "rust", "h3", 10, true).unwrap();
    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();
    let fc = db.file_by_path("src/c.rs").unwrap().unwrap();

    // Current DB edges are acyclic: A→B, B→C (no C→A).
    db.insert_import(fa.id, "src/b.rs", Some(fb.id), 1, "use", None)
        .unwrap();
    db.insert_import(fb.id, "src/c.rs", Some(fc.id), 1, "use", None)
        .unwrap();

    // Engine was loaded when C→A still existed, so it reports a cycle.
    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(fa.id, fb.id), (fb.id, fc.id), (fc.id, fa.id)],
        })
        .unwrap();
    assert_eq!(engine.query_cycles().unwrap().len(), 1);

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: Some(&engine),
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let cycle_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "no_cycles")
        .collect();
    assert!(
        cycle_findings.is_empty(),
        "stale cycle with resolvable IDs but missing backing edge should be skipped, got: {:?}",
        cycle_findings.iter().map(|f| &f.detail).collect::<Vec<_>>()
    );
}

#[test]
fn cycle_within_glob_scope_attributed_to_named_constraint() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "no_cycles"
name = "wrapper-no-cycles"
scope = "src/**"
severity = "advisory"
"#,
    )
    .unwrap();

    db.upsert_file("src/a.rs", "rust", "h1", 10, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "h2", 10, true).unwrap();
    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();
    db.insert_import(fa.id, "src/b.rs", Some(fb.id), 1, "use", None)
        .unwrap();
    db.insert_import(fb.id, "src/a.rs", Some(fa.id), 1, "use", None)
        .unwrap();

    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(fa.id, fb.id), (fb.id, fa.id)],
        })
        .unwrap();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: Some(&engine),
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let cycle_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "no_cycles")
        .collect();
    assert_eq!(cycle_findings.len(), 1);
    assert_ne!(cycle_findings[0].constraint_id.as_ref(), "builtin:cycles");
    assert_eq!(
        cycle_findings[0].constraint_name.as_deref(),
        Some("wrapper-no-cycles")
    );
    assert_eq!(cycle_findings[0].severity, sutra::rules::Severity::Advisory);
}

#[test]
fn cycle_partially_outside_glob_scope_falls_back_to_builtin() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "no_cycles"
name = "wrapper-no-cycles"
scope = "src/**"
"#,
    )
    .unwrap();

    db.upsert_file("src/a.rs", "rust", "h1", 10, true).unwrap();
    db.upsert_file("tests/b.rs", "rust", "h2", 10, true)
        .unwrap();
    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("tests/b.rs").unwrap().unwrap();
    db.insert_import(fa.id, "tests/b.rs", Some(fb.id), 1, "use", None)
        .unwrap();
    db.insert_import(fb.id, "src/a.rs", Some(fa.id), 1, "use", None)
        .unwrap();

    let engine = DdEngine::new(Duration::from_secs(1800));
    engine
        .ingest(DdFacts {
            import_edges: vec![(fa.id, fb.id), (fb.id, fa.id)],
        })
        .unwrap();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: Some(&engine),
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let cycle_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "no_cycles")
        .collect();
    assert_eq!(cycle_findings.len(), 1);
    assert_eq!(cycle_findings[0].constraint_id.as_ref(), "builtin:cycles");
    assert!(cycle_findings[0].constraint_name.is_none());
}

/// An idiomatic `mod.rs` re-export module: the parent declares its child with
/// `mod child;` and the child reaches shared items back up via `use super::X`.
/// That closes a file-import cycle held together *only* by the module-tree
/// edge, which is not architectural coupling — the builtin must stay silent
/// (sutra/304).
#[test]
fn module_declaration_cycle_not_reported() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(rules_dir.join("rules.toml"), "").unwrap();

    db.upsert_file("src/synthesis/mod.rs", "rust", "h1", 10, true)
        .unwrap();
    db.upsert_file("src/synthesis/child.rs", "rust", "h2", 10, true)
        .unwrap();
    let mod_rs = db.file_by_path("src/synthesis/mod.rs").unwrap().unwrap();
    let child = db.file_by_path("src/synthesis/child.rs").unwrap().unwrap();

    // Parent declares the child (module-tree wiring).
    db.insert_import(mod_rs.id, "self::child", Some(child.id), 1, "mod", None)
        .unwrap();
    // Child reaches a hoisted item back up in the parent module file.
    db.insert_import(
        child.id,
        "super::Candidate",
        Some(mod_rs.id),
        1,
        "import",
        None,
    )
    .unwrap();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: None,
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let cycle_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "no_cycles")
        .collect();
    assert!(
        cycle_findings.is_empty(),
        "a mod.rs re-export cycle is module-tree wiring, not coupling, got: {:?}",
        cycle_findings.iter().map(|f| &f.detail).collect::<Vec<_>>()
    );
}

/// Dropping module-declaration edges must not suppress a genuine peer cycle
/// that happens to share the workspace with module-tree wiring. `a` and `b`
/// import each other's real APIs via `use`; that loop still fires even while a
/// separate `mod.rs`/child pair is filtered out (sutra/304).
#[test]
fn genuine_use_cycle_survives_module_edge_filtering() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(rules_dir.join("rules.toml"), "").unwrap();

    for (path, hash) in [
        ("src/wiring/mod.rs", "h1"),
        ("src/wiring/leaf.rs", "h2"),
        ("src/a.rs", "h3"),
        ("src/b.rs", "h4"),
    ] {
        db.upsert_file(path, "rust", hash, 10, true).unwrap();
    }
    let mod_rs = db.file_by_path("src/wiring/mod.rs").unwrap().unwrap();
    let leaf = db.file_by_path("src/wiring/leaf.rs").unwrap().unwrap();
    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();

    // Module-tree cycle — suppressed.
    db.insert_import(mod_rs.id, "self::leaf", Some(leaf.id), 1, "mod", None)
        .unwrap();
    db.insert_import(leaf.id, "super::Shared", Some(mod_rs.id), 1, "import", None)
        .unwrap();

    // Genuine peer cycle through real `use` edges — must still fire.
    db.insert_import(fa.id, "crate::b::Thing", Some(fb.id), 1, "import", None)
        .unwrap();
    db.insert_import(fb.id, "crate::a::Other", Some(fa.id), 1, "import", None)
        .unwrap();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: None,
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let cycle_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "no_cycles")
        .collect();
    assert_eq!(
        cycle_findings.len(),
        1,
        "exactly the a<->b use-cycle should fire, got: {:?}",
        cycle_findings.iter().map(|f| &f.detail).collect::<Vec<_>>()
    );
    let detail = &cycle_findings[0].detail;
    assert!(
        detail.contains("src/a.rs") && detail.contains("src/b.rs"),
        "reported cycle should be a<->b, got: {detail}"
    );
    assert!(
        !detail.contains("wiring"),
        "module-tree pair must not appear in a cycle, got: {detail}"
    );
}

#[test]
fn evaluate_dd_finds_forbidden_pattern_violations() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = '(unsafe_block) @match'
name = "no-unsafe"
severity = "advisory"
"#,
    )
    .unwrap();

    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        src_dir.join("lib.rs"),
        "fn risky() { unsafe { core::ptr::null::<u8>().read() }; }\n",
    )
    .unwrap();
    std::fs::write(src_dir.join("safe.rs"), "fn safe() { let x = 1; }\n").unwrap();

    db.upsert_file("src/lib.rs", "rust", "h1", 1, true).unwrap();
    db.upsert_file("src/safe.rs", "rust", "h2", 1, true)
        .unwrap();

    let engine = DdEngine::new(Duration::from_secs(60));

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: Some(&engine),
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let pattern_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "forbidden_pattern")
        .collect();
    assert_eq!(pattern_findings.len(), 1);
    assert_eq!(pattern_findings[0].from_path, "src/lib.rs");
    assert!(pattern_findings[0].line.is_some());
    assert!(pattern_findings[0].snippet.is_some());
}

/// `.pyi` stubs are never indexed (they would double-count the symbols their
/// `.py` sibling declares), so workspace-scope evaluation has to find them on
/// disk. Rollups stay clean because no file row is created for them.
#[test]
fn evaluate_dd_finds_pattern_violations_in_unindexed_pyi_stubs() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_pattern"
language = "python"
query = '''
(function_definition
  name: (identifier) @_name (#eq? @_name "__new__")
  return_type: (type (identifier) @_ret) (#eq? @_ret "Never")) @match
'''
name = "no-new-returning-never"
severity = "advisory"
"#,
    )
    .unwrap();

    let pkg_dir = dir.path().join("python/swisseph_rs");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(
        pkg_dir.join("azalt.pyi"),
        "from typing import Never, final\n\n@final\nclass RefracDir:\n    \
         TRUE_TO_APP: RefracDir\n    def __new__(cls, _: Never, /) -> Never: ...\n",
    )
    .unwrap();
    std::fs::write(pkg_dir.join("azalt.py"), "class RefracDir:\n    pass\n").unwrap();

    // Only the .py is indexed — the stub has no file row.
    db.upsert_file("python/swisseph_rs/azalt.py", "python", "h1", 1, true)
        .unwrap();

    let engine = DdEngine::new(Duration::from_secs(60));
    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: Some(&engine),
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let pattern_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "forbidden_pattern")
        .collect();
    assert_eq!(pattern_findings.len(), 1, "findings: {:#?}", outcome.active);
    assert_eq!(
        pattern_findings[0].from_path,
        "python/swisseph_rs/azalt.pyi"
    );

    // The rule matches a real file, so it must not be reported as inert.
    let dead: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "dead_constraint")
        .collect();
    assert!(dead.is_empty(), "dead findings: {dead:#?}");
}

#[test]
fn evaluate_dd_pattern_changed_files_scope_only_scans_changed() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = '(unsafe_block) @match'
name = "no-unsafe"
"#,
    )
    .unwrap();

    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("a.rs"), "fn a() { unsafe { }; }\n").unwrap();
    std::fs::write(src_dir.join("b.rs"), "fn b() { unsafe { }; }\n").unwrap();

    db.upsert_file("src/a.rs", "rust", "h1", 1, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "h2", 1, true).unwrap();
    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();

    let engine = DdEngine::new(Duration::from_secs(60));

    let changed_ids: std::collections::HashSet<i64> = [fa.id].into_iter().collect();
    let old_edges: std::collections::HashSet<(i64, i64)> = std::collections::HashSet::new();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: Some(&engine),
        },
        dir.path(),
        EvalScope::ChangedFiles {
            changed_ids: &changed_ids,
            old_edges: &old_edges,
            changed_pattern_only_paths: &[],
        },
        &registry,
    )
    .unwrap();

    let pattern_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "forbidden_pattern")
        .collect();
    assert_eq!(
        pattern_findings.len(),
        1,
        "only src/a.rs is changed, src/b.rs should not be scanned"
    );
    assert_eq!(pattern_findings[0].from_path, "src/a.rs");
}

#[test]
fn evaluate_dd_pattern_symbol_level_waiver() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = '(unsafe_block) @match'
name = "no-unsafe"
"#,
    )
    .unwrap();

    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        src_dir.join("lib.rs"),
        "fn waived_fn() { unsafe { }; }\nfn other_fn() { unsafe { }; }\n",
    )
    .unwrap();

    db.upsert_file("src/lib.rs", "rust", "h1", 2, true).unwrap();

    let mut rules = sutra::rules::load_rules(dir.path()).unwrap();
    let (constraints, _) = rules.all_constraints();
    let pc = constraints
        .iter()
        .find(|c| {
            matches!(
                c.kind,
                sutra::rules::ConstraintKind::ForbiddenPattern { .. }
            )
        })
        .unwrap();

    // Symbol-level waiver: only suppresses findings inside waived_fn
    db.create_constraint_waiver(
        &pc.id,
        pc.name.as_deref(),
        "src/lib.rs",
        Some("waived_fn"),
        "Justified unsafe in waived_fn",
        "josh",
    )
    .unwrap();

    let engine = DdEngine::new(Duration::from_secs(60));
    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: Some(&engine),
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let active_pattern: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "forbidden_pattern")
        .collect();
    assert_eq!(
        active_pattern.len(),
        1,
        "other_fn's unsafe should remain active"
    );
    assert_eq!(
        active_pattern[0].enclosing_symbol.as_deref(),
        Some("other_fn"),
    );

    let waived_pattern: Vec<_> = outcome
        .waived
        .iter()
        .filter(|w| w.finding.constraint_kind == "forbidden_pattern")
        .collect();
    assert_eq!(
        waived_pattern.len(),
        1,
        "waived_fn's unsafe should be waived"
    );
    assert_eq!(
        waived_pattern[0].finding.enclosing_symbol.as_deref(),
        Some("waived_fn"),
    );
}

#[test]
fn evaluate_raw_pattern_waiver_on_edgeless_file() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = '(unsafe_block) @match'
name = "no-unsafe"
"#,
    )
    .unwrap();

    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        src_dir.join("standalone.rs"),
        "fn do_thing() { unsafe { }; }\n",
    )
    .unwrap();

    // File exists in DB but has NO import edges
    db.upsert_file("src/standalone.rs", "rust", "h1", 1, true)
        .unwrap();

    let mut rules = sutra::rules::load_rules(dir.path()).unwrap();
    let (constraints, _) = rules.all_constraints();
    let pc = constraints
        .iter()
        .find(|c| {
            matches!(
                c.kind,
                sutra::rules::ConstraintKind::ForbiddenPattern { .. }
            )
        })
        .unwrap();

    db.create_constraint_waiver(
        &pc.id,
        pc.name.as_deref(),
        "src/standalone.rs",
        Some("do_thing"),
        "justified unsafe",
        "josh",
    )
    .unwrap();

    let db_path = dir.path().join("test").join("index.db");
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    let file_id: i64 = conn
        .query_row(
            "SELECT id FROM files WHERE path = 'src/standalone.rs'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::RawConn(&conn),
        dir.path(),
        EvalScope::SingleFile(file_id),
        &registry,
    )
    .unwrap();

    assert!(
        outcome
            .active
            .iter()
            .all(|f| f.constraint_kind != "forbidden_pattern"),
        "waived pattern on edgeless file should not appear as active"
    );
    assert_eq!(
        outcome
            .waived
            .iter()
            .filter(|w| w.finding.constraint_kind == "forbidden_pattern")
            .count(),
        1,
        "waived pattern should appear in waived list"
    );
}

// ---------------------------------------------------------------------------
// Ratchet violation detection
// ---------------------------------------------------------------------------

#[test]
fn ratchet_violation_deletion_detected() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    // rules.toml with NO constraints — the ratcheted one is "missing"
    std::fs::write(rules_dir.join("rules.toml"), "").unwrap();

    // Register a ratchet for a constraint that doesn't exist in rules.toml
    db.upsert_constraint_ratchet(
        "abc12345",
        Some("no-clone"),
        "forbidden_dep: a → b",
        "blocking",
    )
    .unwrap();

    let engine = DdEngine::new(Duration::from_secs(60));
    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: Some(&engine),
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let ratchet_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "ratchet_violation")
        .collect();
    assert_eq!(ratchet_findings.len(), 1);
    assert_eq!(ratchet_findings[0].constraint_id.as_ref(), "abc12345");
    assert_eq!(
        ratchet_findings[0].severity,
        sutra::rules::Severity::Blocking
    );
    assert!(ratchet_findings[0].detail.contains("removed or modified"));
    assert!(ratchet_findings[0].detail.contains("forbidden_dep: a → b"));
}

#[test]
fn ratchet_violation_severity_below_floor_detected() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/a/**"
to = "src/b/**"
name = "no-a-to-b"
severity = "advisory"
"#,
    )
    .unwrap();

    // Ratchet was registered at "blocking" but constraint is now "advisory"
    let mut loaded = sutra::rules::load_rules(dir.path()).unwrap();
    let (constraints, _) = loaded.all_constraints();
    let constraint_id = &constraints[0].id;
    db.upsert_constraint_ratchet(
        constraint_id,
        Some("no-a-to-b"),
        "forbidden_dep",
        "blocking",
    )
    .unwrap();

    let engine = DdEngine::new(Duration::from_secs(60));
    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: Some(&engine),
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let ratchet_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "ratchet_violation")
        .collect();
    assert_eq!(ratchet_findings.len(), 1);
    assert!(ratchet_findings[0].detail.contains("severity downgraded"));
    assert!(ratchet_findings[0].detail.contains("advisory"));
    assert!(ratchet_findings[0].detail.contains("blocking"));
}

#[test]
fn ratchet_violation_released_row_silent() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(rules_dir.join("rules.toml"), "").unwrap();

    // Register then release the ratchet
    db.upsert_constraint_ratchet(
        "abc12345",
        Some("no-clone"),
        "forbidden_dep: a → b",
        "blocking",
    )
    .unwrap();
    db.release_constraint_ratchet("abc12345", "josh", "no longer needed")
        .unwrap();

    let engine = DdEngine::new(Duration::from_secs(60));
    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: Some(&engine),
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let ratchet_findings: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "ratchet_violation")
        .collect();
    assert_eq!(
        ratchet_findings.len(),
        0,
        "released ratchet should not fire"
    );
}

#[test]
fn ratchet_violation_non_waivable() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(rules_dir.join("rules.toml"), "").unwrap();

    // Register a ratchet for a missing constraint
    db.upsert_constraint_ratchet(
        "abc12345",
        Some("no-clone"),
        "forbidden_dep: a → b",
        "blocking",
    )
    .unwrap();

    // Create a waiver targeting that constraint ID
    db.create_constraint_waiver(
        "abc12345",
        Some("no-clone"),
        "",
        None,
        "bypass attempt",
        "agent",
    )
    .unwrap();

    let engine = DdEngine::new(Duration::from_secs(60));
    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: Some(&engine),
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    let ratchet_active: Vec<_> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "ratchet_violation")
        .collect();
    assert_eq!(
        ratchet_active.len(),
        1,
        "ratchet violation must not be waivable"
    );

    let ratchet_waived = outcome
        .waived
        .iter()
        .filter(|w| w.finding.constraint_kind == "ratchet_violation")
        .count();
    assert_eq!(
        ratchet_waived, 0,
        "ratchet violation must never appear in waived list"
    );
}

// --- test-directed escape hatch for dep and cycle rules (sutra/296) ---

/// Run `rules` against a two-file test-only cycle under `tests/`, end to end.
/// The importing files are test targets by path, so their imports carry
/// `is_test = true` — the shape a rule aimed at `tests/**` has to survive.
fn test_dir_cycle_details(rules: &str) -> Vec<String> {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(rules_dir.join("rules.toml"), rules).unwrap();

    db.upsert_file("tests/a.rs", "rust", "h1", 10, true)
        .unwrap();
    db.upsert_file("tests/b.rs", "rust", "h2", 10, true)
        .unwrap();
    let fa = db.file_by_path("tests/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("tests/b.rs").unwrap().unwrap();

    db.insert_import_with_scope(fa.id, "tests/b.rs", Some(fb.id), 1, "use", None, true)
        .unwrap();
    db.insert_import_with_scope(fb.id, "tests/a.rs", Some(fa.id), 1, "use", None, true)
        .unwrap();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: None,
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "no_cycles")
        .map(|f| f.detail.clone())
        .collect()
}

#[test]
fn no_cycles_scoped_to_tests_fires_without_include_tests() {
    let details = test_dir_cycle_details(
        r#"
[[constraint]]
kind = "no_cycles"
scope = "tests/**"
name = "no-cycles-in-integration-tests"
"#,
    );
    assert_eq!(
        details.len(),
        1,
        "a no_cycles rule scoped to tests/ must not be muted by test-scope exclusion, got: {details:?}"
    );
}

#[test]
fn unscoped_no_cycles_still_ignores_a_test_only_cycle() {
    // The negative direction: widening the escape hatch must not turn the
    // default exclusion off for rules that never mentioned tests.
    let details = test_dir_cycle_details(
        r#"
[[constraint]]
kind = "no_cycles"
name = "no-module-cycles"
"#,
    );
    assert!(
        details.is_empty(),
        "an unscoped rule keeps the default test exclusion, got: {details:?}"
    );
}

/// Run `rules` against a single `tests/a.rs -> src/daemon.rs` edge whose import
/// is test-scoped, plus one production edge so the graph is non-empty.
fn test_dir_dep_details(rules: &str) -> Vec<String> {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(rules_dir.join("rules.toml"), rules).unwrap();

    for (path, hash) in [
        ("tests/a.rs", "h1"),
        ("src/daemon.rs", "h2"),
        ("src/other.rs", "h3"),
    ] {
        db.upsert_file(path, "rust", hash, 10, true).unwrap();
    }
    let ta = db.file_by_path("tests/a.rs").unwrap().unwrap();
    let daemon = db.file_by_path("src/daemon.rs").unwrap().unwrap();
    let other = db.file_by_path("src/other.rs").unwrap().unwrap();

    db.insert_import_with_scope(
        ta.id,
        "src/daemon.rs",
        Some(daemon.id),
        1,
        "use",
        None,
        true,
    )
    .unwrap();
    db.insert_import(ta.id, "src/other.rs", Some(other.id), 2, "use", None)
        .unwrap();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: None,
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind == "forbidden_dep")
        .map(|f| f.detail.clone())
        .collect()
}

#[test]
fn forbidden_dep_from_tests_fires_without_include_tests() {
    let details = test_dir_dep_details(
        r#"
[[constraint]]
kind = "forbidden_dep"
from = "tests/**"
to = "src/daemon.rs"
name = "tests-must-not-touch-daemon"
"#,
    );
    assert_eq!(
        details.len(),
        1,
        "a forbidden_dep written for tests/ must not be muted by test-scope exclusion, got: {details:?}"
    );
}

#[test]
fn forbidden_dep_not_aimed_at_tests_still_ignores_test_edges() {
    let details = test_dir_dep_details(
        r#"
[[constraint]]
kind = "forbidden_dep"
from = "**"
to = "src/daemon.rs"
name = "nobody-touches-daemon"
"#,
    );
    assert!(
        details.is_empty(),
        "a rule that never mentioned tests keeps the default exclusion, got: {details:?}"
    );
}

// ---------------------------------------------------------------------------
// Session-lifetime graph staleness (sutra/297)
//
// A shared DdEngine outlives the request that ingested its graph. Every reparse
// deletes and re-inserts the `files` row, and `files.id` is AUTOINCREMENT, so
// the reparsed file gets a brand-new id. Forbidden pairs are resolved fresh
// against live ids each evaluation, so a cached graph doesn't merely lag — it
// goes disjoint, and the violations semijoin comes back empty. Silently.
// ---------------------------------------------------------------------------

/// Index two files with an import edge from `src/ui/view.rs` to
/// `src/db/query.rs`, plus an unresolved external import. Returns the two ids.
fn index_dep_fixture(db: &sutra::db::Db) -> (i64, i64) {
    db.upsert_file("src/ui/view.rs", "rust", "h1", 10, true)
        .unwrap();
    db.upsert_file("src/db/query.rs", "rust", "h2", 10, true)
        .unwrap();
    let view = db.file_by_path("src/ui/view.rs").unwrap().unwrap().id;
    let query = db.file_by_path("src/db/query.rs").unwrap().unwrap().id;
    // Two import statements against the same target: `import_edges` repeats the
    // pair, so this also exercises multiplicity bookkeeping in the DD worker.
    db.insert_import(view, "src/db/query.rs", Some(query), 1, "use", None)
        .unwrap();
    db.insert_import(view, "src/db/query.rs", Some(query), 2, "use", None)
        .unwrap();
    db.insert_import(view, "serde::Serialize", None, 3, "use", None)
        .unwrap();
    (view, query)
}

/// Delete and re-insert both files the way a reparse does, minting fresh ids.
fn remint_dep_fixture(db: &sutra::db::Db) -> (i64, i64) {
    for path in ["src/ui/view.rs", "src/db/query.rs"] {
        let id = db.file_by_path(path).unwrap().unwrap().id;
        db.delete_file_cascade(id).unwrap();
    }
    index_dep_fixture(db)
}

fn dep_rules(severity: &str) -> String {
    format!(
        r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/ui/**"
to = "src/db/**"
name = "ui-must-not-touch-db"
severity = "{severity}"

[[constraint]]
kind = "forbidden_external"
from = "src/**"
crates = ["serde"]
name = "no-serde"

[[constraint]]
kind = "no_cycles"
name = "no-module-cycles"
"#
    )
}

fn dep_kinds(outcome: &sutra::constraints::check::CheckOutcome) -> Vec<String> {
    let mut kinds: Vec<String> = outcome
        .active
        .iter()
        .filter(|f| f.constraint_kind != "dead_constraint")
        .map(|f| f.constraint_kind.clone())
        .collect();
    kinds.sort();
    kinds
}

#[test]
fn edge_derived_violations_survive_file_id_reminting_in_one_session() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(rules_dir.join("rules.toml"), dep_rules("advisory")).unwrap();

    index_dep_fixture(&db);

    let registry = default_registry();
    let engine = DdEngine::new(std::time::Duration::from_secs(600));
    let run = |db: &Db| {
        evaluate(
            &FactsSource::DdBacked {
                db,
                dd_engine: Some(&engine),
            },
            dir.path(),
            EvalScope::Workspace,
            &registry,
        )
        .unwrap()
    };

    let first = dep_kinds(&run(&db));
    assert_eq!(
        first,
        vec!["forbidden_dep", "forbidden_external"],
        "baseline: both edge-derived kinds report on the first query"
    );

    // Five reparse/query cycles, matching the reported sequence.
    for cycle in 1..=5 {
        let (view, query) = remint_dep_fixture(&db);
        assert!(
            db.import_edges().unwrap().contains(&(view, query)),
            "cycle {cycle}: the edge is still in the index"
        );
        assert_eq!(
            dep_kinds(&run(&db)),
            first,
            "cycle {cycle}: violations must be reported identically after reminting"
        );
    }
}

#[test]
fn forbidden_dep_survives_a_rules_reload_of_an_unrelated_constraint() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(rules_dir.join("rules.toml"), dep_rules("advisory")).unwrap();

    index_dep_fixture(&db);

    let registry = default_registry();
    let engine = DdEngine::new(std::time::Duration::from_secs(600));
    let run = |db: &Db| {
        evaluate(
            &FactsSource::DdBacked {
                db,
                dd_engine: Some(&engine),
            },
            dir.path(),
            EvalScope::Workspace,
            &registry,
        )
        .unwrap()
    };

    assert!(
        dep_kinds(&run(&db)).contains(&"forbidden_dep".to_string()),
        "baseline: the layering rule reports before the rules edit"
    );

    // The edit that preceded the disappearance: an unrelated field changes, so
    // the constraint's blake3 id is untouched and its waivers stay keyed.
    std::fs::write(rules_dir.join("rules.toml"), dep_rules("blocking")).unwrap();
    remint_dep_fixture(&db);

    let after = run(&db);
    let dep: Vec<_> = after
        .active
        .iter()
        .filter(|f| f.constraint_kind == "forbidden_dep")
        .collect();
    assert_eq!(
        dep.len(),
        1,
        "a rules reload must not make the layering rule inert, got: {:?}",
        dep_kinds(&after)
    );
    assert_eq!(
        dep[0].severity,
        sutra::rules::Severity::Blocking,
        "the promoted severity should be picked up"
    );
}

#[test]
fn sync_edges_retracts_a_duplicated_edge_exactly() {
    // `Db::import_edges` repeats a pair once per import statement. If the
    // duplicate reached the dataflow, its multiplicity would outlive a single
    // retraction and the violation would persist after the edge was gone.
    let engine = DdEngine::new(std::time::Duration::from_secs(600));
    engine
        .sync_edges(&[(1, 2), (1, 2), (1, 2), (2, 3)])
        .unwrap();
    engine.set_forbidden_pairs(vec![(1, 2), (2, 3)]).unwrap();
    assert_eq!(engine.query_violations().unwrap(), vec![(1, 2), (2, 3)]);

    engine.sync_edges(&[(2, 3)]).unwrap();
    assert_eq!(
        engine.query_violations().unwrap(),
        vec![(2, 3)],
        "the tripled edge must retract on the first sync that drops it"
    );

    engine.sync_edges(&[(1, 2), (2, 3)]).unwrap();
    assert_eq!(
        engine.query_violations().unwrap(),
        vec![(1, 2), (2, 3)],
        "and come back when the index has it again"
    );
}

#[test]
fn no_cycles_survives_file_id_reminting_in_one_session() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "no_cycles"
name = "no-module-cycles"
"#,
    )
    .unwrap();

    let index = |db: &Db| {
        db.upsert_file("src/a.rs", "rust", "h1", 10, true).unwrap();
        db.upsert_file("src/b.rs", "rust", "h2", 10, true).unwrap();
        let fa = db.file_by_path("src/a.rs").unwrap().unwrap().id;
        let fb = db.file_by_path("src/b.rs").unwrap().unwrap().id;
        db.insert_import(fa, "src/b.rs", Some(fb), 1, "use", None)
            .unwrap();
        db.insert_import(fb, "src/a.rs", Some(fa), 1, "use", None)
            .unwrap();
    };
    index(&db);

    let registry = default_registry();
    let engine = DdEngine::new(std::time::Duration::from_secs(600));
    let cycle_count = |db: &Db| {
        evaluate(
            &FactsSource::DdBacked {
                db,
                dd_engine: Some(&engine),
            },
            dir.path(),
            EvalScope::Workspace,
            &registry,
        )
        .unwrap()
        .active
        .iter()
        .filter(|f| f.constraint_kind == "no_cycles")
        .count()
    };

    assert_eq!(cycle_count(&db), 1, "baseline: the cycle is reported");

    // A stale cycle's node ids fail the path_map lookup and get skipped, so
    // reminting turns a blocking cycle rule inert just as quietly.
    for cycle in 1..=3 {
        for path in ["src/a.rs", "src/b.rs"] {
            let id = db.file_by_path(path).unwrap().unwrap().id;
            db.delete_file_cascade(id).unwrap();
        }
        index(&db);
        assert_eq!(
            cycle_count(&db),
            1,
            "cycle {cycle}: the cycle must still be reported after reminting"
        );
    }
}

// ---------------------------------------------------------------------------
// Snapshot coherence under concurrent reparse (sutra/298)
//
// evaluate_dd reads files (→ path_map), components and import edges across
// separate `Mutex<Connection>` acquisitions, and the constraint/orient
// endpoints don't hold the parse lock. A reparse reminting file ids mid-read
// can pair an old-id path_map with new-id edges — the disjoint-id silent-clean
// of sutra/297, now driven by concurrency rather than a stale cached graph.
// `evaluate` guards the read window with the index's data_generation: if it
// moves across the reads the snapshot was incoherent, so it retries, and
// sustained churn surfaces as an explicit error. Invariant under test: a query
// is never Ok-but-empty while the offending edge is present — it is either the
// correct violations or an explicit not-evaluated error, never silently clean.
// ---------------------------------------------------------------------------

// Quarantined, and kept quarantined by design (sutra/299). Driving the shared
// DdEngine's timely worker while another thread reparses the same Db corrupts
// the process heap (~30% of runs): SQLite's LALR parser state is clobbered and
// spins forever in yy_reduce on a trivial query — a heap-corruption data race
// in the third-party timely/differential + bundled-sqlite interaction, not in
// this crate (which has no `unsafe`). sutra/298 makes that overlap structurally
// impossible in the server: the constraints/orient/review endpoints and the
// first-parse path now hold the parse-coordinator lock across DD evaluation, so
// the engine is never driven concurrently with a reparse. This test is retained
// as the reproduction/documentation of the race; do NOT un-ignore it — it
// exercises a configuration the running system now prevents, and running an
// intermittent heap-corruption race in CI would only add flakiness.
#[test]
#[ignore = "sutra/299: heap-corruption race in timely+sqlite under concurrent reparse; \
            structurally prevented in production by the sutra/298 parse-lock serialization"]
fn edge_derived_violations_are_never_silently_clean_under_concurrent_reminting() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(rules_dir.join("rules.toml"), dep_rules("advisory")).unwrap();

    index_dep_fixture(&db);

    let registry = default_registry();
    let engine = DdEngine::new(std::time::Duration::from_secs(600));
    let stop = AtomicBool::new(false);

    std::thread::scope(|s| {
        // A concurrent reparse: continuously delete and re-insert both files,
        // reminting their ids exactly as `replace_file_data` does, so the
        // foreground evaluations straddle a committing reparse. Each
        // `delete_file_cascade` bumps `data_generation`, which is the signal the
        // guard keys on.
        s.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                remint_dep_fixture(&db);
                // A brief yield keeps the reparse from monopolising the shared
                // connection mutex, leaving windows where a full evaluation
                // reads a coherent snapshot — so the test exercises both the
                // retried-correct and not-evaluated outcomes, not just churn.
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        });

        // Many evaluations against the churning index. Without the guard, a
        // read landing in a remint window returns an empty-but-Ok outcome; with
        // it, that window is retried or reported as an error — never clean.
        for i in 0..120 {
            match evaluate(
                &FactsSource::DdBacked {
                    db: &db,
                    dd_engine: Some(&engine),
                },
                dir.path(),
                EvalScope::Workspace,
                &registry,
            ) {
                Ok(outcome) => {
                    let kinds = dep_kinds(&outcome);
                    assert!(
                        kinds.contains(&"forbidden_dep".to_string()),
                        "iteration {i}: forbidden_dep vanished with the edge present — \
                         a coherent snapshot must report it (got {kinds:?})"
                    );
                }
                // A reparse committing across every retry attempt is a
                // legitimate not-evaluated result: explicit, not silently clean.
                Err(_) => {}
            }
        }
        stop.store(true, Ordering::Relaxed);
    });
}

/// Report-path instance acks (sutra/305): acknowledged clones drop off the
/// `violations`/review surface, count-aware, while surplus siblings and any
/// future byte-identical clone stay reported. Report-only — the guard is tested
/// separately (src/guard.rs::pattern_ack_does_not_relax_guard).
#[test]
fn instance_acks_subtract_on_report_count_aware() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::db::Db;
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = '(call_expression function: (field_expression field: (field_identifier) @m (#eq? @m "clone"))) @match'
name = "no-clone"
severity = "blocking"
scope = "src/"
"#,
    )
    .unwrap();

    // Three byte-identical `foo.clone()` in one fn, plus one distinct `bar.clone()`.
    let src = "fn a() {\n    let x = foo.clone();\n    let y = foo.clone();\n    \
               let z = foo.clone();\n    let w = bar.clone();\n}\n";
    let src_path = dir.path().join("src/lib.rs");
    std::fs::create_dir_all(src_path.parent().unwrap()).unwrap();
    std::fs::write(&src_path, src).unwrap();
    db.upsert_file("src/lib.rs", "rust", "h1", 6, true).unwrap();

    let registry = default_registry();
    let count_active = |db: &Db| {
        evaluate(
            &FactsSource::DdBacked {
                db,
                dd_engine: None,
            },
            dir.path(),
            EvalScope::Workspace,
            &registry,
        )
        .unwrap()
        .active
        .iter()
        .filter(|f| f.constraint_kind == "forbidden_pattern")
        .count()
    };

    assert_eq!(
        count_active(&db),
        4,
        "all four clones reported before any ack"
    );

    let rule_id = {
        let mut loaded = sutra::rules::load_rules(dir.path()).unwrap();
        let (cs, _) = loaded.all_constraints();
        cs.iter()
            .find(|c| c.name.as_deref() == Some("no-clone"))
            .unwrap()
            .id
            .to_string()
    };

    // Ack 2 of the 3 identical foo.clone() instances (snippet is the matched
    // node's first line verbatim: "foo.clone()").
    db.create_constraint_instance_ack(
        &rule_id,
        Some("no-clone"),
        "src/lib.rs",
        Some("a"),
        Some("foo.clone()"),
        2,
        Some("owned-required"),
        "test",
    )
    .unwrap();
    assert_eq!(
        count_active(&db),
        2,
        "2 of 3 foo clones acked -> 1 foo surplus + 1 bar still reported"
    );

    // Bump the same key to 3 -> all foo accounted for, only bar remains. The
    // distinct bar.clone() is a different key and is never suppressed.
    db.create_constraint_instance_ack(
        &rule_id,
        Some("no-clone"),
        "src/lib.rs",
        Some("a"),
        Some("foo.clone()"),
        3,
        Some("owned-required"),
        "test",
    )
    .unwrap();
    assert_eq!(
        count_active(&db),
        1,
        "all foo clones acked -> only bar reported"
    );
}
