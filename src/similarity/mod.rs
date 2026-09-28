pub mod codebook;
pub mod diff;
pub mod duplicates;
pub mod encoder;
pub mod hrr;
pub mod minhash;
pub mod search;

use std::borrow::Borrow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use tracing::{info, warn};

use crate::db::{Db, HrrSymbolRow};
use crate::error::{Result, SutraError};
use crate::parser::adapter::{LanguageRegistry, default_registry};

/// Skip HRR encoding for symbols spanning more than this many lines. A giant
/// decompiled function (10k-15k lines as a SINGLE symbol) forces
/// `encode_subtree` to recurse its entire tree-sitter AST, a hard RSS/CPU spike
/// with no similarity payoff — such functions are unique boilerplate, not
/// members of a pattern family (sutra/324).
pub(crate) const MAX_HRR_SYMBOL_LINES: i64 = 2_000;

/// (symbol_id, mode, quantized vector blob) rows destined for `hrr_vectors`.
type VectorRow = (i64, String, Vec<u8>);

/// In auto mode, workspaces above this many function symbols downgrade to
/// strip-only: embed vectors serve only `sutra_similar` (modes dup and embed)
/// and the dup-exists advisory (families, components, and diff use strip), and
/// at this scale a brute-force embed scan is degraded anyway — halving storage is the better trade
/// (sutra/327, cap-at-source precedent from sutra/324).
const AUTO_STRIP_ONLY_SYMBOL_THRESHOLD: i64 = 200_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimilarityMode {
    /// strip + embed vectors for every function symbol.
    Full,
    /// strip vectors only — pattern families and diff keep full fidelity,
    /// `sutra_similar mode=embed` becomes unavailable, and mode=dup and the
    /// dup-exists advisory encode embed in memory up to a cap, reporting the
    /// rest `incomplete`.
    StripOnly,
    /// No HRR encoding at all; existing vectors and families are left as-is.
    Off,
}

impl SimilarityMode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "full" => Some(Self::Full),
            "strip-only" => Some(Self::StripOnly),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// Resolve the effective mode: explicit `SUTRA_SIMILARITY_MODE` wins; the
/// default (`auto`) downgrades to strip-only above the symbol threshold.
fn effective_similarity_mode(db: &Db) -> Result<SimilarityMode> {
    let (mode, downgraded) = resolve_similarity_mode(db)?;
    if let Some(fn_count) = downgraded {
        warn!(
            fn_count,
            threshold = AUTO_STRIP_ONLY_SYMBOL_THRESHOLD,
            "similarity: large workspace — downgrading to strip-only HRR \
             (set SUTRA_SIMILARITY_MODE=full to override)"
        );
    }
    Ok(mode)
}

/// The effective mode, and the function count when `auto` downgraded to
/// strip-only on it. Does not log the downgrade: the query path (sutra/510)
/// resolves the mode per request.
pub(crate) fn resolve_similarity_mode(db: &Db) -> Result<(SimilarityMode, Option<i64>)> {
    // swallow: an unset or non-UTF-8 variable means auto, the default.
    match std::env::var("SUTRA_SIMILARITY_MODE").ok().as_deref() {
        None | Some("auto") | Some("") => {}
        Some(other) => match SimilarityMode::parse(other) {
            Some(mode) => return Ok((mode, None)),
            None => {
                warn!(
                    value = other,
                    "SUTRA_SIMILARITY_MODE not one of full|strip-only|off|auto — using auto"
                );
            }
        },
    }
    let fn_count = db.function_symbol_count()?;
    if fn_count > AUTO_STRIP_ONLY_SYMBOL_THRESHOLD {
        Ok((SimilarityMode::StripOnly, Some(fn_count)))
    } else {
        Ok((SimilarityMode::Full, None))
    }
}

pub fn compute_hrr_vectors(db: &Db, workspace_root: &Path) -> Result<(usize, bool)> {
    let changed_files = db.files_needing_hrr_recompute()?;
    if changed_files.is_empty() {
        return Ok((0, false));
    }

    let mode = effective_similarity_mode(db)?;
    if mode == SimilarityMode::Off {
        // Intentionally return WITHOUT recording hrr_file_hashes: leaving these
        // files "unrecomputed" is what lets a later switch back to full/strip
        // trigger a real recompute instead of treating stale files as done
        // (sutra/328). The repeated no-op re-entry per parse is the cost of that.
        info!("similarity: HRR disabled (mode=off)");
        return Ok((0, false));
    }
    if mode == SimilarityMode::StripOnly && db.has_embed_vectors()? {
        // Embed vectors from before the downgrade would leave `sutra_similar
        // mode=embed` scanning a partial corpus — drop them so the feature is
        // cleanly unavailable instead of quietly wrong. Gated on has_embed_vectors
        // so steady-state strip-only parses don't re-run this each time (sutra/328).
        let dropped = db.delete_embed_vectors()?;
        if dropped > 0 {
            info!(
                dropped,
                "similarity: removed embed vectors (strip-only mode)"
            );
        }
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

    let file_id_to_hash: HashMap<i64, &str> = changed_files
        .iter()
        .map(|f| (f.file_id, f.content_hash.as_str()))
        .collect();

    let mut vectors: Vec<VectorRow> = Vec::new();
    let mut completed_file_ids: Vec<i64> = Vec::new();
    for_each_file_parallel(&symbols, |registry, indices, cb| {
        let mut out = Vec::new();
        let done = encode_file(
            workspace_root,
            registry,
            &symbols,
            indices,
            mode,
            cb,
            &mut out,
        )?;
        Ok((out, done))
    })?
    .into_iter()
    .for_each(|(v, done)| {
        vectors.extend(v);
        completed_file_ids.extend(done);
    });

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

fn hrr_worker_count(file_count: usize) -> usize {
    let default = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    std::env::var("SUTRA_HRR_PARALLELISM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
        .clamp(1, file_count.max(1))
}

/// Run `work` over `symbols` grouped by file, in parallel. Encoding is
/// embarrassingly parallel now that the codebook is content-addressed
/// (sutra/327): each worker gets its own memo cache and produces identical
/// vectors regardless of scheduling. Workers pull file indices from a shared
/// counter so a few giant files don't skew a static partition.
fn for_each_file_parallel<S: Borrow<HrrSymbolRow> + Sync, T: Send>(
    symbols: &[S],
    work: impl Fn(&LanguageRegistry, &[usize], &mut codebook::Codebook) -> Result<T> + Sync,
) -> Result<Vec<T>> {
    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, sym) in symbols.iter().enumerate() {
        by_file.entry(&sym.borrow().file_path).or_default().push(i);
    }
    let files: Vec<Vec<usize>> = by_file.into_values().collect();
    let n_workers = hrr_worker_count(files.len());
    let next = AtomicUsize::new(0);
    let worker_results: Vec<Result<Vec<T>>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..n_workers)
            .map(|_| {
                s.spawn(|| {
                    let registry = default_registry();
                    let mut cb = codebook::Codebook::new();
                    let mut out = Vec::new();
                    while let Some(indices) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                        out.push(work(&registry, indices, &mut cb)?);
                    }
                    Ok(out)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("invariant: HRR encode worker panicked"))
            .collect()
    });
    let mut out = Vec::new();
    for r in worker_results {
        out.extend(r?);
    }
    Ok(out)
}

/// One file's source and tree, as the encoder reads it: `None` when the file
/// is unreadable, in an unknown language, or unparseable.
fn parse_for_hrr(
    workspace_root: &Path,
    registry: &LanguageRegistry,
    path: &str,
    language: &str,
) -> Result<Option<(String, tree_sitter::Tree)>> {
    // swallow: an unreadable file is skipped, not recorded done, so the next parse retries it
    let Ok(source) = std::fs::read_to_string(workspace_root.join(path)) else {
        return Ok(None);
    };
    let Some(adapter) = registry.adapter_for_language(language) else {
        return Ok(None);
    };
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&adapter.grammar())
        .map_err(|e| SutraError::Parse(format!("HRR re-parse grammar: {e}")))?;
    Ok(parser.parse(&source, None).map(|tree| (source, tree)))
}

/// The node `sym` spans, unless it is too long to encode.
fn symbol_node<'t>(
    tree: &'t tree_sitter::Tree,
    sym: &HrrSymbolRow,
) -> Option<tree_sitter::Node<'t>> {
    if sym.end_line - sym.start_line > MAX_HRR_SYMBOL_LINES {
        return None;
    }
    let start = tree_sitter::Point::new((sym.start_line - 1) as usize, sym.start_col as usize);
    let end = tree_sitter::Point::new((sym.end_line - 1) as usize, sym.end_col as usize);
    tree.root_node().descendant_for_point_range(start, end)
}

/// Encode all eligible symbols of one file. Returns the file id when the file
/// was fully processed (so its content hash may be recorded), `None` when the
/// file was skipped (unreadable, unknown language, or unparseable) — a skip
/// must NOT mark the file done or it would never be retried.
fn encode_file(
    workspace_root: &Path,
    registry: &LanguageRegistry,
    symbols: &[HrrSymbolRow],
    indices: &[usize],
    mode: SimilarityMode,
    cb: &mut codebook::Codebook,
    vectors: &mut Vec<VectorRow>,
) -> Result<Option<i64>> {
    let first = &symbols[indices[0]];
    let Some((source, tree)) =
        parse_for_hrr(workspace_root, registry, &first.file_path, &first.language)?
    else {
        return Ok(None);
    };
    for &idx in indices {
        let sym = &symbols[idx];
        if let Some(node) = symbol_node(&tree, sym) {
            let strip = encoder::encode_subtree(&node, source.as_bytes(), cb, false);
            vectors.push((sym.symbol_id, "strip".into(), strip.to_bytes()));

            if mode == SimilarityMode::Full {
                let embed = encoder::encode_subtree(&node, source.as_bytes(), cb, true);
                vectors.push((sym.symbol_id, "embed".into(), embed.to_bytes()));
            }
        }
    }
    Ok(Some(first.file_id))
}

/// Embed vectors for `symbols`, encoded from the worktree without touching
/// the index: the review-time dup check needs them for files an incremental
/// refresh re-indexed, whose stored vectors wait for the next full parse.
/// Each vector goes through the storage quantization so it compares like a
/// stored one. A symbol whose file cannot be read or parsed, or that is too
/// long to encode, is absent from the result.
pub fn encode_embed_vectors(
    workspace_root: &Path,
    symbols: &[&HrrSymbolRow],
) -> Result<HashMap<i64, hrr::HrrVec>> {
    let per_file = for_each_file_parallel(symbols, |registry, indices, cb| {
        let first = symbols[indices[0]];
        let mut out = Vec::new();
        if let Some((source, tree)) =
            parse_for_hrr(workspace_root, registry, &first.file_path, &first.language)?
        {
            for &idx in indices {
                let sym = symbols[idx];
                if let Some(node) = symbol_node(&tree, sym) {
                    let embed = encoder::encode_subtree(&node, source.as_bytes(), cb, true);
                    out.push((sym.symbol_id, hrr::HrrVec::from_bytes(&embed.to_bytes())));
                }
            }
        }
        Ok(out)
    })?;
    Ok(per_file.into_iter().flatten().collect())
}

pub fn compute_pattern_families(db: &Db) -> Result<usize> {
    let mut families = Vec::new();

    let vectors = db.load_all_vectors_by_mode("strip")?;
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

#[cfg(test)]
mod tests {
    use super::SimilarityMode;

    #[test]
    fn mode_parse_known_values() {
        assert_eq!(SimilarityMode::parse("full"), Some(SimilarityMode::Full));
        assert_eq!(
            SimilarityMode::parse("strip-only"),
            Some(SimilarityMode::StripOnly)
        );
        assert_eq!(SimilarityMode::parse("off"), Some(SimilarityMode::Off));
    }

    #[test]
    fn mode_parse_rejects_unknown() {
        assert_eq!(SimilarityMode::parse("strip"), None);
        assert_eq!(SimilarityMode::parse("auto"), None);
        assert_eq!(SimilarityMode::parse(""), None);
    }
}
