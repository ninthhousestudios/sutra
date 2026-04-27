use crate::config::Config;
use crate::workspace::WorkspacesConfig;

pub struct Daemon {
    pub config: Config,
    pub workspaces: WorkspacesConfig,
}
