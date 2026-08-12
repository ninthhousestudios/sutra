use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::lessons::LessonsDb;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadArgs {
    pub workspace: String,
    #[schemars(
        description = "Symbol name to read. Pass a qualified name (e.g. \"evaluate_dd\", \
        \"GuardConfig::from_env\") or a short name (e.g. \"from_env\"). \
        Do NOT prefix with file paths or extensions — \"build_findings\" not \"review.rs::build_findings\"."
    )]
    pub symbol: String,
    #[serde(default)]
    pub context_lines: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub full: Option<bool>,
    #[serde(default)]
    #[schemars(
        description = "Include relevant import statements (default true). Set false to suppress."
    )]
    pub imports: Option<bool>,
}
use crate::error::{Result, SutraError};

const DEFAULT_LINE_CAP: usize = 500;

#[allow(clippy::too_many_arguments)]
pub fn handle(
    db: &Db,
    workspace_root: &Path,
    symbol: &str,
    context_lines: Option<usize>,
    limit: Option<usize>,
    full: bool,
    is_stale: bool,
    include_imports: bool,
    lessons_db: Option<&LessonsDb>,
) -> Result<serde_json::Value> {
    let context_lines = context_lines.unwrap_or(5);
    let line_cap = if full {
        usize::MAX
    } else {
        limit.unwrap_or(DEFAULT_LINE_CAP)
    };

    let sym = db.resolve_symbol(symbol, None)?.ok_or_else(|| {
        let next_action = if let Some(suggestion) = diagnose_symbol_input(symbol) {
            suggestion
        } else {
            let suggestions = suggest_symbols(db, symbol, 5);
            if suggestions.is_empty() {
                "Use sutra_explore to look up the symbol name first.".to_string()
            } else {
                format!(
                    "Did you mean: {}? Use sutra_explore to search more broadly.",
                    suggestions.join(", ")
                )
            }
        };
        SutraError::NotFound {
            tool: "sutra_symbol",
            kind: format!("symbol `{symbol}`"),
            next_action,
        }
    })?;

    let file = db
        .file_by_id(sym.file_id)?
        .ok_or_else(|| SutraError::NotFound {
            tool: "sutra_symbol",
            kind: format!("file for symbol `{symbol}`"),
            next_action: "The file may have been deleted. Run sutra_workspace(action=\"reparse\") to refresh.".to_string(),
        })?;

    let abs_path = workspace_root.join(&*file.path);

    if !abs_path.starts_with(workspace_root) {
        return Err(SutraError::InvalidArgument {
            tool: "sutra_symbol",
            argument: "symbol",
            constraint: "file path must stay within workspace root".to_string(),
            received: Some(file.path.to_string()),
            next_action: "This file path contains path traversal sequences. Report this issue."
                .to_string(),
        });
    }

    if !abs_path.exists() {
        return Ok(json!({
            "symbol": sym.qualified_name,
            "file": file.path,
            "start_line": sym.start_line,
            "end_line": sym.end_line,
            "warning": "file deleted since last parse",
            "is_stale": true,
            "signature": sym.signature,
            "kind": sym.kind,
        }));
    }

    // Tier-2 freshness: withhold content when index is stale
    if is_stale {
        return Ok(json!({
            "symbol": sym.qualified_name,
            "file": file.path,
            "start_line": sym.start_line,
            "end_line": sym.end_line,
            "kind": sym.kind,
            "signature": sym.signature,
            "refused": "content withheld: index is stale",
            "next_action": "Run sutra_workspace(action=\"reparse\") to refresh, then retry.",
        }));
    }

    let source = std::fs::read_to_string(&abs_path)?;
    let lines: Vec<&str> = source.lines().collect();

    let sym_start = (sym.start_line as usize).saturating_sub(1);
    let sym_end = std::cmp::min(sym.end_line as usize, lines.len());
    let sym_lines = sym_end - sym_start;

    let (start, end, truncated) = if sym_lines >= line_cap {
        (sym_start, sym_start + line_cap, true)
    } else {
        let context_budget = line_cap.saturating_sub(sym_lines);
        let pre = std::cmp::min(context_lines, context_budget / 2);
        let post = std::cmp::min(context_lines, context_budget - pre);
        let s = sym_start.saturating_sub(pre);
        let e = std::cmp::min(sym_end + post, lines.len());
        (s, e, false)
    };
    let total_lines = sym_lines + 2 * context_lines;

    let numbered: Vec<_> = lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>5} {}", start + i + 1, line))
        .collect();

    let import_lines = if include_imports {
        collect_relevant_imports(db, sym.file_id, &file.path, &lines, sym_start, sym_end)?
    } else {
        Vec::new()
    };

    let mut result = json!({
        "symbol": sym.qualified_name,
        "file": file.path,
        "start_line": start + 1,
        "end_line": end,
        "content": numbered.join("\n"),
        "kind": sym.kind,
        "signature": sym.signature,
    });
    if !import_lines.is_empty() {
        result["imports"] = json!(import_lines);
    }
    if truncated {
        result["truncated"] = json!(true);
        result["total_lines"] = json!(total_lines);
        result["hint"] = json!(format!(
            "Truncated at {} lines ({} total). Call with full=true or limit={} to see more.",
            line_cap, total_lines, total_lines
        ));
    }

    if let Some(ldb) = lessons_db {
        let project_slug = workspace_root.file_name().and_then(|n| n.to_str());
        let import_paths: Vec<String> = db
            .imports_for_file(sym.file_id)
            .unwrap_or_default()
            .into_iter()
            .map(|i| i.imported_path)
            .collect();
        let import_refs: Vec<&str> = import_paths.iter().map(String::as_str).collect();
        let ws_langs = db.distinct_languages().unwrap_or_default();
        let ctx = crate::lessons::MatchContext {
            symbol_name: &sym.qualified_name,
            file_path: Some(&file.path),
            imports: &import_refs,
            project: project_slug,
            workspace_languages: &ws_langs,
        };
        let mut cl = ldb.query_for_context(&ctx)?;
        if !cl.lessons.is_empty() {
            let resolver = super::remember::build_hash_resolver(db);
            let _ = ldb.apply_staleness(&mut cl.lessons, &resolver);
            result["lessons"] = serde_json::to_value(&cl.lessons).unwrap_or_default();
            if cl.omitted > 0 {
                result["lessons_omitted"] = json!(cl.omitted);
                result["lessons_hint"] = json!("Use sutra_lessons for the full set.");
            }
        }
    }

    Ok(result)
}

pub(crate) fn collect_relevant_imports(
    db: &Db,
    file_id: i64,
    file_path: &str,
    lines: &[&str],
    sym_start: usize,
    sym_end: usize,
) -> Result<Vec<String>> {
    let import_rows = db.imports_for_file(file_id)?;
    if import_rows.is_empty() {
        return Ok(Vec::new());
    }

    let body = lines[sym_start..sym_end].join("\n");
    let is_dart = file_path.ends_with(".dart");

    // Group import rows by source line. For each line, track whether any
    // expanded identifier from that line is referenced in the symbol body.
    let mut line_matched: BTreeMap<usize, bool> = BTreeMap::new();
    for row in &import_rows {
        let line_idx = row.line as usize;
        if *line_matched.get(&line_idx).unwrap_or(&false) {
            continue; // already matched, skip further checks for this line
        }
        let before_symbol = line_idx <= sym_start; // 1-based line vs 0-based index
        let matched = if is_dart {
            before_symbol // Dart imports are file-level; include all that precede the symbol
        } else {
            let ident = imported_identifier(&row.imported_path);
            if ident == "*" {
                before_symbol // globs only from the file header, not nested modules
            } else {
                word_appears_in(&body, ident)
            }
        };
        line_matched
            .entry(line_idx)
            .and_modify(|v| *v = *v || matched)
            .or_insert(matched);
    }

    let result: Vec<String> = line_matched
        .into_iter()
        .filter(|(_, matched)| *matched)
        .filter_map(|(line_num, _)| {
            lines
                .get(line_num.checked_sub(1)?) // 1-based → 0-based
                .map(|l| l.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .collect();

    Ok(result)
}

/// Extract the identifier brought into scope by a Rust import path.
/// `std::collections::HashMap` → `HashMap`
/// `serde_json::Value as JsonValue` → `JsonValue`
/// `std::io::*` → `*`
fn imported_identifier(imported_path: &str) -> &str {
    if let Some(alias) = imported_path.split(" as ").nth(1) {
        return alias.trim();
    }
    imported_path
        .rsplit("::")
        .next()
        .unwrap_or(imported_path)
        .trim()
}

/// Check whether `word` appears in `text` at a word boundary (not as a
/// substring of a larger identifier).
fn word_appears_in(text: &str, word: &str) -> bool {
    let word_bytes = word.as_bytes();
    let text_bytes = text.as_bytes();
    let wlen = word_bytes.len();
    if wlen == 0 || wlen > text_bytes.len() {
        return false;
    }
    let mut pos = 0;
    while let Some(offset) = text[pos..].find(word) {
        let start = pos + offset;
        let end = start + wlen;
        let before_ok = start == 0 || !is_ident_char(text_bytes[start - 1]);
        let after_ok = end == text_bytes.len() || !is_ident_char(text_bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        pos = start + 1;
    }
    false
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

pub fn suggest_symbols(db: &Db, query: &str, limit: usize) -> Vec<String> {
    let query_short = query.rsplit("::").next().unwrap_or(query);
    let query_chars = query_short.chars().count();
    if !(2..=128).contains(&query_chars) {
        return vec![];
    }
    let query_qualifier = query.rsplit_once("::").map(|(prefix, _)| prefix);
    let query_tokens: Vec<&str> = query_short.split('_').collect();

    let symbols = match db.all_symbols_summary() {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    let mut scored: Vec<(f64, &str)> = symbols
        .iter()
        .filter_map(|s| {
            let sn = &s.short_name;
            let qn = &s.qualified_name;
            let sn_chars = sn.chars().count();
            if sn_chars < 2 {
                return None;
            }

            let signal_edit = {
                let ratio =
                    query_chars.min(sn_chars) as f64 / query_chars.max(sn_chars).max(1) as f64;
                if ratio < 0.3 {
                    0.0
                } else {
                    let dist = levenshtein(query_short, sn);
                    let max_len = query_chars.max(sn_chars);
                    1.0 - (dist as f64 / max_len as f64)
                }
            };

            let signal_components = {
                let sym_tokens: Vec<&str> = sn.split('_').collect();
                let shared = query_tokens
                    .iter()
                    .filter(|t| sym_tokens.contains(t))
                    .count();
                let max_tokens = query_tokens.len().max(sym_tokens.len());
                if max_tokens == 0 {
                    0.0
                } else {
                    shared as f64 / max_tokens as f64
                }
            };

            let signal_substring = if sn.contains(query_short) || query_short.contains(sn) {
                let shorter = query_chars.min(sn_chars);
                let longer = query_chars.max(sn_chars);
                if longer > 0 && shorter as f64 / longer as f64 >= 0.5 {
                    0.8
                } else {
                    0.0
                }
            } else {
                0.0
            };

            let mut score = signal_edit.max(signal_components).max(signal_substring);

            if let Some(q) = query_qualifier
                && qn.starts_with(q)
                && qn[q.len()..].starts_with("::")
            {
                score = (score * 1.15).min(1.0);
            }

            if score >= 0.35 && qn.as_str() != query {
                Some((score, qn.as_str()))
            } else {
                None
            }
        })
        .collect();

    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.len().cmp(&b.1.len()))
            .then_with(|| a.1.cmp(b.1))
    });

    let mut seen = HashSet::new();
    scored
        .into_iter()
        .filter(|(_, qn)| seen.insert(*qn))
        .take(limit)
        .map(|(_, qn)| format!("`{qn}`"))
        .collect()
}

fn diagnose_symbol_input(symbol: &str) -> Option<String> {
    // "review.rs::build_findings" or "check.rs::evaluate"
    if let Some(pos) = symbol.find(".rs::") {
        let after = &symbol[pos + 5..];
        return Some(format!(
            "Don't prefix with the file name — pass the symbol name directly: \
             try `{after}` instead of `{symbol}`."
        ));
    }
    // "src/tools/review.rs::build_findings" (path-qualified symbol) or
    // "vidya-core/src/sparql.rs" (bare file path with no symbol part)
    if symbol.contains('/') {
        let bare = symbol.rsplit("::").next().unwrap_or(symbol);
        // No "::" to strip, or the tail is still a path → it's a file, not a
        // symbol. Suggesting `bare` here would echo the input back verbatim.
        if bare == symbol || bare.contains('/') {
            return Some(format!(
                "`{symbol}` is a file path, not a symbol name. \
                 Use sutra_outline(path=\"{symbol}\") to list its symbols, \
                 then sutra_symbol a specific one."
            ));
        }
        return Some(format!(
            "Don't use file paths — pass the symbol name directly: \
             try `{bare}` instead of `{symbol}`."
        ));
    }
    // "review.rs" (just a filename, no symbol)
    if symbol.ends_with(".rs")
        || symbol.ends_with(".ts")
        || symbol.ends_with(".py")
        || symbol.ends_with(".dart")
    {
        return Some(format!(
            "`{symbol}` looks like a file path, not a symbol name. \
             Pass a function or type name (e.g. `build_findings`). \
             Use sutra_outline to list symbols in a file."
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnose_file_dot_rs_prefix() {
        let msg = diagnose_symbol_input("review.rs::build_findings").unwrap();
        assert!(msg.contains("try `build_findings`"));
    }

    #[test]
    fn diagnose_path_prefix() {
        let msg = diagnose_symbol_input("src/tools/review.rs::build_findings").unwrap();
        assert!(msg.contains("try `build_findings`"));
    }

    #[test]
    fn diagnose_bare_filename() {
        let msg = diagnose_symbol_input("review.rs").unwrap();
        assert!(msg.contains("looks like a file path"));
    }

    #[test]
    fn diagnose_bare_path_routes_to_outline() {
        let msg = diagnose_symbol_input("vidya-core/src/sparql.rs").unwrap();
        assert!(msg.contains("sutra_outline"));
        // Must not echo the input back as a "suggestion".
        assert!(!msg.contains("try `vidya-core/src/sparql.rs`"));
    }

    #[test]
    fn no_diagnosis_for_valid_symbol() {
        assert!(diagnose_symbol_input("build_findings").is_none());
        assert!(diagnose_symbol_input("GuardConfig::from_env").is_none());
    }

    #[test]
    fn imported_identifier_last_segment() {
        assert_eq!(imported_identifier("std::collections::HashMap"), "HashMap");
        assert_eq!(imported_identifier("crate::db::Db"), "Db");
    }

    #[test]
    fn imported_identifier_with_alias() {
        assert_eq!(
            imported_identifier("serde_json::Value as JsonValue"),
            "JsonValue"
        );
    }

    #[test]
    fn imported_identifier_glob() {
        assert_eq!(imported_identifier("std::io::*"), "*");
    }

    #[test]
    fn imported_identifier_single_segment() {
        assert_eq!(imported_identifier("serde"), "serde");
    }

    #[test]
    fn word_boundary_match_basic() {
        assert!(word_appears_in(
            "let m: HashMap<K, V> = HashMap::new();",
            "HashMap"
        ));
        assert!(word_appears_in("HashMap", "HashMap"));
    }

    #[test]
    fn word_boundary_rejects_substring() {
        assert!(!word_appears_in("let c = DbConfig::new();", "Db"));
        assert!(!word_appears_in("use HashMaps;", "HashMap"));
    }

    #[test]
    fn word_boundary_at_edges() {
        assert!(word_appears_in("HashMap::new()", "HashMap"));
        assert!(word_appears_in("foo(HashMap)", "HashMap"));
        assert!(word_appears_in("x.HashMap", "HashMap"));
    }

    #[test]
    fn word_boundary_empty() {
        assert!(!word_appears_in("anything", ""));
        assert!(!word_appears_in("", "HashMap"));
    }

    #[test]
    fn levenshtein_empty_strings() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
    }

    #[test]
    fn levenshtein_identical() {
        assert_eq!(levenshtein("find_symbols", "find_symbols"), 0);
    }

    #[test]
    fn levenshtein_single_edit() {
        assert_eq!(levenshtein("find_symbls", "find_symbols"), 1);
        assert_eq!(levenshtein("kitten", "sitten"), 1);
    }

    #[test]
    fn levenshtein_multiple_edits() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }
}
