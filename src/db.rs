use parking_lot::Mutex;
use rusqlite::Connection;

use crate::error::Result;

pub struct Db {
    conn: Mutex<Connection>,
    workspace_id: String,
}

impl Db {
    pub fn open(_workspace_id: &str, _db_dir: &std::path::Path) -> Result<Self> {
        todo!("Issue 2")
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }
}
