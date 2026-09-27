use super::codebook::Codebook;
use super::hrr::{self, HrrVec};
use crate::parser::adapter::node_text;

const MAX_DEPTH: usize = 20;

fn is_operator(kind: &str) -> bool {
    !kind.is_empty() && kind.chars().all(|c| "+-*/%=!<>&|^~?".contains(c))
}

/// Wrapper kinds that add nesting without adding structure. Dart puts a
/// method's `function_signature` under `method_signature`; a top-level
/// function has the `function_signature` directly, so encoding the wrapper
/// would split the same signature into unrelated subspaces (sutra/471).
const TRANSPARENT_WRAPPERS: &[&str] = &["method_signature"];

/// Encode a symbol's node. The root's own kind is not bound in: it only
/// records the declaration form (Dart `method_declaration` vs
/// `function_declaration`, JS `method_definition` vs `function_declaration`),
/// and binding it makes a method and the free helper extracted from it
/// near-orthogonal despite identical bodies (sutra/471).
pub fn encode_subtree(
    node: &tree_sitter::Node,
    source: &[u8],
    codebook: &mut Codebook,
    embed_idents: bool,
) -> HrrVec {
    match encode_children(node, source, codebook, MAX_DEPTH, embed_idents) {
        Some(bundled) => bundled,
        None => encode_recursive(node, source, codebook, MAX_DEPTH, embed_idents),
    }
}

fn sole_named_child<'t>(node: &tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
    if node.named_child_count() != 1 {
        return None;
    }
    node.named_child(0)
}

fn encode_recursive(
    node: &tree_sitter::Node,
    source: &[u8],
    codebook: &mut Codebook,
    depth: usize,
    embed_idents: bool,
) -> HrrVec {
    let kind_vec = codebook.get_or_create(node.kind());

    if depth == 0 || node.child_count() == 0 {
        if embed_idents && (node.kind() == "identifier" || node.kind() == "type_identifier") {
            let text = node_text(*node, source);
            let name_vec = codebook.get_or_create(&format!("id:{text}"));
            return kind_vec.bind(&name_vec);
        }
        return kind_vec;
    }

    if TRANSPARENT_WRAPPERS.contains(&node.kind())
        && let Some(inner) = sole_named_child(node)
    {
        return encode_recursive(&inner, source, codebook, depth, embed_idents);
    }

    match encode_children(node, source, codebook, depth, embed_idents) {
        Some(bundled) => kind_vec.bind(&bundled),
        None => kind_vec,
    }
}

/// Bundle of a node's named children and operator tokens. Each child enters
/// twice: unpositioned, and permuted by its position among siblings. The
/// positional half alone makes one inserted statement shift every later
/// sibling into an unrelated subspace (sutra/503); the unpositioned half keeps
/// the shared children comparable at the cost of weaker order sensitivity.
/// `None` when the node has nothing to bundle.
fn encode_children(
    node: &tree_sitter::Node,
    source: &[u8],
    codebook: &mut Codebook,
    depth: usize,
    embed_idents: bool,
) -> Option<HrrVec> {
    if depth == 0 || node.child_count() == 0 {
        return None;
    }
    let mut bag = Vec::with_capacity(node.child_count());
    let mut positioned = Vec::with_capacity(node.child_count());
    for i in 0..node.child_count() {
        let child = node
            .child(i)
            .expect("invariant: index is below child_count");
        let child_enc = if child.is_named() {
            encode_recursive(&child, source, codebook, depth - 1, embed_idents)
        } else if is_operator(child.kind()) {
            codebook.get_or_create(child.kind())
        } else {
            continue;
        };
        positioned.push(child_enc.permute(positioned.len() + 1));
        bag.push(child_enc);
    }
    if bag.is_empty() {
        return None;
    }
    Some(hrr::bundle(&[hrr::bundle(&bag), hrr::bundle(&positioned)]))
}
