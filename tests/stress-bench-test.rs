//! Opt-in stress bench for large-workspace parse + resolution throughput
//! (sutra/191). Generates a synthetic C corpus big enough to activate the
//! bulk write path (>= BULK_MODE_THRESHOLD files) and asserts resolution
//! quality, so resolver or write-path regressions fail loudly instead of
//! silently turning a Linux-scale parse intractable again.
//!
//! Run with: cargo test --release --test stress-bench-test -- --ignored --nocapture

use std::fmt::Write as _;
use std::path::PathBuf;

use sutra::config::Config;
use sutra::db::Db;
use sutra::parser::adapter::default_registry;
use sutra::pipeline;
use sutra::workspace::WorkspaceEntry;

const FILE_COUNT: usize = 1200;
const FNS_PER_FILE: usize = 8;

fn make_config(db_dir: &std::path::Path) -> Config {
    Config {
        db_dir: db_dir.to_path_buf(),
        workspaces_path: db_dir.join("workspaces.toml"),
        listen_addr: "127.0.0.1:0".to_string(),
        parse_parallelism: 1,
        stale_threshold_sec: 600,
        log_level: "warn".to_string(),
        constraints_idle_timeout_sec: 1800,
        parse_timeout_ms: 5000,
    }
}

/// Every function name is globally unique, and each body calls one same-file
/// helper and one cross-file function, so near-all call refs must resolve
/// via the global short-name path.
fn write_corpus(root: &std::path::Path) {
    for i in 0..FILE_COUNT {
        let mut src = String::new();
        let _ = writeln!(src, "static int helper_{i}(int x) {{ return x + {i}; }}");
        for j in 0..FNS_PER_FILE {
            let ni = (i + 1) % FILE_COUNT;
            let nj = (j + 1) % FNS_PER_FILE;
            let _ = writeln!(
                src,
                "int fn_{i}_{j}(int x) {{ return helper_{i}(x) + fn_{ni}_{nj}(x); }}"
            );
        }
        std::fs::write(root.join(format!("unit_{i}.c")), src).unwrap();
    }
}

#[tokio::test]
#[ignore = "stress bench: run explicitly with --ignored --nocapture, ideally --release"]
async fn stress_c_corpus_parse_and_resolution() {
    let src_dir = tempfile::tempdir().unwrap();
    write_corpus(src_dir.path());

    let db_dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceEntry {
        id: "stress-c".to_string(),
        root: PathBuf::from(src_dir.path()),
        languages: vec!["c".to_string()],
        frozen: false,
    };
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let registry = default_registry();

    let snap = pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    assert_eq!(snap.files_parsed, FILE_COUNT as i64);
    assert_eq!(
        snap.symbols_extracted,
        (FILE_COUNT * (FNS_PER_FILE + 1)) as i64
    );

    // Each fn body has 2 calls; every callee exists and is globally unique,
    // so resolution quality below 90% means the resolver regressed.
    let resolvable = snap.resolved_count + snap.unresolved_count;
    assert!(resolvable > 0, "no resolvable refs extracted");
    let resolved_frac = snap.resolved_count as f64 / resolvable as f64;
    assert!(
        resolved_frac >= 0.9,
        "resolution quality regressed: {} of {} resolved ({resolved_frac:.2})",
        snap.resolved_count,
        resolvable
    );

    let secs = snap.duration_ms as f64 / 1000.0;
    println!(
        "stress bench: {} files, {} symbols, {} refs in {secs:.1}s \
         ({:.0} files/min, {:.0} resolvable refs/sec end-to-end)",
        snap.files_parsed,
        snap.symbols_extracted,
        snap.refs_extracted,
        snap.files_parsed as f64 / secs * 60.0,
        resolvable as f64 / secs
    );

    // A second parse with nothing changed must take the cheap NoChanges path.
    // files_parsed==0 alone doesn't prove the first pass fully resolved: a file
    // stuck at needs_resolution=1 still has an unchanged content hash, so it's
    // skipped by the parse loop (files_parsed stays 0) yet re-resolved via
    // has_pending_work. Asserting zero re-resolution is what actually guards
    // that the bulk-batched first pass committed every file's resolution.
    let snap2 = pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    assert_eq!(snap2.files_parsed, 0, "unchanged reparse reparsed files");
    assert_eq!(
        snap2.resolved_count + snap2.unresolved_count,
        0,
        "unchanged reparse re-resolved refs — a file was left stuck at \
         needs_resolution by the bulk-batched first pass"
    );
}
