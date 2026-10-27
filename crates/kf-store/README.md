# Versioned store

`migrate()` is an explicit operator action. It requires PostgreSQL 16+ and pgvector
0.8.7+, applies transactional SQLx migrations, and optionally uses the fixed
`kf_owner` role when `KF_MIGRATION_ROLE=kf_owner`. The same acquired connection
sets and resets that role. Apply `roles.sql` separately after provisioning roles.
No credentials are included here.

Document versions and chunks are immutable. An incomplete capture remains in
history and cannot replace an existing complete current document. Reusing a hash
with different payload or chunk boundaries is rejected. Search sees only current
versions, while a proposal can cite any retained immutable version. Citation
verification establishes exact UTF-8 byte ranges and quotations, not truth.

Lexical search uses PostgreSQL `ts_rank_cd` with simple, English and French
configurations. Semantic search uses 1,024-dimensional cosine vectors isolated
by generation. At most 2,048 matching vectors use an exact materialized set;
larger sets use the HNSW-eligible query with strict iterative scans. PostgreSQL
chooses the actual plan: an installed index is not proof that a particular query
used it. `status()` reports installation and counts, not fabricated benchmarks.

Queue leases last five minutes. The caller must provide a **unique worker string
per execution/lease**, including a run UUID; reusing a worker string while an old
execution can still respond is unsupported. Finishing requires the same active,
unexpired owner. Expired local jobs retry up to three attempts; jobs whose kind
starts with `google.` or `external.` enter `needs_reconciliation`, and are never
automatically replayed. This API does not assert exactly-once external effects.

Run unit tests with `cargo test -p kf-store`. PostgreSQL integration is explicit:
set `KF_TEST_DATABASE_URL` to a disposable database whose name contains `kf_test`,
then run `cargo test -p kf-store --test postgres -- --ignored`. Missing or invalid
test database configuration fails; no integration test silently reports success.
