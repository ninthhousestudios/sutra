-- Drop health_waivers (sutra/474). User-authored biomarker waivers go with the
-- biomarkers they waived; there is nothing left for them to suppress.
--
-- NOT ephemeral_only: health_waivers is Durable and reindex does not drop it,
-- so 0028 never replays and this must not either.
DROP TABLE IF EXISTS health_waivers;
