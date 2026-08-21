use schemars::JsonSchema;
use serde::Deserialize;

use crate::db::Db;
use crate::error::Result;
use crate::vocabulary;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResolveArgs {
    #[serde(default)]
    pub workspace: String,
    /// The term to resolve against aliases, component names, and anchor names.
    pub query: String,
}

pub fn handle(db: &Db, query: &str) -> Result<serde_json::Value> {
    vocabulary::resolve_to_json(db, query)
}
