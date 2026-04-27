use std::path::Path;

use crate::error::Result;

pub fn git_diff_files(
    _workspace_root: &Path,
    _base: &str,
    _head: &str,
) -> Result<Vec<String>> {
    todo!("Issue 8")
}

pub fn git_cochange_files(
    _workspace_root: &Path,
    _path: &str,
    _window_days: u32,
) -> Result<Vec<(String, u32)>> {
    todo!("Issue 8")
}
