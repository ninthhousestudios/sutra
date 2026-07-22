use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::constraints::{ConstraintFinding, FindingDelta};
use crate::parser::adapter::LanguageRegistry;
use crate::parser::{ExtractedSymbol, flatten_symbols};
use crate::rules::{Constraint, ConstraintKind, scope_matches_path};

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

            let tree = match parser.parse(source, None) {
                Some(t) => t,
                None => continue,
            };

            let symbols = extract_symbols_for_enclosing(adapter, source, path);

            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&compiled, tree.root_node(), source.as_bytes());
            while let Some(m) = matches.next() {
                let Some(capture) = m.captures.first() else {
                    continue;
                };
                let node = capture.node;
                let start = node.start_position();
                let line = (start.row + 1) as u32;
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
                    constraint_id: constraint.id.clone(),
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
