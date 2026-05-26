# SQLite storage with ephemeral/durable partition

Sutra uses a single SQLite file per workspace, but the schema is partitioned
into ephemeral tables (symbols, edges, parse artifacts — recomputable from
code) and durable tables (constraints, boundaries, convention promotions,
waivers, aliases — human intent). The partition is a first-class concept:
ephemeral tables can be dropped and rebuilt; durable tables require proper
migrations and are the only data worth backing up.

We considered adding specialized stores (vector DB for HRR similarity,
time-series DB for health trends) but rejected them. At single-codebase
scale (thousands of symbols), SQLite handles vector scans and timestamped
rows without strain. The operational simplicity of one file outweighs the
ergonomic benefits of purpose-built stores.

The alternative of treating all tables uniformly was rejected because it
hides a critical distinction: losing ephemeral data costs a re-index (seconds
to minutes), losing durable data loses the human's architectural decisions
(irreplaceable without re-entering them).
