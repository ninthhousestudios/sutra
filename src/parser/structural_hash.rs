use std::hash::Hasher;

use tree_sitter::Node;
use xxhash_rust::xxh3::Xxh3;

/// Compute a structural hash of an AST node, excluding leaf tokens that overlap
/// the given byte range (typically the symbol's own name span). Strips comments
/// and normalizes whitespace so formatting-only changes produce identical hashes.
///
/// Ported from sem (MIT) — see NOTICE.
pub fn compute(node: Node, source: &[u8], exclude: Option<(usize, usize)>) -> String {
    let mut hasher = Xxh3::new();
    let (ex_start, ex_end) = exclude.unwrap_or((usize::MAX, usize::MAX));
    hash_tokens(node, source, &mut hasher, ex_start, ex_end);
    format!("{:016x}", hasher.finish())
}

fn hash_tokens(root: Node, source: &[u8], hasher: &mut Xxh3, ex_start: usize, ex_end: usize) {
    let mut worklist = vec![root];
    let mut cursor = root.walk();
    while let Some(node) = worklist.pop() {
        let kind = node.kind();

        if is_comment_node(kind) {
            continue;
        }

        if node.child_count() == 0 {
            let start = node.start_byte();
            let end = node.end_byte();
            if start < ex_end && end > ex_start {
                continue;
            }
            if start < end && end <= source.len() {
                let trimmed = trim_bytes(&source[start..end]);
                if !trimmed.is_empty() {
                    hasher.write(trimmed);
                    hasher.write(b" ");
                }
            }
        } else {
            hasher.write(kind.as_bytes());
            hasher.write(b":");
            push_children_reversed(&mut cursor, node, &mut worklist);
        }
    }
}

#[inline]
fn push_children_reversed<'a>(
    cursor: &mut tree_sitter::TreeCursor<'a>,
    node: Node<'a>,
    worklist: &mut Vec<Node<'a>>,
) {
    let base = worklist.len();
    cursor.reset(node);
    if cursor.goto_first_child() {
        loop {
            worklist.push(cursor.node());
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    worklist[base..].reverse();
}

#[inline]
fn trim_bytes(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |p| p + 1);
    &bytes[start..end]
}

fn is_comment_node(kind: &str) -> bool {
    matches!(
        kind,
        "comment" | "line_comment" | "block_comment" | "doc_comment" | "tag_comment"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_rust(src: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        parser.parse(src, None).unwrap()
    }

    #[test]
    fn whitespace_change_same_hash() {
        let a = "fn foo(x: i32) { x + 1 }";
        let b = "fn  foo( x:  i32 )  {  x  +  1  }";
        let ta = parse_rust(a);
        let tb = parse_rust(b);
        assert_eq!(
            compute(ta.root_node(), a.as_bytes(), None),
            compute(tb.root_node(), b.as_bytes(), None),
        );
    }

    #[test]
    fn comment_change_same_hash() {
        let a = "fn foo() { 1 }";
        let b = "// added comment\nfn foo() { 1 }";
        let ta = parse_rust(a);
        let tb = parse_rust(b);
        assert_eq!(
            compute(ta.root_node(), a.as_bytes(), None),
            compute(tb.root_node(), b.as_bytes(), None),
        );
    }

    #[test]
    fn body_change_different_hash() {
        let a = "fn foo() { 1 }";
        let b = "fn foo() { 2 }";
        let ta = parse_rust(a);
        let tb = parse_rust(b);
        assert_ne!(
            compute(ta.root_node(), a.as_bytes(), None),
            compute(tb.root_node(), b.as_bytes(), None),
        );
    }

    #[test]
    fn name_exclusion_makes_renames_equal() {
        let a = "fn alpha() { 1 + 2 }";
        let b = "fn beta() { 1 + 2 }";
        let ta = parse_rust(a);
        let tb = parse_rust(b);

        let fn_a = ta.root_node().child(0).unwrap();
        let fn_b = tb.root_node().child(0).unwrap();
        let name_a = fn_a.child_by_field_name("name").unwrap();
        let name_b = fn_b.child_by_field_name("name").unwrap();

        let ha = compute(
            fn_a,
            a.as_bytes(),
            Some((name_a.start_byte(), name_a.end_byte())),
        );
        let hb = compute(
            fn_b,
            b.as_bytes(),
            Some((name_b.start_byte(), name_b.end_byte())),
        );
        assert_eq!(ha, hb);
    }

    #[test]
    fn hex_format_16_chars() {
        let src = "fn f() {}";
        let tree = parse_rust(src);
        let h = compute(tree.root_node(), src.as_bytes(), None);
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
