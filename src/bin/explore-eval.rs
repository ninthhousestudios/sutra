//! Explore ranking eval harness (sutra/390).
//!
//! Replays captured `sutra_explore` queries against the CURRENT index and
//! reports how well the live ranking surfaces what the agent actually
//! fetched. It is the ruler every downstream ranking change (sutra/371,
//! sutra/372, the final blend/gate) measures itself against.
//!
//! The scored population is partitioned on the CAPTURED `returned` list, not on
//! the fresh replay, so the denominator is invariant across ranking changes
//! (sutra/390 review F1):
//!   - A fetched target the capture never surfaced is a RECALL MISS — a
//!     retrieval/candidate-generation ceiling, not a rank problem. Fixed by the
//!     capture, so the same rows are excluded from every run.
//!   - A fetched target the capture DID surface is SCORABLE. Its rank is read
//!     from the fresh (budget-truncated) ranking: found → top-1/top-3/MRR;
//!     dropped below budget but still indexed → a genuine ranking miss (RR=0,
//!     counted in the denominator); no longer indexed → GONE (excluded).
//!
//! Matching between the captured labels and the fresh qualified names is
//! collision-aware (review F2): exact equality wins; an unqualified label is
//! only credited to a `::`-tail candidate when that tail is unique, otherwise
//! the row is bucketed AMBIGUOUS and excluded rather than manufacturing a hit.
//!
//! Usage: sutra-explore-eval [PATH_TO_JSONL]
//!   default PATH: ~/soft/sutra-surveys/explore-eval-2026-09-09.jsonl

use std::collections::{HashMap, HashSet};
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
    /// The ranking explore returned at capture time. Defines the recall
    /// ceiling: anything the agent fetched that is NOT here is a recall miss,
    /// independent of the ranking under test.
    #[serde(default)]
    returned: Vec<String>,
    /// Symbols the agent read after the explore call. Recall target.
    #[serde(default)]
    fetched: Vec<String>,
}

/// Whether `a` is a `::`-qualified tail extension of `b` (or vice versa),
/// e.g. `Foo::bar` vs `bar`. Exact equality is handled by the caller.
fn is_tail(a: &str, b: &str) -> bool {
    a.ends_with(&format!("::{b}")) || b.ends_with(&format!("::{a}"))
}

/// Outcome of locating a single fetched label within a candidate name list.
enum Locate {
    /// Matched at this 0-based position (exact, or a unique `::`-tail).
    Rank(usize),
    /// Only tail matches, pointing at >1 distinct candidate — cannot attribute.
    Ambiguous,
    /// No candidate matches.
    Miss,
}

/// Locate one fetched label. Exact equality wins (min index); failing that, a
/// `::`-tail match counts only when it resolves to a single distinct candidate.
fn locate(candidates: &[String], fetched: &str) -> Locate {
    if let Some(r) = candidates.iter().position(|c| c == fetched) {
        return Locate::Rank(r);
    }
    let tails: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| is_tail(c, fetched))
        .map(|(i, _)| i)
        .collect();
    match tails.as_slice() {
        [] => Locate::Miss,
        [only] => Locate::Rank(*only),
        many => {
            let distinct: HashSet<&str> = many.iter().map(|&i| candidates[i].as_str()).collect();
            if distinct.len() == 1 {
                Locate::Rank(many[0])
            } else {
                Locate::Ambiguous
            }
        }
    }
}

/// Best outcome across all fetched labels: a concrete rank if any label
/// resolves to one (lowest rank wins), else Ambiguous if any label collided,
/// else Miss.
fn best_locate(candidates: &[String], fetched: &[String]) -> Locate {
    let mut best: Option<usize> = None;
    let mut ambiguous = false;
    for f in fetched {
        match locate(candidates, f) {
            Locate::Rank(r) => best = Some(best.map_or(r, |b| b.min(r))),
            Locate::Ambiguous => ambiguous = true,
            Locate::Miss => {}
        }
    }
    match (best, ambiguous) {
        (Some(r), _) => Locate::Rank(r),
        (None, true) => Locate::Ambiguous,
        (None, false) => Locate::Miss,
    }
}

/// Does at least one fetched symbol still exist in the index? Prefers an exact
/// qualified match; an unqualified label counts only when it resolves to a
/// single distinct symbol, so the many same-named symbols in a real index
/// (e.g. 34 `handle`s) cannot make a deleted target look present (review F2).
fn any_fetched_resolves(db: &Db, fetched: &[String]) -> bool {
    fetched.iter().any(|f| {
        let seg = f.rsplit("::").next().unwrap_or(f);
        let Ok((rows, _)) = db.find_symbols_by_name_tiered(seg, None, 50) else {
            return false;
        };
        if rows.iter().any(|r| r.qualified_name.as_ref() == f) {
            return true;
        }
        if f.contains("::") {
            // A qualified label must match exactly; no fuzzy fallback.
            return false;
        }
        let distinct: HashSet<&str> = rows
            .iter()
            .filter(|r| is_tail(r.qualified_name.as_ref(), f))
            .map(|r| r.qualified_name.as_ref())
            .collect();
        distinct.len() == 1
    })
}

fn workspace_indexed(db: &Db) -> bool {
    db.distinct_symbol_kinds()
        .map(|k| !k.is_empty())
        .unwrap_or(false)
}

/// Per-slice tallies. Aggregated globally and per workspace. The scored
/// denominator is `hits + miss_below_budget`; recall_miss / ambiguous / gone
/// are reported alongside but excluded from it.
#[derive(Default)]
struct Stats {
    ranks: Vec<usize>,        // rank of each scored hit (found within budget)
    mrr_sum: f64,             // sum of 1/(rank+1) over hits (misses contribute 0)
    miss_below_budget: usize, // surfaced at capture, still indexed, now below budget
    recall_miss: usize,       // never surfaced at capture (run-invariant)
    ambiguous: usize,         // fetched label collides; cannot attribute confidently
    gone: usize,              // surfaced at capture but no longer in the index
}

impl Stats {
    fn record_hit(&mut self, rank: usize) {
        self.ranks.push(rank);
        self.mrr_sum += 1.0 / (rank as f64 + 1.0);
    }

    /// Scorable target the current ranking dropped below budget: counts toward
    /// the denominator with reciprocal rank 0.
    fn record_miss_below_budget(&mut self) {
        self.miss_below_budget += 1;
    }

    fn scored_n(&self) -> usize {
        self.ranks.len() + self.miss_below_budget
    }

    fn report(&self, label: &str) {
        let n = self.scored_n();
        if n == 0 {
            println!(
                "  {label:<20} scored=0  recall_miss={}  ambiguous={}  gone={}",
                self.recall_miss, self.ambiguous, self.gone
            );
            return;
        }
        let top1 = self.ranks.iter().filter(|&&r| r == 0).count();
        let top3 = self.ranks.iter().filter(|&&r| r < 3).count();
        let mrr = self.mrr_sum / n as f64;
        println!(
            "  {label:<20} scored={n:<4} top1={:>5.1}%  top3={:>5.1}%  MRR={mrr:.3}  \
             miss_below_budget={}  recall_miss={}  ambiguous={}  gone={}",
            100.0 * top1 as f64 / n as f64,
            100.0 * top3 as f64 / n as f64,
            self.miss_below_budget,
            self.recall_miss,
            self.ambiguous,
            self.gone,
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

        // Partition on the CAPTURED ranking so the scored denominator does not
        // move with the ranking under test (review F1). Only targets the
        // capture surfaced are scored against the fresh ranking.
        match best_locate(&row.returned, &row.fetched) {
            Locate::Miss => {
                global.recall_miss += 1;
                bucket.recall_miss += 1;
            }
            Locate::Ambiguous => {
                global.ambiguous += 1;
                bucket.ambiguous += 1;
            }
            Locate::Rank(_) => match best_locate(&returned_now, &row.fetched) {
                Locate::Rank(rank) => {
                    global.record_hit(rank);
                    bucket.record_hit(rank);
                }
                Locate::Ambiguous => {
                    global.ambiguous += 1;
                    bucket.ambiguous += 1;
                }
                Locate::Miss => {
                    if any_fetched_resolves(db, &row.fetched) {
                        // Surfaced at capture, still indexed, but the current
                        // ranking dropped it below budget: a real ranking miss.
                        global.record_miss_below_budget();
                        bucket.record_miss_below_budget();
                    } else {
                        global.gone += 1;
                        bucket.gone += 1;
                    }
                }
            },
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
    rows.sort_by_key(|(_, s)| std::cmp::Reverse(s.scored_n()));
    for (ws, stats) in &rows {
        stats.report(ws);
    }

    Ok(())
}
