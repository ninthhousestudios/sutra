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

/// Bundle of a node's named children and operator tokens, each permuted by
/// position. `None` when the node has nothing to bundle.
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
    let mut child_vecs = Vec::with_capacity(node.child_count());
    let mut pos = 0usize;
    for i in 0..node.child_count() {
        let child = node
            .child(i)
            .expect("invariant: index is below child_count");
        if !child.is_named() {
            if is_operator(child.kind()) {
                let op_vec = codebook.get_or_create(child.kind());
                child_vecs.push(op_vec.permute(pos + 1));
                pos += 1;
            }
            continue;
        }
        let child_enc = encode_recursive(&child, source, codebook, depth - 1, embed_idents);
        child_vecs.push(child_enc.permute(pos + 1));
        pos += 1;
    }
    if child_vecs.is_empty() {
        return None;
    }
    Some(hrr::bundle(&child_vecs))
}
