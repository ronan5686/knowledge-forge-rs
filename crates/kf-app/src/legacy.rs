//! Read-only legacy snapshot import and a new, private compatibility export.
use crate::Config;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, SqliteConnection, postgres::PgRow, sqlite::SqliteConnectOptions};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    path::Path,
    time::Duration,
};
use uuid::Uuid;

fn no_symlinks(path: &Path) -> Result<()> {
    for part in path.ancestors() {
        match fs::symlink_metadata(part) {
            Ok(m) => ensure!(
                !m.file_type().is_symlink(),
                "symbolic links are not accepted for migration paths"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn private_directory(path: &Path) -> Result<()> {
    no_symlinks(path)?;
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn file_hash(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(hex::encode(digest.finalize()))
}

async fn readonly(path: &Path) -> Result<SqliteConnection> {
    no_symlinks(path)?;
    ensure!(path.is_file(), "legacy SQLite database is missing");
    Ok(SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .create_if_missing(false)
            .busy_timeout(Duration::from_secs(30)),
    )
    .await?)
}

async fn snapshot(source: &Path, destination: &Path) -> Result<()> {
    no_symlinks(destination)?;
    ensure!(!destination.exists(), "snapshot target already exists");
    let mut original = readonly(source).await?;
    // VACUUM INTO reads a transactionally consistent snapshot, including committed WAL.
    // Its filename is a bound value; no path can become SQL or a shell command.
    sqlx::query("VACUUM INTO ?")
        .bind(
            destination
                .to_str()
                .context("snapshot path must be UTF-8")?,
        )
        .execute(&mut original)
        .await
        .context("read-only SQLite snapshot failed")?;
    original.close().await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o600))?;
    }
    let mut copy = readonly(destination).await?;
    let checks: Vec<String> = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_all(&mut copy)
        .await?;
    ensure!(checks == ["ok"], "SQLite snapshot integrity check failed");
    copy.close().await?;
    Ok(())
}

fn timestamp(raw: Option<String>) -> Option<String> {
    raw.and_then(|s| {
        time::OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).ok()
    })
    .and_then(|t| {
        t.format(&time::format_description::well_known::Rfc3339)
            .ok()
    })
}

fn allowed_setting(key: &str) -> bool {
    matches!(
        key,
        "published_since"
            | "automation_published_since"
            | "reader_blocked"
            | "reader_blocked_reason"
            | "pilot_approved"
    )
}

struct Source {
    id: String,
    catalog: Value,
    channel: Option<String>,
    uploads: Option<String>,
    history_cursor: Option<String>,
    history_done: bool,
    poll_cursor: Option<String>,
}
struct Video {
    id: String,
    source: String,
    published: Option<String>,
    title: String,
    metadata: Value,
    availability: String,
    item: Option<String>,
    state: String,
    uncertain: bool,
    fetched_at: time::OffsetDateTime,
}
struct Quota {
    day: String,
    units: i32,
    stopped: bool,
}
struct Legacy {
    sources: Vec<Source>,
    videos: Vec<Video>,
    quota: Vec<Quota>,
    settings: Vec<(String, Value)>,
}

async fn load(path: &Path, catalog: &Value) -> Result<Legacy> {
    let mut db = readonly(path).await?;
    let mut entries = HashMap::new();
    for entry in catalog["sources"]
        .as_array()
        .context("legacy source catalog must contain a sources array")?
    {
        let id = entry["id"]
            .as_str()
            .context("catalog source has no identifier")?;
        ensure!(
            entries.insert(id.to_string(), entry.clone()).is_none(),
            "duplicate catalog source identity"
        );
    }
    let mut sources = Vec::new();
    for row in sqlx::query("SELECT id,name,url,title,marker,channel_id,uploads_id,target_id,create_state,history_cursor,history_done,poll_cursor,blocked FROM sources ORDER BY id").fetch_all(&mut db).await? {
        let id:String=row.try_get("id")?;let mut entry=entries.remove(&id).context("legacy state source is absent from its authorized catalog")?;
        entry["legacy_import"]=json!({"name":row.try_get::<String,_>("name")?,"url":row.try_get::<String,_>("url")?,"title":row.try_get::<String,_>("title")?,"marker":row.try_get::<String,_>("marker")?,"target_id":row.try_get::<Option<String>,_>("target_id")?,"create_state":row.try_get::<String,_>("create_state")?,"blocked":row.try_get::<Option<String>,_>("blocked")?});
        sources.push(Source { id,catalog:entry,channel:row.try_get("channel_id")?,uploads:row.try_get("uploads_id")?,history_cursor:row.try_get("history_cursor")?,history_done:row.try_get::<i64,_>("history_done")?!=0,poll_cursor:row.try_get("poll_cursor")? });
    }
    ensure!(
        entries.is_empty(),
        "catalog sources are not initialized in legacy state"
    );
    let mut videos = Vec::new();
    let mut identities = HashSet::new();
    for row in sqlx::query("SELECT v.source_id,v.video_id,v.state,v.playlist_item_id,v.uncertain_at,v.published_at,m.title,m.channel_id,m.published_at AS metadata_published_at,m.original_language,m.duration_seconds,m.availability,m.metadata_fetched_at FROM videos v LEFT JOIN video_metadata m ON m.source_id=v.source_id AND m.video_id=v.video_id ORDER BY v.source_id,v.video_id").fetch_all(&mut db).await? {
        let id:String=row.try_get("video_id")?;ensure!(identities.insert(id.clone()),"same video belongs to multiple legacy sources; identity review required");
        let legacy_date:Option<String>=row.try_get("published_at")?;
        let metadata_date:Option<String>=row.try_get("metadata_published_at")?;
        let title:Option<String>=row.try_get("title")?;let availability:Option<String>=row.try_get("availability")?;
        let state:String=row.try_get("state")?;
        let metadata=json!({"legacy_import":{"published_at":legacy_date,"metadata_published_at":metadata_date,"channel_id":row.try_get::<Option<String>,_>("channel_id")?,"original_language":row.try_get::<Option<String>,_>("original_language")?,"duration_seconds":row.try_get::<Option<f64>,_>("duration_seconds")?,"metadata_fetched_at":row.try_get::<Option<f64>,_>("metadata_fetched_at")?,"uncertain_at":row.try_get::<Option<f64>,_>("uncertain_at")?}});
        let fetched:Option<f64>=row.try_get("metadata_fetched_at")?;
        let fetched_at=if let Some(seconds)=fetched {ensure!(seconds.is_finite() && seconds>=0.0 && seconds<=i64::MAX as f64,"invalid legacy metadata acquisition timestamp");time::OffsetDateTime::from_unix_timestamp(seconds as i64).context("legacy metadata timestamp out of range")?}else{time::OffsetDateTime::UNIX_EPOCH};
        videos.push(Video {id,source:row.try_get("source_id")?,published:timestamp(metadata_date).or_else(||timestamp(legacy_date)),title:title.unwrap_or_default(),metadata,availability:availability.unwrap_or_else(||"unknown".into()),item:row.try_get("playlist_item_id")?,uncertain:state=="uncertain",state,fetched_at});
    }
    let mut quota = Vec::new();
    for row in sqlx::query("SELECT day,used,stopped FROM quota ORDER BY day")
        .fetch_all(&mut db)
        .await?
    {
        let day: String = row.try_get("day")?;
        chrono::NaiveDate::parse_from_str(&day, "%Y-%m-%d").context("invalid legacy quota day")?;
        let used: i64 = row.try_get("used")?;
        ensure!(
            (0..=i32::MAX as i64).contains(&used),
            "invalid legacy quota count"
        );
        quota.push(Quota {
            day,
            units: used as i32,
            stopped: row.try_get::<i64, _>("stopped")? != 0,
        });
    }
    let mut settings = Vec::new();
    for row in sqlx::query("SELECT key,value FROM settings")
        .fetch_all(&mut db)
        .await?
    {
        let key: String = row.try_get("key")?;
        if allowed_setting(&key) {
            let raw: Option<String> = row.try_get("value")?;
            settings.push((
                key,
                raw.map(|s| serde_json::from_str(&s).unwrap_or(Value::String(s)))
                    .unwrap_or(Value::Null),
            ));
        }
    }
    db.close().await?;
    Ok(Legacy {
        sources,
        videos,
        quota,
        settings,
    })
}

pub async fn migrate(config: &Config, root: &Path) -> Result<Value> {
    no_symlinks(root)?;
    let root = root.canonicalize()?;
    let catalog_path = root.join("config/sources.json");
    no_symlinks(&catalog_path)?;
    let catalog: Value = serde_json::from_slice(&fs::read(catalog_path)?)?;
    let snapshot_dir = config
        .data_root
        .join("legacy-snapshots")
        .join(Uuid::new_v4().to_string());
    private_directory(&snapshot_dir)?;
    let state_path = snapshot_dir.join("state.sqlite3");
    snapshot(&root.join("state.sqlite3"), &state_path).await?;
    let knowledge_source = root.join("state/knowledge.sqlite3");
    let mut knowledge_hash = None;
    if knowledge_source.exists() {
        let copy = snapshot_dir.join("knowledge.sqlite3");
        snapshot(&knowledge_source, &copy).await?;
        let mut db = readonly(&copy).await?;
        let transcripts: i64 = sqlx::query_scalar("SELECT count(*) FROM transcripts")
            .fetch_one(&mut db)
            .await?;
        db.close().await?;
        ensure!(
            transcripts == 0,
            "legacy library contains transcripts: explicit source-preserving document import is required before this metadata-only migration"
        );
        knowledge_hash = Some(file_hash(&copy)?);
    }
    let hash = file_hash(&state_path)?;
    let legacy = load(&state_path, &catalog).await?;
    let store = config.store().await?;
    let key = format!("legacy_migration:{hash}");
    if let Some(previous) =
        sqlx::query_scalar::<_, Value>("SELECT value FROM kf_settings WHERE key=$1")
            .bind(&key)
            .fetch_optional(store.pool())
            .await?
    {
        // Reconcile the first import's acquisition timestamps only while the
        // row still holds our legacy payload. A Google refresh replaces that
        // payload and is deliberately never overwritten here.
        let repaired=sqlx::query("UPDATE kf_videos SET updated_at=to_timestamp((metadata #>> '{legacy_import,metadata_fetched_at}')::double precision) WHERE metadata ? 'legacy_import' AND jsonb_typeof(metadata #> '{legacy_import,metadata_fetched_at}')='number' AND updated_at IS DISTINCT FROM to_timestamp((metadata #>> '{legacy_import,metadata_fetched_at}')::double precision)").execute(store.pool()).await?.rows_affected();
        return Ok(
            json!({"already_imported":true,"manifest":previous,"reconciled_acquisition_timestamps":repaired}),
        );
    }
    let mut tx = store.pool().begin().await?;
    for source in &legacy.sources {
        let existing = sqlx::query(
            "SELECT channel_id,uploads_id FROM kf_sources WHERE source_id=$1 FOR UPDATE",
        )
        .bind(&source.id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = existing {
            for (field, expected) in [
                ("channel_id", &source.channel),
                ("uploads_id", &source.uploads),
            ] {
                let current: Option<String> = row.try_get(field)?;
                ensure!(
                    current.is_none() || expected.is_none() || &current == expected,
                    "source identity conflicts with current database"
                );
            }
        }
        sqlx::query("INSERT INTO kf_sources(source_id,catalog,channel_id,uploads_id,history_cursor,history_done,poll_cursor) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(source_id) DO UPDATE SET channel_id=COALESCE(kf_sources.channel_id,EXCLUDED.channel_id),uploads_id=COALESCE(kf_sources.uploads_id,EXCLUDED.uploads_id)").bind(&source.id).bind(&source.catalog).bind(&source.channel).bind(&source.uploads).bind(&source.history_cursor).bind(source.history_done).bind(&source.poll_cursor).execute(&mut *tx).await?;
    }
    let mut video_inserted = 0u64;
    for batch in legacy.videos.chunks(500) {
        let ids: Vec<&str> = batch.iter().map(|v| v.id.as_str()).collect();
        let existing =
            sqlx::query("SELECT video_id,source_id FROM kf_videos WHERE video_id=ANY($1)")
                .bind(&ids)
                .fetch_all(&mut *tx)
                .await?;
        for row in existing {
            let id: String = row.try_get("video_id")?;
            let source: String = row.try_get("source_id")?;
            ensure!(
                batch.iter().any(|v| v.id == id && v.source == source),
                "video identity conflicts with existing source"
            );
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
            "INSERT INTO kf_videos(video_id,source_id,published_at,title,metadata,availability,playlist_item_id,state,uncertain,updated_at) ",
        );
        query.push_values(batch, |mut b, v| {
            b.push_bind(&v.id)
                .push_bind(&v.source)
                .push_bind(&v.published)
                .push_unseparated("::timestamptz")
                .push_bind(&v.title)
                .push_bind(&v.metadata)
                .push_bind(&v.availability)
                .push_bind(&v.item)
                .push_bind(&v.state)
                .push_bind(v.uncertain)
                .push_bind(v.fetched_at);
        });
        query.push(" ON CONFLICT(video_id) DO NOTHING");
        video_inserted += query.build().execute(&mut *tx).await?.rows_affected();
    }
    for (name, value) in &legacy.settings {
        sqlx::query("INSERT INTO kf_settings(key,value) VALUES($1,$2) ON CONFLICT(key) DO NOTHING")
            .bind(name)
            .bind(value)
            .execute(&mut *tx)
            .await?;
    }
    for quota in &legacy.quota {
        sqlx::query("INSERT INTO kf_google_quota(day,units) VALUES($1::date,$2) ON CONFLICT(day) DO UPDATE SET units=GREATEST(kf_google_quota.units,EXCLUDED.units)").bind(&quota.day).bind(quota.units).execute(&mut *tx).await?;
        if quota.stopped {
            sqlx::query("INSERT INTO kf_settings(key,value) VALUES($1,'true'::jsonb) ON CONFLICT(key) DO UPDATE SET value='true'::jsonb").bind(format!("google_quota_stopped:{}",quota.day)).execute(&mut *tx).await?;
        }
    }
    let manifest = json!({"schema_version":1,"snapshot_hash":hash,"knowledge_snapshot_hash":knowledge_hash,"catalog_hash":kf_core::sha256(&serde_json::to_vec(&catalog)?),"sources_imported":legacy.sources.len(),"videos_seen":legacy.videos.len(),"videos_inserted":video_inserted,"quota_days_merged":legacy.quota.len(),"settings_imported":legacy.settings.len(),"canonical_documents_imported":0,"originals_modified":false,"secrets_imported":false});
    sqlx::query("INSERT INTO kf_settings(key,value) VALUES($1,$2) ON CONFLICT(key) DO NOTHING")
        .bind(key)
        .bind(&manifest)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    fs::write(
        snapshot_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(json!({"already_imported":false,"manifest":manifest}))
}

const EXPORT_SCHEMA: &str = r#"
CREATE TABLE sources(id TEXT PRIMARY KEY,name TEXT NOT NULL,url TEXT NOT NULL,title TEXT NOT NULL,marker TEXT NOT NULL UNIQUE,channel_id TEXT,uploads_id TEXT,target_id TEXT UNIQUE,create_state TEXT NOT NULL DEFAULT 'new',history_cursor TEXT,history_done INTEGER NOT NULL DEFAULT 0,full_scan_at REAL NOT NULL DEFAULT 0,poll_cursor TEXT,blocked TEXT,uncertain_at REAL NOT NULL DEFAULT 0);
CREATE TABLE videos(source_id TEXT NOT NULL,video_id TEXT NOT NULL,state TEXT NOT NULL,playlist_item_id TEXT,uncertain_at REAL DEFAULT 0,published_at TEXT,PRIMARY KEY(source_id,video_id));
CREATE TABLE video_metadata(source_id TEXT NOT NULL,video_id TEXT NOT NULL,title TEXT NOT NULL DEFAULT '',channel_id TEXT,published_at TEXT,original_language TEXT,duration_seconds REAL,availability TEXT NOT NULL,metadata_fetched_at REAL NOT NULL,PRIMARY KEY(source_id,video_id));
CREATE TABLE unavailable(source_id TEXT NOT NULL,item_key TEXT NOT NULL,PRIMARY KEY(source_id,item_key));
CREATE TABLE quota(day TEXT PRIMARY KEY,used INTEGER NOT NULL DEFAULT 0,stopped INTEGER NOT NULL DEFAULT 0);
CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT);
CREATE TABLE scheduler(id INTEGER PRIMARY KEY,next_source TEXT);
CREATE TABLE knowledge_document_versions(document_id TEXT NOT NULL,hash TEXT NOT NULL,payload TEXT NOT NULL,current INTEGER NOT NULL,PRIMARY KEY(document_id,hash));
"#;

pub async fn export(config: &Config, output: &Path) -> Result<Value> {
    no_symlinks(output)?;
    ensure!(
        !output.exists(),
        "export requires a new output path; existing files are never replaced"
    );
    let parent = output
        .parent()
        .context("export path needs a parent directory")?;
    ensure!(parent.is_dir(), "export parent directory is missing");
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let mut sqlite = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(temporary.path())
            .create_if_missing(false),
    )
    .await?;
    sqlx::raw_sql(EXPORT_SCHEMA).execute(&mut sqlite).await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut sqlite).await?;
    let store = config.store().await?;
    let mut pg = store.pool().begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *pg)
        .await?;
    let sources = sqlx::query("SELECT * FROM kf_sources ORDER BY source_id")
        .fetch_all(&mut *pg)
        .await?;
    let source_count = sources.len();
    for row in sources {
        let id: String = row.try_get("source_id")?;
        let catalog: Value = row.try_get("catalog")?;
        let legacy = &catalog["legacy_import"];
        let name = catalog["name"]
            .as_str()
            .or_else(|| legacy["name"].as_str())
            .unwrap_or(&id);
        let url = catalog["channel_url"]
            .as_str()
            .or_else(|| legacy["url"].as_str())
            .unwrap_or("");
        sqlx::query("INSERT INTO sources(id,name,url,title,marker,channel_id,uploads_id,history_cursor,history_done,poll_cursor) VALUES(?,?,?,?,?,?,?,?,?,?)").bind(&id).bind(name).bind(url).bind(format!("SOURCE | {name}")).bind(format!("hermes-connaissance-youtube:v1:{id}")).bind(row.try_get::<Option<String>,_>("channel_id")?).bind(row.try_get::<Option<String>,_>("uploads_id")?).bind(row.try_get::<Option<String>,_>("history_cursor")?).bind(i32::from(row.try_get::<bool,_>("history_done")?)).bind(row.try_get::<Option<String>,_>("poll_cursor")?).execute(&mut sqlite).await?;
    }
    let videos=sqlx::query("SELECT video_id,source_id,published_at,title,metadata,availability,playlist_item_id,state,uncertain,updated_at FROM kf_videos ORDER BY source_id,video_id").fetch_all(&mut *pg).await?;
    let video_count = videos.len();
    for row in videos {
        export_video(&mut sqlite, row).await?;
    }
    for row in sqlx::query("SELECT day::text AS day,units FROM kf_google_quota ORDER BY day")
        .fetch_all(&mut *pg)
        .await?
    {
        let day: String = row.try_get("day")?;
        let stop: Option<Value> = sqlx::query_scalar("SELECT value FROM kf_settings WHERE key=$1")
            .bind(format!("google_quota_stopped:{day}"))
            .fetch_optional(&mut *pg)
            .await?;
        sqlx::query("INSERT INTO quota(day,used,stopped) VALUES(?,?,?)")
            .bind(day)
            .bind(row.try_get::<i32, _>("units")?)
            .bind(i32::from(stop == Some(Value::Bool(true))))
            .execute(&mut sqlite)
            .await?;
    }
    for row in sqlx::query("SELECT key,value FROM kf_settings ORDER BY key")
        .fetch_all(&mut *pg)
        .await?
    {
        let key: String = row.try_get("key")?;
        if allowed_setting(&key) {
            sqlx::query("INSERT INTO settings(key,value) VALUES(?,?)")
                .bind(key)
                .bind(serde_json::to_string(&row.try_get::<Value, _>("value")?)?)
                .execute(&mut sqlite)
                .await?;
        }
    }
    let documents=sqlx::query("SELECT v.document_id,v.hash,v.payload,(d.current_hash=v.hash) AS current FROM kf_versions v JOIN kf_documents d ON d.id=v.document_id ORDER BY v.document_id,v.created_at").fetch_all(&mut *pg).await?;
    let version_count = documents.len();
    for row in documents {
        sqlx::query("INSERT INTO knowledge_document_versions(document_id,hash,payload,current) VALUES(?,?,?,?)").bind(row.try_get::<Uuid,_>("document_id")?.to_string()).bind(row.try_get::<String,_>("hash")?).bind(serde_json::to_string(&row.try_get::<Value,_>("payload")?)?).bind(i32::from(row.try_get::<bool,_>("current")?)).execute(&mut sqlite).await?;
    }
    pg.commit().await?;
    sqlx::query("COMMIT").execute(&mut sqlite).await?;
    let checks: Vec<String> = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_all(&mut sqlite)
        .await?;
    ensure!(checks == ["ok"], "export integrity check failed");
    sqlite.close().await?;
    let hash = file_hash(temporary.path())?;
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(output).map_err(|e| e.error)?;
    Ok(
        json!({"sources":source_count,"videos":video_count,"document_versions":version_count,"sha256":hash,"new_file":true,"originals_modified":false,"secrets_exported":false,"compatibility":"inventory schema plus immutable knowledge_document_versions extension; not a runnable Python installation"}),
    )
}

async fn export_video(sqlite: &mut SqliteConnection, row: PgRow) -> Result<()> {
    let source: String = row.try_get("source_id")?;
    let id: String = row.try_get("video_id")?;
    let meta: Value = row.try_get("metadata")?;
    let published = row
        .try_get::<Option<time::OffsetDateTime>, _>("published_at")?
        .map(|t| t.format(&time::format_description::well_known::Rfc3339))
        .transpose()?;
    let state: String = row.try_get("state")?;
    sqlx::query("INSERT INTO videos(source_id,video_id,state,playlist_item_id,uncertain_at,published_at) VALUES(?,?,?,?,?,?)").bind(&source).bind(&id).bind(if row.try_get::<bool,_>("uncertain")? {"uncertain"} else if state=="confirmed" {"inserted"} else {&state}).bind(row.try_get::<Option<String>,_>("playlist_item_id")?).bind(0.0_f64).bind(&published).execute(&mut *sqlite).await?;
    let channel = meta["snippet"]["channelId"]
        .as_str()
        .or_else(|| meta["legacy_import"]["channel_id"].as_str());
    let language = meta["snippet"]["defaultAudioLanguage"]
        .as_str()
        .or_else(|| meta["legacy_import"]["original_language"].as_str());
    sqlx::query("INSERT INTO video_metadata(source_id,video_id,title,channel_id,published_at,original_language,duration_seconds,availability,metadata_fetched_at) VALUES(?,?,?,?,?,?,?,?,?)").bind(source).bind(id).bind(row.try_get::<String,_>("title")?).bind(channel).bind(published).bind(language).bind(meta["legacy_import"]["duration_seconds"].as_f64()).bind(row.try_get::<String,_>("availability")?).bind(row.try_get::<time::OffsetDateTime,_>("updated_at")?.unix_timestamp() as f64).execute(sqlite).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn snapshot_includes_committed_wal_without_changing_source() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.sqlite3");
        let destination = temp.path().join("snapshot.sqlite3");
        let mut original = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&source)
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal),
        )
        .await?;
        sqlx::query("CREATE TABLE fixture(value TEXT)")
            .execute(&mut original)
            .await?;
        sqlx::query("INSERT INTO fixture VALUES('complete synthetic value')")
            .execute(&mut original)
            .await?;
        let before = file_hash(&source)?;
        snapshot(&source, &destination).await?;
        assert_eq!(before, file_hash(&source)?);
        let mut copy = readonly(&destination).await?;
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT value FROM fixture")
                .fetch_one(&mut copy)
                .await?,
            "complete synthetic value"
        );
        assert!(snapshot(&source, &destination).await.is_err());
        copy.close().await?;
        original.close().await?;
        Ok(())
    }
    #[test]
    fn settings_never_import_credentials() {
        assert!(allowed_setting("published_since"));
        for key in [
            "access_token",
            "refresh_token",
            "password",
            "google_credentials",
            "arbitrary",
        ] {
            assert!(!allowed_setting(key));
        }
    }
    #[tokio::test]
    async fn compatibility_schema_is_valid() -> Result<()> {
        let mut db = SqliteConnection::connect("sqlite::memory:").await?;
        sqlx::raw_sql(EXPORT_SCHEMA).execute(&mut db).await?;
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type='table'")
                .fetch_one(&mut db)
                .await?;
        assert_eq!(count, 8);
        Ok(())
    }
}
