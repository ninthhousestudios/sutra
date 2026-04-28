pub mod calls;
pub mod cochange;
pub mod deps;
pub mod diff_impact;
pub mod find;
pub mod grep;
pub mod health;
pub mod impact;
pub mod map;
pub mod outline;
pub mod parse;
pub mod read;
pub mod refs;
pub mod tools_meta;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::db::Db;
use crate::error::Result;

pub fn get_or_open_db(
    cache: &Mutex<HashMap<String, Arc<Db>>>,
    workspace_id: &str,
    db_dir: &Path,
) -> Result<Arc<Db>> {
    let mut map = cache.lock();
    if let Some(db) = map.get(workspace_id) {
        return Ok(Arc::clone(db));
    }
    let db = Arc::new(Db::open(workspace_id, db_dir)?);
    map.insert(workspace_id.to_string(), Arc::clone(&db));
    Ok(db)
}
