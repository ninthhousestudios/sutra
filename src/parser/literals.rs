//! String literal occurrences, stored in the index at parse time (sutra/494).
//!
//! The sibling-pattern advisory looks for literal lists and SQL prefixes that
//! survive a rewrite. Without an index it had to read every file of the tree
//! to learn which ones hold a literal; with this table a worktree review reads
//! only the files the index names. The stored form is an index key, not the
//! whole literal: normalized, then cut to [`PREFIX_CHARS`] characters, so an
//! exact match finds both a short list item and the prefix of a long query.

use std::sync::LazyLock;

use tree_sitter::Tree;

use super::adapter::node_text;

/// Characters of a normalized literal kept as its index key. The sibling
/// check's SQL prefix length, and no shorter than its longest list item.
pub const PREFIX_CHARS: usize = 40;

/// Literals this short (quotes included) are never a list item or a query.
const MIN_CHARS: usize = 3;

static LINE_CONTINUATION: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\\\s*\n\s*").expect("invariant: static regex compiles"));
static WHITESPACE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\s+").expect("invariant: static regex compiles"));

/// One string literal: its first line and index key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExtractedLiteral {
    pub line: usize,
    pub text: String,
}

/// Languages whose literals are indexed: the ones the sibling check reads.
/// Kept here, under the parser stamp, so widening it re-extracts every file
/// instead of leaving old rows that silently hold no literals.
pub fn indexed_language(language_id: &str) -> bool {
    matches!(language_id, "rust" | "dart")
}

/// Whether a tree-sitter node kind is a whole string literal (Rust and Dart).
pub fn is_string_literal(kind: &str) -> bool {
    matches!(kind, "string_literal" | "raw_string_literal")
}

/// A literal's source text with line continuations joined and whitespace runs
/// collapsed, so a query split across lines reads as one.
pub fn normalize(text: &str) -> String {
    let joined = LINE_CONTINUATION.replace_all(text, " ");
    WHITESPACE.replace_all(&joined, " ").into_owned()
}

/// The index key of a normalized literal.
pub fn index_key(normalized: &str) -> String {
    normalized.chars().take(PREFIX_CHARS).collect()
}

/// Every string literal in `tree` long enough to matter, deduplicated by line
/// and key. Test code is included: the index narrows, the reader confirms.
pub fn extract(tree: &Tree, src: &[u8]) -> Vec<ExtractedLiteral> {
    let mut out = Vec::new();
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        let whole = is_string_literal(node.kind());
        if whole {
            let text = normalize(node_text(node, src));
            if text.chars().count() >= MIN_CHARS {
                out.push(ExtractedLiteral {
                    line: node.start_position().row + 1,
                    text: index_key(&text),
                });
            }
        }
        if !whole && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                out.sort_unstable();
                out.dedup();
                return out;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::adapter::default_registry;

    fn literals(lang: &str, src: &str) -> Vec<(usize, String)> {
        let registry = default_registry();
        let adapter = registry
            .adapter_for_language(lang)
            .expect("invariant: rust and dart adapters are registered");
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&adapter.grammar())
            .expect("grammar loads");
        let tree = parser.parse(src, None).expect("fixture parses");
        extract(&tree, src.as_bytes())
            .into_iter()
            .map(|l| (l.line, l.text))
            .collect()
    }

    #[test]
    fn rust_keys_are_normalized_and_cut() {
        let src = "fn f() {\n    let a = [\"dart\", \"\", r#\"rust\"#];\n    let q = \"SELECT id, constraint_id, \\\n        constraint_name, more FROM t\";\n}\n";
        assert_eq!(
            literals("rust", src),
            vec![
                (2, "\"dart\"".to_string()),
                (2, "r#\"rust\"#".to_string()),
                (3, "\"SELECT id, constraint_id, constraint_na".to_string()),
            ]
        );
    }

    #[test]
    fn dart_interpolated_string_is_one_literal() {
        let src = "void f(String x) {\n  final a = ['draft', 'final'];\n  print('v: ${x}');\n}\n";
        assert_eq!(
            literals("dart", src),
            vec![
                (2, "'draft'".to_string()),
                (2, "'final'".to_string()),
                (3, "'v: ${x}'".to_string()),
            ]
        );
    }
}
