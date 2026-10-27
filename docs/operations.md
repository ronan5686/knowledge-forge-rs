# Operations

This is a runbook and activation checklist, not a declaration that every item is
installed, connected, tested or enabled. Record actual results separately.

## Independent paths

| Path | Purpose |
| --- | --- |
| `/srv/hermes-connaissance/code` | Public source checkout |
| `/srv/hermes-connaissance/bin` | Root-owned current executable |
| `/srv/hermes-connaissance/releases` | Immutable release binaries and hashes |
| `/srv/hermes-connaissance/data` | Private source archive and derived documents |
| `/srv/hermes-connaissance/data/models` | Approved model assets and licenses |
| `/srv/hermes-connaissance/postgres` | Dedicated PostgreSQL data, never copy live |
| `/etc/hermes-connaissance` | Private configuration and credential files |
| `/run/hermes-connaissance/query` | Reader socket, group `kf-readers` |
| `/run/hermes-connaissance/curator` | Curator socket, group `kf-curators` |

No runtime secret belongs in the public checkout. Private environment files are
root-owned mode 0600 and read by the service manager. Fixed secrets may use
systemd credentials; a refreshed OAuth token additionally needs durable private
storage writable only by ingestion. Do not print URLs containing passwords.

## Provision and promote

1. Verify disk/RAM headroom and absence of the chosen loopback port.
2. Build `deploy/postgres/Dockerfile`; retain its resulting image ID.
3. Start only `deploy/compose.yml`, project `knowledge-forge`. Its only port is
   `127.0.0.1:55433`; PostgreSQL is limited to one CPU and 3 GiB RAM.
4. Execute `deploy/postgres/bootstrap.sql` once in the fresh dedicated cluster.
   Generate unique passwords privately. Bootstrap SQL contains no password.
5. Run `knowledge-forge migrate` with the separate migrator configuration. It
   must SET ROLE to `kf_owner` for the connection creating schema objects.
6. Review the actual migration tables, then apply explicit runtime privileges.
   Prove reader cannot write and curator cannot mutate source documents.
7. Install the binary and declarative systemd units. Create runtime directories
   from `deploy/systemd/knowledge-forge.conf`. Verify units before starting.
8. Start the query service only after models, manifests and permissions pass.
   Keep the worker disabled until ingestion and Google-mutation gates pass.
9. Test thin MCP clients under their real non-root user identities. Never infer
   OS isolation from a profile name or a successful test as root.

The Compose file does not control MAVIE production. Never run another project's
Compose file to deploy this service. Do not give application users Docker access.
No unattended migration runs on service startup.

## Roles

The owner cannot log in. The migrator owns no long-lived service. Ingest, reader
and curator have separate passwords and connection limits. `kf_reader` has
read-only transactions and only explicit SELECT grants. Authentication to other
databases is denied within this dedicated cluster. Permissions on future tables
must be reviewed after migrations, not guessed from their name.

## Backup and restore

Back up a coherent database snapshot, immutable files referenced by that snapshot,
schema version, release hash and a file-hash manifest. Database dumps do not cover
external files or PostgreSQL roles. Keep credential backups separate and encrypted.
Do not put live PostgreSQL data-directory copies into a file backup and call them
consistent database backups. Before garbage collection, retain files referenced
by every retained database snapshot.

For SQLite migration, use its online backup API or a controlled quiescent export.
Do not blindly copy an active database without its WAL/transaction context.

A restore must target a separate database and directory. Verify row counts,
foreign keys, immutable hashes, the last document segment, lexical search and
embedding generation; then test the real query client. Restore does not replay
external writes or reactivate a worker. Only one writer resumes after approval.
An off-host destination is necessary to survive a server or disk loss. Reusing
existing storage can avoid a new subscription; software being free does not create
free off-host storage. A local-only snapshot must be described as local-only.

The `backup` command uses the existing authorized migrator connection for a
read-only dump; it does not require a new persistent backup login. Its document
repository is restricted to `rclone:storage-share:knowledge-forge/restic`, and
its independent encrypted secrets repository to the `knowledge-forge/secrets`
prefix of the same existing remote. The latter contains only this project's
private configuration and ingest credentials, with recovery keys excluded.
Both keys must have an explicitly authorized, protected copy off the server;
creating a key locally is not proof that this recovery copy exists.

`backup --restore-test` restores into the exact disposable database
`kf_test_restore` on `127.0.0.1:55433`, preserves the preinstalled vector
extension, verifies file hashes and database counts/constraints, and never
reactivates credentials or a writer. The separate application read test is
still required. Local staging is retained deliberately; no automatic deletion
or unbounded retention policy is claimed. Initial repository creation requires
the private operator flag `KF_RESTIC_ALLOW_INIT=1`.

The supplied daily timer is a disabled deployment template. A successful manual
backup does not mean the timer is enabled. Enable it only after the off-site
restore exercise and protected off-host recovery-key copies are verified.
The root-only backup operator reuses the existing private rclone configuration;
it is separate from the non-root query and ingestion services.
Its only Linux capability is `CAP_DAC_READ_SEARCH`, needed to read the ingest
user's private credential directory. It does not have `CAP_DAC_OVERRIDE`;
the sandbox permits writes only inside the dedicated backup directory.

References: [PostgreSQL backup](https://www.postgresql.org/docs/current/backup-dump.html),
[SQLite backup](https://www.sqlite.org/backup.html),
[Restic restoration](https://restic.readthedocs.io/en/stable/050_restore.html).

## CI and release

Rust 1.93.0 and Cargo.lock are fixed. The pull-request workflow defines rustfmt,
Clippy, unit tests, native inference compilation, a required synthetic PostgreSQL
job, license/source/security checks and secret scanning. This definition is not
proof that a hosted CI run has succeeded. Explicit database integration execution
must fail without its test URL. Audits use current advisories; suppressions require
a documented reason and expiry. The separate `security.yml` defines a weekly
audit trigger; its presence is not proof of a successful scheduled execution.
Release promotion requires hashes, an SBOM and build provenance.
Run bounded parser/cursor/path fuzz tests before promoting new ingestion formats;
the ordinary workflow is not a claim that a long fuzz campaign has run.

Use GitHub-hosted standard runners for a public repository. Do not attach the
production server as an unrestricted public-PR self-hosted runner. Default token
permissions are read-only; actions are pinned to commit hashes. Release jobs do
not deploy production or receive Google credentials. Artifact retention is bounded.
Public standard runner compute is free under GitHub's documented policy; larger
runners, artifact storage and other services have separate limits.

## Acceptance and rollback

Migration requires a tested restore, unchanged originals, explicit count/hash
reconciliation, correct account/channel lock, no duplicate playlists, negative
permission tests, full paginated reads and unchanged neighboring services.
Measure acquisition, parsing, archive, indexing, retrieval and model time separately.
Do not announce embedding/reranker quality before running its actual evaluation.

Keep the old system read-only during the comparison. Pause the sole YouTube
scheduler before taking the final delta. Avoid dual writing. A rollback after
new writes requires state reconciliation, not only switching an executable link.
