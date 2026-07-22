use std::collections::{HashMap, HashSet};
use std::path::Path;

use parking_lot::Mutex;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::error::{Result, SutraError};

// ---------------------------------------------------------------------------
// LessonsDb
// ---------------------------------------------------------------------------

pub struct LessonsDb {
    conn: Mutex<Connection>,
    /// Read-only handles suppress surfacing bookkeeping (`last_surfaced`).
    /// Set by [`LessonsDb::open_existing`]; see its docs for why.
    read_only: bool,
    /// Cap on rows scanned by the Phase 2 anchor sweep. Unbounded by default;
    /// tightened on the guard path, which runs inside a blocking hook.
    candidate_limit: i64,
}

/// No cap — the default for owning callers (`sutra_read`, `sutra_impact`,
/// `sutra_orient`), which preserves their existing behaviour exactly.
const CANDIDATE_SCAN_UNBOUNDED: i64 = i64::MAX;

/// Phase 2 scan cap for the guard. Anchor rows are ordered verified-then-
/// confidence first, so truncation drops the least useful candidates.
pub const GUARD_CANDIDATE_SCAN_LIMIT: i64 = 256;

/// `busy_timeout` for the guard's handle. The installed PreToolUse hook budget
/// is 3000ms; a purely advisory lookup must not be able to consume it, so this
/// is an order of magnitude below that rather than the 5000ms the owning
/// handle uses (which could exceed the hook budget on its own).
pub const GUARD_BUSY_TIMEOUT_MS: u32 = 250;

const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_initial",
        include_str!("lessons/sql/lessons_schema.sql"),
    ),
    ("0002_fts5", include_str!("lessons/sql/lessons_fts5.sql")),
    (
        "0003_cite_idempotency",
        include_str!("lessons/sql/lessons_cite_idempotency.sql"),
    ),
    (
        "0004_staleness",
        include_str!("lessons/sql/lessons_staleness.sql"),
    ),
    (
        "0005_metadata",
        include_str!("lessons/sql/lessons_metadata.sql"),
    ),
    (
        "0006_category_tag_index",
        include_str!("lessons/sql/lessons_category_tag_index.sql"),
    ),
];

impl LessonsDb {
    pub fn open(db_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(db_dir).map_err(|e| {
            SutraError::Internal(format!("cannot create {}: {e}", db_dir.display()))
        })?;
        let db_path = db_dir.join("lessons.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA synchronous = NORMAL;\
             PRAGMA foreign_keys = ON;\
             PRAGMA busy_timeout = 5000;",
        )?;
        let db = Self {
            conn: Mutex::new(conn),
            read_only: false,
            candidate_limit: CANDIDATE_SCAN_UNBOUNDED,
        };
        db.run_migrations()?;
        let _ = db.archive_decayed(90 * 86400);
        Ok(db)
    }

    /// Open an existing store for reading only, or `Ok(None)` if there is none.
    ///
    /// For callers on a latency-critical, non-owning path — currently the
    /// PreToolUse guard hook, which fires on every Edit/Write. It differs from
    /// [`LessonsDb::open`] in four ways, each load-bearing:
    ///
    /// - **No creation.** A missing `lessons.db` yields `None` instead of
    ///   materialising a directory and an empty schema. A hook that fires on
    ///   every write must not create state as a side effect.
    /// - **No migrations, no decay-archiving.** Both are writes; neither is
    ///   this caller's job. A stale schema simply makes queries fail, and the
    ///   guard treats a failed lookup as "no lessons".
    /// - **`query_only`.** Enforces at the sqlite level what `read_only` asks
    ///   for at the Rust level, so a future write added to a shared query path
    ///   fails loudly here rather than silently taking the write lock inside a
    ///   blocking hook.
    /// - **Short `busy_timeout` and a bounded candidate scan.** See
    ///   [`GUARD_BUSY_TIMEOUT_MS`] and [`GUARD_CANDIDATE_SCAN_LIMIT`].
    ///
    /// `journal_mode` is deliberately not set: it is a write on a fresh db and
    /// pointless on an existing one, which already carries its own mode.
    pub fn open_existing(
        db_dir: &Path,
        busy_timeout_ms: u32,
        candidate_limit: i64,
    ) -> Result<Option<Self>> {
        let db_path = db_dir.join("lessons.db");
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(&format!(
            "PRAGMA busy_timeout = {busy_timeout_ms};\
             PRAGMA foreign_keys = ON;\
             PRAGMA query_only = ON;"
        ))?;
        Ok(Some(Self {
            conn: Mutex::new(conn),
            read_only: true,
            candidate_limit,
        }))
    }

    fn run_migrations(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 name         TEXT    NOT NULL UNIQUE,
                 content_hash TEXT    NOT NULL,
                 applied_at   TEXT    NOT NULL
             )",
        )?;

        for &(name, sql) in MIGRATIONS {
            let hash = blake3::hash(sql.as_bytes()).to_hex().to_string();

            let existing: Option<String> = conn
                .query_row(
                    "SELECT content_hash FROM schema_migrations WHERE name = ?1",
                    params![name],
                    |row| row.get(0),
                )
                .ok();

            if let Some(stored) = existing {
                if stored != hash {
                    return Err(SutraError::Internal(format!(
                        "lessons migration `{name}` hash mismatch: stored={stored}, current={hash}"
                    )));
                }
                continue;
            }

            let sp = format!("migration_{name}");
            conn.execute_batch(&format!("SAVEPOINT {sp}"))?;

            match conn.execute_batch(sql) {
                Ok(()) => {
                    conn.execute(
                        "INSERT INTO schema_migrations (name, content_hash, applied_at) \
                         VALUES (?1, ?2, datetime('now'))",
                        params![name, hash],
                    )?;
                    conn.execute_batch(&format!("RELEASE SAVEPOINT {sp}"))?;
                }
                Err(e) => {
                    let _ = conn.execute_batch(&format!("ROLLBACK TO SAVEPOINT {sp}"));
                    let _ = conn.execute_batch(&format!("RELEASE SAVEPOINT {sp}"));
                    return Err(SutraError::Internal(format!(
                        "lessons migration `{name}` failed: {e}"
                    )));
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Anchor kinds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnchorKind {
    Symbol,
    File,
    ImportPattern,
    Directory,
}

impl AnchorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::File => "file",
            Self::ImportPattern => "import_pattern",
            Self::Directory => "directory",
        }
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

pub struct StoreLessonParams<'a> {
    pub text: &'a str,
    pub anchors: &'a [(AnchorKind, &'a str)],
    pub categories: &'a [&'a str],
    pub source_task_ids: &'a [&'a str],
    pub project_origin: Option<&'a str>,
}

impl LessonsDb {
    pub fn store(&self, params: &StoreLessonParams<'_>) -> Result<String> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let id = uuid::Uuid::now_v7().to_string();

        tx.execute(
            "INSERT INTO lessons (id, text, project_origin) VALUES (?1, ?2, ?3)",
            params![id, params.text, params.project_origin],
        )?;

        for &(kind, value) in params.anchors {
            tx.execute(
                "INSERT INTO anchors (lesson_id, kind, value) VALUES (?1, ?2, ?3)",
                params![id, kind.as_str(), value],
            )?;
        }

        for tag in params.categories {
            tx.execute(
                "INSERT OR IGNORE INTO categories (lesson_id, tag) VALUES (?1, ?2)",
                params![id, tag],
            )?;
        }

        for task_id in params.source_task_ids {
            tx.execute(
                "INSERT OR IGNORE INTO citations (lesson_id, task_id, field) VALUES (?1, ?2, 'source')",
                params![id, task_id],
            )?;
        }

        tx.commit()?;
        Ok(id)
    }
}

// ---------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------

/// Category tags that name a language, derived from the registered adapters
/// rather than a literal. `remember::enrich` seeds language categories from
/// `file.language` (lowercased), which is exactly an adapter's `language_id`,
/// so the two sets stay aligned as adapters are added.
///
/// This must cover every supported language, not just the ones sutra itself is
/// written in: the Phase 3 filter only *excludes* a lesson whose language tags
/// are recognised as language tags. An unrecognised tag makes `lang_tags` empty
/// and the lesson passes unconditionally, so a missing entry here means
/// wrong-language lessons leak into unrelated workspaces.
static LANGUAGE_CATEGORIES: std::sync::LazyLock<HashSet<String>> = std::sync::LazyLock::new(|| {
    crate::parser::adapter::default_registry()
        .language_ids()
        .iter()
        .map(|id| id.to_lowercase())
        .collect()
});

fn is_language_category(cat: &str) -> bool {
    LANGUAGE_CATEGORIES.contains(&cat.to_lowercase())
}

pub struct MatchContext<'a> {
    pub symbol_name: &'a str,
    pub file_path: Option<&'a str>,
    pub imports: &'a [&'a str],
    pub project: Option<&'a str>,
    pub workspace_languages: &'a [String],
}

const CONTEXT_SURFACING_CAP: usize = 10;

/// Whether a context query refreshes the decay timers of what it returns.
///
/// `last_surfaced` is evidence that a lesson was put in front of someone, and
/// [`LessonsDb::archive_decayed`] spares anything recently surfaced. A caller
/// that narrows the result set further after the query must not let the query
/// record on its behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surfacing {
    /// Everything returned is being shown; record it.
    Record,
    /// The caller will cap further and call [`LessonsDb::mark_surfaced`] with
    /// what it actually emits.
    Deferred,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextLessons {
    pub lessons: Vec<SurfacedLesson>,
    pub omitted: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SurfacedLesson {
    pub id: String,
    pub text: String,
    pub verified: bool,
    pub confidence: i64,
    pub project_origin: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<bool>,
    /// How narrowly this lesson's *matching* anchors bind to the query context
    /// (see `anchor_specificity`). Ranking input only — never serialized, and
    /// meaningless outside the context query that produced it.
    #[serde(skip)]
    pub specificity: u8,
    /// Set by `search` only — see `MatchKind`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_kind: Option<MatchKind>,
}

/// Rank an anchor by how narrowly it binds to a single piece of code.
///
/// A lesson anchored to one symbol is nearly always more relevant to the
/// caller's context than one anchored to a whole directory, yet both compete
/// for the same cap slots. Ranking by this lets the cap be tightened without
/// dropping the lesson that actually mattered.
///
/// File anchors straddle the range: `src/lessons.rs` is as narrow as it gets,
/// but `src/**/*.rs` is broader than an import pattern — so a glob file anchor
/// is scored below one.
fn anchor_specificity(kind: &str, value: &str) -> u8 {
    match kind {
        "symbol" => 4,
        "file" if !value.contains(['*', '?', '[']) => 3,
        "import_pattern" => 2,
        "file" => 1,
        _ => 0,
    }
}

const GLOB_OPTS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

fn matches_anchor(kind: &str, value: &str, ctx: &MatchContext<'_>) -> bool {
    match kind {
        "file" => {
            let Some(fp) = ctx.file_path else {
                return false;
            };
            glob::Pattern::new(value)
                .map(|p| p.matches_with(fp, GLOB_OPTS))
                .unwrap_or(false)
        }
        "import_pattern" => {
            let Ok(pat) = glob::Pattern::new(value) else {
                return false;
            };
            ctx.imports.iter().any(|imp| {
                if pat.matches_with(imp, GLOB_OPTS) {
                    return true;
                }
                // Dart imports are `package:foo/bar.dart` but enriched anchors
                // use `foo::*` — normalize to `foo::bar.dart` for matching.
                if let Some(rest) = imp.strip_prefix("package:") {
                    let normalized = rest.replacen('/', "::", 1);
                    return pat.matches_with(&normalized, GLOB_OPTS);
                }
                false
            })
        }
        "directory" => {
            let Some(fp) = ctx.file_path else {
                return false;
            };
            let dir = value.trim_end_matches('/');
            fp.starts_with(dir) && fp.as_bytes().get(dir.len()) == Some(&b'/')
        }
        _ => false,
    }
}

fn map_surfaced_lesson(row: &rusqlite::Row<'_>) -> rusqlite::Result<SurfacedLesson> {
    Ok(SurfacedLesson {
        id: row.get(0)?,
        text: row.get(1)?,
        verified: row.get::<_, i64>(2)? != 0,
        confidence: row.get(3)?,
        project_origin: row.get(4)?,
        created_at: row.get(5)?,
        stale: None,
        specificity: 0,
        match_kind: None,
    })
}

impl LessonsDb {
    pub fn query_for_context(&self, ctx: &MatchContext<'_>) -> Result<ContextLessons> {
        self.query_for_context_capped(ctx, CONTEXT_SURFACING_CAP, Surfacing::Record)
    }

    /// As `query_for_context`, but with a caller-supplied cap and control over
    /// surfacing bookkeeping.
    ///
    /// Callers that merge several contexts (orient walks every file in a
    /// component) need the complete per-context set before they can cap the
    /// merged set honestly — a per-context cap makes their `omitted` count a
    /// lie. Such a caller passes `usize::MAX` with [`Surfacing::Deferred`],
    /// caps the merged set itself, then calls [`LessonsDb::mark_surfaced`]
    /// with what it actually emitted. Recording here instead would refresh the
    /// decay timer of every candidate the query touched, which is how a broad
    /// directory-anchored lesson stays unarchivable forever without ever being
    /// shown to anyone.
    pub fn query_for_context_capped(
        &self,
        ctx: &MatchContext<'_>,
        cap: usize,
        surfacing: Surfacing,
    ) -> Result<ContextLessons> {
        let conn = self.conn.lock();

        let mut seen = HashSet::new();
        let mut lessons = Vec::new();
        // Track which anchor keys actually caused each lesson to surface
        let mut matched_anchors: HashMap<String, HashSet<String>> = HashMap::new();

        // Phase 1: symbol match (indexed, fast). Also checks short name
        // so anchors stored as "foo" match when the caller passes "Mod::foo".
        let short_name = ctx
            .symbol_name
            .rsplit("::")
            .next()
            .unwrap_or(ctx.symbol_name);
        let has_qualifier = short_name != ctx.symbol_name;
        {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT l.id, l.text, l.verified, l.confidence,
                        l.project_origin, l.created_at, a.value
                 FROM lessons l
                 JOIN anchors a ON a.lesson_id = l.id
                 WHERE a.kind = 'symbol' AND (a.value = ?1 OR (?3 AND a.value = ?2))
                   AND l.archived = 0
                   AND (l.project_origin IS NULL OR l.project_origin = ?4 OR ?4 IS NULL)
                 ORDER BY l.verified DESC, l.confidence DESC",
            )?;
            let mut rows = stmt.query(params![
                ctx.symbol_name,
                short_name,
                has_qualifier,
                ctx.project
            ])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let anchor_val: String = row.get(6)?;
                let key = format!("symbol:{anchor_val}");
                matched_anchors.entry(id.clone()).or_default().insert(key);
                if !seen.contains(&id) {
                    seen.insert(id);
                    lessons.push(map_surfaced_lesson(row)?);
                }
            }
        }

        // Phase 2: file/import/directory anchors — load candidates, filter in Rust
        if ctx.file_path.is_some() || !ctx.imports.is_empty() {
            let mut stmt = conn.prepare(
                "SELECT l.id, l.text, l.verified, l.confidence,
                        l.project_origin, l.created_at, a.kind, a.value
                 FROM lessons l
                 JOIN anchors a ON a.lesson_id = l.id
                 WHERE a.kind IN ('file', 'import_pattern', 'directory')
                   AND l.archived = 0
                   AND (l.project_origin IS NULL OR l.project_origin = ?1 OR ?1 IS NULL)
                 ORDER BY l.verified DESC, l.confidence DESC
                 LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![ctx.project, self.candidate_limit])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let anchor_kind: String = row.get(6)?;
                let anchor_value: String = row.get(7)?;
                if matches_anchor(&anchor_kind, &anchor_value, ctx) {
                    let key = format!("{anchor_kind}:{anchor_value}");
                    matched_anchors.entry(id.clone()).or_default().insert(key);
                    if !seen.contains(&id) {
                        seen.insert(id);
                        lessons.push(map_surfaced_lesson(row)?);
                    }
                }
            }
        }

        // Phase 3: category filtering — exclude language-specific lessons
        // irrelevant to this workspace. Skip when workspace_languages is empty
        // (no workspace context → surface everything).
        if !lessons.is_empty() && !ctx.workspace_languages.is_empty() {
            let ids: Vec<&str> = lessons.iter().map(|l| l.id.as_str()).collect();
            let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut stmt = conn.prepare(&format!(
                "SELECT lesson_id, tag FROM categories WHERE lesson_id IN ({placeholders})"
            ))?;
            let mut rows = stmt.query(rusqlite::params_from_iter(ids.iter()))?;
            let mut cat_map: HashMap<String, Vec<String>> = HashMap::new();
            while let Some(row) = rows.next()? {
                let lid: String = row.get(0)?;
                let tag: String = row.get(1)?;
                cat_map.entry(lid).or_default().push(tag);
            }
            drop(rows);
            drop(stmt);

            // Lowercased on both sides: `is_language_category` matches
            // case-insensitively, so an exact-match membership test here would
            // recognise a "Rust" tag as a language and then fail to match the
            // workspace's "rust", dropping the lesson.
            let ws_lang_set: HashSet<String> = ctx
                .workspace_languages
                .iter()
                .map(|s| s.to_lowercase())
                .collect();

            lessons.retain(|l| {
                let Some(cats) = cat_map.get(&l.id) else {
                    return true;
                };
                if cats.is_empty() {
                    return true;
                }
                let lang_tags: Vec<String> = cats
                    .iter()
                    .filter(|c| is_language_category(c))
                    .map(|c| c.to_lowercase())
                    .collect();
                if lang_tags.is_empty() {
                    return true;
                }
                lang_tags.iter().any(|t| ws_lang_set.contains(t))
            });
        }

        // Phase 4: verified-first surfacing priority — suppress unverified
        // lessons when a verified lesson matched the same anchor in this context.
        if lessons.iter().any(|l| l.verified) && lessons.iter().any(|l| !l.verified) {
            let verified_matched: HashSet<&str> = lessons
                .iter()
                .filter(|l| l.verified)
                .flat_map(|l| {
                    matched_anchors
                        .get(&l.id)
                        .into_iter()
                        .flatten()
                        .map(|s| s.as_str())
                })
                .collect();

            lessons.retain(|l| {
                if l.verified {
                    return true;
                }
                let dominated = matched_anchors
                    .get(&l.id)
                    .map(|keys| keys.iter().any(|k| verified_matched.contains(k.as_str())))
                    .unwrap_or(false);
                !dominated
            });
        }

        // Tag all surviving unverified lessons
        for l in &mut lessons {
            if !l.verified {
                l.text = format!("[unverified] {}", l.text);
            }
        }

        // Score by the anchors that actually matched this context — not every
        // anchor on the lesson. A lesson anchored to both a symbol and a
        // directory is only symbol-specific when it surfaced via the symbol.
        for l in &mut lessons {
            l.specificity = matched_anchors
                .get(&l.id)
                .into_iter()
                .flatten()
                .filter_map(|key| key.split_once(':'))
                .map(|(kind, value)| anchor_specificity(kind, value))
                .max()
                .unwrap_or(0);
        }

        // Sort before applying the cap so the priority slots go to verified,
        // then narrowly-anchored, then high-confidence lessons.
        lessons.sort_by(|a, b| {
            b.verified
                .cmp(&a.verified)
                .then(b.specificity.cmp(&a.specificity))
                .then(b.confidence.cmp(&a.confidence))
        });

        let total = lessons.len();
        let omitted = total.saturating_sub(cap);
        lessons.truncate(cap);

        if surfacing == Surfacing::Record {
            let ids: Vec<&str> = lessons.iter().map(|l| l.id.as_str()).collect();
            Self::mark_surfaced_locked(&conn, &ids, self.read_only)?;
        }

        Ok(ContextLessons { lessons, omitted })
    }

    /// Record that these lessons were actually put in front of someone.
    ///
    /// For callers that used [`Surfacing::Deferred`]; see the note there.
    pub fn mark_surfaced(&self, ids: &[&str]) -> Result<()> {
        let conn = self.conn.lock();
        Self::mark_surfaced_locked(&conn, ids, self.read_only)
    }

    /// Surfacing bookkeeping is a write, and the guard's handle is inside a
    /// blocking PreToolUse hook — it must not contend for the write lock. The
    /// cost is that guard-only surfacing does not refresh decay timers; that is
    /// the right trade, since an advisory nudge is not evidence the lesson was
    /// read.
    fn mark_surfaced_locked(conn: &Connection, ids: &[&str], read_only: bool) -> Result<()> {
        if ids.is_empty() || read_only {
            return Ok(());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        conn.execute(
            &format!(
                "UPDATE lessons SET last_surfaced = datetime('now') WHERE id IN ({placeholders})"
            ),
            rusqlite::params_from_iter(ids.iter()),
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

fn quote_fts5_query(raw: &str) -> String {
    raw.split_whitespace()
        .map(|term| {
            let escaped = term.replace('"', "\"\"");
            format!("\"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Words too generic to be a useful intent signal. Short tokens are dropped
/// wholesale below; these are the ones long enough to survive that.
const TAG_QUERY_STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "when", "how", "not", "use", "using", "into", "from", "that",
    "this", "what", "why", "does", "should",
];

/// Split a free-text query into candidate category tokens.
fn tag_query_tokens(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_lowercase())
        .filter(|t| !TAG_QUERY_STOPWORDS.contains(&t.as_str()))
        .collect()
}

/// Which of the store's category tags a free-text query is asking about.
///
/// Matching is per-segment so a query of "golden testing" reaches the
/// `golden-testing` tag. Deliberately generous: category hits rank below text
/// hits, so a loose match costs a tail slot rather than a top one.
fn tags_matching_query(all_tags: &[String], query: &str) -> Vec<String> {
    let tokens = tag_query_tokens(query);
    if tokens.is_empty() {
        return Vec::new();
    }
    all_tags
        .iter()
        .filter(|tag| {
            let lowered = tag.to_lowercase();
            tokens.iter().any(|t| {
                lowered == *t || lowered.split(['-', '_', '.']).any(|seg| seg == t.as_str())
            })
        })
        .cloned()
        .collect()
}

/// Why a lesson appeared in a search result. Absent outside search, where the
/// anchor match is the whole story.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchKind {
    /// Full-text hit on the lesson's own prose.
    Text,
    /// The query named one of the lesson's category tags. Categories describe
    /// what you are about to do rather than where the code lives, so this is
    /// the tier that works on greenfield code with no anchors to match.
    Category,
}

pub struct LessonsSearchParams<'a> {
    pub query: Option<&'a str>,
    pub category: Option<&'a str>,
    pub symbol: Option<&'a str>,
    pub verified: Option<bool>,
    pub project: Option<&'a str>,
    pub include_archived: bool,
    pub limit: usize,
}

/// The filters that apply to every search tier, built once so the text and
/// category passes cannot drift apart.
struct SearchFilters {
    joins: String,
    conditions: Vec<String>,
    binds: Vec<String>,
    next_idx: usize,
}

impl SearchFilters {
    /// `start_idx` is the first free bind slot; tiers reserve their own
    /// placeholders before these.
    fn build(params: &LessonsSearchParams<'_>, start_idx: usize) -> Self {
        let mut f = SearchFilters {
            joins: String::new(),
            conditions: if params.include_archived {
                vec![]
            } else {
                vec!["l.archived = 0".to_string()]
            },
            binds: Vec::new(),
            next_idx: start_idx,
        };

        if let Some(cat) = params.category {
            f.joins.push_str(" JOIN categories c ON c.lesson_id = l.id");
            f.conditions.push(format!("c.tag = ?{}", f.next_idx));
            f.binds.push(cat.to_string());
            f.next_idx += 1;
        }

        if let Some(sym) = params.symbol {
            f.joins.push_str(" JOIN anchors a ON a.lesson_id = l.id");
            f.conditions
                .push(format!("a.kind = 'symbol' AND a.value = ?{}", f.next_idx));
            f.binds.push(sym.to_string());
            f.next_idx += 1;
        }

        if let Some(true) = params.verified {
            f.conditions.push("l.verified = 1".to_string());
        }

        if let Some(proj) = params.project {
            f.conditions.push(format!(
                "(l.project_origin IS NULL OR l.project_origin = ?{})",
                f.next_idx
            ));
            f.binds.push(proj.to_string());
            f.next_idx += 1;
        }

        f
    }

    /// Append the shared conditions to a tier that has already opened its own
    /// `WHERE`.
    fn append_to(&self, sql: &mut String) {
        for cond in &self.conditions {
            sql.push_str(" AND ");
            sql.push_str(cond);
        }
    }
}

const SEARCH_COLUMNS: &str = "SELECT DISTINCT l.id, l.text, l.verified, l.confidence, \
                              l.project_origin, l.created_at FROM lessons l";

impl LessonsDb {
    /// Search in two tiers: full text over the lesson's own prose, then the
    /// category index for what the query named by intent rather than wording.
    ///
    /// The category tier is what makes the store usable at plan time — tags
    /// like `sqlite` or `golden-testing` describe the work about to be done,
    /// so they match before any of the code a lesson is anchored to exists.
    /// Text hits always come first; category-only hits fill the tail, and each
    /// result carries the tier that produced it.
    pub fn search(&self, params: &LessonsSearchParams<'_>) -> Result<Vec<SurfacedLesson>> {
        let conn = self.conn.lock();
        let limit = params.limit as i64;

        let Some(q) = params.query else {
            let filters = SearchFilters::build(params, 1);
            let mut sql = format!("{SEARCH_COLUMNS}{}", filters.joins);
            if !filters.conditions.is_empty() {
                sql.push_str(" WHERE ");
                sql.push_str(&filters.conditions.join(" AND "));
            }
            sql.push_str(&format!(
                " ORDER BY l.verified DESC, l.confidence DESC LIMIT ?{}",
                filters.next_idx
            ));

            let mut refs = to_sql_refs(&filters.binds);
            refs.push(&limit);
            return query_lessons(&conn, &sql, &refs, None);
        };

        // Tier 1: full text.
        let filters = SearchFilters::build(params, 2);
        let mut sql = format!(
            "{SEARCH_COLUMNS} JOIN lessons_fts ON lessons_fts.rowid = l.rowid{} \
             WHERE lessons_fts MATCH ?1",
            filters.joins
        );
        filters.append_to(&mut sql);
        sql.push_str(&format!(" ORDER BY rank LIMIT ?{}", filters.next_idx));

        let fts_query = quote_fts5_query(q);
        let mut refs: Vec<&dyn rusqlite::types::ToSql> = vec![&fts_query];
        refs.extend(to_sql_refs(&filters.binds));
        refs.push(&limit);
        let mut lessons = query_lessons(&conn, &sql, &refs, MatchKind::Text)?;

        // Tier 2: categories, filling only the slots the text tier left.
        let remaining = (params.limit.saturating_sub(lessons.len())) as i64;
        if remaining == 0 {
            return Ok(lessons);
        }
        let all_tags: Vec<String> = conn
            .prepare("SELECT DISTINCT tag FROM categories")?
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let hit_tags = tags_matching_query(&all_tags, q);
        if hit_tags.is_empty() {
            return Ok(lessons);
        }

        let by_category = {
            let seen: Vec<&str> = lessons.iter().map(|l| l.id.as_str()).collect();
            let filters = SearchFilters::build(params, 1 + hit_tags.len() + seen.len());
            let mut sql = format!(
                "{SEARCH_COLUMNS} JOIN categories ct ON ct.lesson_id = l.id{} WHERE ct.tag IN ({})",
                filters.joins,
                placeholders(1, hit_tags.len()),
            );
            if !seen.is_empty() {
                sql.push_str(&format!(
                    " AND l.id NOT IN ({})",
                    placeholders(1 + hit_tags.len(), seen.len())
                ));
            }
            filters.append_to(&mut sql);
            sql.push_str(&format!(
                " ORDER BY l.verified DESC, l.confidence DESC LIMIT ?{}",
                filters.next_idx
            ));

            let mut refs = to_sql_refs(&hit_tags);
            refs.extend(seen.iter().map(|s| s as &dyn rusqlite::types::ToSql));
            refs.extend(to_sql_refs(&filters.binds));
            refs.push(&remaining);
            query_lessons(&conn, &sql, &refs, MatchKind::Category)?
        };

        lessons.extend(by_category);
        Ok(lessons)
    }
}

/// `?n, ?n+1, ...` for `count` bind slots starting at `start`.
fn placeholders(start: usize, count: usize) -> String {
    (start..start + count)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn to_sql_refs(values: &[String]) -> Vec<&dyn rusqlite::types::ToSql> {
    values
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect()
}

fn query_lessons(
    conn: &Connection,
    sql: &str,
    binds: &[&dyn rusqlite::types::ToSql],
    kind: impl Into<Option<MatchKind>>,
) -> Result<Vec<SurfacedLesson>> {
    let kind = kind.into();
    let mut stmt = conn.prepare(sql)?;
    let lessons = stmt
        .query_map(binds, map_surfaced_lesson)?
        .map(|r| {
            r.map(|mut l| {
                l.match_kind = kind;
                l
            })
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(lessons)
}

// ---------------------------------------------------------------------------
// Citation / verification lifecycle
// ---------------------------------------------------------------------------

const VERIFICATION_THRESHOLD: i64 = 2;

pub struct CiteResult {
    pub new_confidence: i64,
    pub verified: bool,
    pub crossed_threshold: bool,
}

pub struct AntiVerifyResult {
    pub new_confidence: i64,
    pub verified: bool,
}

/// Resolves an anchor (kind, value) to the current content hash of its backing file.
/// Returns `None` for anchor kinds that aren't hashable (directory, import_pattern)
/// or when the symbol/file can't be resolved.
pub type HashResolver<'a> = dyn Fn(&str, &str) -> Option<String> + 'a;

impl LessonsDb {
    pub fn cite(
        &self,
        lesson_id: &str,
        task_id: Option<&str>,
        hash_resolver: Option<&HashResolver<'_>>,
    ) -> Result<CiteResult> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;

        let (old_confidence, old_verified): (i64, bool) = tx
            .query_row(
                "SELECT confidence, verified FROM lessons WHERE id = ?1 AND archived = 0",
                params![lesson_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| {
                SutraError::Internal(format!("lesson not found or archived: {lesson_id}"))
            })?;

        tx.execute(
            "INSERT OR IGNORE INTO citations (lesson_id, task_id, field) VALUES (?1, ?2, 'cite')",
            params![lesson_id, task_id.unwrap_or("")],
        )?;
        let inserted = tx.changes() > 0;

        let new_confidence = if inserted {
            old_confidence + 1
        } else {
            old_confidence
        };
        let now_verified = new_confidence >= VERIFICATION_THRESHOLD;
        let crossed = now_verified && !old_verified;

        if inserted {
            let verified_at_clause = if crossed {
                ", verified_at = datetime('now')"
            } else {
                ""
            };
            tx.execute(
                &format!(
                    "UPDATE lessons SET confidence = ?1, verified = ?2, last_cited = datetime('now'){verified_at_clause} \
                     WHERE id = ?3"
                ),
                params![new_confidence, now_verified, lesson_id],
            )?;
        }

        if crossed {
            Self::snapshot_anchor_hashes(&tx, lesson_id, hash_resolver)?;
        }

        tx.commit()?;
        Ok(CiteResult {
            new_confidence,
            verified: now_verified,
            crossed_threshold: crossed,
        })
    }

    pub fn anti_verify(&self, lesson_id: &str) -> Result<AntiVerifyResult> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;

        let old_confidence: i64 = tx
            .query_row(
                "SELECT confidence FROM lessons WHERE id = ?1 AND archived = 0",
                params![lesson_id],
                |row| row.get(0),
            )
            .map_err(|_| {
                SutraError::Internal(format!("lesson not found or archived: {lesson_id}"))
            })?;

        let vote_seq: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM citations WHERE lesson_id = ?1 AND field = 'anti_verify'",
                params![lesson_id],
                |row| row.get(0),
            )
            .unwrap_or(0);
        tx.execute(
            "INSERT OR IGNORE INTO citations (lesson_id, task_id, field) VALUES (?1, ?2, 'anti_verify')",
            params![lesson_id, format!("anti:{vote_seq}")],
        )?;

        let new_confidence = (old_confidence - 1).max(0);
        let still_verified = new_confidence >= VERIFICATION_THRESHOLD;

        tx.execute(
            "UPDATE lessons SET confidence = ?1, verified = ?2 WHERE id = ?3",
            params![new_confidence, still_verified, lesson_id],
        )?;

        tx.commit()?;
        Ok(AntiVerifyResult {
            new_confidence,
            verified: still_verified,
        })
    }
}

// ---------------------------------------------------------------------------
// Decay / archive
// ---------------------------------------------------------------------------

impl LessonsDb {
    /// Archive unverified lessons that haven't been cited or surfaced within
    /// `window_secs` seconds. Returns the number of lessons archived.
    /// When this lesson was last put in front of someone, or `None` if never.
    pub fn last_surfaced(&self, lesson_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let value = conn
            .query_row(
                "SELECT last_surfaced FROM lessons WHERE id = ?1",
                params![lesson_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        Ok(value)
    }

    pub fn archive_decayed(&self, window_secs: i64) -> Result<usize> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "UPDATE lessons SET archived = 1
             WHERE archived = 0
               AND verified = 0
               AND (last_cited IS NULL OR last_cited < datetime('now', ?1))
               AND (last_surfaced IS NULL OR last_surfaced < datetime('now', ?1))
               AND created_at < datetime('now', ?1)",
            params![format!("-{window_secs} seconds")],
        )?;
        Ok(changed)
    }
}

// ---------------------------------------------------------------------------
// Staleness detection
// ---------------------------------------------------------------------------

impl LessonsDb {
    fn snapshot_anchor_hashes(
        tx: &rusqlite::Transaction<'_>,
        lesson_id: &str,
        hash_resolver: Option<&HashResolver<'_>>,
    ) -> Result<()> {
        let resolver = match hash_resolver {
            Some(r) => r,
            None => return Ok(()),
        };

        let mut stmt = tx.prepare("SELECT id, kind, value FROM anchors WHERE lesson_id = ?1")?;
        let anchors: Vec<(i64, String, String)> = stmt
            .query_map(params![lesson_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        for (anchor_id, kind, value) in &anchors {
            if let Some(hash) = resolver(kind, value) {
                tx.execute(
                    "INSERT INTO anchor_verification (lesson_id, anchor_id, content_hash, verified_at)
                     VALUES (?1, ?2, ?3, datetime('now'))
                     ON CONFLICT(anchor_id) DO UPDATE SET content_hash = ?3, verified_at = datetime('now')",
                    params![lesson_id, anchor_id, hash],
                )?;
            }
        }
        Ok(())
    }

    /// For each surfaced lesson, check whether any verified anchor's content
    /// has changed since verification. Returns a map of lesson_id → stale.
    /// Only lessons with `anchor_verification` rows are checked.
    pub fn check_staleness(
        &self,
        lesson_ids: &[&str],
        hash_resolver: &HashResolver<'_>,
    ) -> Result<HashMap<String, bool>> {
        if lesson_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let conn = self.conn.lock();
        let placeholders = lesson_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT av.lesson_id, a.kind, a.value, av.content_hash
             FROM anchor_verification av
             JOIN anchors a ON a.id = av.anchor_id
             WHERE av.lesson_id IN ({placeholders})"
        ))?;
        let rows: Vec<(String, String, String, String)> = stmt
            .query_map(rusqlite::params_from_iter(lesson_ids.iter()), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let mut result: HashMap<String, bool> = HashMap::new();
        for (lesson_id, kind, value, snapshot_hash) in &rows {
            let stale_entry = result.entry(lesson_id.clone()).or_insert(false);
            if *stale_entry {
                continue;
            }
            match hash_resolver(kind, value) {
                Some(current_hash) if current_hash == *snapshot_hash => {}
                Some(_) | None => {
                    *stale_entry = true;
                }
            }
        }
        Ok(result)
    }

    /// Annotate surfaced lessons with staleness flags in place.
    /// Verified lessons with anchor_verification snapshots get `Some(true/false)`;
    /// unverified lessons stay `None`.
    pub fn apply_staleness(
        &self,
        lessons: &mut [SurfacedLesson],
        hash_resolver: &HashResolver<'_>,
    ) -> Result<()> {
        let verified_ids: Vec<&str> = lessons
            .iter()
            .filter(|l| l.verified)
            .map(|l| l.id.as_str())
            .collect();
        if verified_ids.is_empty() {
            return Ok(());
        }
        let stale_map = self.check_staleness(&verified_ids, hash_resolver)?;
        for lesson in lessons.iter_mut() {
            if lesson.verified {
                lesson.stale = stale_map.get(&lesson.id).copied().map(Some).unwrap_or(None);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Anchor hygiene
// ---------------------------------------------------------------------------

impl LessonsDb {
    /// Delete auto-generated `import_pattern` anchors whose root crate appears
    /// in more files than `cap`. Only touches generated anchors (pattern `<root>::*`).
    pub fn prune_high_freq_import_anchors(
        &self,
        freq: &std::collections::HashMap<String, usize>,
        cap: usize,
    ) -> Result<usize> {
        let roots: Vec<&String> = freq
            .iter()
            .filter(|&(_, &count)| count > cap)
            .map(|(root, _)| root)
            .collect();
        if roots.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let mut deleted = 0usize;
        for root in &roots {
            let pattern = format!("{root}::*");
            deleted += conn.execute(
                "DELETE FROM anchors WHERE kind = 'import_pattern' AND value = ?1",
                params![pattern],
            )?;
        }
        Ok(deleted)
    }
}

// ---------------------------------------------------------------------------
// Test support
// ---------------------------------------------------------------------------

impl LessonsDb {
    #[doc(hidden)]
    pub fn conn_for_test(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }
}
