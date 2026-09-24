//! Structural erosion: how concentrated a scope's complexity mass is (sutra/403,
//! sutra/442; adapted from trellis's `erosion.ts`).
//!
//! A standalone parse-derived metric, NOT a health biomarker: it produces no
//! findings, no producer outcome and no deduction, and never enters a score
//! basis. It is a pure function of the `symbols` table.
//!
//! - `mass(fn) = cognitive × sqrt(sloc)`, where sloc is the symbol's line span.
//! - A function is eroded when its cognitive score reaches [`COGNITIVE_THRESHOLD`].
//! - Only outermost complexity-bearing symbols count: the complexity walkers
//!   descend into nested functions, so a nested JS/TS function's score is already
//!   inside its parent's and counting both would double it.
//! - Test code is excluded: test DSLs (Dart `group()`/`test()` closures) inflate
//!   cognitive nesting without being debt.
//! - Scopes always SUM function masses; a scope is never an average of child
//!   scopes. Rank and trend by absolute `eroded_mass`; `eroded_share` is
//!   descriptive only (it is non-monotone and bimodal at component scope).

use std::collections::{HashMap, HashSet};

use serde_json::json;

use crate::components::is_test_file;
use crate::db::{Db, FileRow, InsertSymbolParams, SymbolComplexityRow};
use crate::error::Result;
use crate::parser::flags_mark_test;

/// Version of the erosion formula stored on each snapshot. Bump on any change
/// to mass, the threshold, or symbol selection: trend compares erosion only
/// between snapshots with the same version. Selection also depends on parser
/// output — a parser change to test flags, cognitive scores, or parent links
/// that alters which symbols count needs a bump too; the parser stamp forces a
/// reparse but does not gate trend comparability.
///
/// 2: free `#[cfg(test)]` Rust items are flagged and excluded (sutra/445).
pub const EROSION_VERSION: i64 = 2;

/// Cognitive complexity at or above which a function is over threshold. Shared
/// by erosion ("eroded") and diff_impact's risk gate so the two never disagree
/// on what "complex" means. 15 is Sonar's default.
pub const COGNITIVE_THRESHOLD: i64 = 15;

/// One outermost, non-test function's complexity input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FunctionSample {
    pub cognitive: i64,
    pub sloc: i64,
}

/// Summed erosion over a scope. Empty scopes carry `None` share/percentiles —
/// "no functions" is not "no erosion".
#[derive(Debug, Clone, PartialEq)]
pub struct ErosionAggregate {
    pub total_mass: f64,
    pub eroded_mass: f64,
    pub eroded_count: usize,
    pub function_count: usize,
    pub eroded_share: Option<f64>,
    pub cognitive_p50: Option<i64>,
    pub cognitive_p90: Option<i64>,
    pub cognitive_max: Option<i64>,
}

pub fn function_mass(sample: FunctionSample) -> f64 {
    sample.cognitive as f64 * (sample.sloc.max(1) as f64).sqrt()
}

pub fn is_eroded(cognitive: i64) -> bool {
    cognitive >= COGNITIVE_THRESHOLD
}

pub fn aggregate<'a>(samples: impl IntoIterator<Item = &'a FunctionSample>) -> ErosionAggregate {
    let mut total_mass = 0.0;
    let mut eroded_mass = 0.0;
    let mut eroded_count = 0;
    let mut cognitive: Vec<i64> = Vec::new();
    for &s in samples {
        let mass = function_mass(s);
        total_mass += mass;
        if is_eroded(s.cognitive) {
            eroded_mass += mass;
            eroded_count += 1;
        }
        cognitive.push(s.cognitive);
    }
    cognitive.sort_unstable();
    ErosionAggregate {
        total_mass,
        eroded_mass,
        eroded_count,
        function_count: cognitive.len(),
        eroded_share: (total_mass > 0.0).then(|| eroded_mass / total_mass),
        cognitive_p50: nearest_rank(&cognitive, 50),
        cognitive_p90: nearest_rank(&cognitive, 90),
        cognitive_max: cognitive.last().copied(),
    }
}

/// Nearest-rank percentile of an ascending slice: the value at rank
/// `ceil(p/100 × n)` (1-based). `None` when empty.
fn nearest_rank(sorted: &[i64], percentile: usize) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (percentile * sorted.len()).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

/// Outermost, non-test function samples grouped by file id.
///
/// `test_files` holds files excluded by path ([`is_test_file`]);
/// `languages` maps file id to language for flag interpretation.
pub fn samples_by_file(
    rows: &[SymbolComplexityRow],
    test_files: &HashSet<i64>,
    languages: &HashMap<i64, &str>,
) -> HashMap<i64, Vec<FunctionSample>> {
    let mut out: HashMap<i64, Vec<FunctionSample>> = HashMap::new();
    for (row, sample) in select_samples(rows, test_files, languages) {
        out.entry(row.file_id).or_default().push(sample);
    }
    out
}

/// The erosion selection rule, shared by the index path ([`samples_by_file`])
/// and the diff path ([`parsed_samples`]) so the two cannot drift: every
/// complexity-bearing row that is outermost and not test code, with its sample.
///
/// A symbol is dropped when its file is in `test_files`, when any ancestor
/// carries a cognitive score (it is already folded into that ancestor), or when
/// it or any ancestor is flagged as test code.
pub fn select_samples<'r>(
    rows: &'r [SymbolComplexityRow],
    test_files: &HashSet<i64>,
    languages: &HashMap<i64, &str>,
) -> Vec<(&'r SymbolComplexityRow, FunctionSample)> {
    let by_id: HashMap<i64, &SymbolComplexityRow> = rows.iter().map(|r| (r.id, r)).collect();
    let is_test = |r: &SymbolComplexityRow| {
        flags_mark_test(r.flags, languages.get(&r.file_id).copied().unwrap_or(""))
    };

    let mut out = Vec::new();
    for row in rows {
        let Some(cognitive) = row.cognitive else {
            continue;
        };
        if test_files.contains(&row.file_id) || is_test(row) {
            continue;
        }
        // Walk the ancestor chain; the step bound guards against a corrupt
        // parent cycle rather than looping forever.
        let mut excluded = false;
        let mut parent = row.parent_symbol_id;
        let mut steps = 0;
        while let Some(pid) = parent
            && steps < rows.len()
        {
            let Some(ancestor) = by_id.get(&pid) else {
                break;
            };
            if ancestor.cognitive.is_some() || is_test(ancestor) {
                excluded = true;
                break;
            }
            parent = ancestor.parent_symbol_id;
            steps += 1;
        }
        if excluded {
            continue;
        }
        out.push((
            row,
            FunctionSample {
                cognitive,
                sloc: row.end_line - row.start_line + 1,
            },
        ));
    }
    out
}

/// Erosion samples of one freshly parsed file that is not read from the index
/// (a diff's base or head side), as `(index into flat, sample)`.
///
/// `flat`/`parents` must come from
/// [`crate::parser::persist::flatten_symbols_for_insert`], the flattening the
/// index persists: the rows handed to [`select_samples`] then match what
/// [`load_samples_for_files`] reads back for the same bytes, up to row ids.
pub fn parsed_samples(
    flat: &[InsertSymbolParams<'_>],
    parents: &[Option<usize>],
    path: &str,
    language: &str,
) -> Vec<(usize, FunctionSample)> {
    const FILE: i64 = 0;
    // Row id = flat index; the parent index maps onto the same id space.
    let rows: Vec<SymbolComplexityRow> = (0_i64..)
        .zip(flat.iter().zip(parents))
        .map(|(id, (p, parent))| SymbolComplexityRow {
            id,
            file_id: FILE,
            parent_symbol_id: parent.and_then(|pi| i64::try_from(pi).ok()),
            cognitive: p.cognitive,
            start_line: p.start_line,
            end_line: p.end_line,
            flags: p.flags,
        })
        .collect();
    let test_files: HashSet<i64> = file_exclusion(path).map(|_| FILE).into_iter().collect();
    let languages = HashMap::from([(FILE, language)]);
    select_samples(&rows, &test_files, &languages)
        .into_iter()
        .map(|(row, sample)| {
            let idx = usize::try_from(row.id).expect("invariant: row ids are flat indices");
            (idx, sample)
        })
        .collect()
}

/// Load the erosion samples of every indexed file.
pub fn load_samples_by_file(db: &Db) -> Result<HashMap<i64, Vec<FunctionSample>>> {
    let files = db.all_files()?;
    let rows = db.symbol_complexity_rows()?;
    Ok(samples_for(files.iter(), &rows))
}

/// Load the erosion samples of `file_ids` only — for callers that report a
/// handful of files and must not scan the whole `symbols` table.
pub fn load_samples_for_files(
    db: &Db,
    file_ids: &[i64],
) -> Result<HashMap<i64, Vec<FunctionSample>>> {
    let files = db.files_by_ids(file_ids)?;
    let rows = db.symbol_complexity_rows_for_files(file_ids)?;
    Ok(samples_for(files.values(), &rows))
}

fn samples_for<'a>(
    files: impl IntoIterator<Item = &'a FileRow>,
    rows: &[SymbolComplexityRow],
) -> HashMap<i64, Vec<FunctionSample>> {
    let mut test_files: HashSet<i64> = HashSet::new();
    let mut languages: HashMap<i64, &str> = HashMap::new();
    for f in files {
        if is_test_file(&f.path) {
            test_files.insert(f.id);
        }
        languages.insert(f.id, f.language.as_str());
    }
    samples_by_file(rows, &test_files, &languages)
}

/// Why a file is excluded from erosion wholesale, if it is. Path-excluded test
/// files carry no samples, so without this marker their block would read the
/// same as a file with no functions.
pub fn file_exclusion(path: &str) -> Option<&'static str> {
    is_test_file(path).then_some("test_file")
}

/// One file's erosion block, with an `excluded` marker when the whole file is
/// out of scope.
pub fn file_json(
    by_file: &HashMap<i64, Vec<FunctionSample>>,
    file_id: i64,
    path: &str,
) -> serde_json::Value {
    let mut block = to_json(&files_aggregate(by_file, [file_id]));
    if let Some(reason) = file_exclusion(path) {
        block["excluded"] = json!(reason);
    }
    block
}

/// Workspace erosion: summed over files (a file can belong to several
/// components, so summing components would double-count).
pub fn workspace_aggregate(by_file: &HashMap<i64, Vec<FunctionSample>>) -> ErosionAggregate {
    aggregate(by_file.values().flatten())
}

/// Erosion summed over a set of files (e.g. a component's members).
pub fn files_aggregate(
    by_file: &HashMap<i64, Vec<FunctionSample>>,
    file_ids: impl IntoIterator<Item = i64>,
) -> ErosionAggregate {
    let unique: HashSet<i64> = file_ids.into_iter().collect();
    aggregate(unique.iter().filter_map(|id| by_file.get(id)).flatten())
}

pub fn to_json(agg: &ErosionAggregate) -> serde_json::Value {
    json!({
        "eroded_mass": round2(agg.eroded_mass),
        "total_mass": round2(agg.total_mass),
        "eroded_count": agg.eroded_count,
        "function_count": agg.function_count,
        "eroded_share": agg.eroded_share.map(|s| (s * 1000.0).round() / 1000.0),
        "cognitive_p50": agg.cognitive_p50,
        "cognitive_p90": agg.cognitive_p90,
        "cognitive_max": agg.cognitive_max,
        "threshold": COGNITIVE_THRESHOLD,
    })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::rust::{FLAG_CFG_TEST, FLAG_TEST};

    fn s(cognitive: i64, sloc: i64) -> FunctionSample {
        FunctionSample { cognitive, sloc }
    }

    fn row(
        id: i64,
        file_id: i64,
        parent: Option<i64>,
        cognitive: Option<i64>,
    ) -> SymbolComplexityRow {
        SymbolComplexityRow {
            id,
            file_id,
            parent_symbol_id: parent,
            cognitive,
            start_line: 1,
            end_line: 4,
            flags: 0,
        }
    }

    #[test]
    fn mass_is_cognitive_times_sqrt_sloc() {
        assert_eq!(function_mass(s(10, 16)), 40.0);
        assert_eq!(function_mass(s(0, 100)), 0.0);
        // A degenerate span never yields a zero/NaN root.
        assert_eq!(function_mass(s(3, 0)), 3.0);
    }

    #[test]
    fn threshold_is_inclusive() {
        assert!(!is_eroded(COGNITIVE_THRESHOLD - 1));
        assert!(is_eroded(COGNITIVE_THRESHOLD));
    }

    #[test]
    fn aggregate_sums_masses() {
        let samples = [s(20, 4), s(5, 9), s(0, 1)];
        let agg = aggregate(&samples);
        assert_eq!(agg.eroded_mass, 40.0);
        assert_eq!(agg.total_mass, 55.0);
        assert_eq!(agg.eroded_count, 1);
        assert_eq!(agg.function_count, 3);
        assert_eq!(agg.eroded_share, Some(40.0 / 55.0));
        assert_eq!(agg.cognitive_max, Some(20));
    }

    #[test]
    fn empty_scope_reports_null_share_and_percentiles() {
        let agg = aggregate(&[]);
        assert_eq!(agg.total_mass, 0.0);
        assert_eq!(agg.function_count, 0);
        assert_eq!(agg.eroded_share, None);
        assert_eq!(agg.cognitive_p50, None);
        assert_eq!(agg.cognitive_p90, None);
        assert_eq!(agg.cognitive_max, None);
    }

    #[test]
    fn zero_mass_scope_has_null_share() {
        let agg = aggregate(&[s(0, 10), s(0, 3)]);
        assert_eq!(agg.function_count, 2);
        assert_eq!(agg.eroded_share, None);
        assert_eq!(agg.cognitive_p50, Some(0));
    }

    #[test]
    fn nearest_rank_percentiles() {
        let v: Vec<i64> = (1..=10).collect();
        assert_eq!(nearest_rank(&v, 50), Some(5));
        assert_eq!(nearest_rank(&v, 90), Some(9));
        assert_eq!(nearest_rank(&[7], 50), Some(7));
        assert_eq!(nearest_rank(&[7], 90), Some(7));
        assert_eq!(nearest_rank(&[1, 2, 3], 50), Some(2));
        assert_eq!(nearest_rank(&[1, 2, 3], 90), Some(3));
        assert_eq!(nearest_rank(&[], 50), None);
    }

    #[test]
    fn only_outermost_complexity_bearing_symbols_count() {
        // 1: class (no score) > 2: method (scored) > 3: nested fn (scored).
        // 4: top-level fn. The nested fn is folded into its method.
        let rows = [
            row(1, 10, None, None),
            row(2, 10, Some(1), Some(20)),
            row(3, 10, Some(2), Some(18)),
            row(4, 10, None, Some(2)),
        ];
        let langs = HashMap::from([(10, "javascript")]);
        let by_file = samples_by_file(&rows, &HashSet::new(), &langs);
        let mut cogs: Vec<i64> = by_file[&10].iter().map(|s| s.cognitive).collect();
        cogs.sort_unstable();
        assert_eq!(cogs, vec![2, 20]);
    }

    #[test]
    fn test_files_and_flags_are_excluded() {
        let mut flagged = row(2, 10, None, Some(30));
        flagged.flags = i64::from(FLAG_TEST);
        let mut cfg_test_rust = row(3, 11, None, Some(30));
        cfg_test_rust.flags = i64::from(FLAG_CFG_TEST);
        // 0x02 on TypeScript is `override`, not a test marker.
        let mut ts_override = row(4, 12, None, Some(30));
        ts_override.flags = i64::from(FLAG_CFG_TEST);
        // A child of a test-flagged container is test code too.
        let mut test_mod = row(5, 11, None, None);
        test_mod.flags = i64::from(FLAG_CFG_TEST);
        let in_test_mod = row(6, 11, Some(5), Some(30));
        let rows = [
            row(1, 13, None, Some(30)),
            flagged,
            cfg_test_rust,
            ts_override,
            test_mod,
            in_test_mod,
        ];
        let langs = HashMap::from([
            (10, "javascript"),
            (11, "rust"),
            (12, "typescript"),
            (13, "dart"),
        ]);
        let by_file = samples_by_file(&rows, &HashSet::from([13]), &langs);
        assert!(!by_file.contains_key(&13), "path-excluded test file");
        assert!(!by_file.contains_key(&10), "FLAG_TEST symbol");
        assert!(
            !by_file.contains_key(&11),
            "cfg(test) fn and test-module child"
        );
        assert_eq!(by_file[&12].len(), 1, "TS override is not test code");
    }

    #[test]
    fn scopes_sum_function_masses_and_dedupe_files() {
        let by_file = HashMap::from([(1, vec![s(20, 4)]), (2, vec![s(5, 9)])]);
        let ws = workspace_aggregate(&by_file);
        assert_eq!(ws.total_mass, 55.0);
        let comp = files_aggregate(&by_file, [1, 1, 3]);
        assert_eq!(comp.total_mass, 40.0);
        assert_eq!(comp.function_count, 1);
        let empty = files_aggregate(&by_file, [3]);
        assert_eq!(empty.eroded_share, None);
    }
}
