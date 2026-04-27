use std::sync::Arc;

use crate::config::Config;
use crate::workspace::WorkspacesConfig;

pub struct SutraServer {
    pub config: Arc<Config>,
    pub workspaces: WorkspacesConfig,
}
