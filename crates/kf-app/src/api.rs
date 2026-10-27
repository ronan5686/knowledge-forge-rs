use crate::{Config, models};
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use kf_core::{ReadRequest, SearchMode, SearchRequest, SearchResponse, WikiProposal};
use kf_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path as FsPath, sync::Arc};

#[derive(Clone)]
struct AppState {
    store: Store,
    curator: Option<Store>,
    models: models::Runtime,
    counter: Option<Arc<models::Counter>>,
}

struct ApiError(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self.0.downcast_ref::<kf_core::Error>() {
            Some(kf_core::Error::NotFound) => (StatusCode::NOT_FOUND, "not found".to_string()),
            Some(kf_core::Error::VersionChanged) => (
                StatusCode::CONFLICT,
                "document version changed; restart reading".to_string(),
            ),
            Some(kf_core::Error::Invalid(message)) => (StatusCode::BAD_REQUEST, message.clone()),
            Some(kf_core::Error::Unavailable(message)) => {
                (StatusCode::SERVICE_UNAVAILABLE, message.clone())
            }
            None => {
                tracing::warn!(error=%self.0,"private service error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "operation failed; inspect the private service log".into(),
                )
            }
        };
        (status, Json(json!({"error":message}))).into_response()
    }
}

async fn status(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let mut value = s.store.status().await?;
    value["capabilities"] = json!({"lexical":true,"embeddings":s.models.ready(),"hybrid":s.models.ready(),"reranker":s.models.reranker_ready(),"automatic_captions":false,"publication":false});
    value["model_generation"] = json!(s.models.generation());
    value["caption_reader_state"] = json!("suspended_public_reader_blocked");
    Ok(Json(value))
}
async fn sources(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(s.store.sources().await?))
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiReadRequest {
    pub proposal_id: Option<uuid::Uuid>,
    #[serde(default)]
    pub cursor: usize,
    #[serde(default = "wiki_read_limit")]
    #[schemars(range(min = 1, max = 8000))]
    pub limit: usize,
    pub expected_hash: Option<String>,
}
fn wiki_read_limit() -> usize {
    8000
}

async fn read_wiki(
    State(s): State<AppState>,
    Json(request): Json<WikiReadRequest>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        s.store
            .read_wiki(
                request.proposal_id,
                request.cursor,
                request.limit,
                request.expected_hash.as_deref(),
            )
            .await?,
    ))
}
async fn read(
    State(s): State<AppState>,
    Json(r): Json<ReadRequest>,
) -> Result<Json<kf_core::ReadPage>, ApiError> {
    let doc = s.store.get_document(r.document_id).await?;
    Ok(Json(kf_core::read_page(&doc, &r)?))
}
async fn provenance(
    State(s): State<AppState>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(s.store.provenance(id).await?))
}
async fn resolve(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({"document_id":s.store.resolve_external(&id).await?}),
    ))
}

#[derive(Deserialize)]
struct EvidenceCheckRequest {
    evidence: Vec<kf_core::Evidence>,
}

async fn evidence_check(
    State(s): State<AppState>,
    Json(request): Json<EvidenceCheckRequest>,
) -> Result<Json<Value>, ApiError> {
    if request.evidence.is_empty() || request.evidence.len() > 100 {
        return Err(
            kf_core::Error::Invalid("evidence check accepts 1..=100 references".into()).into(),
        );
    }
    for evidence in &request.evidence {
        s.store.check_evidence(evidence).await?;
    }
    Ok(Json(
        json!({"valid":true,"count":request.evidence.len(),"verification":"immutable_database_version_and_exact_utf8_range","truth_certification":false}),
    ))
}

async fn search(
    State(s): State<AppState>,
    Json(r): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    r.validate()?;
    let started = std::time::Instant::now();
    if r.mode == SearchMode::Deep && r.limit > 10 {
        return Err(
            kf_core::Error::Invalid("deep search accepts at most 10 results".into()).into(),
        );
    }
    let mut candidates = r.clone();
    candidates.limit = 50;
    let lexical = if r.mode != SearchMode::Semantic {
        s.store.lexical(&candidates).await?
    } else {
        Vec::new()
    };
    let semantic = if r.mode != SearchMode::Lexical {
        let generation = s
            .models
            .generation()
            .ok_or_else(|| kf_core::Error::Unavailable("model manifest not installed".into()))?;
        let vectors = s.models.embed(vec![r.query.clone()]).await?;
        s.store
            .semantic(&candidates, &vectors[0], generation)
            .await?
    } else {
        Vec::new()
    };
    let mut hits = match r.mode {
        SearchMode::Lexical => lexical,
        SearchMode::Semantic => semantic,
        SearchMode::Hybrid | SearchMode::Deep => kf_core::rrf(&lexical, &semantic, 50),
    };
    if r.mode == SearchMode::Deep {
        hits.truncate(10);
        let ranked = s
            .models
            .rerank(r.query, hits.iter().map(|h| h.text.clone()).collect())
            .await?;
        hits = ranked
            .into_iter()
            .map(|(index, score)| {
                let mut h = hits[index].clone();
                h.score = score;
                h
            })
            .collect();
    }
    hits.truncate(r.limit);
    tracing::info!(elapsed_ms=started.elapsed().as_millis(),mode=?r.mode,hits=hits.len(),"search completed");
    Ok(Json(SearchResponse {
        mode_executed: r.mode,
        index_generation: s.models.generation().map(str::to_owned),
        hits,
    }))
}

#[derive(Deserialize, Serialize)]
pub struct EmbedRequest {
    pub texts: Vec<String>,
}
async fn embed(
    State(s): State<AppState>,
    Json(r): Json<EmbedRequest>,
) -> Result<Json<Value>, ApiError> {
    let generation = s
        .models
        .generation()
        .ok_or_else(|| kf_core::Error::Unavailable("model manifest missing".into()))?
        .to_string();
    Ok(Json(
        json!({"generation":generation,"vectors":s.models.embed(r.texts).await?}),
    ))
}
async fn chunk(
    State(s): State<AppState>,
    Json(doc): Json<kf_core::PreparedDocument>,
) -> Result<Json<Value>, ApiError> {
    let counter = s
        .counter
        .as_ref()
        .ok_or_else(|| kf_core::Error::Unavailable("model tokenizer missing".into()))?;
    Ok(Json(
        json!({"chunks":kf_ingest::chunk(&doc,counter.as_ref(),350,60)?}),
    ))
}
async fn propose(
    State(s): State<AppState>,
    Json(p): Json<WikiProposal>,
) -> Result<Json<Value>, ApiError> {
    let store = s.curator.as_ref().ok_or_else(|| {
        kf_core::Error::Unavailable("curator database role not configured".into())
    })?;
    Ok(Json(store.submit_proposal(&p).await?))
}

fn routes(state: AppState, curator: bool) -> Router {
    let router = Router::new()
        .route("/v1/status", get(status))
        .route("/v1/sources", get(sources))
        .route("/v1/search", post(search))
        .route("/v1/read", post(read))
        .route("/v1/wiki/read", post(read_wiki))
        .route("/v1/provenance/{id}", get(provenance))
        .route("/v1/resolve/{id}", get(resolve))
        .route(
            "/v1/evidence-check",
            post(evidence_check).layer(DefaultBodyLimit::max(1024 * 1024)),
        )
        .route("/v1/embedding", post(embed))
        .route("/v1/chunks", post(chunk));
    let router = if curator {
        router.route("/v1/wiki/proposals", post(propose))
    } else {
        router
    };
    router
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .layer(tower::limit::ConcurrencyLimitLayer::new(16))
        .with_state(state)
}

#[cfg(unix)]
fn listener(path: &FsPath) -> Result<tokio::net::UnixListener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    ensure!(path.is_absolute(), "socket must be absolute");
    let parent = path.parent().context("socket parent missing")?;
    ensure!(
        parent.is_dir(),
        "socket parent must be provisioned before startup"
    );
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        ensure!(
            metadata.file_type().is_socket(),
            "refusing to replace a non-socket file"
        );
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => bail!("service socket already active"),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                std::fs::remove_file(path)?
            }
            Err(e) => return Err(e.into()),
        }
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    // Access is restricted by the separately provisioned 0750 parent directories.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

pub async fn serve(config: Config) -> Result<()> {
    #[cfg(unix)]
    {
        let store = config.store().await?;
        let models = models::Runtime::load(&config.model_manifest)?;
        let counter = models
            .manifest
            .as_ref()
            .map(models::counter)
            .transpose()?
            .map(Arc::new);
        let curator = match std::env::var("KF_CURATOR_DATABASE_URL") {
            Ok(url) => Some(Store::connect(&url, 2).await?),
            Err(_) => None,
        };
        let state = AppState {
            store,
            curator,
            models,
            counter,
        };
        let query_listener = listener(&config.socket)?;
        let curator_listener = listener(&config.curator_socket)?;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let stop = cancellation.clone();
        tokio::spawn(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal handler");
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
            stop.cancel();
        });
        tracing::info!(
            embeddings = state.models.ready(),
            reranker = state.models.reranker_ready(),
            "knowledge service starting"
        );
        let query = axum::serve(query_listener, routes(state.clone(), false))
            .with_graceful_shutdown(cancellation.clone().cancelled_owned());
        let curator = axum::serve(curator_listener, routes(state, true))
            .with_graceful_shutdown(cancellation.cancelled_owned());
        tokio::try_join!(query, curator)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = config;
        bail!("production Unix sockets require Linux or another Unix system");
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
}
impl Client {
    pub fn new(socket: &FsPath) -> Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                http: reqwest::Client::builder()
                    .unix_socket(socket)
                    .timeout(std::time::Duration::from_secs(60))
                    .build()?,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = socket;
            bail!("Unix socket client requires Unix");
        }
    }
    pub async fn get(&self, path: &str) -> Result<Value> {
        self.finish(
            self.http
                .get(format!("http://localhost{path}"))
                .send()
                .await?,
        )
        .await
    }
    pub async fn post(&self, path: &str, value: &impl Serialize) -> Result<Value> {
        self.finish(
            self.http
                .post(format!("http://localhost{path}"))
                .json(value)
                .send()
                .await?,
        )
        .await
    }
    async fn finish(&self, response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        let bytes = response.bytes().await?;
        ensure!(
            bytes.len() <= 16 * 1024 * 1024,
            "internal response exceeds limit"
        );
        let value: Value = serde_json::from_slice(&bytes)?;
        if !status.is_success() {
            bail!(
                "{}",
                value["error"].as_str().unwrap_or("internal request failed")
            );
        }
        Ok(value)
    }
}
