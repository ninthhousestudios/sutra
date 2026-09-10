//! Lexical scoring for `explore` (sutra/371), ported from graft's `ask.ts`.
//!
//! One tokenizer feeds both index-thinking and query time, so a symbol's score
//! is a faithful function of the same text-splitting on both sides — the point
//! graft makes about the sidecar being a correct cache only when both halves
//! call the same code. Scoring is IDF-weighted so a token appearing across the
//! whole corpus (`handle`, `new`) weighs ~0 and can't dominate a two-word query
//! on raw term count. The body field (signature + docstring) is scored with
//! BM25 so a long definition can't win on bulk, and a multiplicative test-path
//! de-rank keeps a test that merely mirrors a symbol's tokens below the
//! definition it exercises.
//!
//! sutra stores no full symbol body_text, so the "body" field is signature +
//! docstring only — graft's "leading body lines" have no source here without a
//! reparse-scoped schema change, out of scope for the lexical seed.
//!
//! The query is represented purely by the [`IdfMap`]: its keys are the query's
//! distinct tokens and its values their IDF weights. Query term frequency is
//! binary (a repeated word counts once), so there is no separate query bag to
//! carry. Everything here is DB-free and pure — the caller (`explore::handle`)
//! supplies the IDF map (built via [`build_idf`], wired to the DB's
//! `fts_doc_frequency` and `symbol_count`) and the tokenized fields, so the
//! whole module is unit-testable without an index.

use std::collections::HashMap;

/// A token → term-frequency bag for one field.
pub(crate) type Bag = HashMap<String, u32>;

/// A query token → IDF weight map. Its keys are the query's distinct tokens
/// (the query term set); its values weight each by corpus rarity. Every query
/// token is present, including corpus-absent ones (which take the df=0 weight),
/// so the scorers never need a fallback default.
pub(crate) type IdfMap = HashMap<String, f64>;

/// Words too common/short to carry query intent — dropped before scoring.
/// Mirrors graft's STOP set.
const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "of", "to", "in", "is", "are", "how", "does", "do", "what", "where", "which",
    "that", "this", "it", "for", "on", "and", "or", "with", "i", "we", "get", "set", "use", "used",
    "using", "when", "why", "can",
];

/// Field weights: a name match is worth 3× a body match, a path match 2×
/// (graft's blend). The body field carries no extra multiplier — its BM25 score
/// already sits on a comparable scale.
const NAME_WEIGHT: f64 = 3.0;
const PATH_WEIGHT: f64 = 2.0;

/// How much a tight name match — the query accounts for most of the symbol's
/// name, not just a fragment of it — lifts the name component. This restores
/// the signal the old cheap-win formula got from its exact-short_name tier: a
/// query `parse` should rank a symbol *named* `parse` above one named
/// `parse_imports` that merely contains the token. At the max (the query covers
/// the whole name) the name component roughly doubles.
const EXACT_NAME_BONUS: f64 = 1.0;

/// Standard BM25 params. `k1` saturates repeated occurrences, `b` controls
/// length normalization against the corpus-average field length.
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

/// Multiplicative de-rank for a test-path symbol on a query that isn't about
/// tests. Tests still appear (they matter for "where are the tests"), just
/// below the definition they exercise. Graft's `TEST_RANK_PENALTY`.
pub(crate) const TEST_RANK_PENALTY: f64 = 0.35;

/// Split prose + identifiers into lowercased subword tokens (camelCase, snake,
/// kebab). The single source of truth for tokenization — the same function
/// tokenizes a symbol's fields for scoring and the incoming query, so the two
/// halves can never disagree on what a "token" is. camelCase is split only at a
/// lower/digit → upper boundary (matching graft's `([a-z0-9])([A-Z])`), so
/// `parseImports` → `parse imports` but a run of capitals like `HTMLParser`
/// stays one token. Tokens shorter than two chars and stop words are dropped.
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    // Insert a boundary before an uppercase that follows a lowercase/digit, so
    // the split below separates camelCase segments.
    let mut spaced = String::with_capacity(text.len() + 8);
    let mut prev: Option<char> = None;
    for c in text.chars() {
        if let Some(p) = prev
            && (p.is_ascii_lowercase() || p.is_ascii_digit())
            && c.is_ascii_uppercase()
        {
            spaced.push(' ');
        }
        spaced.push(c);
        prev = Some(c);
    }
    spaced
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| s.chars().count() > 1 && !STOP_WORDS.contains(&s.as_str()))
        .collect()
}

/// Term-frequency bag for a field's tokens. Consumes the token vec, moving each
/// string into the map key (no allocation beyond the map itself).
pub(crate) fn counts(tokens: Vec<String>) -> Bag {
    let mut m = Bag::new();
    for t in tokens {
        *m.entry(t).or_insert(0) += 1;
    }
    m
}

/// Inverse document frequency: `ln(1 + n/(1+df))`. Monotonically decreasing in
/// `df`, always positive, ~0 for a token present in nearly every document.
/// Matches graft's `idfFromDf`. `n` is the corpus size (total symbol count).
pub(crate) fn idf(df: i64, n: i64) -> f64 {
    (1.0 + n as f64 / (1.0 + df.max(0) as f64)).ln()
}

/// Build the query IDF map: the distinct tokens of `query`, each weighted by
/// `idf(df, n)`. `df_of` yields a token's corpus document frequency (the caller
/// wires it to `Db::fts_doc_frequency`); a token unseen in the corpus takes the
/// df=0 weight naturally. Empty when the query has no usable tokens after
/// tokenization — the caller treats that as "no searchable terms".
pub(crate) fn build_idf(query: &str, n: i64, mut df_of: impl FnMut(&str) -> i64) -> IdfMap {
    let mut map = IdfMap::new();
    for t in tokenize(query) {
        if map.contains_key(&t) {
            continue;
        }
        let weight = idf(df_of(&t), n);
        map.insert(t, weight);
    }
    map
}

/// Summed term-frequency of the field tokens a query token matches: the exact
/// case (`doc == t`) plus prefix matches (`doc` starts with `t`). This mirrors
/// the `"t"*` FTS retrieval, so the scorer credits exactly what recall
/// surfaced — a query `import` scores against a symbol whose token is `imports`
/// — and subsumes graft's singular/plural folding as a special case. Doc bags
/// are tiny (a name is a few tokens, a body a few dozen), so the scan is cheap.
fn prefix_tf(field: &Bag, t: &str) -> u32 {
    let mut total = 0;
    for (doc, &c) in field {
        if doc.starts_with(t) {
            total += c;
        }
    }
    total
}

/// Whether any field token matches the query token by the same prefix rule as
/// [`prefix_tf`] — the presence test behind coverage.
fn field_has_prefix(field: &Bag, t: &str) -> bool {
    field.keys().any(|doc| doc.starts_with(t))
}

/// Fraction (0..1) of the field's distinct tokens that some query token covers
/// (by the [`prefix_tf`] prefix rule). Its complement measures how much of the
/// name is NOT the query — the "tightness" of a name match, distinct from
/// coverage (which measures how much of the QUERY the field carries).
fn covered_fraction(field: &Bag, idf: &IdfMap) -> f64 {
    if field.is_empty() {
        return 0.0;
    }
    let covered = field
        .keys()
        .filter(|doc| idf.keys().any(|t| doc.starts_with(t)))
        .count();
    covered as f64 / field.len() as f64
}

/// IDF-weighted overlap of the query against a short field (name / path): each
/// query token contributes `matched_field_tf · idf`. Graft's `score()` with a
/// binary query, extended to the prefix match rule (see [`prefix_tf`]).
fn idf_overlap(idf: &IdfMap, field: &Bag) -> f64 {
    let mut s = 0.0;
    for (t, &w) in idf {
        let dn = prefix_tf(field, t);
        if dn > 0 {
            s += dn as f64 * w;
        }
    }
    s
}

/// Sum of a bag's counts — a field's length in tokens.
fn field_len(bag: &Bag) -> f64 {
    bag.values().map(|&v| v as f64).sum()
}

/// Body length (in tokens) of a symbol's fields — exposed so the caller can
/// compute the corpus-average body length (`avgdl`) BM25 normalizes against.
pub(crate) fn body_len(fields: &DocFields) -> f64 {
    field_len(&fields.body)
}

/// BM25 term score for the (potentially long) body field. Unlike raw term
/// frequency, it saturates repeated occurrences (`k1`) and normalizes by field
/// length against the corpus average (`b`, `avgdl`), so a sprawling definition
/// can't outrank a tight one merely by containing more words. Graft's `bm25()`.
fn bm25(idf: &IdfMap, doc: &Bag, dl: f64, avgdl: f64) -> f64 {
    let norm = BM25_K1 * (1.0 - BM25_B + BM25_B * dl / avgdl.max(1.0));
    let mut s = 0.0;
    for (t, &w) in idf {
        let tf = prefix_tf(doc, t);
        if tf > 0 {
            let tf = tf as f64;
            s += w * (tf * (BM25_K1 + 1.0)) / (tf + norm);
        }
    }
    s
}

/// IDF-weighted share (0..1) of the query's distinct terms matched across the
/// given fields — graft's `matchedIdfShare`. Each query term counts by its
/// rarity, so a prompt whose rare, discriminating tokens all hit scores near 1,
/// while one that only grazes common tokens scores low. The IDF map already
/// weights corpus-absent tokens with the df=0 weight, so an all-miss query
/// reads 0 without a separate default.
fn matched_idf_share(idf: &IdfMap, fields: &[&Bag]) -> f64 {
    let mut matched = 0.0;
    let mut total = 0.0;
    for (t, &w) in idf {
        total += w;
        if fields.iter().any(|f| field_has_prefix(f, t)) {
            matched += w;
        }
    }
    if total > 0.0 { matched / total } else { 0.0 }
}

/// One symbol's tokenized fields.
pub(crate) struct DocFields {
    /// short_name ∪ qualified_name tokens — the highest-weight field.
    pub name: Bag,
    /// File path tokens.
    pub path: Bag,
    /// signature + docstring tokens — the length-normalized body field.
    pub body: Bag,
}

/// Result of scoring one symbol against a query.
pub(crate) struct Scored {
    pub score: f64,
    /// IDF-weighted share of query terms matched across name + path + body — a
    /// relevance signal for the caller ("did the query's words land at all?").
    pub coverage: f64,
    /// IDF-weighted share matched in the NAME field ONLY — a match-STRENGTH
    /// signal ("did the query hit a high-value field, or only incidental body
    /// tokens?"). A body-only collision has `coverage_strong == 0` while
    /// `coverage` can still look respectable.
    pub coverage_strong: f64,
}

/// Score one symbol's lexical relevance to the query: IDF-weighted name and
/// path overlap plus BM25 over the body. `avgdl` is the corpus-average body
/// length driving BM25 normalization. This is the RAW lexical axis only — the
/// caller normalizes it against the candidate max, blends in the structural
/// (pagerank) prior, and applies the test-path de-rank to the blend (see
/// `explore::handle`), so a strong test can't be lifted back to the top by
/// normalization the way a pre-normalization penalty would allow.
pub(crate) fn score_doc(idf: &IdfMap, fields: &DocFields, avgdl: f64) -> Scored {
    // A tighter name match (the query covers more of the name) lifts the name
    // component, so `parse` ranks the symbol named `parse` above `parse_imports`.
    let name_tightness = 1.0 + EXACT_NAME_BONUS * covered_fraction(&fields.name, idf);
    let lexical = idf_overlap(idf, &fields.name) * NAME_WEIGHT * name_tightness
        + idf_overlap(idf, &fields.path) * PATH_WEIGHT
        + bm25(idf, &fields.body, field_len(&fields.body), avgdl);
    Scored {
        score: lexical,
        coverage: matched_idf_share(idf, &[&fields.name, &fields.path, &fields.body]),
        coverage_strong: matched_idf_share(idf, &[&fields.name]),
    }
}

/// Structural prior weight: how much a symbol's normalized global pagerank can
/// lift its normalized lexical score. Deliberately gentle (≈ the old cheap-win
/// formula's 0.15 structural share), NOT graft's 0.5 — graft's 0.5 weights a
/// QUERY-PERSONALIZED pagerank, whereas this global pagerank is query-
/// independent. The query-personalized re-rank that will supersede this global
/// prior with a query-relevant one is sutra/372; the seed keeps global pagerank
/// as the structural prior so dropping the old formula's structural term
/// doesn't sink ranking (eval confirms 0.5 beats a gentler 0.15 here).
pub(crate) const GRAPH_WEIGHT: f64 = 0.5;

/// Blend a symbol's normalized lexical score (0..1 over the candidate set) with
/// its normalized structural prior (global pagerank, 0..1), then apply the
/// test-path de-rank. `test_factor` multiplies the BLEND rather than the raw
/// lexical, so a test that is the strongest raw match isn't restored to the top
/// once lexical is renormalized to the candidate max (graft's observation).
pub(crate) fn blend(lexical_norm: f64, pagerank_norm: f64, test_factor: f64) -> f64 {
    (lexical_norm + GRAPH_WEIGHT * pagerank_norm) * test_factor
}

/// A query that ASKS about tests wants test files on top, so it gets no
/// de-rank; every other query wants the real definition first (graft's
/// query-aware `wantsTests`). Checked against the query's own tokens so it
/// fires on "test"/"tests"/"spec"/… as words, not as substrings of unrelated
/// identifiers.
pub(crate) fn wants_tests(query: &str) -> bool {
    const TEST_INTENT: &[&str] = &[
        "test",
        "tests",
        "spec",
        "specs",
        "coverage",
        "assert",
        "asserts",
        "assertion",
        "assertions",
        "fixture",
        "fixtures",
        "mock",
        "mocks",
    ];
    tokenize(query)
        .iter()
        .any(|t| TEST_INTENT.contains(&t.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bag(tokens: &[&str]) -> Bag {
        counts(tokens.iter().map(|s| s.to_string()).collect())
    }

    /// An IDF map doubling as the query term set — keys are the query tokens.
    fn idf_of(pairs: &[(&str, f64)]) -> IdfMap {
        pairs.iter().map(|(t, w)| (t.to_string(), *w)).collect()
    }

    #[test]
    fn tokenize_splits_camel_snake_kebab() {
        assert_eq!(tokenize("parseImports"), vec!["parse", "imports"]);
        assert_eq!(tokenize("parse_imports"), vec!["parse", "imports"]);
        assert_eq!(tokenize("parse-imports"), vec!["parse", "imports"]);
        assert_eq!(tokenize("workspace_root"), vec!["workspace", "root"]);
    }

    #[test]
    fn tokenize_lowercases_and_drops_short_and_stopwords() {
        // "The"/"of" are stop words; "a" is < 2 chars and a stop word.
        assert_eq!(tokenize("The Config of a Handle"), vec!["config", "handle"]);
        // single-char tokens dropped.
        assert_eq!(tokenize("x y ab"), vec!["ab"]);
    }

    #[test]
    fn tokenize_keeps_capital_run_together() {
        // No lower/digit → upper boundary inside a run of capitals.
        assert_eq!(tokenize("HTMLParser"), vec!["htmlparser"]);
    }

    #[test]
    fn idf_decreases_with_df_and_nears_zero_for_ubiquitous() {
        let n = 1000;
        assert!(idf(1, n) > idf(500, n));
        // A token in nearly every document weighs ~0.
        assert!(idf(999, n) < 0.75);
        // A rare token weighs heavily.
        assert!(idf(1, n) > 5.0);
    }

    #[test]
    fn build_idf_dedups_and_weights_absent_token_high() {
        // "config" seen once in a 100-doc corpus; "absent" never seen (df 0).
        let n = 100;
        let df = |t: &str| match t {
            "config" => 40,
            _ => 0,
        };
        let idf = build_idf("config Config absent", n, df);
        // "config"/"Config" collapse to one token.
        assert_eq!(idf.len(), 2);
        assert!(
            idf["absent"] > idf["config"],
            "an unseen token weighs the most"
        );
    }

    #[test]
    fn common_token_does_not_dominate_two_word_query() {
        // Two-word query "handle config": "handle" is common (low idf), "config"
        // is rare (high idf). A doc matching only the common word must score
        // below a doc matching only the rare word — the keyword-collision fix.
        let idf = idf_of(&[("handle", 0.2), ("config", 6.0)]);
        let avgdl = 4.0;

        let matches_common = DocFields {
            name: bag(&["handle", "request"]),
            path: Bag::new(),
            body: bag(&["handle", "request"]),
        };
        let matches_rare = DocFields {
            name: bag(&["config", "loader"]),
            path: Bag::new(),
            body: bag(&["config", "loader"]),
        };

        let common = score_doc(&idf, &matches_common, avgdl);
        let rare = score_doc(&idf, &matches_rare, avgdl);
        assert!(
            rare.score > common.score,
            "rare-token match {} should beat common-token match {}",
            rare.score,
            common.score
        );
    }

    #[test]
    fn blend_test_factor_deranks_below_equivalent_definition() {
        // Identical lexical + pagerank; only the test-path factor differs. The
        // penalty applies to the blend, so it can't be normalized away.
        let def = blend(1.0, 0.5, 1.0);
        let test = blend(1.0, 0.5, TEST_RANK_PENALTY);
        assert!(test < def);
        assert!((test - def * TEST_RANK_PENALTY).abs() < 1e-9);
    }

    #[test]
    fn blend_pagerank_breaks_a_lexical_tie() {
        // Two equal lexical matches; the more central (higher pagerank) wins.
        let central = blend(1.0, 1.0, 1.0);
        let isolated = blend(1.0, 0.0, 1.0);
        assert!(central > isolated);
    }

    #[test]
    fn coverage_strong_zero_on_body_only_match() {
        // Query term hits the body but not the name → coverage positive,
        // coverage_strong zero (the match-strength distinction).
        let idf = idf_of(&[("widget", 3.0)]);
        let fields = DocFields {
            name: bag(&["render", "loop"]),
            path: Bag::new(),
            body: bag(&["builds", "widget", "tree"]),
        };
        let scored = score_doc(&idf, &fields, 4.0);
        assert!(scored.coverage > 0.0);
        assert_eq!(scored.coverage_strong, 0.0);
    }

    #[test]
    fn coverage_full_when_all_terms_in_name() {
        let idf = idf_of(&[("parse", 3.0), ("imports", 4.0)]);
        let fields = DocFields {
            name: bag(&["parse", "imports"]),
            path: Bag::new(),
            body: Bag::new(),
        };
        let scored = score_doc(&idf, &fields, 1.0);
        assert!((scored.coverage - 1.0).abs() < 1e-9);
        assert!((scored.coverage_strong - 1.0).abs() < 1e-9);
    }

    #[test]
    fn coverage_zero_when_nothing_matches() {
        let idf = idf_of(&[("absent", 5.0)]);
        let fields = DocFields {
            name: bag(&["other"]),
            path: Bag::new(),
            body: bag(&["misc"]),
        };
        let scored = score_doc(&idf, &fields, 2.0);
        assert_eq!(scored.coverage, 0.0);
        assert_eq!(scored.coverage_strong, 0.0);
    }

    #[test]
    fn wants_tests_fires_on_word_not_substring() {
        assert!(wants_tests("where are the tests"));
        assert!(wants_tests("spec coverage for parser"));
        assert!(wants_tests("mock the client"));
        // "attest" contains "test" as a substring but is not test-intent.
        assert!(!wants_tests("attest signature"));
        assert!(!wants_tests("parse imports"));
    }

    #[test]
    fn bm25_saturates_repeated_terms() {
        // Doubling a term's frequency less-than-doubles its BM25 contribution.
        let idf = idf_of(&[("token", 3.0)]);
        let once = DocFields {
            name: Bag::new(),
            path: Bag::new(),
            body: bag(&["token"]),
        };
        let many = DocFields {
            name: Bag::new(),
            path: Bag::new(),
            body: counts(vec!["token".to_string(); 8]),
        };
        let s1 = score_doc(&idf, &once, 4.0).score;
        let s8 = score_doc(&idf, &many, 4.0).score;
        assert!(s8 > s1, "more occurrences still score higher");
        assert!(s8 < s1 * 8.0, "but sub-linearly (BM25 saturation)");
    }
}
