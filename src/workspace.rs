use std::fs;
use std::path::{Component, Path, PathBuf};

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

/// Reject DB directories that would place a workspace's index inside that
/// workspace root. This catches misconfigurations like:
/// `SUTRA_DB_DIR=/home/user/projects` with workspace id `app` and root
/// `/home/user/projects/app`, which would create `/home/user/projects/app/index.db`.
pub fn validate_db_dir_outside_workspaces(db_dir: &Path, config: &WorkspacesConfig) -> Result<()> {
    for entry in &config.workspace {
        validate_db_dir_for_workspace(db_dir, entry)?;
    }
    Ok(())
}

pub fn validate_db_dir_for_workspace(db_dir: &Path, entry: &WorkspaceEntry) -> Result<()> {
    let root = normalize_path(&entry.root);
    let db_workspace_dir = normalize_path(&db_dir.join(&entry.id));

    if db_workspace_dir == root || db_workspace_dir.starts_with(&root) {
        return Err(SutraError::InvalidArgument {
            tool: "startup",
            argument: "SUTRA_DB_DIR",
            constraint: format!(
                "database directory '{}' would place workspace '{}' index inside its root '{}'",
                db_dir.display(),
                entry.id,
                entry.root.display(),
            ),
            received: Some(db_dir.display().to_string()),
            next_action: "Set SUTRA_DB_DIR=/home/josh/.sutra and SUTRA_WORKSPACES=/home/josh/.sutra/workspaces.toml, then restart Sutra.".to_string(),
        });
    }

    Ok(())
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
/// Falls back to matching by root path or root basename if the exact id
/// doesn't match, so callers can pass `/home/user/project` instead of `project`.
pub fn resolve_workspace<'a>(config: &'a WorkspacesConfig, id: &str) -> Result<&'a WorkspaceEntry> {
    let id_trimmed = id.trim_end_matches('/');
    config
        .workspace
        .iter()
        .find(|w| w.id == id_trimmed)
        .or_else(|| {
            let path = std::path::Path::new(id_trimmed);
            config
                .workspace
                .iter()
                .find(|w| w.root == path)
                .or_else(|| {
                    let basename = path.file_name()?.to_str()?;
                    config.workspace.iter().find(|w| w.id == basename)
                })
        })
        .ok_or_else(|| SutraError::NotFound {
            tool: "workspaces",
            kind: format!("workspace '{id}'"),
            next_action: "List workspaces with 'sutra workspaces list'".to_string(),
        })
}

/// Add a new workspace entry to the config file. Returns an error if an entry
/// with the same id already exists, or if the new root overlaps an existing
/// workspace's root in either direction (ancestor or descendant). Overlap is
/// banned to prevent concurrent reparses against the same files.
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
    if let Some(existing) = config
        .workspace
        .iter()
        .find(|w| roots_overlap(&w.root, &entry.root))
    {
        return Err(SutraError::InvalidArgument {
            tool: "workspaces",
            argument: "root",
            constraint: format!(
                "workspace root '{}' overlaps existing workspace '{}' (root '{}'); \
                 overlapping roots cause concurrent reparses to race",
                entry.root.display(),
                existing.id,
                existing.root.display(),
            ),
            received: Some(entry.root.display().to_string()),
            next_action: "Pick a non-overlapping root, or remove the conflicting workspace first."
                .to_string(),
        });
    }
    config.workspace.push(entry);
    save_workspaces(path, &config)
}

fn roots_overlap(a: &Path, b: &Path) -> bool {
    let a = normalize_path(a);
    let b = normalize_path(b);
    a == b || a.starts_with(&b) || b.starts_with(&a)
}

fn normalize_path(path: &Path) -> PathBuf {
    let absolute = path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    });

    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
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
