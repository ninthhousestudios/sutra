-- Total paths a commit touched, indexed or not (sutra/476). commit_files holds
-- only indexed files, so a 99-file vendor sync with 23 indexed files read as a
-- 23-file co-edit and slipped under the cochange fan-out cap. NULL = rows
-- ingested before this column existed; the cochange query falls back to the
-- indexed count for them until the next history ingest rewrites the table.
--
-- ephemeral_only: commits is Ephemeral. On reindex, 0025 recreates it and this
-- replays to re-add the column.
ALTER TABLE commits ADD COLUMN file_count INTEGER;
