-- Add a `lex_tokens` column to symbols_fts so explore's lexical retrieval can
-- reach an interior camelCase component of a name/signature/docstring (sutra/394).
--
-- The problem: symbols_fts uses SQLite's default `unicode61` tokenizer, which
-- does NOT split camelCase. `RequestContext` is therefore stored as the single
-- token `requestcontext`, and a prefix query `"context"*` never matches it — so
-- a symbol findable only by a non-leading component was invisible to explore,
-- and its document frequency read 0 (inflating its IDF). See migration 0069,
-- which added `signature` but inherited this tokenizer mismatch.
--
-- The fix: a dedicated `lex_tokens` column holding every searchable field
-- pre-split by the shared Rust tokenizer (crate::lexical_tokenize::tokenize) and
-- space-joined, so unicode61 re-splitting it is a no-op and `context` is its own
-- token. explore queries `{lex_tokens}`; find/lookup keep querying the raw
-- name columns, so their contract is untouched (the sutra/371 column-scoping).
--
-- FTS5 cannot `ALTER TABLE ... ADD COLUMN`, so widen by drop + recreate.
-- ephemeral_only=true: symbols_fts is Ephemeral (reindex drops it and 0001
-- recreates the base shape), so this must replay to re-add the column — same
-- reasoning as 0069.
--
-- The table is left EMPTY on purpose (no INSERT..SELECT): unlike the raw
-- columns, `lex_tokens` cannot be filled in pure SQL — it needs the Rust
-- tokenizer. Db::open_unchecked repopulates symbols_fts from the durable
-- `symbols` rows immediately after migrations run (rebuild_symbols_fts_if_stale,
-- gated on a symbols/symbols_fts row-count mismatch), reading only the symbols
-- table — no file reparse (cheaper than the 0054/0055 content_hash resets, which
-- had no durable source to rebuild from). On a fresh index / reindex, symbols is
-- empty here and the parse pass fills both raw columns and lex_tokens via the
-- insert sites.
DROP TABLE IF EXISTS symbols_fts;

CREATE VIRTUAL TABLE symbols_fts USING fts5(
    symbol_id UNINDEXED,
    short_name,
    qualified_name,
    docstring,
    signature,
    lex_tokens
);
