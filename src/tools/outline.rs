use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::{Db, SymbolRow};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OutlineArgs {
    #[serde(default)]
    pub workspace: String,
    #[serde(alias = "file")]
    pub path: String,
    /// If true, drop signatures too and return only structural fields
    /// (qualified_name, kind, line range, visibility). Leanest tier.
    #[serde(default)]
    pub compact: Option<bool>,
    /// If true, include the heavy fields — docstrings, complexity metrics,
    /// short_name, and parent_symbol_id. Off by default; takes precedence over
    /// `compact` if both are set.
    #[serde(default)]
    pub verbose: Option<bool>,
}

/// How much per-symbol detail an outline emits. Ordered cheapest to richest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OutlineDetail {
    /// Structural fields only: qualified_name, kind, line range, visibility.
    Minimal,
    /// Structural + signature. The default: the shape of every symbol without
    /// fetching bodies or doc prose.
    Signatures,
    /// Everything: adds docstring, complexity metrics, short_name, parent id.
    Full,
}

impl OutlineDetail {
    /// Resolve the tier from the two request flags. `verbose` wins over
    /// `compact` when both are set; neither set means the `Signatures` default.
    pub fn from_flags(compact: Option<bool>, verbose: Option<bool>) -> Self {
        if verbose.unwrap_or(false) {
            Self::Full
        } else if compact.unwrap_or(false) {
            Self::Minimal
        } else {
            Self::Signatures
        }
    }
}

use crate::error::{Result, SutraError};

/// The signature sutra_outline renders at its default (Signatures) tier, or
/// None when the symbol has none (many modules, consts, and some fields carry
/// no signature). Source-formatting whitespace — the line breaks and
/// indentation of a multi-line declaration — is collapsed to single spaces so
/// the signature reads on one line in a JSON field or a symbol TOC, with no
/// loss of meaning. Exposed so sutra_explore presents the identical string and
/// the two tools can never drift apart (sutra/389).
pub fn rendered_signature(sym: &SymbolRow) -> Option<String> {
    let sig = sym.signature.as_deref()?;
    Some(sig.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub fn handle(db: &Db, path: &str, detail: OutlineDetail) -> Result<serde_json::Value> {
    let file = db.file_by_path(path)?.ok_or_else(|| SutraError::NotFound {
        tool: "sutra_outline",
        kind: format!("file `{path}`"),
        next_action: "Check the path and try again. Use sutra_map to list available files."
            .to_string(),
    })?;

    let symbols = db.find_symbols_by_file(file.id)?;

    let items: Vec<_> = symbols
        .iter()
        .map(|s| {
            // Structural base is present at every tier.
            let mut entry = json!({
                "qualified_name": s.qualified_name,
                "kind": s.kind,
                "start_line": s.start_line,
                "end_line": s.end_line,
                "visibility": s.visibility,
            });
            if detail >= OutlineDetail::Signatures {
                entry["signature"] = json!(rendered_signature(s));
            }
            if detail == OutlineDetail::Full {
                entry["short_name"] = json!(s.short_name);
                entry["parent_symbol_id"] = json!(s.parent_symbol_id);
                entry["docstring"] = json!(s.docstring);
                if let Some(c) = s.cyclomatic {
                    entry["cyclomatic"] = json!(c);
                }
                if let Some(c) = s.cognitive {
                    entry["cognitive"] = json!(c);
                }
            }
            entry
        })
        .collect();

    Ok(json!({
        "path": file.path,
        "language": file.language,
        "symbols": items,
        "total": items.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::{OutlineArgs, OutlineDetail};

    #[test]
    fn detail_defaults_to_signatures() {
        assert_eq!(
            OutlineDetail::from_flags(None, None),
            OutlineDetail::Signatures
        );
    }

    #[test]
    fn compact_selects_minimal() {
        assert_eq!(
            OutlineDetail::from_flags(Some(true), None),
            OutlineDetail::Minimal
        );
    }

    #[test]
    fn verbose_selects_full_and_wins_over_compact() {
        assert_eq!(
            OutlineDetail::from_flags(None, Some(true)),
            OutlineDetail::Full
        );
        assert_eq!(
            OutlineDetail::from_flags(Some(true), Some(true)),
            OutlineDetail::Full
        );
    }

    #[test]
    fn file_alias_deserializes_to_path() {
        // Agents intuitively reach for `file:`; the serde alias must accept it.
        let args: OutlineArgs =
            serde_json::from_value(serde_json::json!({ "file": "src/lib.rs" })).unwrap();
        assert_eq!(args.path, "src/lib.rs");

        let args: OutlineArgs =
            serde_json::from_value(serde_json::json!({ "path": "src/lib.rs" })).unwrap();
        assert_eq!(args.path, "src/lib.rs");
    }
}
