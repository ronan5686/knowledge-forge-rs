//! The only Google writer. Tokens never enter tool responses or request URLs.
use crate::Config;
use anyhow::{Context, Result, bail, ensure};
use oauth2::{ClientId, ClientSecret, RefreshToken, TokenResponse, TokenUrl, basic::BasicClient};
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

const MARKER: &str = "knowledge-forge-rs:common:v1";
const SINCE: &str = "2026-07-26T22:00:00Z";
const AUTO_SINCE: &str = "2026-10-27T23:00:00Z";

pub fn secret_dir() -> PathBuf {
    std::env::var_os("KF_GOOGLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/hermes-connaissance/google"))
}

fn read_private(path: &Path) -> Result<Value> {
    ensure!(
        !path.symlink_metadata()?.file_type().is_symlink(),
        "credential symlink rejected"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            path.metadata()?.permissions().mode() & 0o077 == 0,
            "credential file must be private"
        );
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

async fn refresh(dir: &Path) -> Result<String> {
    let path = dir.join("youtube-token.json");
    let mut token = read_private(&path)?;
    let client = BasicClient::new(ClientId::new(
        token["client_id"]
            .as_str()
            .context("OAuth client missing")?
            .into(),
    ))
    .set_client_secret(ClientSecret::new(
        token["client_secret"]
            .as_str()
            .context("OAuth client secret missing")?
            .into(),
    ))
    .set_token_uri(TokenUrl::new("https://oauth2.googleapis.com/token".into())?);
    let http = oauth2::reqwest::Client::builder()
        .redirect(oauth2::reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = client
        .exchange_refresh_token(&RefreshToken::new(
            token["refresh_token"]
                .as_str()
                .context("OAuth durable authorization missing")?
                .into(),
        ))
        .request_async(&http)
        .await
        .map_err(|_| {
            anyhow::anyhow!("Google OAuth refresh failed; reconnect the dedicated account")
        })?;
    let access = response.access_token().secret().to_string();
    token["token"] = json!(access);
    let expiry = time::OffsetDateTime::now_utc()
        + time::Duration::seconds(response.expires_in().map_or(3600, |d| d.as_secs()) as i64);
    token["expiry"] = json!(expiry.format(&time::format_description::well_known::Rfc3339)?);
    if let Some(refresh) = response.refresh_token() {
        token["refresh_token"] = json!(refresh.secret());
    }
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    use std::io::Write;
    file.write_all(&serde_json::to_vec_pretty(&token)?)?;
    file.as_file().sync_all()?;
    file.persist(&path).map_err(|e| e.error)?;
    Ok(access)
}

struct Google {
    http: reqwest::Client,
    token: String,
    store: kf_store::Store,
    #[cfg(test)]
    test_endpoint: Option<String>,
}
impl Google {
    fn endpoint(&self, resource: &str) -> String {
        #[cfg(test)]
        if let Some(base) = &self.test_endpoint {
            return format!("{base}/{resource}");
        }
        format!("https://www.googleapis.com/youtube/v3/{resource}")
    }
    async fn reserve(&self, cost: i32) -> Result<()> {
        let day = chrono::Utc::now()
            .with_timezone(&chrono_tz::US::Pacific)
            .date_naive()
            .to_string();
        let stopped: bool = sqlx::query_scalar(
            "SELECT COALESCE((SELECT value='true'::jsonb FROM kf_settings WHERE key=$1),false)",
        )
        .bind(format!("google_quota_stopped:{day}"))
        .fetch_one(self.store.pool())
        .await?;
        ensure!(
            !stopped,
            "the migrated Google quota is suspended until the next Pacific day"
        );
        let result=sqlx::query("INSERT INTO kf_google_quota(day,units) VALUES($1::date,$2) ON CONFLICT(day) DO UPDATE SET units=kf_google_quota.units+EXCLUDED.units WHERE kf_google_quota.units+EXCLUDED.units<=8000 RETURNING units")
            .bind(&day).bind(cost).fetch_optional(self.store.pool()).await?;
        ensure!(
            result.is_some(),
            "YouTube daily internal quota exhausted; suspended until Pacific reset"
        );
        Ok(())
    }
    async fn get(&self, resource: &str, params: &[(&str, String)]) -> Result<Value> {
        ensure!(
            matches!(
                resource,
                "channels" | "playlists" | "playlistItems" | "videos"
            ),
            "Google endpoint not allowed"
        );
        self.reserve(1).await?;
        let response = self
            .http
            .get(self.endpoint(resource))
            .bearer_auth(&self.token)
            .query(params)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Google read transport failed"))?;
        let status = response.status();
        if !status.is_success() {
            bail!(
                "Google read refused (HTTP {}); no mutation attempted",
                status.as_u16()
            );
        }
        Ok(response.json().await?)
    }
    async fn post(&self, resource: &str, body: &Value, key: &str) -> Result<Value> {
        ensure!(
            matches!(resource, "playlists" | "playlistItems"),
            "Google mutation not allowed"
        );
        let exists = sqlx::query("SELECT state FROM kf_google_writes WHERE op_key=$1")
            .bind(key)
            .fetch_optional(self.store.pool())
            .await?;
        if let Some(row) = exists {
            bail!(
                "write already recorded as {}; reconcile before any repeat",
                row.get::<String, _>("state")
            );
        }
        self.reserve(50).await?;
        sqlx::query("INSERT INTO kf_google_writes(id,op_key,kind,payload,state) VALUES($1,$2,$3,$4,'uncertain')").bind(Uuid::new_v4()).bind(key).bind(resource).bind(body).execute(self.store.pool()).await?;
        let part = if resource == "playlists" {
            "snippet,status"
        } else {
            "snippet"
        };
        let response = self
            .http
            .post(self.endpoint(resource))
            .bearer_auth(&self.token)
            .query(&[("part", part)])
            .json(body)
            .send()
            .await
            .map_err(|_| {
                anyhow::anyhow!("Google write outcome uncertain; reconciliation is required")
            })?;
        if !response.status().is_success() {
            bail!(
                "Google write outcome unresolved (HTTP {}); reconciliation required",
                response.status().as_u16()
            );
        }
        let value: Value = response
            .json()
            .await
            .context("Google write response incomplete; reconcile")?;
        let id = value["id"]
            .as_str()
            .context("Google write response has no identifier; reconcile")?;
        sqlx::query("UPDATE kf_google_writes SET state='confirmed',external_id=$1,updated_at=now() WHERE op_key=$2").bind(id).bind(key).execute(self.store.pool()).await?;
        Ok(value)
    }
    async fn pinned_channel(&self, dir: &Path) -> Result<String> {
        let policy = read_private(&dir.join("account-policy.json"))?;
        let expected = policy["intended_youtube_channel_id"]
            .as_str()
            .context("pinned YouTube channel missing")?;
        let mine = self
            .get(
                "channels",
                &[("part", "id".into()), ("mine", "true".into())],
            )
            .await?;
        let items = mine["items"]
            .as_array()
            .context("Google channel response invalid")?;
        ensure!(
            items.len() == 1 && items[0]["id"].as_str() == Some(expected),
            "YouTube account channel does not match private authorization"
        );
        Ok(expected.into())
    }

    /// Fetch metadata before advancing a page cursor. Missing or stale IDs are batched at 50.
    async fn refresh_upload_ids(
        &self,
        source_id: &str,
        channel: Option<&str>,
        ids: &[String],
    ) -> Result<(usize, usize)> {
        let unique: Vec<String> = ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let fresh: Vec<String> = sqlx::query_scalar(
            "SELECT video_id FROM kf_videos WHERE video_id=ANY($1) AND updated_at>now()-interval '30 days'",
        ).bind(&unique).fetch_all(self.store.pool()).await?;
        let fresh: std::collections::BTreeSet<_> = fresh.into_iter().collect();
        let pending: Vec<_> = unique
            .into_iter()
            .filter(|id| !fresh.contains(id))
            .collect();
        let mut refreshed = 0;
        for batch in pending.chunks(50) {
            let videos = self
                .get(
                    "videos",
                    &[
                        ("part", "snippet,contentDetails,status".into()),
                        ("id", batch.join(",")),
                    ],
                )
                .await?;
            for video in videos["items"].as_array().context("video list invalid")? {
                let id = video["id"].as_str().context("video ID missing")?;
                ensure!(
                    batch.iter().any(|requested| requested == id),
                    "unrequested video returned"
                );
                ensure!(
                    channel.is_some() && video["snippet"]["channelId"].as_str() == channel,
                    "source channel mismatch"
                );
                let availability = video["status"]["privacyStatus"]
                    .as_str()
                    .unwrap_or("unknown");
                let written = sqlx::query("INSERT INTO kf_videos(video_id,source_id,published_at,title,metadata,availability) VALUES($1,$2,$3::timestamptz,$4,$5,$6) ON CONFLICT(video_id) DO UPDATE SET title=EXCLUDED.title,metadata=EXCLUDED.metadata,published_at=EXCLUDED.published_at,availability=EXCLUDED.availability,updated_at=now() WHERE kf_videos.source_id=EXCLUDED.source_id")
                    .bind(id).bind(source_id).bind(video["snippet"]["publishedAt"].as_str())
                    .bind(video["snippet"]["title"].as_str().unwrap_or(""))
                    .bind(video).bind(availability).execute(self.store.pool()).await?;
                ensure!(
                    written.rows_affected() == 1,
                    "video source identity conflict"
                );
                refreshed += 1;
            }
        }
        Ok((pending.len(), refreshed))
    }

    async fn uploads_page(&self, uploads: &str, cursor: Option<&str>) -> Result<Value> {
        let mut params = vec![
            ("part", "contentDetails".into()),
            ("playlistId", uploads.into()),
            ("maxResults", "50".into()),
        ];
        if let Some(cursor) = cursor {
            params.push(("pageToken", cursor.into()));
        }
        self.get("playlistItems", &params).await
    }

    async fn poll_source(
        &self,
        source_id: &str,
        uploads: &str,
        channel: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<(usize, usize)> {
        // The head is always inspected first; a long historical scan must not delay new uploads.
        let head = self.uploads_page(uploads, None).await?;
        let (mut discovered, mut refreshed) = self.refresh_page(source_id, channel, &head).await?;
        let next = if let Some(cursor) = cursor {
            // This independent cursor progresses one older page per visit.
            let deep = self.uploads_page(uploads, Some(cursor)).await?;
            let (d, r) = self.refresh_page(source_id, channel, &deep).await?;
            discovered += d;
            refreshed += r;
            deep["nextPageToken"].as_str().map(str::to_owned)
        } else {
            // The first head request also serves as the first historical page.
            head["nextPageToken"].as_str().map(str::to_owned)
        };
        // Failed metadata acquisition never commits cursor progress.
        sqlx::query("UPDATE kf_sources SET updated_at=now(),poll_cursor=$2 WHERE source_id=$1")
            .bind(source_id)
            .bind(next)
            .execute(self.store.pool())
            .await?;
        Ok((discovered, refreshed))
    }

    async fn refresh_page(
        &self,
        source: &str,
        channel: Option<&str>,
        page: &Value,
    ) -> Result<(usize, usize)> {
        let ids = page["items"]
            .as_array()
            .context("uploads list invalid")?
            .iter()
            .filter_map(|item| {
                item["contentDetails"]["videoId"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        self.refresh_upload_ids(source, channel, &ids).await
    }
    async fn common_playlist(
        &self,
        channel: &str,
        allow_create: bool,
    ) -> Result<(Option<String>, bool)> {
        let mut found = Vec::new();
        let mut cursor = None;
        for _ in 0..200 {
            let mut params = vec![
                ("part", "snippet,status".into()),
                ("mine", "true".into()),
                ("maxResults", "50".into()),
            ];
            if let Some(c) = cursor {
                params.push(("pageToken", c));
            }
            let value = self.get("playlists", &params).await?;
            for item in value["items"].as_array().context("playlist list missing")? {
                if item["snippet"]["description"]
                    .as_str()
                    .is_some_and(|s| s.contains(MARKER))
                {
                    ensure!(
                        item["snippet"]["channelId"].as_str() == Some(channel)
                            && item["status"]["privacyStatus"].as_str() == Some("private"),
                        "managed playlist owner/privacy mismatch"
                    );
                    found.push(
                        item["id"]
                            .as_str()
                            .context("playlist ID missing")?
                            .to_string(),
                    );
                }
            }
            cursor = value["nextPageToken"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        ensure!(cursor.is_none(), "playlist reconciliation limit reached");
        ensure!(
            found.len() <= 1,
            "multiple managed playlists require operator reconciliation"
        );
        if let Some(id) = found.pop() {
            sqlx::query("UPDATE kf_google_writes SET state='confirmed',external_id=$1,updated_at=now() WHERE op_key='create:common:v1'").bind(&id).execute(self.store.pool()).await?;
            return Ok((Some(id), false));
        }
        if !allow_create {
            return Ok((None, false));
        }
        let created=self.post("playlists",&json!({"snippet":{"title":"Knowledge Forge — Veille","description":format!("Private listening queue. {MARKER}")},"status":{"privacyStatus":"private"}}),"create:common:v1").await?;
        Ok((
            Some(
                created["id"]
                    .as_str()
                    .context("playlist ID missing")?
                    .into(),
            ),
            true,
        ))
    }
}

pub async fn sync(config: &Config, max_writes: usize) -> Result<Value> {
    ensure!(max_writes <= 20, "write budget exceeds per-run limit");
    crate::operator::resource_guard(&config.data_root)?;
    let store = config.store().await?;
    let dir = secret_dir();
    // Keep a PostgreSQL session lock across the entire network reconciliation.
    let mut guard = store.pool().acquire().await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(779231055)")
        .fetch_one(&mut *guard)
        .await?;
    ensure!(locked, "another Google writer is active");
    let started = chrono::Utc::now().to_rfc3339();
    let result = async {
        record_sync_receipt(&store, &started, "running", None).await?;
        let google = Google {
            http: reqwest::Client::builder()
                .https_only(true)
                .timeout(Duration::from_secs(30))
                .build()?,
            token: refresh(&dir).await?,
            store: store.clone(),
            #[cfg(test)]
            test_endpoint: None,
        };
        sync_locked(&google, &dir, max_writes).await
    }
    .await;
    let receipt = record_sync_receipt(
        &store,
        &started,
        if result.is_ok() {
            "succeeded"
        } else {
            "stopped"
        },
        result.as_ref().ok(),
    )
    .await;
    let unlock = sqlx::query("SELECT pg_advisory_unlock(779231055)")
        .execute(&mut *guard)
        .await;
    if unlock.is_err() {
        guard.close().await?;
    }
    receipt.context("Google sync receipt could not be persisted")?;
    result
}

async fn record_sync_receipt(
    store: &kf_store::Store,
    started: &str,
    state: &str,
    result: Option<&Value>,
) -> Result<()> {
    let uncertain: i64 =
        sqlx::query_scalar("SELECT count(*) FROM kf_google_writes WHERE state='uncertain'")
            .fetch_one(store.pool())
            .await?;
    // Deliberate allowlist: never persist transport errors, credentials, account data or URLs.
    let mut receipt = json!({"started_at":started,"observed_at":chrono::Utc::now().to_rfc3339(),
        "state":state,"uncertain_writes":uncertain,"automatic_retry_of_uncertain_writes":false});
    if let Some(result) = result {
        for key in [
            "sources_polled",
            "new_or_stale_ids",
            "metadata_refreshed",
            "writes_confirmed",
        ] {
            if let Some(count) = result.get(key).and_then(Value::as_u64) {
                receipt[key] = json!(count);
            }
        }
    } else if state == "stopped" {
        receipt["reason"] = json!("sync_failed_operator_diagnosis_required");
    }
    sqlx::query("INSERT INTO kf_settings(key,value) VALUES('last_google_sync',$1) ON CONFLICT(key) DO UPDATE SET value=EXCLUDED.value")
        .bind(receipt).execute(store.pool()).await?;
    Ok(())
}

async fn sync_locked(google: &Google, dir: &Path, max_writes: usize) -> Result<Value> {
    let channel = google.pinned_channel(dir).await?;
    let old = google.store.pool();
    let (playlist, created) = google.common_playlist(&channel, max_writes > 0).await?;
    let mut writes = usize::from(created);
    if let Some(id) = &playlist {
        sqlx::query("INSERT INTO kf_settings(key,value) VALUES('common_playlist_id',$1) ON CONFLICT(key) DO UPDATE SET value=EXCLUDED.value").bind(json!(id)).execute(old).await?;
    }
    let sources=sqlx::query("SELECT source_id,catalog,channel_id,uploads_id,poll_cursor FROM kf_sources ORDER BY updated_at,source_id LIMIT 13").fetch_all(old).await?;
    let mut sources_polled = 0;
    let mut discovered = 0usize;
    let mut refreshed = 0usize;
    for source in sources {
        let source_id: String = source.get("source_id");
        let uploads: Option<String> = source.get("uploads_id");
        let Some(uploads) = uploads else {
            continue;
        };
        let cursor: Option<String> = source.get("poll_cursor");
        let expected: Option<String> = source.get("channel_id");
        let (d, r) = google
            .poll_source(&source_id, &uploads, expected.as_deref(), cursor.as_deref())
            .await?;
        sources_polled += 1;
        discovered += d;
        refreshed += r;
    }
    // Refresh an independent stale batch as well: an old video need not occur in the first uploads page.
    let stale=sqlx::query("SELECT video_id,source_id FROM kf_videos WHERE updated_at<now()-interval '29 days' ORDER BY updated_at,video_id LIMIT 50").fetch_all(old).await?;
    if !stale.is_empty() {
        let ids: Vec<String> = stale.iter().map(|v| v.get("video_id")).collect();
        let result = google
            .get(
                "videos",
                &[
                    ("part", "snippet,contentDetails,status".into()),
                    ("id", ids.join(",")),
                ],
            )
            .await?;
        let items = result["items"]
            .as_array()
            .context("stale video list invalid")?;
        for id in ids {
            if let Some(video) = items.iter().find(|v| v["id"].as_str() == Some(&id)) {
                sqlx::query("UPDATE kf_videos SET title=$2,metadata=$3,availability='public',updated_at=now() WHERE video_id=$1").bind(&id).bind(video["snippet"]["title"].as_str().unwrap_or("")).bind(video).execute(old).await?;
            } else {
                sqlx::query("UPDATE kf_videos SET availability='unavailable',updated_at=now() WHERE video_id=$1").bind(&id).execute(old).await?;
            }
            refreshed += 1;
        }
    }
    if let Some(playlist) = playlist {
        let mut cursor = None;
        for _ in 0..200 {
            let mut p = vec![
                ("part", "snippet,contentDetails".into()),
                ("playlistId", playlist.clone()),
                ("maxResults", "50".into()),
            ];
            if let Some(c) = cursor {
                p.push(("pageToken", c));
            }
            let page = google.get("playlistItems", &p).await?;
            for item in page["items"]
                .as_array()
                .context("managed playlist list invalid")?
            {
                if let (Some(id), Some(item_id)) = (
                    item["contentDetails"]["videoId"].as_str(),
                    item["id"].as_str(),
                ) {
                    sqlx::query("UPDATE kf_videos SET playlist_item_id=$2,state='confirmed',uncertain=false WHERE video_id=$1").bind(id).bind(item_id).execute(old).await?;
                    sqlx::query("UPDATE kf_google_writes SET state='confirmed',external_id=$2,updated_at=now() WHERE op_key=$1").bind(format!("insert:{playlist}:{id}")).bind(item_id).execute(old).await?;
                }
            }
            cursor = page["nextPageToken"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        ensure!(
            cursor.is_none(),
            "managed playlist pagination incomplete; writes suspended"
        );
        let candidates=sqlx::query("SELECT video_id FROM kf_videos WHERE published_at >= $1::timestamptz AND published_at >= $2::timestamptz AND availability='public' AND playlist_item_id IS NULL AND NOT uncertain ORDER BY published_at,video_id LIMIT $3")
            .bind(SINCE).bind(AUTO_SINCE).bind((max_writes-writes) as i64).fetch_all(old).await?;
        for candidate in candidates {
            let id: String = candidate.get("video_id");
            // post() records uncertainty before network I/O; known quota failures do not poison the candidate.
            let result=google.post("playlistItems",&json!({"snippet":{"playlistId":playlist,"resourceId":{"kind":"youtube#video","videoId":id}}}),&format!("insert:{playlist}:{id}")).await?;
            sqlx::query("UPDATE kf_videos SET playlist_item_id=$2,state='confirmed',uncertain=false WHERE video_id=$1").bind(&id).bind(result["id"].as_str()).execute(old).await?;
            writes += 1;
        }
    }
    Ok(
        json!({"sources_polled":sources_polled,"new_or_stale_ids":discovered,"metadata_refreshed":refreshed,"writes_confirmed":writes,"historical_cutoff":SINCE,"automatic_playlist_cutoff":AUTO_SINCE,"caption_acquisition":"suspended_public_reader_blocked"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Query, State},
        http::StatusCode,
        response::{IntoResponse, Response},
        routing::get,
    };
    use std::{
        collections::HashMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    #[derive(Default)]
    struct FakeGoogle {
        calls: Mutex<Vec<String>>,
        fail_metadata: AtomicBool,
    }
    async fn fake_uploads(
        State(state): State<Arc<FakeGoogle>>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        let cursor = query.get("pageToken").map(String::as_str).unwrap_or("head");
        state.calls.lock().unwrap().push(format!("page:{cursor}"));
        let upload = &query["playlistId"];
        Json(
            json!({"items":[{"contentDetails":{"videoId":format!("{upload}-{cursor}")}}],
            "nextPageToken":if cursor=="head" { "head-next" } else { "deep-next" }}),
        )
    }
    async fn fake_videos(
        State(state): State<Arc<FakeGoogle>>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Response {
        let ids: Vec<_> = query["id"].split(',').collect();
        state
            .calls
            .lock()
            .unwrap()
            .push(format!("metadata:{}:{}", ids.len(), ids[0]));
        assert!(ids.len() <= 50, "official videos.list batch limit exceeded");
        if state.fail_metadata.load(Ordering::SeqCst) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Json(json!({"items":ids.into_iter().map(|id|json!({"id":id,
            "snippet":{"channelId":"synthetic-channel","publishedAt":"2026-10-28T00:01:00Z","title":"Synthetic upload"},
            "status":{"privacyStatus":"public"}})).collect::<Vec<_>>()})).into_response()
    }

    #[tokio::test]
    #[ignore = "requires an explicitly configured disposable PostgreSQL database; HTTP is synthetic loopback only"]
    async fn head_poll_cursor_batches_receipt_and_uncertain_writes() -> Result<()> {
        let url = std::env::var("KF_TEST_DATABASE_URL")
            .expect("KF_TEST_DATABASE_URL is mandatory for this explicit integration test");
        assert!(
            url.split('?')
                .next()
                .unwrap_or_default()
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .contains("kf_test"),
            "Google integration tests require a dedicated kf_test database"
        );
        let store = kf_store::Store::connect(&url, 4).await?;
        store.migrate().await?;
        let fake = Arc::new(FakeGoogle::default());
        let router = Router::new()
            .route("/playlistItems", get(fake_uploads))
            .route("/videos", get(fake_videos))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let google = Google {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()?,
            token: "synthetic-not-a-credential".into(),
            store: store.clone(),
            test_endpoint: Some(endpoint),
        };
        let source = format!("google-test-{}", Uuid::new_v4());
        sqlx::query("INSERT INTO kf_sources(source_id,catalog,channel_id,uploads_id,poll_cursor) VALUES($1,'{}','synthetic-channel',$1,'old-cursor')")
            .bind(&source).execute(store.pool()).await?;
        let counts = google
            .poll_source(
                &source,
                &source,
                Some("synthetic-channel"),
                Some("old-cursor"),
            )
            .await?;
        assert_eq!(counts, (2, 2));
        let calls = fake.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                "page:head".to_string(),
                format!("metadata:1:{source}-head"),
                "page:old-cursor".to_string(),
                format!("metadata:1:{source}-old-cursor")
            ]
        );
        let cursor: String =
            sqlx::query_scalar("SELECT poll_cursor FROM kf_sources WHERE source_id=$1")
                .bind(&source)
                .fetch_one(store.pool())
                .await?;
        assert_eq!(
            cursor, "deep-next",
            "the head token must not overwrite historical progress"
        );
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM kf_videos WHERE video_id=$1 AND source_id=$2)",
        )
        .bind(format!("{source}-head"))
        .bind(&source)
        .fetch_one(store.pool())
        .await?;
        assert!(
            exists,
            "new head metadata must not wait for historical completion"
        );

        // No historical cursor: only one page request, also used as the first deep page.
        fake.calls.lock().unwrap().clear();
        google
            .poll_source(&source, &source, Some("synthetic-channel"), None)
            .await?;
        assert_eq!(*fake.calls.lock().unwrap(), vec!["page:head"]);
        let cursor: String =
            sqlx::query_scalar("SELECT poll_cursor FROM kf_sources WHERE source_id=$1")
                .bind(&source)
                .fetch_one(store.pool())
                .await?;
        assert_eq!(cursor, "head-next");

        // More than one page of IDs still respects the official metadata batch maximum.
        fake.calls.lock().unwrap().clear();
        let ids = (0..75)
            .map(|n| format!("{source}-batch-{n:02}"))
            .collect::<Vec<_>>();
        assert_eq!(
            google
                .refresh_upload_ids(&source, Some("synthetic-channel"), &ids)
                .await?,
            (75, 75)
        );
        let calls = fake.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].starts_with("metadata:50:") && calls[1].starts_with("metadata:25:"));

        // A metadata failure leaves the committed cursor unchanged and never reaches a deep request.
        fake.calls.lock().unwrap().clear();
        fake.fail_metadata.store(true, Ordering::SeqCst);
        assert!(
            google
                .poll_source(
                    &source,
                    &format!("{source}-new"),
                    Some("synthetic-channel"),
                    Some("head-next")
                )
                .await
                .is_err()
        );
        assert_eq!(fake.calls.lock().unwrap().len(), 2);
        let cursor: String =
            sqlx::query_scalar("SELECT poll_cursor FROM kf_sources WHERE source_id=$1")
                .bind(&source)
                .fetch_one(store.pool())
                .await?;
        assert_eq!(cursor, "head-next");

        // An uncertain outcome remains stopped without a second HTTP write or quota reservation.
        let op_key = format!("synthetic:{source}");
        sqlx::query("INSERT INTO kf_google_writes(id,op_key,kind,payload,state) VALUES($1,$2,'playlistItems','{}','uncertain')")
            .bind(Uuid::new_v4()).bind(&op_key).execute(store.pool()).await?;
        let calls_before = fake.calls.lock().unwrap().len();
        assert!(
            google
                .post("playlistItems", &json!({}), &op_key)
                .await
                .is_err()
        );
        assert_eq!(fake.calls.lock().unwrap().len(), calls_before);
        let state: String =
            sqlx::query_scalar("SELECT state FROM kf_google_writes WHERE op_key=$1")
                .bind(&op_key)
                .fetch_one(store.pool())
                .await?;
        assert_eq!(state, "uncertain");
        record_sync_receipt(
            &store,
            "2026-10-27T12:00:00Z",
            "succeeded",
            Some(&json!({
            "sources_polled":1,"metadata_refreshed":77,"writes_confirmed":0,
            "token":"must-not-persist","url":"https://example.invalid/private","error":"unsafe"})),
        )
        .await?;
        let status = store.status().await?;
        assert!(status["google_uncertain_writes"].as_i64().unwrap() >= 1);
        assert_eq!(status["last_google_sync"]["sources_polled"], 1);
        assert!(status["last_google_sync"].get("token").is_none());
        assert!(status["last_google_sync"].get("url").is_none());
        assert!(status["last_google_sync"].get("error").is_none());
        record_sync_receipt(&store, "2026-10-27T12:00:00Z", "stopped", None).await?;
        assert_eq!(
            store.status().await?["last_google_sync"]["state"],
            "stopped"
        );
        server.abort();
        Ok(())
    }
}
