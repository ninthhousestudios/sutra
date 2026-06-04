pub mod codebook;
pub mod duplicates;
pub mod encoder;
pub mod hrr;

use std::collections::HashMap;
use std::path::Path;

use tracing::info;

use crate::db::Db;
use crate::error::{Result, SutraError};
use crate::parser::adapter::default_registry;

pub fn compute_hrr_vectors(db: &Db, workspace_root: &Path) -> Result<usize> {
    let symbols = db.function_symbols_for_hrr()?;
    if symbols.is_empty() {
        return Ok(0);
    }

    let existing = db.load_hrr_codebook()?;
    let mut cb = codebook::Codebook::from_entries(existing);

    let registry = default_registry();

    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, sym) in symbols.iter().enumerate() {
        by_file.entry(&sym.file_path).or_default().push(i);
    }

    let mut vectors: Vec<(i64, String, Vec<u8>)> = Vec::new();

    for (path, indices) in &by_file {
        let full_path = workspace_root.join(path);
        let source = match std::fs::read_to_string(&full_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let lang = &symbols[indices[0]].language;
        let adapter = match registry.adapter_for_language(lang) {
            Some(a) => a,
            None => continue,
        };

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&adapter.grammar())
            .map_err(|e| SutraError::Parse(format!("HRR re-parse grammar: {e}")))?;

        let tree = match parser.parse(&source, None) {
            Some(t) => t,
            None => continue,
        };

        for &idx in indices {
            let sym = &symbols[idx];
            // DB stores 1-indexed lines; tree-sitter Points are 0-indexed
            let start =
                tree_sitter::Point::new((sym.start_line - 1) as usize, sym.start_col as usize);
            let end = tree_sitter::Point::new((sym.end_line - 1) as usize, sym.end_col as usize);
            if let Some(node) = tree.root_node().descendant_for_point_range(start, end) {
                let strip =
                    encoder::encode_subtree(&node, source.as_bytes(), &mut cb, false);
                vectors.push((sym.symbol_id, "strip".into(), strip.to_bytes()));

                let embed =
                    encoder::encode_subtree(&node, source.as_bytes(), &mut cb, true);
                vectors.push((sym.symbol_id, "embed".into(), embed.to_bytes()));
            }
        }
    }

    let vec_refs: Vec<(i64, &str, &[u8])> = vectors
        .iter()
        .map(|(id, mode, blob)| (*id, mode.as_str(), blob.as_slice()))
        .collect();
    db.replace_hrr_vectors(&vec_refs)?;

    let new_entries = cb.into_new_entries();
    let new_count = db.save_hrr_codebook_entries(&new_entries)?;
    if new_count > 0 {
        info!(new_count, "new HRR codebook entries");
    }

    Ok(symbols.len())
}

pub fn compute_pattern_families(db: &Db) -> Result<usize> {
    let vectors = db.load_all_strip_vectors()?;
    if vectors.is_empty() {
        return Ok(0);
    }

    let families = duplicates::find_pattern_families(&vectors, 0.85, 3);
    let count = families.len();
    db.replace_pattern_families(&families)?;
    Ok(count)
}
