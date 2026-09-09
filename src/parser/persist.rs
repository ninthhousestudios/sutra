//! Extraction → persistence normalization (sutra/383).
//!
//! These functions map the parser's [`ParseResult`] (extractor output) into the
//! `db` insert-param rows a re-parse persists: the symbol-tree flattening, the
//! ref/import field mapping, and the per-file size caps. They shape *what a
//! re-parse of the same bytes writes*, so a change here changes extracted output
//! without touching any `src/parser/*` grammar or adapter.
//!
//! Keeping them under `src/parser/` is load-bearing. `build.rs` hashes every
//! `src/parser/**/*.rs` file into `PARSER_STAMP` (sutra/364), so this module is
//! covered and an edit here correctly forces one full re-extraction on the next
//! parse. Do **not** move extraction normalization back into `src/pipeline.rs`:
//! that file is deliberately *not* hashed (it is full of parse-orchestration and
//! DD code that must not rev the stamp on every unrelated edit), so relocating
//! this logic there would silently stop invalidating unchanged files — the exact
//! silent-staleness bug sutra/364 fixed, relocated. A `build.rs` assertion keeps
//! `flatten_symbols_dfs` inside the hashed tree.

use tracing::warn;

use crate::db::{InsertImportParams, InsertRefParams, InsertSymbolParams};
use crate::parser::{ExtractedSymbol, ParseResult};

/// Maximum lines per file — files larger than this are skipped with a warning.
pub(crate) const MAX_LINES: usize = 100_000;

/// Safety valve for pathological single files (e.g. Ghidra/BinaryNinja
/// decompiled functions with thousands of `var_XXXX` locals): cap the number of
/// references indexed per file. A single 15k-line decompiled function can emit
/// tens of thousands of refs; across a large corpus these dominate the
/// whole-corpus `all_resolved_refs` Vec that `GraphData::load` materializes,
/// which is the primary driver of reparse RSS (sutra/324). Real source files
/// never approach this bound, so it only truncates decompiled noise.
pub(crate) const MAX_REFS_PER_FILE: usize = 15_000;

fn flatten_symbols_dfs<'a>(
    symbols: &'a [ExtractedSymbol],
    parent_idx: Option<usize>,
    out: &mut Vec<InsertSymbolParams<'a>>,
    parents: &mut Vec<Option<usize>>,
) {
    for sym in symbols {
        let my_idx = out.len();
        out.push(InsertSymbolParams {
            file_id: 0, // filled by replace_file_data
            qualified_name: &sym.qualified_name,
            short_name: &sym.short_name,
            kind: sym.kind.as_str(),
            signature: sym.signature.as_deref(),
            signature_hash: sym.signature_hash.as_deref(),
            structural_hash: sym.structural_hash.as_deref(),
            visibility: sym.visibility.as_deref(),
            start_line: sym.start_line as i64,
            start_col: sym.start_col as i64,
            end_line: sym.end_line as i64,
            end_col: sym.end_col as i64,
            parent_symbol_id: None, // resolved via parent_indices
            docstring: sym.docstring.as_deref(),
            cyclomatic: sym.cyclomatic.map(|v| v as i64),
            cognitive: sym.cognitive.map(|v| v as i64),
            max_nesting: sym.max_nesting.map(|v| v as i64),
            flags: sym.flags as i64,
            language_attrs: sym.language_attrs.as_deref(),
        });
        parents.push(parent_idx);
        flatten_symbols_dfs(&sym.children, Some(my_idx), out, parents);
    }
}

/// Flatten the extracted symbol tree (depth-first) into insert rows plus a
/// parallel parent-index sidecar; `replace_file_data` resolves the indices into
/// `parent_symbol_id`s once the rows have ids.
pub(crate) fn flatten_symbols_for_insert(
    symbols: &[ExtractedSymbol],
) -> (Vec<InsertSymbolParams<'_>>, Vec<Option<usize>>) {
    let mut out = Vec::new();
    let mut parents = Vec::new();
    flatten_symbols_dfs(symbols, None, &mut out, &mut parents);
    (out, parents)
}

/// Map a parse result's imports into insert params.
pub(crate) fn build_import_params(result: &ParseResult) -> Vec<InsertImportParams<'_>> {
    result
        .imports
        .iter()
        .map(|imp| InsertImportParams {
            imported_path: &imp.raw_path,
            line: imp.line as i64,
            kind: imp.kind,
            alias: imp.alias.as_deref(),
            is_test: imp.is_test,
        })
        .collect()
}

/// Map a parse result's references into insert params, truncated to
/// [`MAX_REFS_PER_FILE`]. `rel_path` is used only for the truncation warning.
pub(crate) fn build_ref_params<'a>(
    result: &'a ParseResult,
    rel_path: &str,
) -> Vec<InsertRefParams<'a>> {
    if result.references.len() > MAX_REFS_PER_FILE {
        warn!(
            path = %rel_path,
            refs = result.references.len(),
            max = MAX_REFS_PER_FILE,
            "file exceeds per-file ref cap, truncating (pathological decompiled function?)"
        );
    }
    result
        .references
        .iter()
        .take(MAX_REFS_PER_FILE)
        .map(|rf| InsertRefParams {
            unresolved_name: Some(&rf.name),
            line: rf.line as i64,
            col: rf.col as i64,
            context_kind: rf.context_kind.as_str(),
            resolved_local_target: rf.resolved_local_target.as_deref(),
            receiver: rf.receiver.as_deref(),
        })
        .collect()
}
