use std::collections::{HashSet, VecDeque};
use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::{Db, ResolveResult, SymbolRow};
use crate::diagnostics::{CandidateInfo, Diagnostic};
use crate::error::Result;
use crate::graph::EdgeKind;
use crate::lessons::LessonsDb;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContextArgs {
    pub workspace: String,
    /// Symbol name to build context for (qualified or short name).
    pub symbol: String,
    /// Token budget for the packed context (default 8000).
    #[serde(default)]
    pub token_budget: Option<usize>,
    /// Max BFS depth for transitive neighbors (default 2).
    #[serde(default)]
    pub depth: Option<usize>,
}

const DEFAULT_BUDGET: usize = 8000;
const DEFAULT_DEPTH: usize = 2;
const MAX_DEPTH: usize = 4;
const MAX_TRANSITIVE_PER_ROLE: usize = 25;
const MAX_WALK_NODES: usize = 500;

pub fn estimate_tokens(content: &str) -> usize {
    let words = content.split_whitespace().count() * 13 / 10;
    let chars = content.chars().count() / 4;
    words.max(chars).max(1)
}

fn signature_line(content: &str) -> &str {
    content.lines().next().unwrap_or("")
}

fn truncate_head(content: &str, cap_tokens: usize) -> Option<(String, usize)> {
    let total_lines = content.lines().count();
    let mut kept = String::new();
    let mut kept_lines = 0usize;
    for line in content.lines() {
        let candidate = if kept.is_empty() {
            line.to_string()
        } else {
            format!("{kept}\n{line}")
        };
        let marker = format!(
            "\n\u{2026} truncated: {} more lines",
            total_lines.saturating_sub(kept_lines + 1)
        );
        if estimate_tokens(&format!("{candidate}{marker}")) > cap_tokens {
            break;
        }
        kept = candidate;
        kept_lines += 1;
    }
    if kept.is_empty() || kept_lines == total_lines {
        return None;
    }
    let out = format!(
        "{kept}\n\u{2026} truncated: {} more lines",
        total_lines - kept_lines
    );
    let tokens = estimate_tokens(&out);
    Some((out, tokens))
}

fn is_test_symbol(sym: &SymbolRow, file_path: &str) -> bool {
    const TEST_FLAGS: i64 = 0x03; // FLAG_TEST | FLAG_CFG_TEST
    sym.flags & TEST_FLAGS != 0
        || sym.qualified_name.starts_with("test_")
        || sym.qualified_name.starts_with("tests::")
        || *sym.qualified_name == *"tests"
        || file_path.contains("/tests/")
        || file_path.starts_with("tests/")
        || file_path.ends_with("_test.dart")
}

struct Neighbor {
    sym: SymbolRow,
    file_path: String,
    edge_kind: EdgeKind,
    depth: usize,
}

fn walk_dependencies(db: &Db, root: &SymbolRow, max_depth: usize) -> Result<Vec<Neighbor>> {
    let mut visited = HashSet::new();
    visited.insert(root.id);
    let mut queue: VecDeque<(i64, i64, i64, i64, usize)> = VecDeque::new();
    queue.push_back((root.id, root.file_id, root.start_line, root.end_line, 0));
    let mut result = Vec::new();

    while let Some((_sid, file_id, start, end, d)) = queue.pop_front() {
        if d >= max_depth || result.len() >= MAX_WALK_NODES {
            continue;
        }
        let refs = db.find_refs_in_file(file_id)?;
        for r in refs.iter().filter(|r| r.line >= start && r.line <= end) {
            if result.len() >= MAX_WALK_NODES {
                break;
            }
            if let Some(target_id) = r.target_symbol_id {
                if !visited.insert(target_id) {
                    continue;
                }
                if let Some(target_sym) = db.symbol_by_id(target_id)? {
                    let file_path = db
                        .file_by_id(target_sym.file_id)
                        .ok()
                        .flatten()
                        .map(|f| f.path.to_string())
                        .unwrap_or_default();
                    queue.push_back((
                        target_sym.id,
                        target_sym.file_id,
                        target_sym.start_line,
                        target_sym.end_line,
                        d + 1,
                    ));
                    result.push(Neighbor {
                        sym: target_sym,
                        file_path,
                        edge_kind: EdgeKind::from_context_kind(&r.context_kind),
                        depth: d + 1,
                    });
                }
            }
        }
    }
    Ok(result)
}

fn walk_dependents(db: &Db, root_id: i64, max_depth: usize) -> Result<Vec<Neighbor>> {
    let mut visited = HashSet::new();
    visited.insert(root_id);
    let mut queue: VecDeque<(i64, usize)> = VecDeque::new();
    queue.push_back((root_id, 0));
    let mut result = Vec::new();

    while let Some((sid, d)) = queue.pop_front() {
        if d >= max_depth || result.len() >= MAX_WALK_NODES {
            continue;
        }
        let refs = db.find_refs_to_symbol(sid)?;
        for r in &refs {
            if result.len() >= MAX_WALK_NODES {
                break;
            }
            if let Some(caller_sym) = db.find_enclosing_symbol(r.file_id, r.line)? {
                if !visited.insert(caller_sym.id) {
                    continue;
                }
                let file_path = db
                    .file_by_id(r.file_id)
                    .ok()
                    .flatten()
                    .map(|f| f.path.to_string())
                    .unwrap_or_default();
                queue.push_back((caller_sym.id, d + 1));
                result.push(Neighbor {
                    sym: caller_sym,
                    file_path,
                    edge_kind: EdgeKind::from_context_kind(&r.context_kind),
                    depth: d + 1,
                });
            }
        }
    }
    Ok(result)
}

fn sort_neighbors(neighbors: &mut [Neighbor]) {
    neighbors.sort_by(|a, b| {
        let aw = a.edge_kind.clustering_weight();
        let bw = b.edge_kind.clustering_weight();
        bw.partial_cmp(&aw)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let ap = a.sym.pagerank.unwrap_or(0.0);
                let bp = b.sym.pagerank.unwrap_or(0.0);
                bp.partial_cmp(&ap).unwrap_or(std::cmp::Ordering::Equal)
            })
    });
}

fn read_body(workspace_root: &Path, db: &Db, sym: &SymbolRow) -> Option<String> {
    let file = db.file_by_id(sym.file_id).ok()??;
    let abs_path = workspace_root.join(&*file.path);
    let source = std::fs::read_to_string(&abs_path).ok()?;
    let lines: Vec<&str> = source.lines().collect();
    let start = (sym.start_line as usize).saturating_sub(1);
    let end = std::cmp::min(sym.end_line as usize, lines.len());
    if start >= end {
        return None;
    }
    Some(lines[start..end].join("\n"))
}

struct OmittedTail {
    role: String,
    entities: usize,
    tests: usize,
}

fn tally(tails: &mut Vec<OmittedTail>, role: &str, is_test: bool) {
    if let Some(tail) = tails.iter_mut().find(|t| t.role == role) {
        tail.entities += 1;
        if is_test {
            tail.tests += 1;
        }
    } else {
        tails.push(OmittedTail {
            role: role.to_string(),
            entities: 1,
            tests: usize::from(is_test),
        });
    }
}

#[allow(clippy::too_many_lines)]
pub fn handle(
    db: &Db,
    workspace_root: &Path,
    symbol: &str,
    token_budget: Option<usize>,
    depth: Option<usize>,
    is_stale: bool,
    lessons_db: Option<&LessonsDb>,
) -> Result<serde_json::Value> {
    let budget = token_budget.unwrap_or(DEFAULT_BUDGET);
    let max_depth = depth.unwrap_or(DEFAULT_DEPTH).min(MAX_DEPTH);

    let sym = match db.resolve_symbol_diagnostic(symbol, None)? {
        ResolveResult::Unique(s) => s,
        ResolveResult::NotFound => {
            return Ok(json!({
                "symbol": symbol,
                "diagnostic": serde_json::to_value(Diagnostic::NoSuchSymbol {
                    queried_name: symbol.to_string(),
                    queried_kind: None,
                    indexed_kinds: db.distinct_symbol_kinds().unwrap_or_default(),
                    freshness: None,
                    suggestion: "Use sutra_explore to search by partial name, \
                                 or sutra_grep for a text search.".to_string(),
                }).unwrap(),
            }));
        }
        ResolveResult::Ambiguous(candidates) => {
            let infos: Vec<CandidateInfo> = candidates
                .iter()
                .map(|c| CandidateInfo {
                    qualified_name: c.qualified_name.to_string(),
                    kind: c.kind.to_string(),
                    file: db
                        .file_by_id(c.file_id)
                        .ok()
                        .flatten()
                        .map(|f| f.path.to_string())
                        .unwrap_or_default(),
                })
                .collect();
            return Ok(json!({
                "symbol": symbol,
                "diagnostic": serde_json::to_value(Diagnostic::Ambiguous {
                    queried_name: symbol.to_string(),
                    candidates: infos,
                    freshness: None,
                    suggestion: "Use the fully qualified name to disambiguate.".to_string(),
                }).unwrap(),
            }));
        }
    };

    let file = db
        .file_by_id(sym.file_id)?
        .ok_or_else(|| crate::error::SutraError::NotFound {
            tool: "sutra_context",
            kind: format!("file for symbol `{symbol}`"),
            next_action: "Run sutra_workspace(action=\"reparse\") to refresh.".to_string(),
        })?;
    let file_path = file.path.to_string();

    if is_stale {
        return Ok(json!({
            "symbol": sym.qualified_name,
            "file": file_path,
            "token_budget": budget,
            "refused": "content withheld: index is stale",
            "next_action": "Run sutra_workspace(action=\"reparse\") to refresh, then retry.",
        }));
    }

    let mut context_entries: Vec<serde_json::Value> = Vec::new();
    let mut tokens_used: usize = 0;
    let mut truncated = false;
    let mut included_ids: HashSet<i64> = HashSet::new();
    let mut omitted: Vec<OmittedTail> = Vec::new();

    // ---------------------------------------------------------------
    // 1. Target entity — full body → head-truncated (70% cap) → sig → omitted
    //    Imports are collected separately and prepended AFTER packing so
    //    the signature fallback is always the actual symbol signature.
    // ---------------------------------------------------------------
    let abs_path = workspace_root.join(&*file.path);
    let (target_body, target_imports) = if abs_path.exists() {
        std::fs::read_to_string(&abs_path)
            .ok()
            .and_then(|source| {
                let lines: Vec<&str> = source.lines().collect();
                let start = (sym.start_line as usize).saturating_sub(1);
                let end = std::cmp::min(sym.end_line as usize, lines.len());
                if start >= end {
                    return None;
                }
                let body = lines[start..end].join("\n");
                let imports = super::read::collect_relevant_imports(
                    db,
                    sym.file_id,
                    &file.path,
                    &lines,
                    start,
                    end,
                )
                .unwrap_or_default();
                Some((body, imports))
            })
            .map_or((None, Vec::new()), |(b, i)| (Some(b), i))
    } else {
        (None, Vec::new())
    };

    let target_packed = pack_target(&target_body, &sym, budget);
    match target_packed {
        Some((mut content, mut tokens, trunc)) => {
            truncated |= trunc;
            // Prepend imports if they fit within the remaining budget
            if !target_imports.is_empty() {
                let import_block = target_imports.join("\n");
                let with_imports = format!("{import_block}\n\n{content}");
                let with_tokens = estimate_tokens(&with_imports);
                if with_tokens <= budget {
                    content = with_imports;
                    tokens = with_tokens;
                }
            }
            context_entries.push(json!({
                "symbol": sym.qualified_name,
                "kind": sym.kind,
                "file": &file_path,
                "role": "target",
                "tokens": tokens,
                "content": content,
            }));
            tokens_used += tokens;
            included_ids.insert(sym.id);
        }
        None => {
            return Ok(json!({
                "symbol": sym.qualified_name,
                "file": &file_path,
                "token_budget": budget,
                "tokens_used": 0,
                "truncated": true,
                "target_omitted": true,
                "context": [],
                "omitted": [],
            }));
        }
    }

    // No single neighbor may cost more than the target did (floor: budget/10).
    let neighbor_full_cap = tokens_used.max(budget / 10);
    let target_is_test = is_test_symbol(&sym, &file_path);

    // ---------------------------------------------------------------
    // 2. Walk neighbors
    // ---------------------------------------------------------------
    let mut dependencies = walk_dependencies(db, &sym, max_depth)?;
    let mut dependents = walk_dependents(db, sym.id, max_depth)?;
    sort_neighbors(&mut dependencies);
    sort_neighbors(&mut dependents);

    let (direct_deps, transitive_deps): (Vec<_>, Vec<_>) =
        dependencies.into_iter().partition(|n| n.depth == 1);
    let (direct_depts, transitive_depts): (Vec<_>, Vec<_>) =
        dependents.into_iter().partition(|n| n.depth == 1);

    // ---------------------------------------------------------------
    // 3. Direct dependencies — full body or signature
    // ---------------------------------------------------------------
    for n in &direct_deps {
        pack_direct_neighbor(
            db,
            workspace_root,
            n,
            "direct_dependency",
            target_is_test,
            budget,
            neighbor_full_cap,
            &mut tokens_used,
            &mut truncated,
            &mut context_entries,
            &mut included_ids,
            &mut omitted,
        );
    }

    // ---------------------------------------------------------------
    // 4. Direct dependents — full body or signature
    // ---------------------------------------------------------------
    for n in &direct_depts {
        pack_direct_neighbor(
            db,
            workspace_root,
            n,
            "direct_dependent",
            target_is_test,
            budget,
            neighbor_full_cap,
            &mut tokens_used,
            &mut truncated,
            &mut context_entries,
            &mut included_ids,
            &mut omitted,
        );
    }

    // ---------------------------------------------------------------
    // 5. Transitive dependencies — signature only, cap 25
    // ---------------------------------------------------------------
    pack_transitive_tier(
        &transitive_deps,
        "transitive_dependency",
        target_is_test,
        budget,
        &mut tokens_used,
        &mut truncated,
        &mut context_entries,
        &mut included_ids,
        &mut omitted,
    );

    // ---------------------------------------------------------------
    // 6. Transitive dependents — signature only, cap 25
    // ---------------------------------------------------------------
    pack_transitive_tier(
        &transitive_depts,
        "transitive_dependent",
        target_is_test,
        budget,
        &mut tokens_used,
        &mut truncated,
        &mut context_entries,
        &mut included_ids,
        &mut omitted,
    );

    // ---------------------------------------------------------------
    // 7. Assemble response
    // ---------------------------------------------------------------
    let mut result = json!({
        "symbol": sym.qualified_name,
        "file": file_path,
        "token_budget": budget,
        "tokens_used": tokens_used,
        "truncated": truncated,
        "target_omitted": false,
        "context": context_entries,
    });

    if !omitted.is_empty() {
        result["omitted"] = json!(
            omitted
                .iter()
                .map(|o| json!({
                    "role": o.role,
                    "entities": o.entities,
                    "tests": o.tests,
                }))
                .collect::<Vec<_>>()
        );
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
            file_path: Some(&file_path),
            imports: &import_refs,
            project: project_slug,
            workspace_languages: &ws_langs,
        };
        if let Ok(mut cl) = ldb.query_for_context(&ctx)
            && !cl.lessons.is_empty()
        {
            let resolver = super::remember::build_hash_resolver(db);
            let _ = ldb.apply_staleness(&mut cl.lessons, &resolver);
            result["lessons"] = serde_json::to_value(&cl.lessons).unwrap_or_default();
        }
    }

    Ok(result)
}

/// Pack the target symbol's content within the budget.
/// Returns (content, tokens, was_truncated) or None if budget too small.
fn pack_target(
    body: &Option<String>,
    sym: &SymbolRow,
    budget: usize,
) -> Option<(String, usize, bool)> {
    if let Some(body) = body {
        let full_tokens = estimate_tokens(body);
        if full_tokens <= budget {
            return Some((body.into(), full_tokens, false));
        }
        let sig = signature_line(body);
        let sig_tokens = estimate_tokens(sig);
        let head_cap = (budget * 7 / 10).max(sig_tokens);
        if let Some((head, head_tokens)) = truncate_head(body, head_cap) {
            return Some((head, head_tokens, true));
        }
        if sig_tokens <= budget {
            return Some((sig.to_string(), sig_tokens, true));
        }
        return None;
    }
    // No body readable — fall back to DB signature
    sym.signature.as_deref().and_then(|sig| {
        let t = estimate_tokens(sig);
        (t <= budget).then(|| (sig.to_string(), t, true))
    })
}

#[allow(clippy::too_many_arguments)]
fn pack_direct_neighbor(
    db: &Db,
    workspace_root: &Path,
    n: &Neighbor,
    role: &str,
    target_is_test: bool,
    budget: usize,
    full_cap: usize,
    tokens_used: &mut usize,
    truncated: &mut bool,
    entries: &mut Vec<serde_json::Value>,
    included_ids: &mut HashSet<i64>,
    omitted: &mut Vec<OmittedTail>,
) {
    if included_ids.contains(&n.sym.id) {
        return;
    }
    if !target_is_test && is_test_symbol(&n.sym, &n.file_path) {
        tally(omitted, role, true);
        return;
    }

    if let Some(body) = read_body(workspace_root, db, &n.sym) {
        let full_tokens = estimate_tokens(&body);
        if full_tokens <= full_cap && *tokens_used + full_tokens <= budget {
            entries.push(json!({
                "symbol": n.sym.qualified_name,
                "kind": n.sym.kind,
                "file": &n.file_path,
                "role": role,
                "tokens": full_tokens,
                "content": body,
            }));
            *tokens_used += full_tokens;
            included_ids.insert(n.sym.id);
            return;
        }
        *truncated = true;
        let sig = signature_line(&body);
        let sig_tokens = estimate_tokens(sig);
        if *tokens_used + sig_tokens <= budget {
            entries.push(json!({
                "symbol": n.sym.qualified_name,
                "kind": n.sym.kind,
                "file": &n.file_path,
                "role": role,
                "tokens": sig_tokens,
                "content": sig,
            }));
            *tokens_used += sig_tokens;
            included_ids.insert(n.sym.id);
            return;
        }
    }

    // Fallback: DB signature
    if let Some(sig) = &n.sym.signature {
        let sig_tokens = estimate_tokens(sig);
        if *tokens_used + sig_tokens <= budget {
            entries.push(json!({
                "symbol": n.sym.qualified_name,
                "kind": n.sym.kind,
                "file": &n.file_path,
                "role": role,
                "tokens": sig_tokens,
                "content": sig,
            }));
            *tokens_used += sig_tokens;
            included_ids.insert(n.sym.id);
        } else {
            *truncated = true;
            tally(omitted, role, false);
        }
    } else {
        *truncated = true;
        tally(omitted, role, false);
    }
}

#[allow(clippy::too_many_arguments)]
fn pack_transitive_tier(
    neighbors: &[Neighbor],
    role: &str,
    target_is_test: bool,
    budget: usize,
    tokens_used: &mut usize,
    truncated: &mut bool,
    entries: &mut Vec<serde_json::Value>,
    included_ids: &mut HashSet<i64>,
    omitted: &mut Vec<OmittedTail>,
) {
    let mut packed = 0usize;
    for n in neighbors {
        if included_ids.contains(&n.sym.id) {
            continue;
        }
        if !target_is_test && is_test_symbol(&n.sym, &n.file_path) {
            tally(omitted, role, true);
            continue;
        }
        if packed >= MAX_TRANSITIVE_PER_ROLE {
            tally(omitted, role, false);
            continue;
        }
        let sig = match &n.sym.signature {
            Some(s) => s,
            None => {
                tally(omitted, role, false);
                continue;
            }
        };
        let sig_tokens = estimate_tokens(sig);
        if *tokens_used + sig_tokens > budget {
            *truncated = true;
            tally(omitted, role, false);
            continue;
        }
        entries.push(json!({
            "symbol": n.sym.qualified_name,
            "kind": n.sym.kind,
            "file": &n.file_path,
            "role": role,
            "tokens": sig_tokens,
            "content": sig,
        }));
        *tokens_used += sig_tokens;
        included_ids.insert(n.sym.id);
        packed += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_word_dominant() {
        assert_eq!(estimate_tokens("hello world foo bar baz"), 6);
    }

    #[test]
    fn estimate_tokens_char_dominant() {
        assert_eq!(
            estimate_tokens("fn foo(a_long_param: SomeLongTypeName) -> AnotherLongType"),
            14
        );
    }

    #[test]
    fn estimate_tokens_minimum_one() {
        assert_eq!(estimate_tokens(""), 1);
    }

    #[test]
    fn truncate_head_returns_none_if_fits() {
        assert!(truncate_head("short line", 100).is_none());
    }

    #[test]
    fn truncate_head_returns_none_if_first_line_too_big() {
        assert!(truncate_head(&"x".repeat(500), 2).is_none());
    }

    #[test]
    fn truncate_head_produces_marker() {
        let body = (0..100)
            .map(|i| format!("    line_{i} = compute_{i}()"))
            .collect::<Vec<_>>()
            .join("\n");
        let content = format!("fn big():\n{body}");
        let (truncated, tokens) = truncate_head(&content, 50).expect("should truncate");
        assert!(truncated.contains("\u{2026} truncated:"));
        assert!(tokens <= 50);
        assert!(truncated.lines().count() < content.lines().count());
    }

    fn make_sym(qualified_name: &str) -> SymbolRow {
        SymbolRow {
            id: 1,
            file_id: 1,
            qualified_name: qualified_name.into(),
            short_name: qualified_name.into(),
            kind: "function".into(),
            signature: None,
            signature_hash: None,
            structural_hash: None,
            visibility: None,
            start_line: 1,
            start_col: 0,
            end_line: 10,
            end_col: 0,
            parent_symbol_id: None,
            docstring: None,
            pagerank: None,
            cyclomatic: None,
            cognitive: None,
            max_nesting: None,
            flags: 0,
            language_attrs: None,
        }
    }

    #[test]
    fn test_is_test_symbol() {
        let sym = make_sym("test_something");
        assert!(is_test_symbol(&sym, "src/lib.rs"));

        let non_test = make_sym("handle");
        assert!(!is_test_symbol(&non_test, "src/lib.rs"));
        assert!(is_test_symbol(&non_test, "tests/integration.rs"));

        // Parser-flagged test (FLAG_TEST = 0x01)
        let mut flagged = make_sym("verify_output");
        flagged.flags = 0x01;
        assert!(is_test_symbol(&flagged, "src/lib.rs"));

        // Parser-flagged cfg(test) module (FLAG_CFG_TEST = 0x02)
        let mut cfg_test = make_sym("helper_in_test_mod");
        cfg_test.flags = 0x02;
        assert!(is_test_symbol(&cfg_test, "src/lib.rs"));
    }

    #[test]
    fn pack_target_full_body_fits() {
        let body = Some("fn foo() {\n    42\n}".to_string());
        let sym = make_sym("foo");
        let (content, tokens, trunc) = pack_target(&body, &sym, 1000).unwrap();
        assert_eq!(content, "fn foo() {\n    42\n}");
        assert!(!trunc);
        assert!(tokens > 0);
    }

    #[test]
    fn pack_target_head_truncated() {
        let body_lines: Vec<String> = (0..200)
            .map(|i| format!("    line_{i} = compute_{i}()"))
            .collect();
        let body = Some(format!("fn big():\n{}", body_lines.join("\n")));
        let sym = make_sym("big");
        let (content, tokens, trunc) = pack_target(&body, &sym, 100).unwrap();
        assert!(trunc);
        assert!(tokens <= 100);
        assert!(content.contains("\u{2026} truncated:"));
    }

    #[test]
    fn pack_target_omitted_when_too_small() {
        let body = Some("x".repeat(10000));
        let sym = make_sym("huge");
        assert!(pack_target(&body, &sym, 2).is_none());
    }

    #[test]
    fn pack_target_falls_back_to_db_signature() {
        let mut sym = make_sym("no_body");
        sym.signature = Some("fn no_body(x: i32) -> bool".to_string());
        let (content, _, trunc) = pack_target(&None, &sym, 1000).unwrap();
        assert!(trunc);
        assert_eq!(content, "fn no_body(x: i32) -> bool");
    }

    #[test]
    fn pack_target_signature_fallback_is_body_not_import() {
        // When imports are separated, the fallback sig should be the
        // function signature, not a use-statement. pack_target receives
        // only the body (imports are handled by the caller).
        let body = Some("fn handle() {\n    long_body()\n}".to_string());
        let sym = make_sym("handle");
        let (content, _, trunc) = pack_target(&body, &sym, 5).unwrap();
        assert!(trunc);
        assert!(
            content.starts_with("fn handle()"),
            "fallback should be the function sig, got: {content}"
        );
    }
}
