# Knowledge Forge

An independent Rust document library and bounded MCP integration for Hermes.
Original sources remain immutable; search results and wiki proposals retain
versioned evidence. The public code contains no production corpus or credentials.

## Scope

- deterministic document ingestion and complete paginated reading;
- PostgreSQL lexical/vector search with embedding-generation separation;
- source-backed proposals, durable jobs and explicit availability states;
- local Unix-socket query service and role-specific MCP stdio clients;
- operator commands for migration, qualification and recovery.

Hermes remains the external orchestrator. The service does not modify Optimus or
merge audit and marketing agents. Runtime capabilities must be verified; source
code, a configured adapter and a successful production connection are different
states. This README makes no claim of 100% source coverage or measured model quality.

## Build

Use Rust 1.93.0 and the committed Cargo.lock:

```console
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo check -p kf-app --features inference --locked
cargo build -p kf-app --release --features inference --locked
```

Copy `.env.example` only for local synthetic development, then supply private
configuration outside the checkout. Never commit account addresses, OAuth files,
transcripts, databases, logs or model caches. `deploy/compose.yml` starts only a
new independent PostgreSQL instance; its bootstrap and passwords require explicit
operator preparation described in the runbook.

The `inference` feature compiles the local ONNX adapter. Without it, model
capabilities remain unavailable; a successful build alone does not enable them.
Model weights are obtained separately, with pinned revisions, hashes and licenses.

## Synthetic qualification tools

```console
cargo bench -p kf-ingest --bench conversion -- --test
cargo +nightly fuzz run ingest_formats -- -max_total_time=30 -runs=1000 -max_len=65536
```

The Criterion cases exercise parsing/rendering and full UTF-8 pagination, not
embedding speed or retrieval quality. The fuzz command requires separately
installed `cargo-fuzz` and a supported nightly/Clang environment. Merely compiling
the target is not an executed fuzz campaign. The optional manual coverage workflow
measures synthetic tests; it neither downloads the private corpus nor certifies
model accuracy. Actual campaign and benchmark results must be recorded separately.
The qualification tool version is
[cargo-fuzz 0.13.1](https://crates.io/crates/cargo-fuzz/0.13.1), following the
[official Rust fuzzing guide](https://rust-fuzz.github.io/book/cargo-fuzz.html).

## Documentation

- [Architecture and trust boundaries](docs/architecture.md)
- [Operator runbook and activation gates](docs/operations.md)
- [Internal module contract](docs/CONTRACT.md)

## License

Original code: [Apache-2.0](LICENSE). Third-party code, model weights and documents
retain their own licenses; see [NOTICE](NOTICE) and each release's SBOM.

This repository distributes source code. Building from source requires the
documented native dependencies. A precompiled distributable must additionally
include the required third-party license and copyright notices, including native
ONNX Runtime components; an SBOM and this project's NOTICE alone are insufficient.
The manual release workflow prepares a candidate for review and does not publish a
GitHub release or production deployment automatically.
