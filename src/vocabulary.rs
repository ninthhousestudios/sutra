use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::db::Db;
use crate::error::Result;
use crate::error::SutraError;

// ---------------------------------------------------------------------------
// TOML config
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct AliasConfig {
    #[serde(default)]
    pub component: HashMap<String, String>,
    #[serde(default)]
    pub file: HashMap<String, String>,
    #[serde(default)]
    pub symbol: HashMap<String, String>,
}

pub fn parse_aliases(content: &str) -> Result<AliasConfig> {
    toml::from_str(content)
        .map_err(|e| SutraError::Internal(format!("aliases.toml parse error: {e}")))
}

pub fn load_aliases(root: &Path) -> Result<AliasConfig> {
    parse_aliases(&read_aliases_source(root)?)
}

/// Raw contents of `.sutra/aliases.toml`, or the empty string when the file is
/// absent (an absent file is a valid "no aliases" state, not an error). Kept
/// separate from parsing so callers can hash the exact bytes the projection was
/// built from.
fn read_aliases_source(root: &Path) -> Result<String> {
    let path = root.join(".sutra/aliases.toml");
    if !path.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(&path).map_err(|e| SutraError::Internal(format!("{e}")))
}

/// Content hash of `.sutra/aliases.toml` as it currently sits on disk. Cheap
/// (a read + blake3) and stable across runs, so startup can decide whether the
/// persisted `aliases` projection is stale without a parse.
pub fn current_aliases_hash(root: &Path) -> Result<String> {
    Ok(blake3::hash(read_aliases_source(root)?.as_bytes())
        .to_hex()
        .to_string())
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

/// Re-project `.sutra/aliases.toml` into the `aliases` table only when the file
/// has changed since the last projection (tracked by the `alias_sync` marker).
/// Returns `Some(count)` when a sync ran, `None` when the projection was already
/// fresh. This is the cheap path that keeps forward alias resolution alive on
/// startups that skip a full parse (frozen indexes, or fresh indexes whose
/// source didn't change).
pub fn sync_aliases_if_changed(db: &Db, root: &Path) -> Result<Option<usize>> {
    let current = current_aliases_hash(root)?;
    if db.get_alias_sync_marker()?.as_deref() == Some(current.as_str()) {
        return Ok(None);
    }
    Ok(Some(sync_aliases(db, root)?))
}

pub fn sync_aliases(db: &Db, root: &Path) -> Result<usize> {
    let source = read_aliases_source(root)?;
    let config = parse_aliases(&source)?;
    let mut seen: HashMap<&str, &str> = HashMap::new();
    let mut tuples: Vec<(String, String, String, String)> = Vec::new();

    let sections: [(&str, &HashMap<String, String>); 3] = [
        ("component", &config.component),
        ("file", &config.file),
        ("symbol", &config.symbol),
    ];

    for (section, map) in &sections {
        for (term, target_ref) in *map {
            if let Some(prev) = seen.get(term.as_str()) {
                return Err(SutraError::Parse(format!(
                    "duplicate alias term '{}' appears in [{}] and [{}]",
                    term, prev, section
                )));
            }
            seen.insert(term.as_str(), section);
            tuples.push((
                Uuid::now_v7().to_string(),
                term.clone(),
                (*section).into(),
                target_ref.clone(),
            ));
        }
    }

    let count = tuples.len();
    let hash = blake3::hash(source.as_bytes()).to_hex().to_string();
    db.replace_all_aliases(&tuples, &hash)?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct CodeLocation {
    pub path: String,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
}

#[derive(Debug)]
pub struct ResolveMatch {
    pub source: String,
    pub target_kind: String,
    pub target_ref: String,
    pub component_id: Option<String>,
    pub orphan: bool,
    pub locations: Vec<CodeLocation>,
}

pub fn resolve(db: &Db, query: &str) -> Result<Vec<ResolveMatch>> {
    let mut matches = Vec::new();
    let query_lower = query.to_lowercase();

    // Priority 1: alias (exact match)
    if let Some(alias) = db.find_alias(query)? {
        let orphan = check_orphan(db, &alias.target_kind, &alias.target_ref)?;
        let component_id = if alias.target_kind == "component" {
            find_component_id(db, &alias.target_ref)?
        } else {
            None
        };
        let locations = lookup_locations(
            db,
            &alias.target_kind,
            &alias.target_ref,
            component_id.as_deref(),
        )?;
        matches.push(ResolveMatch {
            source: "alias".into(),
            target_kind: alias.target_kind,
            target_ref: alias.target_ref,
            component_id,
            orphan,
            locations,
        });
    }

    // Priority 2: component name (case-insensitive substring)
    let components = db.all_components()?;
    for c in &components {
        if c.name.to_lowercase().contains(&query_lower) {
            let locations = lookup_locations(db, "component", &c.name, Some(&c.id))?;
            matches.push(ResolveMatch {
                source: "component".into(),
                target_kind: "component".into(),
                target_ref: c.name.clone(),
                component_id: Some(c.id.clone()),
                orphan: false,
                locations,
            });
        }
    }

    // Priority 3: anchor name (case-insensitive substring)
    let all_anchors = db.all_anchors_grouped()?;
    for (comp_id, anchors) in &all_anchors {
        for a in anchors {
            if a.symbol_name.to_lowercase().contains(&query_lower) {
                let locations = lookup_locations(db, "symbol", &a.symbol_name, Some(comp_id))?;
                matches.push(ResolveMatch {
                    source: "anchor".into(),
                    target_kind: "symbol".into(),
                    target_ref: a.symbol_name.clone(),
                    component_id: Some(comp_id.clone()),
                    orphan: false,
                    locations,
                });
            }
        }
    }

    Ok(matches)
}

pub fn resolve_to_json(db: &Db, query: &str) -> Result<serde_json::Value> {
    let matches = resolve(db, query)?;
    let (orphans, valid): (Vec<_>, Vec<_>) = matches.into_iter().partition(|m| m.orphan);

    let format = |m: &ResolveMatch| {
        let locs: Vec<_> = m
            .locations
            .iter()
            .map(|l| {
                let mut loc = json!({"path": l.path});
                if let Some(sl) = l.start_line {
                    loc["start_line"] = json!(sl);
                }
                if let Some(el) = l.end_line {
                    loc["end_line"] = json!(el);
                }
                loc
            })
            .collect();
        json!({
            "source": m.source,
            "target_kind": m.target_kind,
            "target_ref": m.target_ref,
            "component_id": m.component_id,
            "locations": locs,
        })
    };

    Ok(json!({
        "query": query,
        "matches": valid.iter().map(format).collect::<Vec<_>>(),
        "orphans": orphans.iter().map(format).collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn lookup_locations(
    db: &Db,
    target_kind: &str,
    target_ref: &str,
    component_id: Option<&str>,
) -> Result<Vec<CodeLocation>> {
    match target_kind {
        "component" => {
            if let Some(cid) = component_id {
                let paths = db.component_file_paths(cid)?;
                Ok(paths
                    .into_iter()
                    .map(|p| CodeLocation {
                        path: p,
                        start_line: None,
                        end_line: None,
                    })
                    .collect())
            } else {
                Ok(vec![])
            }
        }
        "file" => Ok(vec![CodeLocation {
            path: target_ref.to_string(),
            start_line: None,
            end_line: None,
        }]),
        "symbol" => {
            let syms = db.find_symbols_by_name(target_ref, None, 5)?;
            let mut locs = Vec::new();
            for sym in &syms {
                if let Some(f) = db.file_by_id(sym.file_id)? {
                    locs.push(CodeLocation {
                        path: f.path.to_string(),
                        start_line: Some(sym.start_line),
                        end_line: Some(sym.end_line),
                    });
                }
            }
            Ok(locs)
        }
        _ => Ok(vec![]),
    }
}

fn check_orphan(db: &Db, target_kind: &str, target_ref: &str) -> Result<bool> {
    match target_kind {
        "component" => {
            let components = db.all_components()?;
            Ok(!components
                .iter()
                .any(|c| c.name == target_ref || c.id == target_ref))
        }
        "file" => {
            let files = db.all_files()?;
            Ok(!files.iter().any(|f| &*f.path == target_ref))
        }
        "symbol" => {
            let found = db.find_symbols_by_name(target_ref, None, 1)?;
            Ok(found.is_empty())
        }
        _ => Ok(true),
    }
}

fn find_component_id(db: &Db, name_or_id: &str) -> Result<Option<String>> {
    let components = db.all_components()?;
    for c in &components {
        if c.name == name_or_id || c.id == name_or_id {
            return Ok(Some(c.id.clone()));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_aliases_valid() {
        let toml = r#"
[component]
auth = "authentication"
db-layer = "db"

[file]
config = "src/config.rs"

[symbol]
UP = "UserProfile"
"#;
        let config = parse_aliases(toml).unwrap();
        assert_eq!(config.component.len(), 2);
        assert_eq!(config.component["auth"], "authentication");
        assert_eq!(config.component["db-layer"], "db");
        assert_eq!(config.file.len(), 1);
        assert_eq!(config.file["config"], "src/config.rs");
        assert_eq!(config.symbol.len(), 1);
        assert_eq!(config.symbol["UP"], "UserProfile");
    }

    #[test]
    fn test_parse_aliases_empty() {
        let config = parse_aliases("").unwrap();
        assert!(config.component.is_empty());
        assert!(config.file.is_empty());
        assert!(config.symbol.is_empty());
    }

    #[test]
    fn test_parse_aliases_partial() {
        let toml = r#"
[component]
auth = "authentication"
"#;
        let config = parse_aliases(toml).unwrap();
        assert_eq!(config.component.len(), 1);
        assert!(config.file.is_empty());
        assert!(config.symbol.is_empty());
    }

    #[test]
    fn test_parse_aliases_invalid_toml() {
        let result = parse_aliases("[invalid\ngarbage");
        assert!(result.is_err());
    }
}
