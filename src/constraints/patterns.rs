use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::constraints::{ConstraintFinding, FindingDelta};
use crate::parser::adapter::{LanguageRegistry, ParseContext, line_in_ranges, node_text};
use crate::parser::{ExtractedSymbol, flatten_symbols};
use crate::rules::{Constraint, ConstraintKind, scope_matches_path};

/// Content fingerprint of a forbidden-pattern match, stable across line moves
/// and re-indentation: `(constraint_id, enclosing_symbol, snippet)`. `snippet`
/// is the matched node's first line verbatim (node-relative, so leading
/// indentation is excluded). The identity of a report-path instance ack
/// (sutra/305). The guard does not use it: see [`introduced_in_file`].
pub type MatchKey = (Arc<str>, Option<String>, Option<String>);

/// The [`MatchKey`] of a single finding. The clones build an owned key from a
/// borrowed finding: the `Arc<str>` clone is a refcount bump, and the key is
/// looked up in a map whose keys are owned (read from stored acks), so a
/// borrowed key is not an option.
pub fn match_key(f: &ConstraintFinding) -> MatchKey {
    (
        Arc::clone(&f.constraint_id),
        f.enclosing_symbol.clone(),
        f.snippet.clone(),
    )
}

/// Cancel `findings` against a `prior` multiset of match keys, preserving order:
/// each finding whose key still has budget in `prior` is dropped, and the budget
/// for that key is spent. What remains is the surplus — the matches `prior` does
/// not account for. The report path uses it to subtract accepted instance-ack
/// counts. `prior` is taken by value and spent in place.
pub fn subtract_multiset(
    findings: Vec<ConstraintFinding>,
    mut prior: HashMap<MatchKey, usize>,
) -> Vec<ConstraintFinding> {
    let surplus = surplus_mask(&findings, &mut prior, match_key);
    keep_masked(findings, surplus)
}

/// The guard's introduced-only diff for one file: the `proposed` matches that
/// the `disk` matches do not account for. Two passes, each disk match
/// cancelling at most one proposed match:
///
/// 1. By `(constraint_id, enclosing_symbol, snippet)`. A grandfathered match
///    moved into another existing function is still new — deliberately, since
///    moving a guarded construct around is what the rule wants surfaced.
/// 2. What is left cancels by `(constraint_id, snippet)` between a disk match
///    whose enclosing symbol vanished and a proposed match whose enclosing
///    symbol appeared — "vanished"/"appeared" counted per qualified name, so
///    renaming one of two same-named symbols is seen. A pure rename changes the
///    symbol of every match inside the function, and without this pass would
///    "introduce" all of them (observed on f9e19e6), forcing justifications onto
///    code the edit never touched. Requiring the destination to be new keeps
///    "delete A, move its match into existing B" a move, not a rename.
///
/// `justified` are the proposed matches a `justify` comment waives. They are
/// never returned, but spend disk budget before `proposed` does in both passes:
/// otherwise justifying the grandfathered match and adding an identical
/// unjustified one would let the new match inherit the old one's budget.
///
/// Symbols are only extracted when pass 1 leaves both sides with a remainder.
pub fn introduced_in_file(
    proposed: Vec<ConstraintFinding>,
    justified: &[&ConstraintFinding],
    disk: &[ConstraintFinding],
    path: &str,
    disk_source: &str,
    proposed_source: &str,
    registry: &LanguageRegistry,
) -> Vec<ConstraintFinding> {
    let mut exact: HashMap<(&str, Option<&str>, Option<&str>), usize> = HashMap::new();
    for f in disk {
        *exact.entry(symbol_match_key(f)).or_default() += 1;
    }
    let justified_left: Vec<&ConstraintFinding> = justified
        .iter()
        .copied()
        .filter(|f| !spend(&mut exact, &symbol_match_key(f)))
        .collect();
    let mut surplus = surplus_mask(&proposed, &mut exact, symbol_match_key);

    let disk_left = exact.values().any(|&n| n > 0);
    if disk_left
        && surplus.contains(&true)
        && let Some(adapter) = registry.adapter_for_pattern_path(path)
    {
        let disk_symbols = extract_symbols_for_enclosing(adapter, disk_source, path);
        let proposed_symbols = extract_symbols_for_enclosing(adapter, proposed_source, path);
        let disk_names = name_counts(&disk_symbols);
        let proposed_names = name_counts(&proposed_symbols);
        let count = |names: &HashMap<&str, usize>, sym: &str| names.get(sym).copied().unwrap_or(0);
        let vanished = |sym: &str| count(&disk_names, sym) > count(&proposed_names, sym);
        let appeared = |f: &ConstraintFinding| {
            f.enclosing_symbol
                .as_deref()
                .is_some_and(|sym| count(&proposed_names, sym) > count(&disk_names, sym))
        };

        let mut orphaned: HashMap<(&str, Option<&str>), usize> = HashMap::new();
        for ((constraint, symbol, snippet), n) in exact {
            if n > 0 && symbol.is_some_and(vanished) {
                *orphaned.entry((constraint, snippet)).or_default() += n;
            }
        }
        for f in justified_left {
            if appeared(f) {
                spend(&mut orphaned, &(&*f.constraint_id, f.snippet.as_deref()));
            }
        }
        for (f, keep) in proposed.iter().zip(surplus.iter_mut()) {
            if *keep
                && appeared(f)
                && spend(&mut orphaned, &(&*f.constraint_id, f.snippet.as_deref()))
            {
                *keep = false;
            }
        }
    }
    keep_masked(proposed, surplus)
}

/// Occurrences of each qualified name among `symbols`.
fn name_counts(symbols: &[(String, usize, usize)]) -> HashMap<&str, usize> {
    let mut counts = HashMap::new();
    for (name, _, _) in symbols {
        *counts.entry(name.as_str()).or_default() += 1;
    }
    counts
}

/// `(constraint_id, enclosing_symbol, snippet)` borrowed from the finding: the
/// [`MatchKey`] content, for diffs that never outlive their findings.
fn symbol_match_key(f: &ConstraintFinding) -> (&str, Option<&str>, Option<&str>) {
    (
        &f.constraint_id,
        f.enclosing_symbol.as_deref(),
        f.snippet.as_deref(),
    )
}

/// For each finding in order, whether it survives cancellation against the
/// `prior` multiset: `false` when its key still had budget (which is spent),
/// `true` for the surplus.
fn surplus_mask<'f, K: std::hash::Hash + Eq>(
    findings: &'f [ConstraintFinding],
    prior: &mut HashMap<K, usize>,
    key: impl Fn(&'f ConstraintFinding) -> K,
) -> Vec<bool> {
    findings.iter().map(|f| !spend(prior, &key(f))).collect()
}

/// Spend one unit of `key`'s budget in `prior`; `false` when none is left.
/// The one cancellation rule every match diff shares.
fn spend<K: std::hash::Hash + Eq>(prior: &mut HashMap<K, usize>, key: &K) -> bool {
    match prior.get_mut(key) {
        Some(count) if *count > 0 => {
            *count -= 1;
            true
        }
        _ => false,
    }
}

fn keep_masked(findings: Vec<ConstraintFinding>, mask: Vec<bool>) -> Vec<ConstraintFinding> {
    findings
        .into_iter()
        .zip(mask)
        .filter_map(|(f, keep)| keep.then_some(f))
        .collect()
}

/// Walk the workspace for files that are pattern-eligible but never indexed
/// (e.g. Python `.pyi` stubs) and return their workspace-relative paths, sorted.
///
/// These files have no row in the files table by design — indexing them would
/// double-count symbols their `.py` sibling already declares — so constraint
/// evaluation discovers them on disk instead.
pub fn scan_pattern_only_files(root: &Path, registry: &LanguageRegistry) -> Vec<String> {
    let exts = registry.pattern_only_extensions();
    if exts.is_empty() {
        return Vec::new();
    }
    crate::pipeline::walk_source_files(root, &exts)
        .iter()
        .filter_map(|p| p.strip_prefix(root).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// True when `path` has an extension that is pattern-eligible but never indexed.
/// Callers that work from a changed-path list (review) use this to keep stubs
/// visible, since stubs have no file id to travel with.
pub fn is_pattern_only_path(path: &str, registry: &LanguageRegistry) -> bool {
    registry
        .pattern_only_extensions()
        .iter()
        .any(|ext| path.ends_with(&format!(".{ext}")))
}

pub fn check_forbidden_patterns(
    constraints: &[Constraint],
    sources: &[(&str, &str)],
    registry: &LanguageRegistry,
) -> Vec<ConstraintFinding> {
    let pattern_constraints: Vec<_> = constraints
        .iter()
        .filter_map(|c| match &c.kind {
            ConstraintKind::ForbiddenPattern { language, query } => {
                Some((c, language.as_str(), query.as_str()))
            }
            _ => None,
        })
        .collect();
    if pattern_constraints.is_empty() {
        return Vec::new();
    }

    let mut findings = Vec::new();
    // Test-only line ranges and comment layout are properties of the file, not
    // of the constraint, so they survive across the per-constraint loop.
    let mut test_ranges: HashMap<&str, Vec<(u32, u32)>> = HashMap::new();
    let mut comment_maps: HashMap<&str, CommentMap<'_>> = HashMap::new();
    for &(constraint, lang, query_str) in &pattern_constraints {
        let adapter = match registry.adapter_for_language(lang) {
            Some(a) => a,
            None => continue,
        };
        let grammar = adapter.grammar();
        let compiled = match Query::new(&grammar, query_str) {
            Ok(q) => q,
            Err(_) => continue,
        };
        // A capture named `match` marks the node the rule is about; without
        // it the first capture stands in. A rule that also captures a receiver
        // would otherwise report the receiver's line (sutra/417).
        let match_capture = compiled.capture_index_for_name("match");

        let mut parser = Parser::new();
        if parser.set_language(&grammar).is_err() {
            continue;
        }

        let matching_exts: Vec<&str> = adapter.pattern_extensions().to_vec();
        let scope_is_test_directed = constraint
            .scope
            .as_deref()
            .is_some_and(|s| super::glob_targets_tests(s, &|p| adapter.is_test_path(p)));

        for &(path, source) in sources {
            if let Some(scope) = &constraint.scope
                && !scope_matches_path(scope, path)
            {
                continue;
            }

            let has_matching_ext = matching_exts
                .iter()
                .any(|ext| path.ends_with(&format!(".{ext}")));
            if !has_matching_ext {
                continue;
            }

            // Whole-file test targets (Rust `tests/`, Dart `test/`) have no
            // attribute for `test_line_ranges` to find, so they are excluded by
            // path — unless the rule opted in, or aimed itself at tests
            // (sutra/292).
            if !constraint.include_tests && !scope_is_test_directed && adapter.is_test_path(path) {
                continue;
            }

            let tree = match parser.parse(source, None) {
                Some(t) => t,
                None => continue,
            };

            let symbols = extract_symbols_for_enclosing(adapter, source, path);

            // Test code exercises the very constructs production rules forbid
            // (`.unwrap()` in assertions, clones in fixtures). Matches inside
            // it are excluded unless the rule opts in (sutra/290).
            let skip_ranges: &[(u32, u32)] = if constraint.include_tests {
                &[]
            } else {
                test_ranges.entry(path).or_insert_with(|| {
                    let ctx = ParseContext {
                        source: source.as_bytes(),
                        tree: &tree,
                        file_path: path,
                    };
                    adapter.test_line_ranges(&ctx)
                })
            };

            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&compiled, tree.root_node(), source.as_bytes());
            while let Some(m) = matches.next() {
                let Some(capture) = match_capture
                    .and_then(|idx| m.captures.iter().find(|c| c.index == idx))
                    .or_else(|| m.captures.first())
                else {
                    continue;
                };
                let node = capture.node;
                let start = node.start_position();
                let line = (start.row + 1) as u32;
                if line_in_ranges(skip_ranges, line) {
                    continue;
                }
                let justification = constraint.justify.as_deref().and_then(|marker| {
                    comment_maps
                        .entry(path)
                        .or_insert_with(|| CommentMap::build(tree.root_node(), source))
                        .justification(start.row, marker)
                });
                let byte_range = node.byte_range();
                let snippet = source
                    .get(byte_range.clone())
                    .unwrap_or("")
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();

                let enclosing = find_enclosing_symbol(&symbols, line);

                findings.push(ConstraintFinding {
                    constraint_id: Arc::clone(&constraint.id),
                    constraint_name: constraint.name.clone(),
                    constraint_kind: "forbidden_pattern".to_string(),
                    severity: constraint.severity,
                    provenance: constraint.provenance.clone(),
                    from_path: path.to_string(),
                    to_path: String::new(),
                    component_context: None,
                    detail: format!(
                        "forbidden pattern match in {path}:{line}: {}",
                        truncate_snippet(&snippet, 80),
                    ),
                    delta: FindingDelta::Unknown,
                    line: Some(line),
                    snippet: Some(snippet),
                    enclosing_symbol: enclosing,
                    justification,
                });
            }
        }
    }
    findings
}

/// The comments of one source file, laid out by row, for resolving a rule's
/// `justify` marker against a match.
struct CommentMap<'s> {
    /// `(start_row, end_row, text)` of every comment node, in document order.
    comments: Vec<(usize, usize, &'s str)>,
    /// Rows holding nothing but comment text — the rows a comment run above a
    /// match may span. A trailing comment after code does not make its row one.
    comment_only_rows: HashSet<usize>,
}

impl<'s> CommentMap<'s> {
    fn build(root: Node<'_>, source: &'s str) -> Self {
        let lines: Vec<&str> = source.lines().collect();
        let mut comments = Vec::new();
        let mut comment_only_rows = HashSet::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            // Every grammar sutra ships names its comment nodes `*comment*`
            // (`line_comment`, `block_comment`, `comment`,
            // `documentation_comment`). Nested doc-comment children are not
            // descended into, so each comment is recorded once.
            if node.kind().contains("comment") {
                let (start, end) = (node.start_position(), node.end_position());
                let starts_line = lines
                    .get(start.row)
                    .and_then(|l| l.get(..start.column))
                    .is_some_and(|prefix| prefix.trim().is_empty());
                if starts_line {
                    comment_only_rows.extend(start.row..=end.row);
                }
                comments.push((start.row, end.row, node_text(node, source.as_bytes())));
                continue;
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
        Self {
            comments,
            comment_only_rows,
        }
    }

    /// The reason a match starting on `row` is justified: the text after
    /// `marker` in a comment on that row, or in the contiguous run of
    /// comment-only rows directly above it. `None` when no such comment exists
    /// or the text after the marker is empty — a bare marker justifies nothing.
    fn justification(&self, row: usize, marker: &str) -> Option<String> {
        let mut top = row;
        while top > 0 && self.comment_only_rows.contains(&(top - 1)) {
            top -= 1;
        }
        self.comments
            .iter()
            .filter(|&&(start, end, _)| (start <= row && row <= end) || (start >= top && end < row))
            .find_map(|&(_, _, text)| reason_after_marker(text, marker))
    }
}

/// The non-empty text following `marker` at the start of any line of a comment,
/// after its delimiters (`//`, `///`, `#`, `/*`, ` * `). Requiring the marker to
/// lead the line keeps prose that merely mentions it from counting.
fn reason_after_marker(comment: &str, marker: &str) -> Option<String> {
    comment.lines().find_map(|line| {
        let body = line
            .trim_start()
            .trim_start_matches(['/', '*', '#', '!'])
            .trim_start();
        let reason = body
            .strip_prefix(marker)?
            .trim()
            .trim_end_matches("*/")
            .trim_end();
        (!reason.is_empty()).then(|| reason.to_string())
    })
}

fn extract_symbols_for_enclosing(
    adapter: &dyn crate::parser::adapter::LanguageAdapter,
    source: &str,
    path: &str,
) -> Vec<(String, usize, usize)> {
    let pool_result = {
        let mut pool = crate::parser::adapter::ParserPool::new(std::time::Duration::from_secs(5));
        pool.parse_with(adapter, source, path)
    };
    match pool_result {
        Ok(result) => flatten_extracted(&result.symbols),
        Err(_) => Vec::new(),
    }
}

fn flatten_extracted(symbols: &[ExtractedSymbol]) -> Vec<(String, usize, usize)> {
    flatten_symbols(symbols)
        .into_iter()
        .map(|s| (s.qualified_name.clone(), s.start_line, s.end_line))
        .collect()
}

fn find_enclosing_symbol(symbols: &[(String, usize, usize)], line: u32) -> Option<String> {
    let line = line as usize;
    let mut best: Option<&(String, usize, usize)> = None;
    for s in symbols {
        if s.1 <= line && line <= s.2 {
            match best {
                None => best = Some(s),
                Some(prev) if (s.2 - s.1) < (prev.2 - prev.1) => best = Some(s),
                _ => {}
            }
        }
    }
    best.map(|s| s.0.clone())
}

fn truncate_snippet(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::adapter::default_registry;
    use crate::rules::{Severity, parse_rules};

    fn pattern_constraints(toml: &str) -> Vec<Constraint> {
        parse_rules(toml).unwrap().all_constraints().0
    }

    #[test]
    fn rust_unsafe_block_detected() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
name = "no-unsafe"
"#;
        let cs = pattern_constraints(toml);
        let registry = default_registry();
        let source = r#"
fn safe_fn() {}
fn dangerous() {
    unsafe { std::ptr::null::<u8>().read() };
}
"#;
        let findings = check_forbidden_patterns(&cs, &[("src/lib.rs", source)], &registry);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].from_path, "src/lib.rs");
        assert_eq!(findings[0].line, Some(4));
        assert!(findings[0].snippet.as_deref().unwrap().contains("unsafe"));
        assert_eq!(findings[0].severity, Severity::Advisory);
        assert_eq!(findings[0].constraint_name.as_deref(), Some("no-unsafe"));
    }

    #[test]
    fn cfg_test_module_excluded_by_default() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
name = "no-unsafe"
"#;
        let cs = pattern_constraints(toml);
        let registry = default_registry();
        let source = r#"
fn prod() {
    unsafe { std::ptr::null::<u8>().read() };
}

#[cfg(test)]
mod tests {
    #[test]
    fn t() {
        unsafe { std::ptr::null::<u8>().read() };
    }
}
"#;
        let findings = check_forbidden_patterns(&cs, &[("src/lib.rs", source)], &registry);
        assert_eq!(
            findings.len(),
            1,
            "only the production match should survive"
        );
        assert_eq!(findings[0].line, Some(3));
    }

    #[test]
    fn include_tests_opt_in_restores_test_matches() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
name = "no-unsafe"
include_tests = true
"#;
        let cs = pattern_constraints(toml);
        assert!(cs[0].include_tests);
        let registry = default_registry();
        let source = r#"
fn prod() {
    unsafe { std::ptr::null::<u8>().read() };
}

#[cfg(test)]
mod tests {
    fn t() {
        unsafe { std::ptr::null::<u8>().read() };
    }
}
"#;
        let findings = check_forbidden_patterns(&cs, &[("src/lib.rs", source)], &registry);
        assert_eq!(findings.len(), 2);
    }

    const UNSAFE_RULE: &str = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
name = "no-unsafe"
"#;

    const UNSAFE_SOURCE: &str = r#"
fn helper() {
    unsafe { std::ptr::null::<u8>().read() };
}
"#;

    #[test]
    fn rust_integration_test_target_excluded_by_path() {
        let cs = pattern_constraints(UNSAFE_RULE);
        let registry = default_registry();
        for path in [
            "tests/integration.rs",
            "tests/helpers/fixture.rs",
            "crates/core/tests/integration.rs",
            "benches/throughput.rs",
        ] {
            let findings = check_forbidden_patterns(&cs, &[(path, UNSAFE_SOURCE)], &registry);
            assert!(
                findings.is_empty(),
                "{path} is a test target, got {findings:?}"
            );
        }
    }

    #[test]
    fn path_exclusion_does_not_swallow_production_lookalikes() {
        let cs = pattern_constraints(UNSAFE_RULE);
        let registry = default_registry();
        for path in ["src/lib.rs", "src/tests.rs", "src/attest/mod.rs"] {
            let findings = check_forbidden_patterns(&cs, &[(path, UNSAFE_SOURCE)], &registry);
            assert_eq!(findings.len(), 1, "{path} is production, got {findings:?}");
        }
    }

    #[test]
    fn include_tests_opt_in_restores_test_target_matches() {
        let toml = format!("{UNSAFE_RULE}include_tests = true\n");
        let cs = pattern_constraints(&toml);
        let registry = default_registry();
        let findings =
            check_forbidden_patterns(&cs, &[("tests/integration.rs", UNSAFE_SOURCE)], &registry);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn test_scoped_rule_still_fires_in_its_own_scope() {
        let toml = format!("{UNSAFE_RULE}scope = \"tests/**\"\n");
        let cs = pattern_constraints(&toml);
        let registry = default_registry();
        let findings =
            check_forbidden_patterns(&cs, &[("tests/integration.rs", UNSAFE_SOURCE)], &registry);
        assert_eq!(
            findings.len(),
            1,
            "a rule aimed at tests/ must not be muted by test-path exclusion"
        );
    }

    #[test]
    fn scope_targets_tests_only_for_test_directed_scopes() {
        let registry = default_registry();
        let rust = registry
            .adapter_for_language("rust")
            .expect("invariant: default registry always carries a rust adapter");
        let is_test = |p: &str| rust.is_test_path(p);
        for scope in ["tests", "tests/", "tests/**", "crates/core/tests/**"] {
            assert!(super::super::glob_targets_tests(scope, &is_test), "{scope}");
        }
        for scope in ["src/**", "**/*.rs", "src/"] {
            assert!(
                !super::super::glob_targets_tests(scope, &is_test),
                "{scope}"
            );
        }
    }

    #[test]
    fn dart_test_files_excluded_by_path() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "dart"
query = "(assignment_expression) @cap"
name = "no-assign"
"#;
        let cs = pattern_constraints(toml);
        let registry = default_registry();
        let source = "void main() { var x = 0; x = 1; }\n";
        let prod = check_forbidden_patterns(&cs, &[("lib/widget.dart", source)], &registry);
        assert_eq!(prod.len(), 1, "production dart still reports");
        for path in [
            "test/widget_test.dart",
            "test/support/fixture.dart",
            "packages/ui/test/widget_test.dart",
            "lib/src/thing_test.dart",
            "integration_test/app_test.dart",
        ] {
            let findings = check_forbidden_patterns(&cs, &[(path, source)], &registry);
            assert!(findings.is_empty(), "{path} is test code, got {findings:?}");
        }
    }

    /// Every language that classifies test paths, checked through the same
    /// door a real rule uses (sutra/295). Each case is
    /// `(language, query, production path + source, test paths)`.
    #[test]
    fn remaining_languages_exclude_test_paths() {
        struct Case {
            language: &'static str,
            query: &'static str,
            source: &'static str,
            production: &'static str,
            tests: &'static [&'static str],
        }
        let cases = [
            Case {
                language: "python",
                query: "(assert_statement) @cap",
                source: "def f():\n    assert True\n",
                production: "app/models.py",
                tests: &[
                    "tests/test_models.py",
                    "app/tests/test_models.py",
                    "app/test_models.py",
                    "app/models_test.py",
                ],
            },
            Case {
                language: "c",
                query: "(goto_statement) @cap",
                source: "int f(void) { goto done; done: return 0; }\n",
                production: "src/engine.c",
                tests: &["tests/engine.c", "src/tests/engine.c", "src/engine_test.c"],
            },
            Case {
                language: "typescript",
                query: "(debugger_statement) @cap",
                source: "function f() { debugger; }\n",
                production: "src/app.ts",
                tests: &[
                    "src/app.test.ts",
                    "src/app.spec.tsx",
                    "src/__tests__/app.ts",
                    "test/app.ts",
                    "packages/ui/tests/app.ts",
                ],
            },
            Case {
                language: "javascript",
                query: "(debugger_statement) @cap",
                source: "function f() { debugger; }\n",
                production: "src/app.js",
                tests: &["src/app.test.js", "src/__tests__/app.js", "test/app.mjs"],
            },
        ];

        let registry = default_registry();
        for case in cases {
            let toml = format!(
                "[[constraint]]\nkind = \"forbidden_pattern\"\nlanguage = \"{}\"\nquery = \"{}\"\nname = \"no-x\"\n",
                case.language, case.query
            );
            let cs = pattern_constraints(&toml);
            let prod = check_forbidden_patterns(&cs, &[(case.production, case.source)], &registry);
            assert_eq!(
                prod.len(),
                1,
                "{} production path {} should report",
                case.language,
                case.production
            );
            for path in case.tests {
                let findings = check_forbidden_patterns(&cs, &[(*path, case.source)], &registry);
                assert!(
                    findings.is_empty(),
                    "{} test path {path} should be excluded, got {findings:?}",
                    case.language
                );
            }

            let opt_in = pattern_constraints(&format!("{toml}include_tests = true\n"));
            let restored =
                check_forbidden_patterns(&opt_in, &[(case.tests[0], case.source)], &registry);
            assert_eq!(
                restored.len(),
                1,
                "{} include_tests must restore {}",
                case.language,
                case.tests[0]
            );
        }
    }

    #[test]
    fn bare_test_attribute_on_free_function_excluded() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
"#;
        let cs = pattern_constraints(toml);
        let registry = default_registry();
        let source = r#"
#[test]
fn standalone() {
    unsafe { std::ptr::null::<u8>().read() };
}

#[tokio::test]
async fn async_case() {
    unsafe { std::ptr::null::<u8>().read() };
}

fn prod() {
    unsafe { std::ptr::null::<u8>().read() };
}
"#;
        let findings = check_forbidden_patterns(&cs, &[("src/lib.rs", source)], &registry);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].line, Some(13));
    }

    #[test]
    fn cfg_not_test_stays_production() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
"#;
        let cs = pattern_constraints(toml);
        let registry = default_registry();
        // `not(test)` and a `test`-named feature are both production code —
        // misreading either would silently mute a real rule.
        let source = r#"
#[cfg(not(test))]
fn only_in_release() {
    unsafe { std::ptr::null::<u8>().read() };
}

#[cfg(feature = "test-helpers")]
fn feature_gated() {
    unsafe { std::ptr::null::<u8>().read() };
}
"#;
        let findings = check_forbidden_patterns(&cs, &[("src/lib.rs", source)], &registry);
        assert_eq!(findings.len(), 2);
    }

    #[test]
    fn rust_no_match_yields_no_findings() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
"#;
        let cs = pattern_constraints(toml);
        let registry = default_registry();
        let source = "fn safe() { let x = 1; }\n";
        let findings = check_forbidden_patterns(&cs, &[("src/lib.rs", source)], &registry);
        assert!(findings.is_empty());
    }

    #[test]
    fn rust_multiple_matches() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
"#;
        let cs = pattern_constraints(toml);
        let registry = default_registry();
        let source = r#"
fn a() { unsafe { } }
fn b() { unsafe { } }
fn c() { unsafe { } }
"#;
        let findings = check_forbidden_patterns(&cs, &[("src/lib.rs", source)], &registry);
        assert_eq!(findings.len(), 3);
    }

    #[test]
    fn scope_filters_files() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
scope = "src/core"
"#;
        let cs = pattern_constraints(toml);
        let registry = default_registry();
        let source = "fn f() { unsafe { } }\n";
        let in_scope = check_forbidden_patterns(&cs, &[("src/core/lib.rs", source)], &registry);
        assert_eq!(in_scope.len(), 1);

        let out_of_scope =
            check_forbidden_patterns(&cs, &[("src/tools/lib.rs", source)], &registry);
        assert!(out_of_scope.is_empty());
    }

    #[test]
    fn skips_wrong_language_files() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
"#;
        let cs = pattern_constraints(toml);
        let registry = default_registry();
        let source = "fn f() { unsafe { } }\n";
        let findings = check_forbidden_patterns(&cs, &[("lib/main.dart", source)], &registry);
        assert!(findings.is_empty());
    }

    #[test]
    fn enclosing_symbol_resolved() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
"#;
        let cs = pattern_constraints(toml);
        let registry = default_registry();
        let source = r#"fn outer() {
    unsafe { }
}
"#;
        let findings = check_forbidden_patterns(&cs, &[("src/lib.rs", source)], &registry);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].enclosing_symbol.as_deref(), Some("outer"),);
    }

    #[test]
    fn dart_forbidden_pattern() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "dart"
query = "(throw_expression) @cap"
name = "no-throw"
"#;
        let cs = pattern_constraints(toml);
        let registry = default_registry();
        let source = r#"
void safe() {}
void risky() {
  throw Exception('boom');
}
"#;
        let findings = check_forbidden_patterns(&cs, &[("lib/src/app.dart", source)], &registry);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].from_path, "lib/src/app.dart");
        assert_eq!(findings[0].line, Some(4));
        assert!(findings[0].snippet.as_deref().unwrap().contains("throw"));
    }

    /// Real stub text from pyswisseph-rs `python/swisseph_rs/azalt.pyi` (c4f527e),
    /// with `RETURN` standing in for the return annotation. `-> Never` is the
    /// form that broke mypy's class-callable inference (pyswisseph-rs/30);
    /// `-> Self` is what shipped.
    fn azalt_stub(ret: &str) -> String {
        format!(
            "from typing import Never, Self, final\n\
             \n\
             @final\n\
             class RefracDir:\n    \
             TRUE_TO_APP: RefracDir\n    \
             APP_TO_TRUE: RefracDir\n    \
             def __new__(cls, _: Never, /) -> {ret}: ...\n\
             \n\
             def refrac(inalt: float, dir: RefracDir) -> float: ...\n"
        )
    }

    fn new_returns_never_constraints() -> Vec<Constraint> {
        pattern_constraints(
            r#"
[[constraint]]
kind = "forbidden_pattern"
language = "python"
query = '''
(function_definition
  name: (identifier) @_name (#eq? @_name "__new__")
  return_type: (type (identifier) @_ret) (#eq? @_ret "Never")) @match
'''
name = "no-new-returning-never"
"#,
        )
    }

    #[test]
    fn python_stub_new_returning_never_detected() {
        let cs = new_returns_never_constraints();
        let registry = default_registry();
        let source = azalt_stub("Never");
        let findings =
            check_forbidden_patterns(&cs, &[("python/swisseph_rs/azalt.pyi", &source)], &registry);
        assert_eq!(findings.len(), 1, "findings: {findings:#?}");
        assert_eq!(findings[0].from_path, "python/swisseph_rs/azalt.pyi");
        assert!(findings[0].snippet.as_deref().unwrap().contains("__new__"));
    }

    #[test]
    fn python_stub_new_returning_self_is_clean() {
        let cs = new_returns_never_constraints();
        let registry = default_registry();
        let source = azalt_stub("Self");
        let findings =
            check_forbidden_patterns(&cs, &[("python/swisseph_rs/azalt.pyi", &source)], &registry);
        assert!(findings.is_empty(), "findings: {findings:#?}");
    }

    /// `.pyi` is pattern-eligible but must stay out of the index — a stub
    /// declares the same symbols as its `.py` sibling, so indexing it would
    /// double-count every symbol in the graph.
    #[test]
    fn pyi_is_pattern_eligible_but_not_indexed() {
        let registry = default_registry();
        let python = registry.adapter_for_language("python").unwrap();
        assert!(!python.extensions().contains(&"pyi"));
        assert!(python.pattern_extensions().contains(&"pyi"));
        assert!(registry.adapter_for_extension("pyi").is_none());
        assert_eq!(registry.pattern_only_extensions(), vec!["pyi"]);
    }

    #[test]
    fn identity_propagated_to_findings() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
name = "no-unsafe"
provenance = "docs/adr.md"
"#;
        let cs = pattern_constraints(toml);
        let registry = default_registry();
        let source = "fn f() { unsafe { } }\n";
        let findings = check_forbidden_patterns(&cs, &[("src/lib.rs", source)], &registry);
        assert_eq!(findings[0].constraint_id, cs[0].id);
        assert_eq!(findings[0].provenance.as_deref(), Some("docs/adr.md"));
    }

    /// A receiver-capturing rule on a split chain: the receiver capture comes
    /// first by position, but the finding must land on the `@match` line — the
    /// one a diff adds (sutra/417's `.unwrap_or(false)`).
    #[test]
    fn match_capture_reported_over_receiver() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = '(call_expression function: (field_expression value: (_) @r field: (field_identifier) @match (#eq? @match "unwrap_or")))'
name = "no-unwrap-or"
"#;
        let cs = pattern_constraints(toml);
        let registry = default_registry();
        let source =
            "fn f(o: Option<u8>) -> bool {\n    o.map(|x| x > 1)\n        .unwrap_or(false)\n}\n";
        let findings = check_forbidden_patterns(&cs, &[("src/lib.rs", source)], &registry);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].line, Some(3));
        assert_eq!(findings[0].snippet.as_deref(), Some("unwrap_or"));

        // Without a `@match` capture the first capture still stands in.
        let first_only = pattern_constraints(&toml.replace("@match", "@m"));
        let findings = check_forbidden_patterns(&first_only, &[("src/lib.rs", source)], &registry);
        assert_eq!(findings[0].line, Some(2));
    }

    const JUSTIFIED_UNSAFE_RULE: &str = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @match"
name = "no-unsafe"
justify = "swallow:"
"#;

    fn justifications(source: &str) -> Vec<Option<String>> {
        let cs = pattern_constraints(JUSTIFIED_UNSAFE_RULE);
        check_forbidden_patterns(&cs, &[("src/lib.rs", source)], &default_registry())
            .into_iter()
            .map(|f| f.justification)
            .collect()
    }

    #[test]
    fn justify_marker_on_match_line_or_comment_run_above() {
        let source = r#"
fn trailing() {
    unsafe { } // swallow: trailing reason
}
fn above() {
    // swallow: first line of the run
    // more context
    unsafe { }
}
fn block() {
    /* swallow: block reason */
    unsafe { }
}
fn doc_marker_mid_run() {
    // context first
    //swallow:   tight marker
    unsafe { }
}
"#;
        assert_eq!(
            justifications(source),
            vec![
                Some("trailing reason".to_string()),
                Some("first line of the run".to_string()),
                Some("block reason".to_string()),
                Some("tight marker".to_string()),
            ]
        );
    }

    #[test]
    fn justify_marker_rejected_when_empty_detached_or_not_a_comment() {
        let source = r#"
fn bare() {
    // swallow:
    unsafe { }
}
fn blank_line_breaks_run() {
    // swallow: too far away

    unsafe { }
}
fn trailing_comment_on_code_above() {
    let x = 1; // swallow: belongs to the line above
    unsafe { }
}
fn string_literal() {
    let s = "swallow: not a comment"; unsafe { }
}
fn prose_mentioning_marker() {
    // see the swallow: convention
    unsafe { }
}
"#;
        assert_eq!(justifications(source), vec![None; 5]);
    }

    #[test]
    fn justify_is_per_rule_and_python_uses_hash_comments() {
        // The same comment under a rule without `justify` justifies nothing.
        let cs = pattern_constraints(UNSAFE_RULE);
        let source = "fn f() {\n    // swallow: reason\n    unsafe { }\n}\n";
        let findings =
            check_forbidden_patterns(&cs, &[("src/lib.rs", source)], &default_registry());
        assert_eq!(findings[0].justification, None);

        let py = pattern_constraints(
            "[[constraint]]\nkind = \"forbidden_pattern\"\nlanguage = \"python\"\nquery = \"(assert_statement) @match\"\njustify = \"swallow:\"\n",
        );
        let source = "def f():\n    # swallow: invariant check\n    assert True\n";
        let findings = check_forbidden_patterns(&py, &[("app/m.py", source)], &default_registry());
        assert_eq!(
            findings[0].justification.as_deref(),
            Some("invariant check")
        );
    }

    #[test]
    fn justify_rejected_on_other_kinds_and_when_empty() {
        let other = "[[constraint]]\nkind = \"forbidden_dep\"\nfrom = \"a\"\nto = \"b\"\njustify = \"x:\"\n";
        let empty = "[[constraint]]\nkind = \"forbidden_pattern\"\nlanguage = \"rust\"\nquery = \"(unsafe_block) @m\"\njustify = \" \"\n";
        for toml in [other, empty] {
            let (constraints, errors) = parse_rules(toml).unwrap().all_constraints();
            assert!(constraints.is_empty() && !errors.is_empty(), "{toml}");
        }
    }

    #[test]
    fn non_pattern_constraints_ignored() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "a"
to = "b"
"#;
        let cs = pattern_constraints(toml);
        let registry = default_registry();
        let findings = check_forbidden_patterns(&cs, &[("src/lib.rs", "fn f() {}")], &registry);
        assert!(findings.is_empty());
    }
}
