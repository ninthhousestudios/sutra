use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use serde::Serialize;

use crate::error::Result;
use crate::git;
use crate::parser::{
    self, ExtractedRef, ExtractedSymbol, ParseResult, RefContextKind, flatten_symbols,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Deleted,
    SignatureChanged,
    BodyChanged,
    CosmeticChanged,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalleeDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolChange {
    pub symbol: String,
    pub kind: String,
    pub change: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callee_diff: Option<CalleeDiff>,
}

fn body_hash(source: &str, sym: &ExtractedSymbol) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let start = sym.start_line.saturating_sub(1);
    let end = sym.end_line.min(lines.len());
    let body = &lines[start..end];
    let joined = body.join("\n");
    blake3::hash(joined.as_bytes()).to_hex().to_string()
}

fn callees_in_range(refs: &[ExtractedRef], start_line: usize, end_line: usize) -> BTreeSet<String> {
    refs.iter()
        .filter(|r| {
            r.context_kind == RefContextKind::Call && r.line >= start_line && r.line <= end_line
        })
        .map(|r| r.name.clone())
        .collect()
}

fn callee_diff(
    old_refs: &[ExtractedRef],
    new_refs: &[ExtractedRef],
    old_sym: &ExtractedSymbol,
    new_sym: &ExtractedSymbol,
) -> CalleeDiff {
    let old_callees = callees_in_range(old_refs, old_sym.start_line, old_sym.end_line);
    let new_callees = callees_in_range(new_refs, new_sym.start_line, new_sym.end_line);
    CalleeDiff {
        added: new_callees.difference(&old_callees).cloned().collect(),
        removed: old_callees.difference(&new_callees).cloned().collect(),
    }
}

pub fn classify_symbols(
    old_parse: &ParseResult,
    new_parse: &ParseResult,
    old_source: &str,
    new_source: &str,
) -> Vec<SymbolChange> {
    let old_flat = flatten_symbols(&old_parse.symbols);
    let new_flat = flatten_symbols(&new_parse.symbols);

    type SymKey<'a> = (&'a str, &'a str);
    fn sym_key(s: &ExtractedSymbol) -> SymKey<'_> {
        (s.qualified_name.as_str(), s.kind.as_str())
    }

    let old_map: HashMap<SymKey<'_>, &ExtractedSymbol> =
        old_flat.iter().map(|s| (sym_key(s), *s)).collect();
    let new_map: HashMap<SymKey<'_>, &ExtractedSymbol> =
        new_flat.iter().map(|s| (sym_key(s), *s)).collect();

    let mut changes = Vec::new();

    for new_sym in &new_flat {
        let name = new_sym.qualified_name.as_str();
        match old_map.get(&sym_key(new_sym)) {
            None => {
                changes.push(SymbolChange {
                    symbol: name.to_string(),
                    kind: new_sym.kind.as_str().to_string(),
                    change: ChangeKind::Added,
                    callee_diff: None,
                });
            }
            Some(old_sym) => {
                let sig_changed = match (&old_sym.signature_hash, &new_sym.signature_hash) {
                    (Some(oh), Some(nh)) => oh != nh,
                    (None, Some(_)) | (Some(_), None) => true,
                    (None, None) => false,
                };
                if sig_changed {
                    changes.push(SymbolChange {
                        symbol: name.to_string(),
                        kind: new_sym.kind.as_str().to_string(),
                        change: ChangeKind::SignatureChanged,
                        callee_diff: None,
                    });
                } else {
                    let old_hash = body_hash(old_source, old_sym);
                    let new_hash = body_hash(new_source, new_sym);
                    if old_hash != new_hash {
                        let is_cosmetic = match (&old_sym.structural_hash, &new_sym.structural_hash)
                        {
                            (Some(oh), Some(nh)) => oh == nh,
                            _ => false,
                        };
                        if is_cosmetic {
                            changes.push(SymbolChange {
                                symbol: name.to_string(),
                                kind: new_sym.kind.as_str().to_string(),
                                change: ChangeKind::CosmeticChanged,
                                callee_diff: None,
                            });
                        } else {
                            let cd = callee_diff(
                                &old_parse.references,
                                &new_parse.references,
                                old_sym,
                                new_sym,
                            );
                            let cd = if cd.added.is_empty() && cd.removed.is_empty() {
                                None
                            } else {
                                Some(cd)
                            };
                            changes.push(SymbolChange {
                                symbol: name.to_string(),
                                kind: new_sym.kind.as_str().to_string(),
                                change: ChangeKind::BodyChanged,
                                callee_diff: cd,
                            });
                        }
                    }
                }
            }
        }
    }

    for old_sym in &old_flat {
        let name = old_sym.qualified_name.as_str();
        if !new_map.contains_key(&sym_key(old_sym)) {
            changes.push(SymbolChange {
                symbol: name.to_string(),
                kind: old_sym.kind.as_str().to_string(),
                change: ChangeKind::Deleted,
                callee_diff: None,
            });
        }
    }

    changes
}

fn language_for_path(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?;
    match ext {
        "rs" => Some("rust"),
        "dart" => Some("dart"),
        _ => None,
    }
}

pub fn diff_file(
    workspace_root: &Path,
    path: &str,
    base: &str,
    head: &str,
) -> Result<Vec<SymbolChange>> {
    let language = match language_for_path(path) {
        Some(l) => l,
        None => return Ok(Vec::new()),
    };

    let old_source = git::git_file_content_at(workspace_root, base, path)?;
    let new_source = git::git_file_content_at(workspace_root, head, path)?;

    match (old_source, new_source) {
        (None, None) => Ok(Vec::new()),
        (None, Some(new_src)) => {
            let new_parse = parser::parse_file(&new_src, language, path)?;
            let flat = flatten_symbols(&new_parse.symbols);
            Ok(flat
                .iter()
                .map(|s| SymbolChange {
                    symbol: s.qualified_name.clone(),
                    kind: s.kind.as_str().to_string(),
                    change: ChangeKind::Added,
                    callee_diff: None,
                })
                .collect())
        }
        (Some(old_src), None) => {
            let old_parse = parser::parse_file(&old_src, language, path)?;
            let flat = flatten_symbols(&old_parse.symbols);
            Ok(flat
                .iter()
                .map(|s| SymbolChange {
                    symbol: s.qualified_name.clone(),
                    kind: s.kind.as_str().to_string(),
                    change: ChangeKind::Deleted,
                    callee_diff: None,
                })
                .collect())
        }
        (Some(old_src), Some(new_src)) => {
            let old_parse = parser::parse_file(&old_src, language, path)?;
            let new_parse = parser::parse_file(&new_src, language, path)?;
            Ok(classify_symbols(&old_parse, &new_parse, &old_src, &new_src))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ExtractedSymbol, ParseResult, SymbolKind};

    fn make_sym(
        name: &str,
        kind: SymbolKind,
        sig_hash: Option<&str>,
        start: usize,
        end: usize,
    ) -> ExtractedSymbol {
        make_sym_full(name, kind, sig_hash, None, start, end)
    }

    fn make_sym_full(
        name: &str,
        kind: SymbolKind,
        sig_hash: Option<&str>,
        structural_hash: Option<&str>,
        start: usize,
        end: usize,
    ) -> ExtractedSymbol {
        ExtractedSymbol {
            qualified_name: name.to_string(),
            short_name: name.to_string(),
            kind,
            signature: None,
            signature_hash: sig_hash.map(|s| s.to_string()),
            structural_hash: structural_hash.map(|s| s.to_string()),
            visibility: None,
            start_line: start,
            start_col: 0,
            end_line: end,
            end_col: 0,
            children: vec![],
            parent_symbol_id: None,
            docstring: None,
            cyclomatic: None,
            cognitive: None,
            max_nesting: None,
            flags: 0,
            language_attrs: None,
        }
    }

    fn make_parse(symbols: Vec<ExtractedSymbol>, references: Vec<ExtractedRef>) -> ParseResult {
        ParseResult {
            file_path: "test.rs".to_string(),
            language: "rust".to_string(),
            symbols,
            references,
            imports: vec![],
            parsed_ok: true,
            line_count: 100,
        }
    }

    fn make_ref(name: &str, line: usize) -> ExtractedRef {
        ExtractedRef {
            name: name.to_string(),
            line,
            col: 0,
            context_kind: RefContextKind::Call,
        }
    }

    #[test]
    fn test_all_added() {
        let old = make_parse(vec![], vec![]);
        let new = make_parse(
            vec![make_sym("foo", SymbolKind::Function, Some("aaa"), 1, 5)],
            vec![],
        );
        let changes = classify_symbols(&old, &new, "", "fn foo() { 1 }");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].symbol, "foo");
        assert_eq!(changes[0].change, ChangeKind::Added);
    }

    #[test]
    fn test_all_deleted() {
        let old = make_parse(
            vec![make_sym("bar", SymbolKind::Function, Some("bbb"), 1, 3)],
            vec![],
        );
        let new = make_parse(vec![], vec![]);
        let changes = classify_symbols(&old, &new, "fn bar() {}", "");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].symbol, "bar");
        assert_eq!(changes[0].change, ChangeKind::Deleted);
    }

    #[test]
    fn test_signature_changed() {
        let old = make_parse(
            vec![make_sym(
                "baz",
                SymbolKind::Function,
                Some("old_hash"),
                1,
                3,
            )],
            vec![],
        );
        let new = make_parse(
            vec![make_sym(
                "baz",
                SymbolKind::Function,
                Some("new_hash"),
                1,
                3,
            )],
            vec![],
        );
        let changes = classify_symbols(&old, &new, "fn baz() {}", "fn baz(x: i32) {}");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::SignatureChanged);
    }

    #[test]
    fn test_body_changed() {
        let old = make_parse(
            vec![make_sym("calc", SymbolKind::Function, Some("same"), 1, 3)],
            vec![],
        );
        let source_old = "fn calc() {\n  1 + 1\n}";
        let source_new = "fn calc() {\n  2 + 2\n}";
        let new = make_parse(
            vec![make_sym("calc", SymbolKind::Function, Some("same"), 1, 3)],
            vec![],
        );
        let changes = classify_symbols(&old, &new, source_old, source_new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::BodyChanged);
    }

    #[test]
    fn test_unchanged_omitted() {
        let source = "fn noop() {\n  42\n}";
        let old = make_parse(
            vec![make_sym("noop", SymbolKind::Function, Some("x"), 1, 3)],
            vec![],
        );
        let new = make_parse(
            vec![make_sym("noop", SymbolKind::Function, Some("x"), 1, 3)],
            vec![],
        );
        let changes = classify_symbols(&old, &new, source, source);
        assert!(changes.is_empty());
    }

    #[test]
    fn test_callee_diff() {
        let source_old = "fn run() {\n  old_call()\n  shared()\n}";
        let source_new = "fn run() {\n  new_call()\n  shared()\n}";
        let old = make_parse(
            vec![make_sym("run", SymbolKind::Function, Some("h"), 1, 3)],
            vec![make_ref("old_call", 2), make_ref("shared", 3)],
        );
        let new = make_parse(
            vec![make_sym("run", SymbolKind::Function, Some("h"), 1, 3)],
            vec![make_ref("new_call", 2), make_ref("shared", 3)],
        );
        let changes = classify_symbols(&old, &new, source_old, source_new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::BodyChanged);
        let cd = changes[0].callee_diff.as_ref().unwrap();
        assert_eq!(cd.added, vec!["new_call"]);
        assert_eq!(cd.removed, vec!["old_call"]);
    }

    #[test]
    fn test_sig_none_to_some_is_changed() {
        let old = make_parse(
            vec![make_sym("thing", SymbolKind::Struct, None, 1, 3)],
            vec![],
        );
        let new = make_parse(
            vec![make_sym("thing", SymbolKind::Struct, Some("abc"), 1, 3)],
            vec![],
        );
        let changes = classify_symbols(&old, &new, "struct thing {}", "struct thing {}");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::SignatureChanged);
    }

    #[test]
    fn test_callee_diff_empty_when_calls_unchanged() {
        let source_old = "fn f() {\n  a()\n}";
        let source_new = "fn f() {\n  a()\n  let x = 1;\n}";
        let old = make_parse(
            vec![make_sym("f", SymbolKind::Function, Some("h"), 1, 2)],
            vec![make_ref("a", 2)],
        );
        let new = make_parse(
            vec![make_sym("f", SymbolKind::Function, Some("h"), 1, 3)],
            vec![make_ref("a", 2)],
        );
        let changes = classify_symbols(&old, &new, source_old, source_new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::BodyChanged);
        assert!(changes[0].callee_diff.is_none());
    }

    #[test]
    fn test_struct_and_impl_not_conflated() {
        let source = "struct Foo {}\nimpl Foo {\n  fn bar() {}\n}";
        let old = make_parse(
            vec![
                make_sym("Foo", SymbolKind::Struct, None, 1, 1),
                make_sym("Foo", SymbolKind::Impl, None, 2, 4),
            ],
            vec![],
        );
        let new_source = "struct Foo { x: i32 }\nimpl Foo {\n  fn bar() {}\n}";
        let new = make_parse(
            vec![
                make_sym("Foo", SymbolKind::Struct, None, 1, 1),
                make_sym("Foo", SymbolKind::Impl, None, 2, 4),
            ],
            vec![],
        );
        let changes = classify_symbols(&old, &new, source, new_source);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, "struct");
        assert_eq!(changes[0].change, ChangeKind::BodyChanged);
    }

    #[test]
    fn test_cosmetic_change_reformat() {
        let source_old = "fn calc() {\n  1 + 1\n}";
        let source_new = "fn calc() {\n    1 + 1\n}";
        let old = make_parse(
            vec![make_sym_full(
                "calc",
                SymbolKind::Function,
                Some("same"),
                Some("structural_a"),
                1,
                3,
            )],
            vec![],
        );
        let new = make_parse(
            vec![make_sym_full(
                "calc",
                SymbolKind::Function,
                Some("same"),
                Some("structural_a"),
                1,
                3,
            )],
            vec![],
        );
        let changes = classify_symbols(&old, &new, source_old, source_new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::CosmeticChanged);
        assert!(changes[0].callee_diff.is_none());
    }

    #[test]
    fn test_structural_hash_differs_is_body_changed() {
        let source_old = "fn calc() {\n  1 + 1\n}";
        let source_new = "fn calc() {\n  2 + 2\n}";
        let old = make_parse(
            vec![make_sym_full(
                "calc",
                SymbolKind::Function,
                Some("same"),
                Some("structural_a"),
                1,
                3,
            )],
            vec![],
        );
        let new = make_parse(
            vec![make_sym_full(
                "calc",
                SymbolKind::Function,
                Some("same"),
                Some("structural_b"),
                1,
                3,
            )],
            vec![],
        );
        let changes = classify_symbols(&old, &new, source_old, source_new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::BodyChanged);
    }

    #[test]
    fn test_cosmetic_falls_back_when_no_structural_hash() {
        let source_old = "fn f() {\n  1\n}";
        let source_new = "fn f() {\n    1\n}";
        let old = make_parse(
            vec![make_sym("f", SymbolKind::Function, Some("h"), 1, 3)],
            vec![],
        );
        let new = make_parse(
            vec![make_sym("f", SymbolKind::Function, Some("h"), 1, 3)],
            vec![],
        );
        let changes = classify_symbols(&old, &new, source_old, source_new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::BodyChanged);
    }
}
