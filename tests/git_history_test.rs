//! Tests for the pinned-HEAD, absolute-committer-time history ingestion added
//! for the health input contract (sutra/415). These build real temp git repos
//! with controlled committer dates so the absolute cutoff — and its immunity to
//! git's default `--since` early-stop on out-of-order dates — is exercised.

use std::path::Path;
use std::process::Command;

use sutra::git::{git_commit_files_since, head_commit, history_boundaries, repo_identity};

/// Run a git command in `dir`, optionally pinning committer+author date to a
/// fixed unix timestamp, and assert success.
fn git(dir: &Path, args: &[&str], date: Option<i64>) {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    if let Some(ts) = date {
        let val = format!("@{ts} +0000");
        cmd.env("GIT_COMMITTER_DATE", &val)
            .env("GIT_AUTHOR_DATE", &val);
    }
    let out = cmd.output().expect("git spawn");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q"], None);
    git(dir, &["config", "user.email", "test@example.com"], None);
    git(dir, &["config", "user.name", "Test"], None);
    // Deterministic default branch regardless of the host git's init.defaultBranch.
    git(dir, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
}

fn write(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).expect("write file");
}

/// Commit `files` (name, contents) with a fixed committer/author timestamp.
fn commit(dir: &Path, files: &[(&str, &str)], msg: &str, ts: i64) {
    for (name, contents) in files {
        write(dir, name, contents);
    }
    git(dir, &["add", "-A"], None);
    git(dir, &["commit", "-q", "-m", msg], Some(ts));
}

#[test]
fn head_commit_reports_unborn_on_fresh_repo() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    // A freshly-initialised repo has an unborn HEAD: present repo, zero commits.
    assert_eq!(head_commit(dir.path()).unwrap(), None);
}

#[test]
fn head_commit_resolves_after_first_commit() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    commit(dir.path(), &[("a.txt", "1")], "first", 1_700_000_000);
    let sha = head_commit(dir.path()).unwrap().expect("resolved HEAD");
    assert_eq!(sha.len(), 40, "full sha expected, got {sha}");
    assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn repo_identity_points_at_the_git_dir() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let id = repo_identity(dir.path()).unwrap();
    assert!(id.ends_with(".git"), "expected a .git dir, got {id}");
    assert!(Path::new(&id).is_absolute(), "identity should be absolute");
}

#[test]
fn history_boundaries_non_shallow_repo() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    commit(dir.path(), &[("a.txt", "1")], "first", 1_700_000_000);
    let fp = history_boundaries(dir.path()).unwrap();
    assert_eq!(fp, "shallow=false;boundaries=;replace=");
}

#[test]
fn history_boundaries_shifts_when_a_shallow_clone_is_deepened() {
    // Regression for sutra/427: fingerprinting only the is-shallow boolean left
    // the stamp unchanged when a shallow clone was deepened (`git fetch
    // --deepen`) at an unchanged HEAD, so demand refresh reused stale evidence
    // even though newly-accessible qualifying ancestors now exist. The
    // fingerprint must track the actual shallow boundary commit set.
    let origin = tempfile::tempdir().unwrap();
    init_repo(origin.path());
    commit(origin.path(), &[("a.txt", "1")], "c1", 1_000);
    commit(origin.path(), &[("a.txt", "2")], "c2", 2_000);
    commit(origin.path(), &[("a.txt", "3")], "c3", 3_000);

    // A depth-1 clone is shallow: HEAD present, ancestry truncated at a boundary.
    let clone = tempfile::tempdir().unwrap();
    let clone_path = clone.path().join("work");
    let origin_url = format!("file://{}", origin.path().display());
    git(
        clone.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &origin_url,
            clone_path.to_str().unwrap(),
        ],
        None,
    );

    let head_before = head_commit(&clone_path).unwrap().unwrap();
    let fp_shallow = history_boundaries(&clone_path).unwrap();
    assert!(
        fp_shallow.starts_with("shallow=true;boundaries="),
        "depth-1 clone must fingerprint as shallow with a boundary set, got {fp_shallow}"
    );
    assert!(
        !fp_shallow.contains("boundaries=;"),
        "a shallow clone must carry at least one boundary commit, got {fp_shallow}"
    );

    // Deepen the clone. HEAD is unchanged, and it may remain shallow, but the
    // boundary set moves — so the fingerprint must change.
    git(&clone_path, &["fetch", "-q", "--deepen", "1"], None);
    let head_after = head_commit(&clone_path).unwrap().unwrap();
    assert_eq!(head_before, head_after, "deepening must not move HEAD");

    let fp_deepened = history_boundaries(&clone_path).unwrap();
    assert_ne!(
        fp_shallow, fp_deepened,
        "deepening a shallow clone at an unchanged HEAD must change the boundary fingerprint"
    );
}

#[test]
fn commit_files_since_excludes_commits_before_cutoff() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    commit(dir.path(), &[("old.txt", "x")], "old", 1_000);
    commit(dir.path(), &[("new.txt", "y")], "new", 2_000);
    let head = head_commit(dir.path()).unwrap().unwrap();

    let cutoff = 1_500;
    let files = git_commit_files_since(dir.path(), &head, cutoff).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert!(paths.contains(&"new.txt"), "commit at 2000 >= cutoff kept");
    assert!(
        !paths.contains(&"old.txt"),
        "commit at 1000 < cutoff dropped"
    );
    // Every returned row carries the committer timestamp, all >= cutoff.
    assert!(files.iter().all(|f| f.timestamp >= cutoff));
}

#[test]
fn commit_files_since_keeps_qualifying_commit_behind_nonmonotonic_date() {
    // Regression for the absolute-cutoff contract: git's default `--since`
    // stops traversal at the first commit older than the cutoff. A commit with
    // an out-of-order (older) committer date sitting *behind* a newer one would
    // then be wrongly dropped even though it qualifies. The `--since-as-filter`
    // path (with defensive Rust filtering) must keep it.
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    commit(dir.path(), &[("a.txt", "a")], "a-newer", 5_000);
    // Child commit with an EARLIER committer date than its parent, but still
    // comfortably >= cutoff.
    commit(
        dir.path(),
        &[("b.txt", "b")],
        "b-older-but-qualifying",
        3_000,
    );
    let head = head_commit(dir.path()).unwrap().unwrap();

    let cutoff = 2_000;
    let files = git_commit_files_since(dir.path(), &head, cutoff).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert!(
        paths.contains(&"a.txt") && paths.contains(&"b.txt"),
        "both qualifying commits must be included despite nonmonotonic dates, got {paths:?}"
    );
}

#[test]
fn commit_files_since_is_anchored_to_the_pinned_head() {
    // Selection is reachability from the pinned HEAD, not "all refs". A commit
    // on another branch not reachable from the pinned sha must not appear.
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    commit(dir.path(), &[("base.txt", "1")], "base", 2_000);
    let base_head = head_commit(dir.path()).unwrap().unwrap();

    git(dir.path(), &["checkout", "-q", "-b", "side"], None);
    commit(dir.path(), &[("side.txt", "1")], "side", 3_000);

    // Ingest against the pinned base HEAD: the side-branch commit is unreachable.
    let files = git_commit_files_since(dir.path(), &base_head, 0).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert!(paths.contains(&"base.txt"));
    assert!(
        !paths.contains(&"side.txt"),
        "commit unreachable from the pinned HEAD must be excluded, got {paths:?}"
    );
}
