//! Private operator backup. Source writers share advisory lock 779231056.
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;
use uuid::Uuid;

const REPOSITORY: &str = "rclone:storage-share:knowledge-forge/restic";
const SECRET_REPOSITORY: &str = "rclone:storage-share:knowledge-forge/secrets";
const LOCK: i64 = 779231056;

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    application_version: String,
    created_at: String,
    database_counts: Value,
    files: BTreeMap<String, String>,
    source_bytes: u64,
    secrets_included: bool,
}

fn safe_path(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "backup paths must be absolute");
    ensure!(
        !path.components().any(|c| matches!(c, Component::ParentDir)),
        "parent path components are forbidden"
    );
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        if let Ok(meta) = fs::symlink_metadata(&current) {
            ensure!(
                !meta.file_type().is_symlink(),
                "symlinks are forbidden in backup paths"
            );
        }
    }
    Ok(())
}

fn private_dir(path: &Path) -> Result<()> {
    safe_path(path)?;
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(hex::encode(hash.finalize()))
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    base: &Path,
    files: &mut BTreeMap<String, String>,
    bytes: &mut u64,
) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    safe_path(source)?;
    private_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(!kind.is_symlink(), "source archive contains a symlink");
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target, base, files, bytes)?;
        } else {
            ensure!(kind.is_file(), "source archive contains a special file");
            *bytes = bytes
                .checked_add(entry.metadata()?.len())
                .context("backup byte counter overflow")?;
            ensure!(
                *bytes <= 20 * 1024 * 1024 * 1024,
                "backup staging exceeds 20 GiB safety limit"
            );
            ensure!(
                files.len() < 250_000,
                "backup file count exceeds safety limit"
            );
            fs::copy(entry.path(), &target)?;
            let relative = target
                .strip_prefix(base)?
                .to_str()
                .context("backup filename is not UTF-8")?
                .replace('\\', "/");
            files.insert(relative, hash_file(&target)?);
        }
    }
    Ok(())
}

async fn execute(mut command: Command, label: &'static str, timeout: Duration) -> Result<Vec<u8>> {
    command.kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| anyhow!("{label} timed out"))?
        .map_err(|_| anyhow!("{label} could not start"))?;
    ensure!(
        output.status.success(),
        "{label} failed; no provider diagnostic is printed because it may contain credentials"
    );
    Ok(output.stdout)
}

fn decoded_uri_component(value: &str) -> Result<String> {
    let mut output = Vec::with_capacity(value.len());
    let mut bytes = value.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|b| (b as char).to_digit(16));
            let low = bytes.next().and_then(|b| (b as char).to_digit(16));
            let (Some(high), Some(low)) = (high, low) else {
                return Err(anyhow!("invalid encoded private database component"));
            };
            output.push((high * 16 + low) as u8);
        } else {
            output.push(byte);
        }
    }
    String::from_utf8(output).map_err(|_| anyhow!("private database component is not UTF-8"))
}

fn postgres(program: &str, url: &str) -> Result<Command> {
    let parsed =
        url::Url::parse(url).map_err(|_| anyhow!("invalid private PostgreSQL configuration"))?;
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env(
            "PGHOST",
            parsed.host_str().context("database host is missing")?,
        )
        .env("PGPORT", parsed.port().unwrap_or(5432).to_string())
        .env("PGUSER", decoded_uri_component(parsed.username())?)
        .env(
            "PGPASSWORD",
            decoded_uri_component(parsed.password().unwrap_or_default())?,
        )
        .env(
            "PGDATABASE",
            decoded_uri_component(parsed.path().trim_start_matches('/'))?,
        )
        .env("PGCONNECT_TIMEOUT", "15");
    Ok(command)
}

fn restic(password_file: &Path) -> Command {
    let mut command = Command::new("/usr/bin/restic");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/root")
        .env(
            "RESTIC_CACHE_DIR",
            "/srv/hermes-connaissance/backups/restic-cache",
        )
        .env("RESTIC_REPOSITORY", REPOSITORY)
        .env("RESTIC_PASSWORD_FILE", password_file)
        .env("RCLONE_CONFIG", "/root/.config/rclone/rclone.conf");
    command
}

fn secret_restic(password_file: &Path) -> Command {
    let mut command = restic(password_file);
    command.env("RESTIC_REPOSITORY", SECRET_REPOSITORY);
    command
}

async fn secret_backup(root: &Path, test_restore: bool) -> Result<Value> {
    let password = PathBuf::from(
        std::env::var("KF_RESTIC_SECRETS_PASSWORD_FILE")
            .context("independent private secrets-repository password file is required")?,
    );
    safe_path(&password)?;
    ensure!(
        password.is_file(),
        "independent secrets-repository key is missing"
    );
    let main_password = PathBuf::from(std::env::var("KF_RESTIC_PASSWORD_FILE")?);
    ensure!(
        fs::read(&password)? != fs::read(&main_password)?,
        "document and secrets repositories require independent recovery keys"
    );
    let stage = root.join(format!("secrets-{}", Uuid::new_v4()));
    private_dir(&stage)?;
    let mut files = BTreeMap::new();
    let mut total = 0;
    let config_source = Path::new("/etc/hermes-connaissance");
    let config_target = stage.join("private-config");
    private_dir(&config_target)?;
    for entry in fs::read_dir(config_source)? {
        let entry = entry?;
        let path = entry.path();
        if path == password || path == main_password {
            continue;
        }
        let destination = config_target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&path, &destination, &stage, &mut files, &mut total)?;
            continue;
        }
        ensure!(
            entry.file_type()?.is_file(),
            "private configuration must contain regular files only"
        );
        fs::copy(&path, &destination)?;
        total += entry.metadata()?.len();
        files.insert(
            destination
                .strip_prefix(&stage)?
                .to_string_lossy()
                .replace('\\', "/"),
            hash_file(&destination)?,
        );
    }
    copy_tree(
        Path::new("/var/lib/knowledge-forge-ingest"),
        &stage.join("ingest-private"),
        &stage,
        &mut files,
        &mut total,
    )?;
    ensure!(
        !files.is_empty(),
        "there are no private project credentials to back up"
    );
    let manifest = Manifest {
        format: 1,
        application_version: env!("CARGO_PKG_VERSION").into(),
        created_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)?,
        database_counts: Value::Null,
        files,
        source_bytes: total,
        secrets_included: true,
    };
    fs::write(
        stage.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let mut probe = secret_restic(&password);
    probe.arg("--json").arg("snapshots");
    if execute(
        probe,
        "secrets repository availability check",
        Duration::from_secs(60),
    )
    .await
    .is_err()
    {
        ensure!(
            std::env::var("KF_RESTIC_ALLOW_INIT").as_deref() == Ok("1"),
            "secrets repository requires explicit initialization"
        );
        let mut init = secret_restic(&password);
        init.arg("init");
        execute(
            init,
            "dedicated encrypted secrets repository initialization",
            Duration::from_secs(120),
        )
        .await?;
    }
    let mut backup = secret_restic(&password);
    backup
        .arg("backup")
        .arg("--json")
        .arg("--tag")
        .arg("knowledge-forge-secrets")
        .arg(&stage);
    let output = execute(
        backup,
        "encrypted off-site secrets backup",
        Duration::from_secs(600),
    )
    .await?;
    let snapshot = output
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find_map(|v| {
            v.get("snapshot_id")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .context("secrets provider did not confirm a snapshot")?;
    ensure!(
        snapshot.len() >= 8 && snapshot.chars().all(|c| c.is_ascii_hexdigit()),
        "invalid encrypted secrets snapshot confirmation"
    );
    let exercise = if test_restore {
        let target = root.join(format!("secrets-restore-{}", Uuid::new_v4()));
        private_dir(&target)?;
        let mut restore = secret_restic(&password);
        restore
            .arg("restore")
            .arg(&snapshot)
            .arg("--target")
            .arg(&target);
        execute(
            restore,
            "encrypted secrets restore exercise",
            Duration::from_secs(600),
        )
        .await?;
        verify_files(&target.join(stage.strip_prefix("/")?), &manifest)?;
        json!({"performed":true,"hashes_verified":manifest.files.len(),"credentials_reactivated":false})
    } else {
        json!({"performed":false})
    };
    Ok(
        json!({"encrypted_offsite_snapshot_confirmed":true,"snapshot":snapshot,"independent_key":true,"recovery_keys_included":false,"restore_test":exercise,"offhost_key_copy":"requires_separate_verified_operator_copy"}),
    )
}

async fn counts(pool: &PgPool) -> Result<Value> {
    let mut connection = pool
        .acquire()
        .await
        .map_err(|_| anyhow!("backup count connection failed"))?;
    counts_connection(&mut connection).await
}

async fn counts_connection(connection: &mut sqlx::PgConnection) -> Result<Value> {
    let mut values = serde_json::Map::new();
    for table in [
        "kf_documents",
        "kf_versions",
        "kf_chunks",
        "kf_embeddings",
        "kf_proposals",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&mut *connection)
            .await
            .map_err(|_| anyhow!("backup database counts failed"))?;
        values.insert(table.into(), json!(count));
    }
    Ok(Value::Object(values))
}

fn restore_url(value: &str) -> Result<()> {
    let parsed = url::Url::parse(value)
        .map_err(|_| anyhow!("invalid private restore database configuration"))?;
    ensure!(
        parsed.path() == "/kf_test_restore",
        "restore is restricted to the exact disposable database kf_test_restore"
    );
    ensure!(
        parsed.host_str() == Some("127.0.0.1") && parsed.port() == Some(55433),
        "restore must use the dedicated local Knowledge Forge cluster"
    );
    Ok(())
}

fn filtered_toc(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes).context("restore table of contents is not UTF-8")?;
    Ok(text
        .lines()
        .filter(|line| {
            !line.contains(" EXTENSION - vector") && !line.contains(" COMMENT - EXTENSION vector")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
}

fn verify_files(root: &Path, manifest: &Manifest) -> Result<()> {
    for (relative, expected) in &manifest.files {
        let path = root.join(relative);
        ensure!(
            !Path::new(relative).is_absolute()
                && !Path::new(relative)
                    .components()
                    .any(|c| matches!(c, Component::ParentDir)),
            "invalid manifest path"
        );
        safe_path(&path)?;
        ensure!(
            hash_file(&path)? == *expected,
            "restored file hash does not match manifest"
        );
    }
    Ok(())
}

async fn restore_test(
    stage: &Path,
    snapshot: &str,
    password: &Path,
    expected: &Manifest,
) -> Result<Value> {
    let url = std::env::var("KF_RESTORE_DATABASE_URL")
        .context("KF_RESTORE_DATABASE_URL is required for the requested restore exercise")?;
    restore_url(&url)?;
    let target = stage
        .parent()
        .context("backup staging has no parent")?
        .join(format!("restore-{}", Uuid::new_v4()));
    private_dir(&target)?;
    let mut restore = restic(password);
    restore
        .arg("restore")
        .arg(snapshot)
        .arg("--target")
        .arg(&target);
    execute(restore, "off-site restore", Duration::from_secs(1800)).await?;
    let relative = stage.strip_prefix(Path::new("/"))?;
    let recovered = target.join(relative);
    let recovered_manifest: Manifest =
        serde_json::from_slice(&fs::read(recovered.join("manifest.json"))?)?;
    ensure!(
        recovered_manifest.files == expected.files,
        "restored manifest differs from captured manifest"
    );
    verify_files(&recovered, &recovered_manifest)?;
    let archive = recovered.join("database.dump");
    let mut list = Command::new("/usr/bin/pg_restore");
    list.arg("--list").arg(&archive);
    let toc = filtered_toc(&execute(list, "restore inventory", Duration::from_secs(60)).await?)?;
    let toc_path = target.join("restore.list");
    fs::write(&toc_path, toc)?;
    let mut restore = postgres("/usr/bin/pg_restore", &url)?;
    restore
        .env("PGOPTIONS", "-c role=kf_owner")
        .arg("--dbname")
        .arg("kf_test_restore")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg("55433")
        .arg("--clean")
        .arg("--if-exists")
        .arg("--no-owner")
        .arg("--no-acl")
        .arg("--exit-on-error")
        .arg("--use-list")
        .arg(&toc_path)
        .arg(&archive);
    execute(
        restore,
        "disposable PostgreSQL restore",
        Duration::from_secs(600),
    )
    .await?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .map_err(|_| anyhow!("restored database verification connection failed"))?;
    ensure!(
        counts(&pool).await? == expected.database_counts,
        "restored database row counts differ"
    );
    let constraints: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM pg_constraint WHERE connamespace='public'::regnamespace AND NOT convalidated)")
        .fetch_one(&pool).await.map_err(|_|anyhow!("restore constraint verification failed"))?;
    ensure!(constraints, "restored database has unvalidated constraints");
    pool.close().await;
    let proof = json!({"performed":true,"offsite_snapshot_restored":true,"file_hashes_verified":expected.files.len(),"database_counts_match":true,"constraints_validated":true,"database":"kf_test_restore","reactivated_workers":false,"document_end_to_end_read_test":"not_performed_by_backup_command"});
    fs::write(
        stage.join("restore-proof.json"),
        serde_json::to_vec_pretty(&proof)?,
    )?;
    Ok(proof)
}

pub async fn run(config: &crate::Config, test_restore: bool) -> Result<Value> {
    ensure!(
        cfg!(unix),
        "private operator backups are supported on the Linux deployment only"
    );
    let url = std::env::var("KF_BACKUP_DATABASE_URL")
        .or_else(|_| std::env::var("KF_DATABASE_URL"))
        .context("a private backup database URL is required")?;
    let source_url = url::Url::parse(&url)
        .map_err(|_| anyhow!("invalid private backup database configuration"))?;
    ensure!(
        source_url.path() == "/knowledge_forge"
            && source_url.host_str() == Some("127.0.0.1")
            && source_url.port() == Some(55433),
        "backup source must be the dedicated local Knowledge Forge database"
    );
    let repository = std::env::var("KF_RESTIC_REPOSITORY").unwrap_or_else(|_| REPOSITORY.into());
    ensure!(
        repository == REPOSITORY,
        "backup repository is outside the approved existing storage prefix"
    );
    let password = PathBuf::from(
        std::env::var("KF_RESTIC_PASSWORD_FILE").context("KF_RESTIC_PASSWORD_FILE is required")?,
    );
    safe_path(&password)?;
    ensure!(password.is_file(), "private backup key file is missing");
    let root = PathBuf::from(
        std::env::var("KF_BACKUP_ROOT")
            .unwrap_or_else(|_| "/srv/hermes-connaissance/backups".into()),
    );
    ensure!(
        root.starts_with("/srv/hermes-connaissance/backups"),
        "backup staging must remain inside its dedicated directory"
    );
    private_dir(&root)?;
    safe_path(&config.data_root)?;
    let stage = root.join(Uuid::new_v4().to_string());
    private_dir(&stage)?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .map_err(|_| anyhow!("backup database connection failed"))?;
    let mut lock = pool
        .acquire()
        .await
        .map_err(|_| anyhow!("backup lock connection failed"))?;
    let obtained: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(LOCK)
        .fetch_one(&mut *lock)
        .await
        .map_err(|_| anyhow!("backup writer lock failed"))?;
    ensure!(
        obtained,
        "source writer is active; backup has not started and must be retried later"
    );
    let staged = async {
        sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *lock)
            .await
            .map_err(|_| anyhow!("backup consistent snapshot transaction failed"))?;
        let snapshot: String = sqlx::query_scalar("SELECT pg_export_snapshot()")
            .fetch_one(&mut *lock)
            .await
            .map_err(|_| anyhow!("backup database snapshot export failed"))?;
        let mut dump = postgres("/usr/bin/pg_dump", &url)?;
        dump.arg("--format=custom")
            .arg("--snapshot")
            .arg(&snapshot)
            .arg("--no-owner")
            .arg("--no-acl")
            .arg("--lock-wait-timeout=10s")
            .arg("--file")
            .arg(stage.join("database.dump"));
        execute(dump, "PostgreSQL snapshot dump", Duration::from_secs(600)).await?;
        let database_counts = counts_connection(&mut lock).await?;
        let mut files = BTreeMap::new();
        let mut bytes = 0;
        for directory in ["raw", "documents"] {
            copy_tree(
                &config.data_root.join(directory),
                &stage.join("data").join(directory),
                &stage,
                &mut files,
                &mut bytes,
            )?;
        }
        let versions: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT document_id::text,hash,payload->>'source_hash' FROM kf_versions",
        )
        .fetch_all(&mut *lock)
        .await
        .map_err(|_| anyhow!("backup source references could not be verified"))?;
        for (document, version, source) in versions {
            ensure!(
                files.contains_key(&format!("data/raw/{source}.bin")),
                "a database source version has no archived raw file"
            );
            for extension in ["md", "json"] {
                ensure!(
                    files.contains_key(&format!("data/documents/{document}/{version}.{extension}")),
                    "a database document version has no archived document file"
                );
            }
        }
        files.insert(
            "database.dump".into(),
            hash_file(&stage.join("database.dump"))?,
        );
        let manifest = Manifest {
            format: 1,
            application_version: env!("CARGO_PKG_VERSION").into(),
            created_at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?,
            database_counts,
            files,
            source_bytes: bytes,
            secrets_included: false,
        };
        fs::write(
            stage.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        Ok::<_, anyhow::Error>(manifest)
    }
    .await;
    let rollback_ok = sqlx::query("ROLLBACK").execute(&mut *lock).await.is_ok();
    if !rollback_ok
        || sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(LOCK)
            .execute(&mut *lock)
            .await
            .is_err()
    {
        lock.close()
            .await
            .map_err(|_| anyhow!("backup lock connection close failed"))?;
    } else {
        drop(lock);
    }
    pool.close().await;
    let manifest = staged?;
    let mut probe = restic(&password);
    probe.arg("--json").arg("snapshots");
    if execute(
        probe,
        "repository availability check",
        Duration::from_secs(60),
    )
    .await
    .is_err()
    {
        ensure!(
            std::env::var("KF_RESTIC_ALLOW_INIT").as_deref() == Ok("1"),
            "backup repository not confirmed; explicit first-time initialization is required"
        );
        let mut init = restic(&password);
        init.arg("init");
        execute(
            init,
            "new dedicated repository initialization",
            Duration::from_secs(120),
        )
        .await?;
    }
    let mut backup = restic(&password);
    backup
        .arg("backup")
        .arg("--json")
        .arg("--tag")
        .arg("knowledge-forge")
        .arg(&stage);
    let result = execute(
        backup,
        "encrypted off-site backup",
        Duration::from_secs(1800),
    )
    .await?;
    let snapshot = result
        .split(|c| *c == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find_map(|v| {
            v.get("snapshot_id")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .context("backup provider did not confirm a snapshot")?;
    ensure!(
        snapshot.len() >= 8 && snapshot.chars().all(|c| c.is_ascii_hexdigit()),
        "invalid backup snapshot confirmation"
    );
    let restore = if test_restore {
        restore_test(&stage, &snapshot, &password, &manifest).await?
    } else {
        json!({"performed":false})
    };
    let secrets = secret_backup(&root, test_restore).await?;
    Ok(
        json!({"offsite_backup_confirmed":true,"snapshot":snapshot,"source_files":manifest.files.len().saturating_sub(1),"source_bytes":manifest.source_bytes,"database_dump_sha256":manifest.files["database.dump"],"secrets_in_document_snapshot":false,"separate_secrets_backup":secrets,"restore_test":restore,"local_staging_retained":true,"workers_started":false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_postgres_uses_explicit_private_connection_environment() {
        let command = postgres(
            "/usr/bin/pg_dump",
            "postgresql://synthetic:p%40ss%2Bword@127.0.0.1:55433/knowledge_forge",
        )
        .unwrap();
        let native = command.as_std();
        let values: BTreeMap<_, _> = native
            .get_envs()
            .map(|(key, value)| (key.to_str().unwrap(), value.unwrap().to_str().unwrap()))
            .collect();
        assert_eq!(values["PGDATABASE"], "knowledge_forge");
        assert_eq!(values["PGUSER"], "synthetic");
        assert_eq!(values["PGPASSWORD"], "p@ss+word");
        assert_eq!(values["PGHOST"], "127.0.0.1");
        assert_eq!(values["PGPORT"], "55433");
        assert_eq!(native.get_args().count(), 0);
        assert!(decoded_uri_component("bad%0").is_err());
    }
    #[test]
    fn restore_cannot_target_production_or_another_host() {
        assert!(restore_url("postgresql://fake:fake@127.0.0.1:55433/kf_test_restore").is_ok());
        assert!(restore_url("postgresql://fake:fake@127.0.0.1:55433/knowledge_forge").is_err());
        assert!(
            restore_url("postgresql://fake:fake@example.invalid:55433/kf_test_restore").is_err()
        );
    }
    #[test]
    fn extension_is_preserved_in_disposable_restore() {
        let toc=b"1; 3079 1 EXTENSION - vector\n2; 0 0 COMMENT - EXTENSION vector\n3; 1259 2 TABLE public kf_documents kf_owner\n";
        let out = filtered_toc(toc).unwrap();
        assert!(!out.contains("EXTENSION"));
        assert!(out.contains("TABLE public kf_documents"));
    }
    #[test]
    fn parent_components_are_rejected() {
        assert!(safe_path(Path::new("/srv/hermes-connaissance/backups/../postgres")).is_err());
    }
}
