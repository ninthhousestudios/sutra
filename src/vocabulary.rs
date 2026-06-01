use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::db::Db;
use crate::error::{Result, SutraError};

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
    let path = root.join(".sutra/aliases.toml");
    if !path.exists() {
        return Ok(AliasConfig::default());
    }
    let content =
        std::fs::read_to_string(&path).map_err(|e| SutraError::Internal(format!("{e}")))?;
    parse_aliases(&content)
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

pub fn sync_aliases(db: &Db, root: &Path) -> Result<usize> {
    let config = load_aliases(root)?;
    let mut tuples: Vec<(String, String, String, String)> = Vec::new();

    for (term, target_ref) in &config.component {
        tuples.push((
            Uuid::now_v7().to_string(),
            term.clone(),
            "component".into(),
            target_ref.clone(),
        ));
    }
    for (term, target_ref) in &config.file {
        tuples.push((
            Uuid::now_v7().to_string(),
            term.clone(),
            "file".into(),
            target_ref.clone(),
        ));
    }
    for (term, target_ref) in &config.symbol {
        tuples.push((
            Uuid::now_v7().to_string(),
            term.clone(),
            "symbol".into(),
            target_ref.clone(),
        ));
    }

    let count = tuples.len();
    db.replace_all_aliases(&tuples)?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ResolveMatch {
    pub source: String,
    pub target_kind: String,
    pub target_ref: String,
    pub component_id: Option<String>,
    pub orphan: bool,
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
        matches.push(ResolveMatch {
            source: "alias".into(),
            target_kind: alias.target_kind,
            target_ref: alias.target_ref,
            component_id,
            orphan,
        });
    }

    // Priority 2: component name (case-insensitive substring)
    let components = db.all_components()?;
    for c in &components {
        if c.name.to_lowercase().contains(&query_lower) {
            matches.push(ResolveMatch {
                source: "component".into(),
                target_kind: "component".into(),
                target_ref: c.name.clone(),
                component_id: Some(c.id.clone()),
                orphan: false,
            });
        }
    }

    // Priority 3: anchor name (case-insensitive substring)
    let all_anchors = db.all_anchors_grouped()?;
    for (comp_id, anchors) in &all_anchors {
        for a in anchors {
            if a.symbol_name.to_lowercase().contains(&query_lower) {
                matches.push(ResolveMatch {
                    source: "anchor".into(),
                    target_kind: "symbol".into(),
                    target_ref: a.symbol_name.clone(),
                    component_id: Some(comp_id.clone()),
                    orphan: false,
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
        json!({
            "source": m.source,
            "target_kind": m.target_kind,
            "target_ref": m.target_ref,
            "component_id": m.component_id,
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

fn check_orphan(db: &Db, target_kind: &str, target_ref: &str) -> Result<bool> {
    match target_kind {
        "component" => {
            let components = db.all_components()?;
            Ok(!components.iter().any(|c| c.name == target_ref || c.id == target_ref))
        }
        "file" => {
            let files = db.all_files()?;
            Ok(!files.iter().any(|f| f.path == target_ref))
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
