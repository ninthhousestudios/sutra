use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

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

pub fn load_workspaces(_path: &Path) -> Result<WorkspacesConfig> {
    todo!("Issue 3")
}

pub fn save_workspaces(_path: &Path, _config: &WorkspacesConfig) -> Result<()> {
    todo!("Issue 3")
}

pub fn resolve_workspace<'a>(
    _config: &'a WorkspacesConfig,
    _id: &str,
) -> Result<&'a WorkspaceEntry> {
    todo!("Issue 3")
}
