//! What a diff changed, symbol by symbol: the hunk-scoped change set `review`
//! and `check` both report (sutra/517). Built from
//! [`crate::tools::symbol_diff::diff_files`] by `review::changed_symbols`,
//! so a member's change is reported on the member, not on every symbol of its
//! file or on its enclosing `impl`.

use std::collections::HashMap;

use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::tools::symbol_diff::{ChangeKind, DiffFilesResult, SymbolChange};

/// Longest changed-symbol list the human render prints before summarising.
const MAX_RENDERED: usize = 40;

/// One changed file's symbol changes.
pub struct FileChanges {
    pub path: String,
    pub changes: Vec<SymbolChange>,
    /// Per change, the index's cognitive complexity for the symbol on the
    /// head side. `None` for a deleted symbol, or one the index has no metric
    /// for.
    pub cognitive: Vec<Option<i64>>,
    /// Why this file's symbols could not be diffed, if they could not.
    pub error: Option<String>,
}

/// The symbol changes of every changed file, in diff order. A file with no
/// parseable language is listed with no changes.
pub struct ChangedSymbols {
    pub files: Vec<FileChanges>,
}

impl ChangedSymbols {
    /// Attach cognitive complexity to an already computed diff of `paths`.
    pub fn from_diff(db: &Db, paths: &[String], mut diff: DiffFilesResult) -> Result<Self> {
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let changes = diff.per_file.remove(path).unwrap_or_default();
            let rows = match db.file_by_path(path)? {
                Some(file) => db.find_symbols_by_file(file.id)?,
                None => Vec::new(),
            };
            let indexed: HashMap<(&str, &str), i64> = rows
                .iter()
                .filter_map(|s| Some(((&*s.qualified_name, &*s.kind), s.cognitive?)))
                .collect();
            let cognitive = changes
                .iter()
                .map(|c| match c.change {
                    ChangeKind::Deleted => None,
                    _ => indexed.get(&(c.symbol.as_str(), c.kind.as_str())).copied(),
                })
                .collect();
            files.push(FileChanges {
                path: path.to_string(),
                changes,
                cognitive,
                error: diff.errors.remove(path),
            });
        }
        Ok(Self { files })
    }

    /// Every change, with the file it is in and its cognitive complexity.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &SymbolChange, Option<i64>)> {
        self.files.iter().flat_map(|f| {
            f.changes
                .iter()
                .zip(&f.cognitive)
                .map(move |(c, cog)| (f.path.as_str(), c, *cog))
        })
    }

    /// `changed_symbols`: the changes flattened across files. The callee diff
    /// stays on the file's `symbol_changes`.
    pub fn to_json(&self) -> Vec<serde_json::Value> {
        self.iter()
            .map(|(file, c, cognitive)| {
                let mut entry = json!({
                    "symbol": c.symbol,
                    "kind": c.kind,
                    "change": c.change,
                    "file": file,
                    "cognitive": cognitive,
                });
                if let Some(from) = &c.from_symbol {
                    entry["from_symbol"] = json!(from);
                }
                if let Some(from) = &c.from_file {
                    entry["from_file"] = json!(from);
                }
                entry
            })
            .collect()
    }

    /// The human form of `changed_symbols`, as `sutra check` prints it.
    pub fn render(&self, out: &mut String) {
        use std::fmt::Write;
        let total = self.iter().count();
        if total > 0 {
            let _ = writeln!(out, "\n{total} changed symbol(s):");
        }
        for (file, c, cognitive) in self.iter().take(MAX_RENDERED) {
            let cognitive = cognitive
                .map(|n| format!("  cognitive {n}"))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "  {file}  {} ({})  {}{cognitive}",
                c.symbol,
                c.kind,
                c.change.as_str()
            );
        }
        if total > MAX_RENDERED {
            let _ = writeln!(out, "  … and {} more", total - MAX_RENDERED);
        }
        for f in &self.files {
            if let Some(e) = &f.error {
                let _ = writeln!(out, "  {}: symbols not diffed: {e}", f.path);
            }
        }
    }
}
