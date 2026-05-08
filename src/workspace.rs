use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, SutraError};

#[derive(Debug, Deserialize, Serialize)]
pub struct WorkspacesConfig {
    #[serde(default)]
    pub workspace: Vec<WorkspaceEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkspaceEntry {
    pub id: String,
    pub root: PathBuf,
    pub languages: Vec<String>,
}

/// Load workspace config from `path`. Returns an empty config if the file does
/// not exist yet.
pub fn load_workspaces(path: &Path) -> Result<WorkspacesConfig> {
    if !path.exists() {
        return Ok(WorkspacesConfig {
            workspace: Vec::new(),
        });
    }
    let raw = fs::read_to_string(path)?;
    let config: WorkspacesConfig = toml::from_str(&raw).map_err(|e| {
        SutraError::Parse(format!(
            "failed to parse workspaces config at {}: {e}",
            path.display()
        ))
    })?;
    Ok(config)
}

/// Serialize `config` to TOML and write it to `path`, creating parent
/// directories as needed. The write is atomic: we write to a temp file in the
/// same directory and then rename.
pub fn save_workspaces(path: &Path, config: &WorkspacesConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let serialized = toml::to_string_pretty(config)
        .map_err(|e| SutraError::Internal(format!("failed to serialize workspaces config: {e}")))?;

    // Atomic write: temp file → rename
    let tmp_path = path.with_extension("toml.tmp");
    fs::write(&tmp_path, serialized.as_bytes())?;
    fs::rename(&tmp_path, path)?;

    Ok(())
}

/// Find a workspace entry by id in an already-loaded config.
pub fn resolve_workspace<'a>(config: &'a WorkspacesConfig, id: &str) -> Result<&'a WorkspaceEntry> {
    config
        .workspace
        .iter()
        .find(|w| w.id == id)
        .ok_or_else(|| SutraError::NotFound {
            tool: "workspaces",
            kind: format!("workspace '{id}'"),
            next_action: "List workspaces with 'sutra workspaces list'".to_string(),
        })
}

/// Add a new workspace entry to the config file. Returns an error if an entry
/// with the same id already exists.
pub fn add_workspace(path: &Path, entry: WorkspaceEntry) -> Result<()> {
    let mut config = load_workspaces(path)?;
    let id = &entry.id;
    if config.workspace.iter().any(|w| &w.id == id) {
        return Err(SutraError::InvalidArgument {
            tool: "workspaces",
            argument: "id",
            constraint: format!("workspace id must be unique, '{id}' already exists"),
            received: Some(id.to_string()),
            next_action: "Choose a different workspace id.".to_string(),
        });
    }
    config.workspace.push(entry);
    save_workspaces(path, &config)
}

/// Remove the workspace with the given id. Returns an error if it is not
/// found.
pub fn remove_workspace(path: &Path, id: &str) -> Result<()> {
    let mut config = load_workspaces(path)?;
    let before = config.workspace.len();
    config.workspace.retain(|w| w.id != id);
    if config.workspace.len() == before {
        return Err(SutraError::NotFound {
            tool: "workspaces",
            kind: format!("workspace '{id}'"),
            next_action: "List workspaces with 'sutra workspaces list'".to_string(),
        });
    }
    save_workspaces(path, &config)
}

/// Load and return all workspace entries.
pub fn list_workspaces(path: &Path) -> Result<Vec<WorkspaceEntry>> {
    Ok(load_workspaces(path)?.workspace)
}
