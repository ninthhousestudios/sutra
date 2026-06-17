use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::error::Result;
use crate::lessons::{LessonsDb, LessonsSearchParams};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LessonsArgs {
    /// Full-text search query
    #[serde(default)]
    pub query: Option<String>,
    /// Filter by category tag
    #[serde(default)]
    pub category: Option<String>,
    /// Filter by anchor symbol name
    #[serde(default)]
    pub symbol: Option<String>,
    /// Filter to verified lessons only
    #[serde(default)]
    pub verified: Option<bool>,
    /// Project scope
    #[serde(default)]
    pub project: Option<String>,
}

pub fn handle(lessons_db: &LessonsDb, args: &LessonsArgs) -> Result<serde_json::Value> {
    let params = LessonsSearchParams {
        query: args.query.as_deref(),
        category: args.category.as_deref(),
        symbol: args.symbol.as_deref(),
        verified: args.verified,
        project: args.project.as_deref(),
        limit: 50,
    };
    let lessons = lessons_db.search(&params)?;
    Ok(json!({
        "total": lessons.len(),
        "lessons": lessons,
    }))
}
