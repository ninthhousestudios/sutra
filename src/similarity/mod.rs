pub mod codebook;
pub mod diff;
pub mod duplicates;
pub mod encoder;
pub mod hrr;
pub mod minhash;
pub mod search;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::db::Db;
use crate::error::{Result, SutraError};
use crate::parser::adapter::default_registry;

/// Skip HRR encoding for symbols spanning more than this many lines. A giant
/// decompiled function (10k-15k lines as a SINGLE symbol) forces
/// `encode_subtree` to recurse its entire tree-sitter AST, a hard RSS/CPU spike
/// with no similarity payoff — such functions are unique boilerplate, not
/// members of a pattern family (sutra/324).
const MAX_HRR_SYMBOL_LINES: i64 = 2_000;

pub fn compute_hrr_vectors(db: &Db, workspace_root: &Path) -> Result<(usize, bool)> {
    let changed_files = db.files_needing_hrr_recompute()?;
    if changed_files.is_empty() {
        return Ok((0, false));
    }

    let file_ids: Vec<i64> = changed_files.iter().map(|f| f.file_id).collect();
    let symbols = db.function_symbols_for_hrr_files(&file_ids)?;

    if symbols.is_empty() {
        let file_hashes: Vec<(i64, &str)> = changed_files
            .iter()
            .map(|f| (f.file_id, f.content_hash.as_str()))
            .collect();
        db.insert_hrr_vectors_and_hashes(&[], &file_hashes)?;
        return Ok((0, true));
    }

    let mut cb = codebook::Codebook::new();

    let registry = default_registry();

    let file_id_to_hash: HashMap<i64, &str> = changed_files
        .iter()
        .map(|f| (f.file_id, f.content_hash.as_str()))
        .collect();

    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, sym) in symbols.iter().enumerate() {
        by_file.entry(&sym.file_path).or_default().push(i);
    }

    let mut vectors: Vec<(i64, String, Vec<u8>)> = Vec::new();
    let mut completed_file_ids: HashSet<i64> = HashSet::new();

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

        let file_id = symbols[indices[0]].file_id;

        for &idx in indices {
            let sym = &symbols[idx];
            if sym.end_line - sym.start_line > MAX_HRR_SYMBOL_LINES {
                continue;
            }
            let start =
                tree_sitter::Point::new((sym.start_line - 1) as usize, sym.start_col as usize);
            let end = tree_sitter::Point::new((sym.end_line - 1) as usize, sym.end_col as usize);
            if let Some(node) = tree.root_node().descendant_for_point_range(start, end) {
                let strip = encoder::encode_subtree(&node, source.as_bytes(), &mut cb, false);
                vectors.push((sym.symbol_id, "strip".into(), strip.to_bytes()));

                let embed = encoder::encode_subtree(&node, source.as_bytes(), &mut cb, true);
                vectors.push((sym.symbol_id, "embed".into(), embed.to_bytes()));
            }
        }

        completed_file_ids.insert(file_id);
    }

    let file_hashes: Vec<(i64, &str)> = completed_file_ids
        .iter()
        .filter_map(|fid| file_id_to_hash.get(fid).map(|h| (*fid, *h)))
        .collect();

    let vec_refs: Vec<(i64, &str, &[u8])> = vectors
        .iter()
        .map(|(id, mode, blob)| (*id, mode.as_str(), blob.as_slice()))
        .collect();
    db.insert_hrr_vectors_and_hashes(&vec_refs, &file_hashes)?;

    Ok((symbols.len(), true))
}

pub fn compute_pattern_families(db: &Db) -> Result<usize> {
    let mut families = Vec::new();

    let vectors = db.load_all_strip_vectors()?;
    if !vectors.is_empty() {
        families.extend(duplicates::find_pattern_families(&vectors, 0.85, 3));
    }

    let names = db.function_symbol_names()?;
    if !names.is_empty() {
        families.extend(duplicates::find_name_families(&names, 0.6, 3));
    }

    let count = families.len();
    db.replace_pattern_families(&families)?;
    Ok(count)
}
