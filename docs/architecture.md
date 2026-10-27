# Architecture

Knowledge Forge is an independent document service. Hermes remains the external
agent orchestrator. Rust owns bounded ingestion, immutable source preservation,
search, proposals, durable jobs, and operator commands. PostgreSQL and native
model runtimes are maintained external dependencies, not rewritten components.

## Modules

| Module | Responsibility | Exclusions |
| --- | --- | --- |
| `kf-core` | Domain types, errors, invariants | Database, network, model calls |
| `kf-ingest` | Format parsing, source offsets, chunking, immutable files | Google credentials, SQL, generation |
| `kf-store` | PostgreSQL migrations, FTS, vector generations, jobs, evidence | Downloads and Google calls |
| `kf-app` | CLI, inference adapters, Unix HTTP, MCP stdio, operator workflows | Arbitrary shell or SQL tools |

Read [CONTRACT.md](CONTRACT.md) for interfaces. A source is untrusted documentary
content, never executable instructions. Full conversion preserves every source
segment; it does not certify the transcription against the recording.

## Storage and evidence

Sources and document versions have immutable hashes. Reimports cannot replace a
complete version with partial text. Derived chunks, embeddings and wiki proposals
refer to a specific version and offsets; changing an embedding model produces a
new generation. Lexical and semantic results remain traceable to original text.
No model-generated wiki page is silently promoted to verified source material.

PostgreSQL 16.15 and pgvector 0.8.7 run in the independent Compose project
`knowledge-forge`. The versioned Dockerfile uses the official PostgreSQL image
digest and pgvector commit `f37c13f68b57d2c3472b2214fbcff699d6d34876`.
Build-time Debian packages still receive repository updates; the build is pinned
at source/image level, not claimed bit-for-bit reproducible.

## Roles and local trust boundaries

`kf_owner` is NOLOGIN. The separate migrator temporarily acts as owner. Runtime
roles are not owners or superusers. Reader is read-only; ingest and curator get
explicit table grants following schema review, not blanket creation privileges.
No schema or role in another PostgreSQL cluster is changed.

System users `kf-ingest`, `kf-query`, `kf-reader`, and `kf-curator` separate the
worker, query service and thin MCP clients. Only the ingest service receives
YouTube OAuth material. Query receives reader/curator database access, not Google
credentials. Reader and curator clients call separate Unix sockets protected by
directory groups. A curator identity may read and propose; a reader cannot reach
the curator socket. No public HTTP MCP endpoint is needed.

Local stdio does not itself change UID. Launching a stdio child from a root Hermes
would still require an explicit privilege boundary. Existing root administrators
on the same host remain privileged; this is not VM isolation.

## Concurrency and failure

Google mutation work has one writer and idempotency keys. An uncertain response
requires reconciliation before repetition. Job leases and retry limits survive
restarts. Source availability, API quotas and credentials are distinct states.
No network blockade is repaired merely by rewriting a connector in Rust.

Only new services use resource limits. Their initial limits are conservative,
and disk headroom must be checked before model downloads and release builds.
The existing MAVIE, marketing, audit and Optimus services are outside this project.

## Open source boundary

The public repository contains code, declarative deployment files and synthetic
fixtures. Production tokens, accounts, transcripts, caches, models, databases,
logs and private reports stay outside Git. Dependencies and model weights retain
their own licenses. Apache-2.0 applies to original application code.

## References

- [PostgreSQL 16.15](https://www.postgresql.org/docs/release/16.15/)
- [pgvector 0.8.7](https://github.com/pgvector/pgvector/releases/tag/v0.8.7)
- [systemd sandboxing](https://freedesktop.org/software/systemd/man/latest/systemd.exec.html)
- [MCP transports](https://modelcontextprotocol.io/specification/latest/basic/transports)

Pin the MCP revision implemented by the installed Hermes and Rust SDK together.
The 2026-07-28 revision changed HTTP session and stream behavior; do not infer
compatibility from the word “latest”.
