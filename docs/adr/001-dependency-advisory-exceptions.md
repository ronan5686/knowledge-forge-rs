# ADR-001: bounded dependency advisory exceptions

Status: accepted engineering decision, 2026-10-27. Review by 2026-11-26 and before
any affected upgrade. Owner: project engineering.

## Context

The selected stack is Rust 1.93.0, SQLx 0.8.6 and Tokenizers 0.23.2. Actual
current-database audits reported two findings: Paste non-maintenance and the
uncompiled SQLx-MySQL RSA lock entry. The initial audit tools also failed on
CVSS4; official compatible tools were updated instead of using an old database.

## Decision

Retain the selected application stack. Accept only the two advisory IDs described
in [the security policy](../security.md), with visible reasons, exact versions and
the Paste checksum. Enforce a separate all-features, all-platforms cargo-deny ban
on `rsa`; acceptance of a lock entry never authorizes its compilation or use.
Preserve all other advisory, source and license controls.

## Alternatives considered

- SQLx 0.9.0 declares Rust 1.94.0, outside this service's current 1.93.0 target.
  A future toolchain migration requires a separately qualified change.
- Replacing Tokenizers with its current 1.0.0 development branch is not a proven
  stable drop-in replacement; FastEmbed also depends on the selected tokenizer.
- Vendoring a fork solely to replace Paste would add an application maintenance
  and review burden. No such fork is silently substituted.
- Declaring the raw audit clean or globally disabling warnings is rejected.

## Consequences and reconsideration

The raw full-lock audit continues to report these accepted findings. Published
results must say that exceptions were applied. The decision does not fix RSA's
timing issue or restore maintenance of Paste. Any compiled RSA dependency, new
advisory, upstream maintained alternative, changed pin/checksum or review deadline
triggers reconsideration. Precompiled binary distribution still requires review
and inclusion of native and Rust dependency license notices.

## Primary references

- https://rustsec.org/advisories/RUSTSEC-2024-0436.html
- https://rustsec.org/advisories/RUSTSEC-2023-0071.html
- https://crates.io/crates/sqlx/0.9.0
- https://github.com/huggingface/tokenizers/blob/main/tokenizers/Cargo.toml
