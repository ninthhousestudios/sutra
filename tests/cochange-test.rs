use std::path::Path;

use sutra::git::git_cochange_files;

fn sutra_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn cochange_returns_ok_for_known_file() {
    let result = git_cochange_files(sutra_root(), "src/git.rs", 90);
    assert!(result.is_ok(), "git_cochange_files failed: {result:?}");
}

#[test]
fn cochange_result_is_sorted_descending() {
    let pairs = git_cochange_files(sutra_root(), "src/mcp.rs", 180).unwrap();
    for window in pairs.windows(2) {
        assert!(
            window[0].1 >= window[1].1,
            "not sorted: {} ({}) before {} ({})",
            window[0].0, window[0].1, window[1].0, window[1].1,
        );
    }
}

#[test]
fn cochange_excludes_queried_file() {
    let pairs = git_cochange_files(sutra_root(), "src/git.rs", 180).unwrap();
    assert!(
        !pairs.iter().any(|(p, _)| p == "src/git.rs"),
        "queried file should be excluded from results"
    );
}
