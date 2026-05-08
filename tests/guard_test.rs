use std::path::PathBuf;

use sutra::guard::{self, FileFacts, GuardConfig, GuardDecision};

fn default_cfg() -> GuardConfig {
    GuardConfig::default()
}

// ---------------------------------------------------------------------------
// evaluate — threshold tests
// ---------------------------------------------------------------------------

#[test]
fn test_evaluate_below_both_thresholds() {
    let facts = FileFacts {
        rel_path: "src/small.rs".into(),
        pagerank: 0.01,
        blast_radius: 3,
        hot_symbols: vec![],
    };
    assert!(matches!(
        guard::evaluate(&facts, &default_cfg(), false),
        GuardDecision::Allow
    ));
}

#[test]
fn test_evaluate_above_pagerank_threshold() {
    let facts = FileFacts {
        rel_path: "src/lib.rs".into(),
        pagerank: 0.10,
        blast_radius: 0,
        hot_symbols: vec![("Config".into(), 0.08)],
    };
    assert!(matches!(
        guard::evaluate(&facts, &default_cfg(), false),
        GuardDecision::Deny { .. }
    ));
}

#[test]
fn test_evaluate_above_blast_threshold() {
    let facts = FileFacts {
        rel_path: "src/db.rs".into(),
        pagerank: 0.01,
        blast_radius: 15,
        hot_symbols: vec![],
    };
    assert!(matches!(
        guard::evaluate(&facts, &default_cfg(), false),
        GuardDecision::Deny { .. }
    ));
}

#[test]
fn test_evaluate_ack_fresh_allows() {
    let facts = FileFacts {
        rel_path: "src/lib.rs".into(),
        pagerank: 0.20,
        blast_radius: 50,
        hot_symbols: vec![("Db".into(), 0.15)],
    };
    assert!(matches!(
        guard::evaluate(&facts, &default_cfg(), true),
        GuardDecision::Allow
    ));
}

#[test]
fn test_evaluate_custom_thresholds() {
    let cfg = GuardConfig {
        pagerank_min: 0.50,
        blast_min: 100,
        ack_ttl_secs: 600,
    };
    let facts = FileFacts {
        rel_path: "src/lib.rs".into(),
        pagerank: 0.20,
        blast_radius: 50,
        hot_symbols: vec![],
    };
    assert!(matches!(
        guard::evaluate(&facts, &cfg, false),
        GuardDecision::Allow
    ));
}

// ---------------------------------------------------------------------------
// ack_is_fresh / touch_ack
// ---------------------------------------------------------------------------

#[test]
fn test_ack_not_fresh_when_missing() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!guard::ack_is_fresh(dir.path(), "src/lib.rs", 600));
}

#[test]
fn test_ack_fresh_after_touch() {
    let dir = tempfile::tempdir().unwrap();
    guard::touch_ack(dir.path(), "src/lib.rs");
    assert!(guard::ack_is_fresh(dir.path(), "src/lib.rs", 600));
}

#[test]
fn test_ack_expired_with_zero_ttl() {
    let dir = tempfile::tempdir().unwrap();
    guard::touch_ack(dir.path(), "src/old.rs");
    std::thread::sleep(std::time::Duration::from_millis(10));
    assert!(!guard::ack_is_fresh(dir.path(), "src/old.rs", 0));
}

// ---------------------------------------------------------------------------
// find_project_root — depth limit
// ---------------------------------------------------------------------------

#[test]
fn test_find_project_root_in_git_repo() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let sub = dir.path().join("a/b/c");
    std::fs::create_dir_all(&sub).unwrap();
    assert_eq!(
        guard::find_project_root(&sub),
        Some(dir.path().to_path_buf())
    );
}

#[test]
fn test_find_project_root_no_git() {
    let dir = tempfile::tempdir().unwrap();
    let deep = dir.path().join("a/b/c/d/e");
    std::fs::create_dir_all(&deep).unwrap();
    assert_eq!(guard::find_project_root(&deep), None);
}

// ---------------------------------------------------------------------------
// relativize_file_path — including new-file fallback
// ---------------------------------------------------------------------------

#[test]
fn test_relativize_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("src/lib.rs");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(&file, "").unwrap();
    let result = guard::relativize_file_path(dir.path(), &file);
    assert_eq!(result, Some("src/lib.rs".into()));
}

#[test]
fn test_relativize_new_file_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let new_file = dir.path().join("src/new.rs");
    let result = guard::relativize_file_path(dir.path(), &new_file);
    assert_eq!(result, Some("src/new.rs".into()));
}

// ---------------------------------------------------------------------------
// render_stdout
// ---------------------------------------------------------------------------

#[test]
fn test_render_stdout_allow_is_none() {
    assert!(guard::render_stdout(&GuardDecision::Allow, None).is_none());
}

#[test]
fn test_render_stdout_deny_pretooluse() {
    let decision = GuardDecision::Deny {
        reason: "test".into(),
    };
    let json_str = guard::render_stdout(&decision, Some("PreToolUse")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[test]
fn test_render_stdout_deny_beforetool() {
    let decision = GuardDecision::Deny {
        reason: "test".into(),
    };
    let json_str = guard::render_stdout(&decision, Some("BeforeTool")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    assert_eq!(v["decision"], "deny");
}

// ---------------------------------------------------------------------------
// workspace_id_from_path
// ---------------------------------------------------------------------------

#[test]
fn test_workspace_id_basic() {
    assert_eq!(
        guard::workspace_id_from_path(&PathBuf::from("/home/josh/projects/sutra")),
        "sutra"
    );
}

#[test]
fn test_workspace_id_spaces() {
    assert_eq!(
        guard::workspace_id_from_path(&PathBuf::from("/home/josh/My Project")),
        "my-project"
    );
}
