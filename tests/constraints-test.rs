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
    db.insert_import(fa.id, "src/b.rs", Some(fb.id), 1, "use")
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
    db.insert_import(fa.id, "src/b.rs", Some(fb.id), 1, "use")
        .unwrap();
    db.insert_import(fb.id, "src/c.rs", Some(fc.id), 1, "use")
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
    db.insert_import(fa.id, "src/b.rs", Some(fb.id), 1, "use")
        .unwrap();
    db.insert_import(fb.id, "src/a.rs", Some(fa.id), 1, "use")
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
    db.insert_import(fa.id, "tests/b.rs", Some(fb.id), 1, "use")
        .unwrap();
    db.insert_import(fb.id, "src/a.rs", Some(fa.id), 1, "use")
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
