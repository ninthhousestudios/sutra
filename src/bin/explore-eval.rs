//! Explore ranking eval harness (sutra/390).
//!
//! Replays captured `sutra_explore` queries against the CURRENT index and
//! reports how well the live ranking surfaces what the agent actually
//! fetched. It is the ruler every downstream ranking change (sutra/371,
//! sutra/372, the final blend/gate) measures itself against.
//!
//! For each eval row it re-runs `explore::handle` with the row's query and
//! budget, then locates the agent's `fetched` symbols in the fresh ranking and
//! reports (over SCORABLE rows only) top-1 hit rate (best fetched symbol at
//! rank 0), top-3 hit rate (some fetched symbol in the top 3), and MRR (mean
//! reciprocal rank of the best fetched symbol).
//!
//! Rows where the agent fetched something explore did NOT surface — but which
//! still exists in the index — are RECALL MISSES, reported separately (the
//! ranking's recall ceiling, not a rank problem). Rows whose workspace or
//! fetched symbols no longer exist are excluded and bucketed so the
//! denominator stays honest.
//!
//! Usage: sutra-explore-eval [PATH_TO_JSONL]
//!   default PATH: ~/soft/sutra-surveys/explore-eval-2026-09-09.jsonl

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use sutra::config::Config;
use sutra::db::Db;
use sutra::error::Result;
use sutra::tools::explore;
use sutra::workspace::{self, WorkspacesConfig};

#[derive(Deserialize)]
struct EvalRow {
    /// Workspace id, root path, or basename — or null when the capture could
    /// not attribute the call to a workspace.
    workspace: Option<String>,
    query: String,
    #[serde(default)]
    budget: Option<i64>,
    /// Symbols the agent read after the explore call. Recall target.
    #[serde(default)]
    fetched: Vec<String>,
}

/// A fetched symbol matches a ranked/candidate name if they are equal or one
/// is the unqualified tail of the other (`Foo::bar` vs `bar`).
fn name_matches(candidate: &str, fetched: &str) -> bool {
    candidate == fetched
        || candidate.ends_with(&format!("::{fetched}"))
        || fetched.ends_with(&format!("::{candidate}"))
}

/// Rank of the best (lowest-index) fetched symbol in the fresh ranking.
fn best_rank(returned_now: &[String], fetched: &[String]) -> Option<usize> {
    fetched
        .iter()
        .filter_map(|f| returned_now.iter().position(|c| name_matches(c, f)))
        .min()
}

/// Does at least one fetched symbol still exist in the index (regardless of
/// whether explore surfaced it)? Distinguishes a recall miss from a symbol
/// that was renamed/deleted since capture.
fn any_fetched_resolves(db: &Db, fetched: &[String]) -> bool {
    fetched.iter().any(|f| {
        let seg = f.rsplit("::").next().unwrap_or(f);
        db.find_symbols_by_name_tiered(seg, None, 50)
            .map(|(rows, _)| {
                rows.iter()
                    .any(|r| name_matches(r.qualified_name.as_ref(), f))
            })
            .unwrap_or(false)
    })
}

fn workspace_indexed(db: &Db) -> bool {
    db.distinct_symbol_kinds()
        .map(|k| !k.is_empty())
        .unwrap_or(false)
}

/// Per-slice tallies. Aggregated globally and per workspace.
#[derive(Default)]
struct Stats {
    ranks: Vec<usize>, // best_rank of each scorable row
    mrr_sum: f64,
    recall_miss: usize,
    unscorable_gone: usize, // workspace present but fetched symbol no longer exists
}

impl Stats {
    fn record_scored(&mut self, rank: usize) {
        self.ranks.push(rank);
        self.mrr_sum += 1.0 / (rank as f64 + 1.0);
    }

    fn report(&self, label: &str) {
        let n = self.ranks.len();
        if n == 0 {
            println!(
                "  {label:<20} scored=0  recall_miss={}  gone={}",
                self.recall_miss, self.unscorable_gone
            );
            return;
        }
        let top1 = self.ranks.iter().filter(|&&r| r == 0).count();
        let top3 = self.ranks.iter().filter(|&&r| r < 3).count();
        let mrr = self.mrr_sum / n as f64;
        println!(
            "  {label:<20} scored={n:<4} top1={:>5.1}%  top3={:>5.1}%  MRR={mrr:.3}  recall_miss={}  gone={}",
            100.0 * top1 as f64 / n as f64,
            100.0 * top3 as f64 / n as f64,
            self.recall_miss,
            self.unscorable_gone,
        );
    }
}

/// Lazily opens (and caches) the index for a resolved workspace. The cache
/// value is `None` when the workspace could not be opened or holds no symbols.
/// Returns the live index and the workspace's canonical id (borrowed from the
/// config), or `None` when the workspace name does not resolve.
fn get_db<'a>(
    cache: &'a mut HashMap<String, Option<Db>>,
    ws_config: &'a WorkspacesConfig,
    config: &Config,
    ws_name: &str,
) -> Option<(&'a Db, &'a str)> {
    let entry = workspace::resolve_workspace(ws_config, ws_name).ok()?;
    if !cache.contains_key(entry.id.as_str()) {
        let opened = Db::open_for_workspace(entry, &config.db_dir)
            .ok()
            .and_then(|db| workspace_indexed(&db).then_some(db));
        cache.insert(String::from(entry.id.as_str()), opened);
    }
    let db = cache.get(entry.id.as_str())?.as_ref()?;
    Some((db, entry.id.as_str()))
}

fn main() -> Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/soft/sutra-surveys/explore-eval-2026-09-09.jsonl")
    });

    let config = Config::from_env()?;
    let ws_config = workspace::load_workspaces(&config.workspaces_path)?;
    let content = std::fs::read_to_string(&path)?;

    let mut db_cache: HashMap<String, Option<Db>> = HashMap::new();
    let mut global = Stats::default();
    let mut per_ws: HashMap<String, Stats> = HashMap::new();

    let mut total = 0usize;
    let mut malformed = 0usize;
    let mut no_workspace = 0usize;
    let mut no_fetch = 0usize;
    let mut explore_errors = 0usize;
    let mut ws_unavailable: HashMap<String, usize> = HashMap::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        total += 1;
        let row: EvalRow = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => {
                malformed += 1;
                continue;
            }
        };
        let Some(ws_name) = row.workspace.as_deref() else {
            no_workspace += 1;
            continue;
        };
        if row.fetched.is_empty() {
            // Nothing to score against; the agent read nothing off this call.
            no_fetch += 1;
            continue;
        }

        let Some((db, ws_id)) = get_db(&mut db_cache, &ws_config, &config, ws_name) else {
            *ws_unavailable.entry(String::from(ws_name)).or_default() += 1;
            continue;
        };

        let budget = row.budget.unwrap_or(10);
        let result = match explore::handle(db, Path::new(""), &row.query, budget, false) {
            Ok(v) => v,
            Err(_) => {
                explore_errors += 1;
                continue;
            }
        };
        let returned_now: Vec<String> = result["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i["symbol"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let bucket = per_ws.entry(String::from(ws_id)).or_default();
        match best_rank(&returned_now, &row.fetched) {
            Some(rank) => {
                global.record_scored(rank);
                bucket.record_scored(rank);
            }
            None if any_fetched_resolves(db, &row.fetched) => {
                global.recall_miss += 1;
                bucket.recall_miss += 1;
            }
            None => {
                global.unscorable_gone += 1;
                bucket.unscorable_gone += 1;
            }
        }
    }

    println!("explore ranking eval — {path}");
    println!(
        "rows total={total}  malformed={malformed}  no_workspace={no_workspace}  \
         no_fetch={no_fetch}  explore_errors={explore_errors}"
    );
    let unavail_total: usize = ws_unavailable.values().sum();
    if unavail_total > 0 {
        let mut names: Vec<_> = ws_unavailable.iter().collect();
        names.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
        let list: Vec<String> = names.iter().map(|(n, c)| format!("{n}={c}")).collect();
        println!(
            "workspace unavailable rows={unavail_total}  [{}]",
            list.join(" ")
        );
    }
    println!();

    println!("OVERALL");
    global.report("all");
    println!();

    println!("PER WORKSPACE");
    let mut rows: Vec<_> = per_ws.iter().collect();
    rows.sort_by_key(|(_, s)| std::cmp::Reverse(s.ranks.len()));
    for (ws, stats) in &rows {
        stats.report(ws);
    }

    Ok(())
}
