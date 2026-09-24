//! Erosion metric (sutra/442): symbol selection over real parses, snapshot
//! persistence, and trend/file_health surfacing.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use sutra::config::Config;
use sutra::db::{Db, SnapshotErosion, SnapshotParams};
use sutra::health::erosion::{self, EROSION_VERSION};
use sutra::parser::adapter::default_registry;
use sutra::pipeline;
use sutra::tools::trend::{self, TrendArgs};
use sutra::workspace::WorkspaceEntry;

fn make_config(db_dir: &Path) -> Config {
    Config {
        db_dir: db_dir.to_path_buf(),
        workspaces_path: db_dir.join("workspaces.toml"),
        listen_addr: "127.0.0.1:0".to_string(),
        parse_parallelism: 1,
        log_level: "warn".to_string(),
        constraints_idle_timeout_sec: 1800,
        parse_timeout_ms: 5000,
    }
}

struct Fixture {
    _src: tempfile::TempDir,
    _db_dir: tempfile::TempDir,
    ws: WorkspaceEntry,
    config: Config,
    db: Db,
}

impl Fixture {
    fn new(id: &str, languages: &[&str], files: &[(&str, &str)]) -> Self {
        let src = tempfile::tempdir().unwrap();
        for (path, body) in files {
            let full = src.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        let db_dir = tempfile::tempdir().unwrap();
        let ws = WorkspaceEntry {
            id: id.to_string(),
            root: PathBuf::from(src.path()),
            languages: languages.iter().map(|l| l.to_string()).collect(),
            frozen: false,
        };
        let config = make_config(db_dir.path());
        let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
        Fixture {
            _src: src,
            _db_dir: db_dir,
            ws,
            config,
            db,
        }
    }

    fn parse(&self) {
        let cancel = AtomicBool::new(false);
        pipeline::parse_workspace(
            &self.ws,
            &self.db,
            &self.config,
            &cancel,
            &default_registry(),
        )
        .unwrap();
    }

    fn file_id(&self, path: &str) -> i64 {
        self.db.file_by_path(path).unwrap().unwrap().id
    }

    fn cognitive_of(&self, file_id: i64, short_name: &str) -> i64 {
        let rows = self.db.find_symbols_by_file(file_id).unwrap();
        rows.iter()
            .find(|s| &*s.short_name == short_name)
            .and_then(|s| s.cognitive)
            .unwrap_or_else(|| panic!("{short_name} has no cognitive score"))
    }
}

const NESTED_JS: &str = r#"
function outer(xs) {
  function inner(y) {
    if (y) {
      for (const z of y) {
        if (z) {
          while (z > 0) {
            if (z > 1) { return 1; }
          }
        }
      }
    }
    return 0;
  }
  if (xs) { return inner(xs); }
  return 0;
}
"#;

#[test]
fn nested_js_function_is_not_counted_twice() {
    let fx = Fixture::new(
        "erosion-js",
        &["javascript"],
        &[("src/nested.js", NESTED_JS)],
    );
    fx.parse();
    let file = fx.file_id("src/nested.js");
    let outer = fx.cognitive_of(file, "outer");
    let inner = fx.cognitive_of(file, "inner");
    // Precondition: the walker folds inner into outer, and the parser also
    // extracts inner as its own scored symbol — the double count the rule avoids.
    assert!(inner > 0 && outer >= inner, "outer {outer}, inner {inner}");

    let by_file = erosion::load_samples_by_file(&fx.db).unwrap();
    let samples = &by_file[&file];
    assert_eq!(samples.len(), 1, "only outer counts: {samples:?}");
    assert_eq!(samples[0].cognitive, outer);
}

const DART_TEST: &str = r#"
void main() {
  group('g', () {
    test('t', () {
      for (var i = 0; i < 3; i++) {
        if (i > 1) {
          while (i > 2) {
            if (i == 3) { break; }
          }
        }
      }
    });
  });
}
"#;

const RUST_LIB: &str = r#"
pub fn prod(x: i32) -> i32 {
    if x > 0 { 1 } else { 0 }
}

#[test]
fn helper(x: i32) -> i32 {
    if x > 0 { if x > 1 { 2 } else { 1 } } else { 0 }
}

#[cfg(test)]
fn cfg_helper(x: i32) -> i32 {
    if x > 0 { if x > 1 { 2 } else { 1 } } else { 0 }
}

#[cfg(test)]
mod tests {
    fn nested(x: i32) -> i32 {
        if x > 0 { if x > 1 { 2 } else { 1 } } else { 0 }
    }
}
"#;

#[test]
fn test_code_contributes_no_mass() {
    let fx = Fixture::new(
        "erosion-tests",
        &["dart", "rust"],
        &[
            ("test/widget_test.dart", DART_TEST),
            ("src/lib.rs", RUST_LIB),
        ],
    );
    fx.parse();
    let dart = fx.file_id("test/widget_test.dart");
    let lib = fx.file_id("src/lib.rs");
    // Precondition: the excluded symbols really do carry complexity.
    assert!(fx.cognitive_of(dart, "main") > 0);
    assert!(fx.cognitive_of(lib, "helper") > 0);
    assert!(fx.cognitive_of(lib, "cfg_helper") > 0);
    assert!(fx.cognitive_of(lib, "nested") > 0);

    let by_file = erosion::load_samples_by_file(&fx.db).unwrap();
    assert!(
        !by_file.contains_key(&dart),
        "Dart test file main() excluded by path"
    );
    let cogs: Vec<i64> = by_file[&lib].iter().map(|s| s.cognitive).collect();
    assert_eq!(cogs, vec![fx.cognitive_of(lib, "prod")], "only prod counts");
}

fn trend_cmp(db: &Db) -> serde_json::Value {
    trend::handle(
        db,
        &TrendArgs {
            workspace: String::new(),
            from: None,
            to: None,
            path: None,
            limit: None,
        },
    )
    .unwrap()
}

#[test]
fn snapshots_record_erosion_and_legacy_rows_read_null() {
    let fx = Fixture::new(
        "erosion-snap",
        &["javascript"],
        &[("src/nested.js", NESTED_JS)],
    );
    fx.parse();
    let snap = fx.db.latest_snapshots(1).unwrap().remove(0);
    let recorded = snap.erosion.expect("a full parse records erosion");
    assert_eq!(recorded.version, EROSION_VERSION);
    let expected = erosion::workspace_aggregate(&erosion::load_samples_by_file(&fx.db).unwrap());
    assert_eq!(recorded.total_mass, expected.total_mass);
    assert_eq!(recorded.eroded_mass, expected.eroded_mass);
    assert!(recorded.total_mass > 0.0);

    // A checkpoint written without erosion (pre-migration shape) reads null, not 0.
    fx.db.insert_snapshot(&SnapshotParams::default()).unwrap();
    let legacy = fx.db.latest_snapshots(1).unwrap().remove(0);
    assert_eq!(legacy.erosion, None);

    // Trend against a legacy side: no erosion delta.
    let cmp = trend_cmp(&fx.db);
    assert!(cmp["deltas"]["eroded_mass"].is_null());
    assert!(cmp["deltas"]["total_mass"].is_null());
    assert!(cmp["to"]["erosion"].is_null());
}

#[test]
fn no_change_parse_carries_erosion_forward() {
    let fx = Fixture::new(
        "erosion-nochange",
        &["javascript"],
        &[("src/nested.js", NESTED_JS)],
    );
    fx.parse();
    let real = fx
        .db
        .latest_snapshots(1)
        .unwrap()
        .remove(0)
        .erosion
        .unwrap();

    // Same-version values are copied verbatim on a NoChanges parse.
    let marker = SnapshotErosion {
        eroded_mass: 123.0,
        total_mass: 456.0,
        version: EROSION_VERSION,
    };
    fx.db
        .insert_snapshot(&SnapshotParams {
            erosion: Some(marker),
            ..SnapshotParams::default()
        })
        .unwrap();
    fx.parse();
    let copied = fx.db.latest_snapshots(1).unwrap().remove(0);
    assert_eq!(copied.files_parsed, 0, "took the NoChanges path");
    assert_eq!(copied.erosion, Some(marker));

    // Trend between two same-version checkpoints reports the mass deltas.
    let cmp = trend_cmp(&fx.db);
    assert_eq!(cmp["deltas"]["eroded_mass"], serde_json::json!(0.0));
    assert_eq!(cmp["deltas"]["total_mass"], serde_json::json!(0.0));

    // A legacy (NULL) or other-version predecessor is recomputed, not copied.
    for previous in [
        None,
        Some(SnapshotErosion {
            version: EROSION_VERSION + 1,
            ..marker
        }),
    ] {
        fx.db
            .insert_snapshot(&SnapshotParams {
                erosion: previous,
                ..SnapshotParams::default()
            })
            .unwrap();
        fx.parse();
        let latest = fx.db.latest_snapshots(1).unwrap().remove(0);
        assert_eq!(latest.erosion, Some(real), "recomputed after {previous:?}");
    }

    // Different versions never produce a delta.
    fx.db
        .insert_snapshot(&SnapshotParams {
            erosion: Some(SnapshotErosion {
                version: EROSION_VERSION + 1,
                ..marker
            }),
            ..SnapshotParams::default()
        })
        .unwrap();
    let cmp = trend_cmp(&fx.db);
    assert!(cmp["deltas"]["eroded_mass"].is_null());
}

/// One linked JS module: calls its two siblings and carries a nested,
/// above-threshold function so every member file contributes erosion mass.
fn linked_module(name: &str, siblings: [&str; 2]) -> String {
    let [a, b] = siblings;
    format!(
        r#"import {{ {a} }} from './{a}.js';
import {{ {b} }} from './{b}.js';

export function {name}(xs) {{
  if (xs) {{
    for (const x of xs) {{
      if (x) {{
        while (x > 0) {{
          if (x > 1) {{ return {a}(x) + {b}(x); }}
        }}
      }}
    }}
  }}
  return {a}(0) + {b}(0);
}}
"#
    )
}

/// Two directories of three mutually-importing files: enough linked files for
/// clustering to form components on a real parse.
fn clustered_fixture(id: &str) -> Fixture {
    let groups = [
        ["core", "alpha", "beta", "gamma"],
        ["ui", "delta", "eps", "zeta"],
    ];
    let files: Vec<(String, String)> = groups
        .iter()
        .flat_map(|[dir, a, b, c]| {
            [(a, [b, c]), (b, [a, c]), (c, [a, b])].map(|(name, sibs)| {
                (
                    format!("src/{dir}/{name}.js"),
                    linked_module(name, [*sibs[0], *sibs[1]]),
                )
            })
        })
        .collect();
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    Fixture::new(id, &["javascript"], &refs)
}

#[test]
fn file_health_reports_file_and_component_erosion() {
    let fx = clustered_fixture("erosion-fh");
    fx.parse();
    let verdict = sutra::health::refresh::current_run_validity(
        &fx.db,
        &fx.ws.root,
        chrono::Utc::now().timestamp(),
    )
    .unwrap();
    let result =
        sutra::tools::file_health::handle(&fx.db, verdict, None, None, Some("all"), None, false)
            .unwrap();

    let file = result["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "src/core/alpha.js")
        .expect("file listed in mode=all");
    let block = &file["erosion"];
    assert_eq!(block["function_count"], 1);
    assert!(block["total_mass"].as_f64().unwrap() > 0.0);

    let components = result["components"].as_array().expect("components array");
    assert!(!components.is_empty(), "fixture must cluster: {result}");
    let by_file = erosion::load_samples_by_file(&fx.db).unwrap();
    let workspace = erosion::workspace_aggregate(&by_file);
    for comp in components {
        let id = comp["id"].as_str().expect("component id");
        let members = fx.db.component_file_ids(id).unwrap();
        assert!(!members.is_empty(), "component {id} has members");
        // Summed over exactly this component's member files.
        let expected = erosion::to_json(&erosion::files_aggregate(&by_file, members));
        assert_eq!(comp["erosion"], expected, "component erosion: {comp}");
        assert!(comp["erosion"]["total_mass"].as_f64().unwrap() > 0.0);
    }
    // With more than one component, no single one carries the whole workspace
    // mass — the block is per-component, not a workspace total echoed.
    if components.len() > 1 {
        for comp in components {
            assert!(comp["erosion"]["total_mass"].as_f64().unwrap() < workspace.total_mass);
        }
    }
}

#[test]
fn stale_membership_drops_component_erosion() {
    use std::sync::Arc;
    use sutra::tools::ToolContext;

    let fx = clustered_fixture("erosion-fh-stale");
    fx.parse();
    let refreshed = sutra::health::refresh::DemandOutcome::Refreshed(
        sutra::health::refresh::RefreshResult::Published(sutra::health::evidence::RunId(1)),
    );
    let db = Arc::new(Db::open_unchecked(&fx.ws.id, &fx.config.db_dir).unwrap());
    let ctx = ToolContext::for_test(db, fx.ws.root.clone());
    let call = || {
        sutra::tools::file_health::handle_ctx(&ctx, refreshed, None, None, Some("all"), None, false)
            .unwrap()
    };

    // Precondition: membership is current right after a full parse, so the
    // component erosion blocks are present.
    let current = call();
    let comps = current["components"]
        .as_array()
        .expect("components when current");
    assert!(!comps.is_empty());
    assert!(comps.iter().all(|c| c["erosion"].is_object()));

    // A clustering-config change the stored membership was not computed under.
    let sutra_dir = fx.ws.root.join(".sutra");
    std::fs::create_dir_all(&sutra_dir).unwrap();
    std::fs::write(sutra_dir.join("components.toml"), "resolution = 7.5").unwrap();

    let stale = call();
    assert!(stale.get("components").is_none(), "stale: {stale}");
    assert!(stale.get("total_components").is_none());
    assert_eq!(
        stale["components_unavailable"]["reason"],
        "stale_membership"
    );
    // Per-file erosion is a distinct axis and still reported.
    let files = stale["files"].as_array().unwrap();
    assert!(files.iter().all(|f| f["erosion"].is_object()));
}

fn file_health_json(fx: &Fixture, path: Option<&str>) -> serde_json::Value {
    let verdict = sutra::health::refresh::current_run_validity(
        &fx.db,
        &fx.ws.root,
        chrono::Utc::now().timestamp(),
    )
    .unwrap();
    sutra::tools::file_health::handle(&fx.db, verdict, path, None, Some("all"), None, false)
        .unwrap()
}

fn erosion_block<'a>(result: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
    &result["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == path)
        .unwrap_or_else(|| panic!("{path} listed: {result}"))["erosion"]
}

#[test]
fn path_excluded_test_file_is_marked_not_clean() {
    let fx = Fixture::new(
        "erosion-excluded",
        &["dart", "javascript"],
        &[
            ("test/widget_test.dart", DART_TEST),
            ("src/nested.js", NESTED_JS),
        ],
    );
    fx.parse();
    let result = file_health_json(&fx, None);

    let excluded = erosion_block(&result, "test/widget_test.dart");
    assert_eq!(excluded["excluded"], "test_file", "{excluded}");
    assert_eq!(excluded["function_count"], 0);

    let counted = erosion_block(&result, "src/nested.js");
    assert!(counted.get("excluded").is_none(), "{counted}");
    assert_eq!(counted["function_count"], 1);
}

#[test]
fn path_filtered_erosion_matches_unfiltered() {
    let fx = clustered_fixture("erosion-fh-path");
    fx.parse();
    let all = file_health_json(&fx, None);
    let one = file_health_json(&fx, Some("src/ui/eps.js"));
    assert_eq!(one["total_files"], 1);
    assert!(one.get("components").is_none());
    assert_eq!(
        erosion_block(&one, "src/ui/eps.js"),
        erosion_block(&all, "src/ui/eps.js")
    );
}

// ── sutra_review erosion delta (sutra/451) ──────────────────────────────────

const TS_CLASS: &str = r#"
class Base { run(x: number): number { return x; } }
class Impl extends Base {
  override run(x: number): number {
    if (x > 0) { for (let i = 0; i < x; i++) { if (i % 2) { while (i > 3) { if (i) { break; } } } } }
    const f = (y: number) => { if (y) { if (y > 1) { return 2; } } return 0; };
    return f(x);
  }
}
"#;

fn sorted_samples(samples: &[erosion::FunctionSample]) -> Vec<(i64, i64)> {
    let mut v: Vec<(i64, i64)> = samples.iter().map(|s| (s.cognitive, s.sloc)).collect();
    v.sort_unstable();
    v
}

/// The review path parses a diff side itself; its samples must be the ones
/// file_health reads from the index for the same bytes — selection, flattening,
/// test exclusion and parent links included.
#[test]
fn review_samples_match_file_health_samples() {
    let files = [
        ("src/nested.js", NESTED_JS),
        ("src/lib.rs", RUST_LIB),
        ("test/widget_test.dart", DART_TEST),
        ("src/impl.ts", TS_CLASS),
    ];
    let fx = Fixture::new(
        "erosion-review-parity",
        &["javascript", "rust", "dart", "typescript"],
        &files,
    );
    fx.parse();
    let ids: Vec<i64> = files.iter().map(|(p, _)| fx.file_id(p)).collect();
    let indexed = erosion::load_samples_for_files(&fx.db, &ids).unwrap();
    let registry = default_registry();
    let mut nonempty = 0;
    for ((path, src), id) in files.iter().zip(&ids) {
        let from_index = sorted_samples(indexed.get(id).map(Vec::as_slice).unwrap_or(&[]));
        let from_review = sorted_samples(
            &sutra::tools::erosion_delta::source_samples(&registry, path, src)
                .unwrap_or_else(|| panic!("{path} parses")),
        );
        assert_eq!(from_review, from_index, "{path}");
        nonempty += usize::from(!from_index.is_empty());
    }
    assert_eq!(
        nonempty, 3,
        "only the path-excluded Dart test file is empty"
    );
}

fn git(root: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git spawn");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A Rust fn whose cognitive score is 1 + 2 + … + depth (nested ifs).
fn nested_rs(name: &str, depth: usize) -> String {
    let mut body = String::from("0");
    for d in (0..depth).rev() {
        body = format!("if x > {d} {{ {body} }} else {{ 1 }}");
    }
    format!("pub fn {name}(x: i32) -> i32 {{\n    {body}\n}}\n")
}

#[test]
fn review_erosion_delta_over_git_diff() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@t"]);
    git(root, &["config", "user.name", "t"]);
    let write = |rel: &str, body: &str| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    write("src/a.rs", &nested_rs("grow", 3));
    write(
        "src/old.rs",
        &format!("{}{}", nested_rs("steady", 6), nested_rs("pad", 2)),
    );
    write("README.md", "docs\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "--no-verify", "-qm", "base"]);

    // grow crosses the threshold; old.rs is renamed unchanged; docs change.
    write("src/a.rs", &nested_rs("grow", 6));
    git(root, &["mv", "src/old.rs", "src/new.rs"]);
    write("README.md", "more docs\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "--no-verify", "-qm", "head"]);

    let scope = sutra::tools::review::resolve_diff_entries(root, "HEAD~1..HEAD").unwrap();
    let renamed = scope
        .entries
        .iter()
        .find(|e| e.path == "src/new.rs")
        .expect("rename entry");
    assert_eq!(renamed.old_path.as_deref(), Some("src/old.rs"));

    let registry = default_registry();
    let delta = sutra::tools::erosion_delta::compute(
        root,
        &scope.entries,
        &scope.base_revision,
        scope.head_revision.as_deref(),
        &registry,
    );
    let j = sutra::tools::erosion_delta::delta_json(&delta).expect("block");
    assert_eq!(j["status"], "complete", "{j}");
    assert_eq!(j["total"]["crossed_up"], 1, "{j}");
    assert_eq!(
        j["total"]["added_eroded"], 0,
        "rename is not an addition: {j}"
    );
    assert_eq!(j["total"]["deleted_eroded"], 0, "{j}");
    assert_eq!(j["functions"].as_array().unwrap().len(), 1, "{j}");
    assert_eq!(j["functions"][0]["symbol"], "grow");
    assert_eq!(j["functions"][0]["change"], "crossed_up");
    let files = j["files"].as_array().unwrap();
    assert!(
        files.iter().all(|f| f["path"] != "README.md"),
        "no adapter, not listed"
    );
    let new_rs = files
        .iter()
        .find(|f| f["path"] == "src/new.rs")
        .expect("renamed file");
    assert_eq!(new_rs["old_path"], "src/old.rs");
    assert_eq!(new_rs["eroded_mass_delta"], 0.0);
    assert!(new_rs["base"]["eroded_mass"].as_f64().unwrap() > 0.0);

    // Unstaged: head is the worktree, base is HEAD.
    write("src/a.rs", &nested_rs("grow", 2));
    let scope = sutra::tools::review::resolve_diff_entries(root, "unstaged").unwrap();
    assert_eq!(scope.head_revision, None);
    let delta = sutra::tools::erosion_delta::compute(
        root,
        &scope.entries,
        &scope.base_revision,
        scope.head_revision.as_deref(),
        &registry,
    );
    let j = sutra::tools::erosion_delta::delta_json(&delta).expect("block");
    assert_eq!(j["total"]["crossed_down"], 1, "{j}");
    assert!(j["total"]["eroded_mass_removed"].as_f64().unwrap() > 0.0);
}

/// Unstaged mode compares the index to the worktree on both sides: staged
/// changes (a threshold crossing, a new file, a rename) must not leak into an
/// unstaged review that only touches comments (sutra/458).
#[test]
fn unstaged_erosion_delta_reads_base_from_index() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@t"]);
    git(root, &["config", "user.name", "t"]);
    let write = |rel: &str, body: &str| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    write("src/a.rs", &nested_rs("grow", 3));
    write("src/old.rs", &nested_rs("steady", 6));
    git(root, &["add", "-A"]);
    git(root, &["commit", "--no-verify", "-qm", "base"]);

    // Staged: grow crosses the threshold, a new eroded file, a rename.
    write("src/a.rs", &nested_rs("grow", 6));
    write("src/fresh.rs", &nested_rs("fresh", 6));
    git(root, &["mv", "src/old.rs", "src/new.rs"]);
    git(root, &["add", "-A"]);

    // Unstaged: comment-only edits on top of each staged file.
    for (rel, body) in [
        ("src/a.rs", nested_rs("grow", 6)),
        ("src/fresh.rs", nested_rs("fresh", 6)),
        ("src/new.rs", nested_rs("steady", 6)),
    ] {
        write(rel, &format!("// unstaged note\n{body}"));
    }

    let scope = sutra::tools::review::resolve_diff_entries(root, "unstaged").unwrap();
    let mut paths = scope.paths();
    paths.sort();
    assert_eq!(paths, ["src/a.rs", "src/fresh.rs", "src/new.rs"]);
    assert_eq!(scope.head_revision, None);
    // Staged-new and staged-renamed files exist on the base (index) side.
    for p in ["src/fresh.rs", "src/new.rs"] {
        let base = sutra::git::file_content_on_side(root, Some(&scope.base_revision), p).unwrap();
        assert!(base.is_some(), "{p} missing from base side");
    }

    let delta = sutra::tools::erosion_delta::compute(
        root,
        &scope.entries,
        &scope.base_revision,
        scope.head_revision.as_deref(),
        &default_registry(),
    );
    let j = sutra::tools::erosion_delta::delta_json(&delta).expect("block");
    assert_eq!(j["status"], "complete", "{j}");
    for key in [
        "crossed_up",
        "crossed_down",
        "added_eroded",
        "deleted_eroded",
    ] {
        assert_eq!(j["total"][key], 0, "{key}: {j}");
    }
    assert_eq!(j["functions"].as_array().unwrap().len(), 0, "{j}");
    for f in j["files"].as_array().unwrap() {
        assert_eq!(f["eroded_mass_delta"], 0.0, "{j}");
        assert!(f["base"]["eroded_mass"].as_f64().unwrap() > 0.0, "{j}");
    }
}
