-- String literal occurrences, extracted at parse time (sutra/494). The
-- sibling-pattern advisory narrows its survivor search for literal lists and
-- SQL prefixes to the files holding a literal, instead of reading every file.
--
-- text is an index key: the literal normalized and cut to 40 characters
-- (parser::literals::PREFIX_CHARS). Rust and Dart only.
--
-- ephemeral_only: extraction data, rebuilt by a reparse like refs.
CREATE TABLE IF NOT EXISTS string_literals (
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    line    INTEGER NOT NULL,
    text    TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_string_literals_text ON string_literals(text, file_id);
CREATE INDEX IF NOT EXISTS idx_string_literals_file ON string_literals(file_id);
