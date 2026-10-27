# Dependency security policy

The project uses Rust 1.93.0, a committed Cargo.lock and current advisory data.
Audits use cargo-deny 0.20.2 and cargo-audit 0.22.2, both compatible with Rust
1.93. Older tool versions could not parse current CVSS4 advisories; replacing
the parser does not remove or suppress those advisories.

## Two named, reviewed exceptions

Engineering decision: 2026-10-27. Review before any affected dependency upgrade
and no later than **2026-11-26**. These are accepted findings, not vulnerability
fixes and not a claim that the unmodified full-lock audit is clean.

| Advisory | Exact scope and rationale | Guard |
| --- | --- | --- |
| [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436.html) | `paste 1.0.15` is an unmaintained proc-macro used by `tokenizers 0.23.2` during compilation. The advisory is informational non-maintenance; it does not announce an exploitable vulnerability. | Frozen version and registry checksum; review Tokenizers/Paste changes and new advisories before upgrading. Only this advisory ID is accepted. |
| [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html) | `rsa 0.9.10` remains in Cargo.lock through the optional SQLx-MySQL facade. RSA has no published fixed version. It is absent from the graph actually selected by this workspace, including all its features and platforms. | cargo-deny explicitly **bans every version of `rsa`** in the selected graph. Any future enabling of MySQL/RSA makes CI fail, regardless of the named full-lock audit exception. |

Paste's locked registry checksum is
`57c0d7b74b563b49d38dae00a0c37d4d6de9b432382b2892f0574ddcae73fd0a`.
The compiler fetches the exact archive recorded by Cargo.lock. This checksum is
an integrity pin; it does not establish absence of malicious or defective code.

`cargo audit` examines the full lockfile, including uncompiled optional packages.
Its CI invocation explicitly names these two IDs. `cargo deny` accepts only the
Paste advisory and separately enforces the RSA ban. No general warning,
vulnerability, package or advisory-database suppression is configured. Other
advisories and denied warnings remain failures.

## Reproduce the decision

```console
cargo tree --workspace --all-features --target x86_64-unknown-linux-gnu --locked -i rsa
cargo tree --workspace --all-features --target all --locked -i rsa
cargo deny --all-features check
cargo audit --deny warnings --ignore RUSTSEC-2024-0436 --ignore RUSTSEC-2023-0071
```

The two tree queries must report no selected reverse dependency. cargo-deny has
no platform filter, so it evaluates every platform, with every declared workspace
feature enabled. New advisories, a changed dependency graph or changed source
checksums require review. A permissive license check does not replace distribution
notices; precompiled candidates remain gated on actual third-party notices.

See [ADR-001](adr/001-dependency-advisory-exceptions.md) for alternatives and
reconsideration criteria. Keep audit logs and actual execution results separate
from this policy document. Review date is a human obligation; no automatic expiry
of these two CLI exception IDs is claimed.
