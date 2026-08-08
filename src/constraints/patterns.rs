use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::constraints::{ConstraintFinding, FindingDelta};
use crate::parser::adapter::{LanguageRegistry, ParseContext, line_in_ranges};
use crate::parser::{ExtractedSymbol, flatten_symbols};
use crate::rules::{Constraint, ConstraintKind, scope_matches_path};

/// Content fingerprint of a forbidden-pattern match, stable across line moves
/// and re-indentation: `(constraint_id, enclosing_symbol, snippet)`. `snippet`
/// is the matched node's first line verbatim (node-relative, so leading
/// indentation is excluded). Shared by the guard's introduced-only diff and the
/// report-path instance-ack subtraction so both fingerprint matches identically
/// (sutra/305).
pub type MatchKey = (Arc<str>, Option<String>, Option<String>);

/// The [`MatchKey`] of a single finding. The clones build an owned key from a
/// borrowed finding: the `Arc<str>` clone is a refcount bump, and the key must
/// outlive the findings it is derived from (they are consumed by
/// [`subtract_multiset`]), so borrowing is not an option.
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
/// not account for.
///
/// The guard uses this to diff proposed matches against what is already on disk
/// (introduced-only enforcement); the report path uses it to subtract accepted
/// instance-ack counts. `prior` is taken by value and spent in place.
pub fn subtract_multiset(
    findings: Vec<ConstraintFinding>,
    mut prior: HashMap<MatchKey, usize>,
) -> Vec<ConstraintFinding> {
    let mut surplus = Vec::new();
    for f in findings {
        let key = match_key(&f);
        if let Some(count) = prior.get_mut(&key)
            && *count > 0
        {
            *count -= 1;
            continue;
        }
        surplus.push(f);
    }
    surplus
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
    // Test-only line ranges are a property of (file, language), not of the
    // constraint, so they survive across the per-constraint loop.
    let mut test_ranges: HashMap<&str, Vec<(u32, u32)>> = HashMap::new();
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
                let Some(capture) = m.captures.first() else {
                    continue;
                };
                let node = capture.node;
                let start = node.start_position();
                let line = (start.row + 1) as u32;
                if line_in_ranges(skip_ranges, line) {
                    continue;
                }
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
                });
            }
        }
    }
    findings
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
