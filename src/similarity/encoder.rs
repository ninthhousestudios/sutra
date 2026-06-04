use super::codebook::Codebook;
use super::hrr::{self, HrrVec};

const MAX_DEPTH: usize = 20;

pub fn encode_subtree(
    node: &tree_sitter::Node,
    source: &[u8],
    codebook: &mut Codebook,
    embed_idents: bool,
) -> HrrVec {
    encode_recursive(node, source, codebook, MAX_DEPTH, embed_idents)
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
        if embed_idents
            && (node.kind() == "identifier" || node.kind() == "type_identifier")
        {
            if let Ok(text) = node.utf8_text(source) {
                let name_vec = codebook.get_or_create(&format!("id:{text}"));
                return kind_vec.bind(&name_vec);
            }
        }
        return kind_vec;
    }

    let mut child_vecs = Vec::with_capacity(node.child_count());
    let mut pos = 0usize;
    for i in 0..node.child_count() {
        let child = node.child(i).unwrap();
        if !child.is_named() {
            continue;
        }
        let child_enc = encode_recursive(&child, source, codebook, depth - 1, embed_idents);
        child_vecs.push(child_enc.permute(pos + 1));
        pos += 1;
    }

    if child_vecs.is_empty() {
        return kind_vec;
    }

    let bundled = hrr::bundle(&child_vecs);
    kind_vec.bind(&bundled)
}
