//! The lexical-search tokenizer — the single source of truth for splitting
//! prose and identifiers into subword tokens (sutra/371, sutra/394).
//!
//! This lives at the crate root, not under `tools/`, because BOTH halves of the
//! lexical index must call it: the index-time writer in `db` (which fills
//! `symbols_fts.lex_tokens`) and the query-time scorer in `tools::explore`. A
//! `tools` home would violate the `db-no-tools` layer boundary; keeping one
//! function here is what makes the FTS index a *correct* cache of what the
//! scorer will re-derive, rather than two tokenizers that silently disagree.
//!
//! The disagreement this prevents is concrete: SQLite's default `unicode61`
//! FTS tokenizer does not split camelCase, so `RequestContext` indexes as the
//! single token `requestcontext` and a prefix query `"context"*` never matches
//! it. Pre-tokenizing every indexed field through this function (into
//! `lex_tokens`) and querying that column makes an interior component like
//! `context` independently findable (sutra/394).

/// Words too common/short to carry query intent — dropped before scoring.
/// Mirrors graft's STOP set.
const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "of", "to", "in", "is", "are", "how", "does", "do", "what", "where", "which",
    "that", "this", "it", "for", "on", "and", "or", "with", "i", "we", "get", "set", "use", "used",
    "using", "when", "why", "can",
];

/// Split prose + identifiers into lowercased subword tokens (camelCase, snake,
/// kebab). The single source of truth for tokenization — the same function
/// tokenizes a symbol's fields at index time (`db`'s `lex_tokens` writer) and
/// the incoming query (`tools::explore`), so the two halves can never disagree
/// on what a "token" is. camelCase is split only at a lower/digit → upper
/// boundary (matching graft's `([a-z0-9])([A-Z])`), so `parseImports` →
/// `parse imports` but a run of capitals like `HTMLParser` stays one token.
/// Tokens shorter than two chars and stop words are dropped.
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tokenize_isolates_trailing_camel_component() {
        // The sutra/394 case: an interior/trailing component is its own token,
        // so a prefix query `context*` can reach `RequestContext`.
        assert_eq!(tokenize("RequestContext"), vec!["request", "context"]);
    }
}
