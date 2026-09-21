//! Live-input probing for the health evidence contract (sutra/415).
//!
//! Builds the [`InputStamp`] axes — graph identity, repository/history
//! observation, owners configuration — from the current DB, git and filesystem
//! state. [`validate`](super::evidence::validate) later compares a recorded
//! stamp against a freshly-probed one to decide whether a retained run is still
//! current. The probes here are deterministic and side-effect free (they read;
//! they never publish); the locked refresh core (Wave B) drives ingestion and
//! assembles the history observation from the ingested result.

use std::path::Path;

use crate::db::Db;
use crate::error::Result;
use crate::git;
use crate::parser::PARSER_STAMP;

use super::evidence::{
    ConfigStamp, Digest, Generation, GraphStamp, Head, InputFailure, RepositoryObservation,
    RepositoryStamp, UtcDay,
};
use super::git_metrics::OwnersConfig;

/// Identity of the health analysis implementation: bump when a producer or the
/// scoring/finding logic changes what findings are emitted for identical inputs.
/// Recorded in every stamp so a version change invalidates retained runs even at
/// an unchanged graph/history (contract: "parser stamp alone does not" suffice).
pub const HEALTH_ANALYSIS_VERSION: &str = "health-analysis-v1";

/// Identity of the raw-history ingestion mapping (selection window semantics,
/// committer-time cutoff, path mapping). Bump when ingestion changes what the
/// same repository yields, independent of HEAD.
pub const HISTORY_INGESTION_VERSION: &str = "history-ingestion-v1";

/// Identity of the reference resolver. Bump when resolution changes the import
/// graph / resolved edges for identical extraction. There is no build-time
/// resolver stamp today, so this is maintained by hand alongside resolver edits.
pub const RESOLVER_VERSION: &str = "resolver-v1";

/// Default UTC window length for git history, in days (contract default).
pub const DEFAULT_WINDOW_DAYS: u32 = 90;

const SECONDS_PER_DAY: i64 = 86_400;

/// Digest of the health-analysis version.
pub fn analysis_version_digest() -> Digest {
    Digest::of(HEALTH_ANALYSIS_VERSION.as_bytes())
}

/// Digest of the extractor identity (`PARSER_STAMP`).
pub fn parser_digest() -> Digest {
    Digest::of(PARSER_STAMP.as_bytes())
}

/// Digest of the resolver identity.
pub fn resolver_digest() -> Digest {
    Digest::of(RESOLVER_VERSION.as_bytes())
}

/// Digest of the history-ingestion identity.
pub fn ingestion_version_digest() -> Digest {
    Digest::of(HISTORY_INGESTION_VERSION.as_bytes())
}

/// The UTC-day bucket containing `now_unix` (`floor(t / 86400)`). History
/// evidence is quantized to this bucket, so a clock crossing midnight (a
/// different bucket) invalidates git biomarkers even at an unchanged HEAD.
pub fn utc_day(now_unix: i64) -> UtcDay {
    UtcDay(now_unix.div_euclid(SECONDS_PER_DAY))
}

/// Absolute committer-time cutoff for a `window_days` trailing window ending at
/// `day`: `day_start - window_days * 86400`, where `day_start` is midnight UTC
/// of the bucket. Matches the contract's
/// `floor(t/86400)*86400 - window_days*86400`. Commits with committer time
/// `>= cutoff` are in-window; there is no upper bound.
pub fn history_cutoff(day: UtcDay, window_days: u32) -> i64 {
    day.0 * SECONDS_PER_DAY - i64::from(window_days) * SECONDS_PER_DAY
}

/// Digest over the sorted set of indexed workspace-relative paths. A file added
/// or removed changes this digest even when no existing file's content moved, so
/// a history/graph run computed against a different file population invalidates.
pub fn indexed_paths_digest<I, S>(paths: I) -> Digest
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut sorted: Vec<String> = paths.into_iter().map(|s| s.as_ref().to_string()).collect();
    sorted.sort();
    sorted.dedup();
    Digest::of(sorted.join("\n").as_bytes())
}

/// Build the shared graph-input identity from the live index. `data_generation`
/// is read after any extraction/resolution/rollup mutation the caller performed;
/// the index epoch is minted lazily if absent.
pub fn probe_graph_stamp(db: &Db) -> Result<GraphStamp> {
    let epoch = db.ensure_index_epoch()?;
    let generation = Generation(db.get_data_generation()?);
    let files = db.all_files()?;
    let indexed_paths = indexed_paths_digest(files.iter().map(|f| f.path.to_string()));
    Ok(GraphStamp {
        epoch,
        generation,
        parser: parser_digest(),
        resolver: resolver_digest(),
        indexed_paths,
    })
}

/// Observe the repository backing `workspace_root`. Only a *confirmed*
/// non-repository makes git biomarkers unsupported; any indeterminate probe
/// failure (git missing, access error, broken objects) is `Unknown`, never a
/// clean absence. A present repository with an unborn HEAD is a valid
/// observation ([`Head::Unborn`]), not a failure.
pub fn probe_repository(workspace_root: &Path) -> RepositoryObservation {
    match git::probe_git_repo(workspace_root) {
        git::RepoProbe::ConfirmedAbsent => RepositoryObservation::ConfirmedAbsent {
            probe: Digest::of(b"repo:confirmed-absent"),
        },
        git::RepoProbe::Unknown => RepositoryObservation::Unknown(InputFailure::ProbeFailed),
        git::RepoProbe::Present => {
            let head = match git::head_commit(workspace_root) {
                Ok(Some(sha)) => Head::Commit(Digest::of(sha.as_bytes())),
                Ok(None) => Head::Unborn,
                Err(_) => return RepositoryObservation::Unknown(InputFailure::ProbeFailed),
            };
            let identity = match git::repo_identity(workspace_root) {
                Ok(id) => Digest::of(id.as_bytes()),
                Err(_) => return RepositoryObservation::Unknown(InputFailure::ProbeFailed),
            };
            let history_boundaries = match git::history_boundaries(workspace_root) {
                Ok(b) => Digest::of(b.as_bytes()),
                Err(_) => return RepositoryObservation::Unknown(InputFailure::ProbeFailed),
            };
            RepositoryObservation::Present(RepositoryStamp {
                identity,
                head,
                history_boundaries,
            })
        }
    }
}

/// The observed ownership configuration: its stamp for validity and the parsed
/// (or defaulted) config the ownership producer consumes. A missing file is a
/// successful `AbsentDefault` observation; a present-but-unreadable or
/// malformed file is a *failure* the ownership producer must treat as missing
/// evidence, never as an empty/default configuration (contract).
pub struct OwnersProbe {
    pub stamp: std::result::Result<ConfigStamp, InputFailure>,
    pub config: OwnersConfig,
}

/// Digest distinguishing "no owners file" from any parsed content.
fn owners_absent_digest() -> Digest {
    Digest::of(b"owners:absent-default")
}

/// Probe `.sutra/owners.toml`, distinguishing genuine absence (NotFound) from an
/// unreadable file (other IO error) and from malformed TOML.
pub fn probe_owners(workspace_root: &Path) -> OwnersProbe {
    let path = workspace_root.join(".sutra/owners.toml");
    match std::fs::read(&path) {
        Ok(bytes) => match std::str::from_utf8(&bytes).ok().and_then(parse_owners) {
            Some(config) => OwnersProbe {
                stamp: Ok(ConfigStamp::Parsed(Digest::of(&bytes))),
                config,
            },
            None => OwnersProbe {
                stamp: Err(InputFailure::ConfigInvalid),
                config: OwnersConfig::default(),
            },
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => OwnersProbe {
            stamp: Ok(ConfigStamp::AbsentDefault(owners_absent_digest())),
            config: OwnersConfig::default(),
        },
        Err(_) => OwnersProbe {
            stamp: Err(InputFailure::ConfigUnreadable),
            config: OwnersConfig::default(),
        },
    }
}

fn parse_owners(content: &str) -> Option<OwnersConfig> {
    toml::from_str(content).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_day_and_cutoff_match_the_contract_formula() {
        // 2026-09-21T14:00:00Z is within UTC day 20717 (1_758_463_200 / 86400).
        let t = 1_758_463_200;
        let day = utc_day(t);
        assert_eq!(day, UtcDay(t.div_euclid(SECONDS_PER_DAY)));
        // 90-day window cutoff = day_start - 90 days.
        let cutoff = history_cutoff(day, 90);
        assert_eq!(cutoff, day.0 * SECONDS_PER_DAY - 90 * SECONDS_PER_DAY);
        // The cutoff is a whole number of days before the bucket's midnight.
        assert_eq!(cutoff % SECONDS_PER_DAY, 0);
    }

    #[test]
    fn utc_day_floors_toward_negative_infinity() {
        // Pre-epoch timestamps must floor, not truncate toward zero.
        assert_eq!(utc_day(-1), UtcDay(-1));
        assert_eq!(utc_day(0), UtcDay(0));
        assert_eq!(utc_day(SECONDS_PER_DAY - 1), UtcDay(0));
    }

    #[test]
    fn indexed_paths_digest_is_order_insensitive_and_deduped() {
        let a = indexed_paths_digest(["src/a.rs", "src/b.rs"]);
        let b = indexed_paths_digest(["src/b.rs", "src/a.rs"]);
        let dup = indexed_paths_digest(["src/a.rs", "src/b.rs", "src/a.rs"]);
        assert_eq!(a, b);
        assert_eq!(a, dup);
    }

    #[test]
    fn indexed_paths_digest_changes_with_population() {
        let a = indexed_paths_digest(["src/a.rs"]);
        let ab = indexed_paths_digest(["src/a.rs", "src/b.rs"]);
        assert_ne!(a, ab);
    }

    #[test]
    fn version_digests_are_distinct() {
        // Distinct version strings must not collide across axes.
        assert_ne!(analysis_version_digest(), resolver_digest());
        assert_ne!(analysis_version_digest(), ingestion_version_digest());
        assert_ne!(parser_digest(), resolver_digest());
    }

    #[test]
    fn owners_absent_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let probe = probe_owners(dir.path());
        assert!(matches!(probe.stamp, Ok(ConfigStamp::AbsentDefault(_))));
        assert!(probe.config.aliases.is_empty());
    }

    #[test]
    fn owners_parsed_when_valid() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".sutra")).unwrap();
        std::fs::write(
            dir.path().join(".sutra/owners.toml"),
            "[aliases]\n\"bot@x\" = \"human@x\"\n",
        )
        .unwrap();
        let probe = probe_owners(dir.path());
        assert!(matches!(probe.stamp, Ok(ConfigStamp::Parsed(_))));
        assert_eq!(
            probe.config.aliases.get("bot@x").map(String::as_str),
            Some("human@x")
        );
    }

    #[test]
    fn owners_malformed_is_config_invalid_not_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".sutra")).unwrap();
        std::fs::write(
            dir.path().join(".sutra/owners.toml"),
            "this is not = valid = toml [[[",
        )
        .unwrap();
        let probe = probe_owners(dir.path());
        assert_eq!(probe.stamp, Err(InputFailure::ConfigInvalid));
        // The config still defaults so a downstream caller can run, but the
        // stamp records the failure so the producer can decline to score.
        assert!(probe.config.aliases.is_empty());
    }

    #[test]
    fn owners_absent_and_parsed_digests_differ() {
        let dir = tempfile::tempdir().unwrap();
        let absent = probe_owners(dir.path()).stamp.unwrap();
        std::fs::create_dir_all(dir.path().join(".sutra")).unwrap();
        std::fs::write(dir.path().join(".sutra/owners.toml"), "[aliases]\n").unwrap();
        let parsed = probe_owners(dir.path()).stamp.unwrap();
        // A distinct default digest, never confusable with a parsed (even empty)
        // configuration.
        assert_ne!(absent, parsed);
    }
}
