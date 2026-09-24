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
