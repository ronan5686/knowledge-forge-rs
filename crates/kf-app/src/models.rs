use crate::Config;
use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use kf_core::sha256;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::Semaphore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub repo: String,
    pub revision: String,
    pub license: String,
    pub root: PathBuf,
    pub files: BTreeMap<String, String>,
    pub graph: String,
    pub external_initializers: Vec<String>,
    pub qualified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualification_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub generation: String,
    pub embedding: Artifact,
    pub reranker: Option<Artifact>,
    pub dimension: usize,
    pub pooling: String,
    pub normalization: String,
    pub chunk_tokens: usize,
    pub overlap_tokens: usize,
}

fn file_hash(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut buf = [0u8; 65536];
    let mut h = Sha256::new();
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

fn check_artifact(model: &Artifact) -> Result<()> {
    ensure!(
        model.revision.len() == 40 && model.revision.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid model revision"
    );
    let root = model.root.canonicalize()?;
    for (relative, expected) in &model.files {
        let p = model.root.join(relative);
        ensure!(
            !p.symlink_metadata()?.file_type().is_symlink(),
            "model symlink rejected"
        );
        ensure!(
            p.canonicalize()?.starts_with(&root),
            "model path outside approved directory"
        );
        ensure!(
            file_hash(&p)? == *expected,
            "model file hash mismatch: {relative}"
        );
    }
    ensure!(
        model.files.contains_key(&model.graph),
        "graph missing from manifest"
    );
    for f in &model.external_initializers {
        ensure!(
            model.files.contains_key(f),
            "external weights missing from manifest"
        );
    }
    for f in [
        "tokenizer.json",
        "config.json",
        "special_tokens_map.json",
        "tokenizer_config.json",
    ] {
        ensure!(
            model.files.contains_key(f),
            "tokenizer files missing from manifest"
        );
    }
    Ok(())
}

pub fn verify(path: &Path) -> Result<Manifest> {
    let m: Manifest =
        serde_json::from_slice(&std::fs::read(path).context("model manifest not installed")?)?;
    ensure!(
        m.schema_version == 1
            && m.dimension == 1024
            && m.pooling == "cls"
            && m.normalization == "l2"
            && m.chunk_tokens == 350
            && m.overlap_tokens == 60,
        "unsupported model contract"
    );
    ensure!(
        m.embedding.repo == "BAAI/bge-m3",
        "unapproved embedding model"
    );
    check_artifact(&m.embedding)?;
    if let Some(r) = &m.reranker {
        ensure!(r.repo == "rozgo/bge-reranker-v2-m3", "unapproved reranker");
        check_artifact(r)?;
        if r.qualified {
            let report = r.root.join("qualification.json");
            ensure!(
                r.qualification_sha256.as_deref() == Some(file_hash(&report)?.as_str()),
                "reranker qualification report missing or changed"
            );
            let proof: serde_json::Value = serde_json::from_slice(&std::fs::read(report)?)?;
            ensure!(
                proof["revision"].as_str() == Some(&r.revision)
                    && proof["passed"] == true
                    && proof["reference_framework_parity"] == false,
                "invalid reranker sanity qualification"
            );
        }
    }
    let expected = generation(&m.embedding);
    ensure!(
        m.generation == expected,
        "generation does not match model manifest"
    );
    Ok(m)
}

fn generation(a: &Artifact) -> String {
    let contract = serde_json::json!({"repo":a.repo,"revision":a.revision,"files":a.files,"dimension":1024,"pooling":"cls","normalization":"l2","precision":"fp32","chunk":350,"overlap":60});
    format!(
        "bge-m3-{}",
        sha256(
            serde_json::to_string(&contract)
                .expect("JSON serialization")
                .as_bytes()
        )
    )
}

async fn fetch_artifact(client: &reqwest::Client, base: &Path, reranker: bool) -> Result<Artifact> {
    let repo = if reranker {
        "rozgo/bge-reranker-v2-m3"
    } else {
        "BAAI/bge-m3"
    };
    let info: serde_json::Value = client
        .get(format!(
            "https://huggingface.co/api/models/{repo}?blobs=true"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let revision = info["sha"]
        .as_str()
        .context("Hugging Face revision unavailable")?
        .to_string();
    let license = info["cardData"]["license"]
        .as_str()
        .context("model license not declared in provider metadata")?
        .to_string();
    ensure!(
        matches!(license.as_str(), "mit" | "apache-2.0"),
        "model license requires separate operator review"
    );
    ensure!(
        revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid model revision"
    );
    let root = base
        .join(if reranker { "reranker" } else { "embedding" })
        .join(&revision);
    std::fs::create_dir_all(&root)?;
    let graph = if reranker {
        "model.onnx"
    } else {
        "onnx/model.onnx"
    };
    let external = if reranker {
        vec!["model.onnx.data"]
    } else {
        vec!["onnx/model.onnx_data", "onnx/Constant_7_attr__value"]
    };
    let mut names = vec![
        graph,
        "tokenizer.json",
        "config.json",
        "special_tokens_map.json",
        "tokenizer_config.json",
    ];
    names.extend(external.iter().copied());
    let siblings = info["siblings"]
        .as_array()
        .context("model file listing unavailable")?;
    let mut files = BTreeMap::new();
    for name in names {
        let entry = siblings
            .iter()
            .find(|s| s["rfilename"].as_str() == Some(name))
            .with_context(|| format!("approved model file absent: {name}"))?;
        let expected = entry["lfs"]["sha256"].as_str();
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path.exists() && expected.is_some_and(|h| file_hash(&path).ok().as_deref() == Some(h)) {
            files.insert(name.into(), file_hash(&path)?);
            continue;
        }
        let response = client
            .get(format!(
                "https://huggingface.co/{repo}/resolve/{revision}/{name}"
            ))
            .send()
            .await?
            .error_for_status()?;
        if let Some(size) = response.content_length() {
            ensure!(size <= 3_000_000_000, "model file exceeds size budget");
        }
        let mut stream = response.bytes_stream();
        let mut temp =
            tempfile::NamedTempFile::new_in(path.parent().context("model directory missing")?)?;
        let mut total = 0u64;
        let mut digest = Sha256::new();
        use std::io::Write;
        while let Some(bytes) = stream.next().await {
            let bytes = bytes?;
            total += bytes.len() as u64;
            ensure!(total <= 3_000_000_000, "model file exceeds size budget");
            temp.write_all(&bytes)?;
            digest.update(&bytes);
        }
        temp.as_file().sync_all()?;
        let hash = hex::encode(digest.finalize());
        if let Some(expected) = expected {
            ensure!(
                hash == expected,
                "downloaded model differs from upstream LFS hash"
            );
        }
        temp.persist(&path).map_err(|e| e.error)?;
        files.insert(name.into(), hash);
        tracing::info!(
            model = repo,
            file = name,
            bytes = total,
            "model artifact verified"
        );
    }
    Ok(Artifact {
        repo: repo.into(),
        revision,
        license,
        root,
        files,
        graph: graph.into(),
        external_initializers: external.into_iter().map(str::to_owned).collect(),
        qualified: !reranker,
        qualification_sha256: None,
    })
}

pub async fn fetch(config: &Config, reranker: bool) -> Result<()> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .user_agent("knowledge-forge-rs/0.1")
        .timeout(std::time::Duration::from_secs(1800))
        .build()?;
    let embedding = fetch_artifact(&client, &config.data_root.join("models"), false).await?;
    let reranker = if reranker {
        Some(fetch_artifact(&client, &config.data_root.join("models"), true).await?)
    } else {
        None
    };
    let manifest = Manifest {
        schema_version: 1,
        generation: generation(&embedding),
        embedding,
        reranker,
        dimension: 1024,
        pooling: "cls".into(),
        normalization: "l2".into(),
        chunk_tokens: 350,
        overlap_tokens: 60,
    };
    let parent = config
        .model_manifest
        .parent()
        .context("manifest directory missing")?;
    std::fs::create_dir_all(parent)?;
    use std::io::Write;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    temp.as_file().sync_all()?;
    temp.persist(&config.model_manifest).map_err(|e| e.error)?;
    super::print_json(&manifest)
}

/// An explicitly scoped artifact smoke qualification, not a claim of equivalence
/// to the publisher's training framework or of retrieval benchmark quality.
pub async fn qualify_reranker(config: &Config) -> Result<()> {
    crate::operator::resource_guard(&config.data_root)?;
    #[cfg(not(feature = "inference"))]
    {
        let _ = config;
        bail!("reranker qualification requires the inference feature");
    }
    #[cfg(feature = "inference")]
    {
        let mut manifest = verify(&config.model_manifest)?;
        let artifact = manifest
            .reranker
            .as_mut()
            .context("download the approved reranker first")?;
        let mut model = fastembed::TextRerank::try_new_from_user_defined(
            fastembed::UserDefinedRerankingModel::new(
                artifact.root.join(&artifact.graph),
                tokenizer_files(artifact)?,
            ),
            fastembed::RerankInitOptionsUserDefined::new()
                .with_max_length(1024)
                .with_intra_threads(4),
        )?;
        let cases = [
            (
                "Which city is the capital of France?",
                vec![
                    "Paris is the capital of France.",
                    "Berlin is the capital of Germany.",
                    "This recipe uses flour and water.",
                ],
                0,
            ),
            (
                "Comment reprendre un traitement après une panne ?",
                vec![
                    "Pour reprendre un traitement après une panne, on conserve un point de reprise durable et on évite de répéter les écritures déjà confirmées.",
                    "Paris est la capitale de la France.",
                    "Le sucre est un ingrédient de pâtisserie.",
                ],
                0,
            ),
            (
                "How can a queue avoid duplicate side effects after a crash?",
                vec![
                    "A rainbow contains several colours.",
                    "Use idempotency keys and reconcile an uncertain external write before retrying it.",
                    "Change the colour of the navigation bar.",
                ],
                1,
            ),
        ];
        let mut results = Vec::new();
        for (query, docs, expected) in cases {
            let first = model.rerank(query, docs.clone(), false, Some(1))?;
            let second = model.rerank(query, docs.clone(), false, Some(1))?;
            ensure!(
                first.len() == docs.len() && second.len() == docs.len(),
                "reranker returned incomplete results"
            );
            ensure!(
                first
                    .iter()
                    .all(|r| r.index < docs.len() && r.score.is_finite()),
                "reranker returned invalid scores"
            );
            ensure!(
                first.first().is_some_and(|r| r.index == expected),
                "reranker failed the multilingual semantic ordering smoke test"
            );
            let mut delta = 0.0f32;
            for result in &first {
                let repeated = second
                    .iter()
                    .find(|r| r.index == result.index)
                    .context("reranker repeat omitted a result")?;
                delta = delta.max((result.score - repeated.score).abs());
            }
            ensure!(delta <= 0.0001, "reranker repeatability tolerance exceeded");
            results.push(serde_json::json!({"query":query,"expected_top_index":expected,"scores":first.iter().map(|r|serde_json::json!({"index":r.index,"score":r.score})).collect::<Vec<_>>(),"repeat_max_abs_delta":delta}));
        }
        let proof = serde_json::json!({"schema_version":1,"repo":artifact.repo,"revision":artifact.revision,"license":artifact.license,"graph_sha256":artifact.files.get(&artifact.graph),"passed":true,"scope":"finite scores, FR/EN semantic ordering and repeatability on three synthetic triplets","reference_framework_parity":false,"production_retrieval_quality_certified":false,"cases":results});
        let path = artifact.root.join("qualification.json");
        let mut file = tempfile::NamedTempFile::new_in(&artifact.root)?;
        use std::io::Write;
        file.write_all(&serde_json::to_vec_pretty(&proof)?)?;
        file.as_file().sync_all()?;
        file.persist(&path).map_err(|e| e.error)?;
        artifact.qualified = true;
        artifact.qualification_sha256 = Some(file_hash(&path)?);
        let mut file = tempfile::NamedTempFile::new_in(
            config
                .model_manifest
                .parent()
                .context("manifest directory unavailable")?,
        )?;
        file.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
        file.as_file().sync_all()?;
        file.persist(&config.model_manifest).map_err(|e| e.error)?;
        super::print_json(&proof)
    }
}

pub struct Counter(pub tokenizers::Tokenizer);
impl kf_ingest::TokenCounter for Counter {
    fn count(&self, text: &str) -> kf_core::Result<usize> {
        self.0
            .encode(text, true)
            .map(|e| e.len())
            .map_err(|_| kf_core::Error::Invalid("tokenization failed".into()))
    }
}

pub fn counter(manifest: &Manifest) -> Result<Counter> {
    let mut tokenizer =
        tokenizers::Tokenizer::from_file(manifest.embedding.root.join("tokenizer.json"))
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    tokenizer
        .with_truncation(None)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    tokenizer.with_padding(None);
    Ok(Counter(tokenizer))
}

#[cfg(feature = "inference")]
struct Loaded {
    embedding: fastembed::TextEmbedding,
    reranker: Option<fastembed::TextRerank>,
    count: Counter,
}
#[cfg(not(feature = "inference"))]
struct Loaded;

#[derive(Clone)]
pub struct Runtime {
    pub manifest: Option<Manifest>,
    loaded: Option<Arc<Mutex<Loaded>>>,
    permits: Arc<Semaphore>,
}

#[cfg(feature = "inference")]
fn tokenizer_files(a: &Artifact) -> Result<fastembed::TokenizerFiles> {
    Ok(fastembed::TokenizerFiles {
        tokenizer_file: std::fs::read(a.root.join("tokenizer.json"))?,
        config_file: std::fs::read(a.root.join("config.json"))?,
        special_tokens_map_file: std::fs::read(a.root.join("special_tokens_map.json"))?,
        tokenizer_config_file: std::fs::read(a.root.join("tokenizer_config.json"))?,
    })
}

impl Runtime {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                manifest: None,
                loaded: None,
                permits: Arc::new(Semaphore::new(1)),
            });
        }
        let manifest = verify(path)?;
        #[cfg(feature = "inference")]
        let loaded = {
            let a = &manifest.embedding;
            let mut model = fastembed::UserDefinedEmbeddingModel::new(
                std::fs::read(a.root.join(&a.graph))?,
                tokenizer_files(a)?,
            )
            .with_pooling(fastembed::Pooling::Cls);
            for relative in &a.external_initializers {
                let basename = Path::new(relative)
                    .file_name()
                    .context("invalid model file")?
                    .to_string_lossy()
                    .to_string();
                model = model
                    .with_external_initializer(basename, std::fs::read(a.root.join(relative))?);
            }
            let embedding = fastembed::TextEmbedding::try_new_from_user_defined(
                model,
                fastembed::InitOptionsUserDefined::new()
                    .with_max_length(512)
                    .with_intra_threads(4),
            )?;
            let reranker = if let Some(a) = manifest.reranker.as_ref().filter(|a| a.qualified) {
                Some(fastembed::TextRerank::try_new_from_user_defined(
                    fastembed::UserDefinedRerankingModel::new(
                        a.root.join(&a.graph),
                        tokenizer_files(a)?,
                    ),
                    fastembed::RerankInitOptionsUserDefined::new()
                        .with_max_length(1024)
                        .with_intra_threads(4),
                )?)
            } else {
                None
            };
            Some(Arc::new(Mutex::new(Loaded {
                embedding,
                reranker,
                count: counter(&manifest)?,
            })))
        };
        #[cfg(not(feature = "inference"))]
        let loaded = None;
        Ok(Self {
            manifest: Some(manifest),
            loaded,
            permits: Arc::new(Semaphore::new(1)),
        })
    }

    pub fn ready(&self) -> bool {
        self.loaded.is_some()
    }
    pub fn generation(&self) -> Option<&str> {
        self.manifest.as_ref().map(|m| m.generation.as_str())
    }
    pub fn reranker_ready(&self) -> bool {
        self.ready()
            && self
                .manifest
                .as_ref()
                .and_then(|m| m.reranker.as_ref())
                .is_some_and(|r| r.qualified)
    }

    pub async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        ensure!(
            !texts.is_empty() && texts.len() <= 4,
            "embedding batch must contain 1..4 texts"
        );
        ensure!(
            texts
                .iter()
                .all(|t| !t.trim().is_empty() && t.len() <= 32768),
            "invalid embedding input"
        );
        let Some(loaded) = self.loaded.clone() else {
            bail!(kf_core::Error::Unavailable(
                "local embeddings are not loaded".into()
            ));
        };
        let permit = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.permits.clone().acquire_owned(),
        )
        .await??;
        #[cfg(feature = "inference")]
        {
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let mut loaded = loaded
                    .lock()
                    .map_err(|_| anyhow::anyhow!("inference lock poisoned"))?;
                for text in &texts {
                    ensure!(
                        kf_ingest::TokenCounter::count(&loaded.count, text)? <= 512,
                        "embedding input would be truncated"
                    );
                }
                let vectors = loaded.embedding.embed(texts, Some(4))?;
                for v in &vectors {
                    ensure!(
                        v.len() == 1024 && v.iter().all(|x| x.is_finite()),
                        "invalid model output"
                    );
                }
                Ok(vectors)
            })
            .await?
        }
        #[cfg(not(feature = "inference"))]
        {
            let _ = (loaded, permit);
            bail!("binary compiled without inference");
        }
    }

    pub async fn rerank(&self, query: String, docs: Vec<String>) -> Result<Vec<(usize, f64)>> {
        ensure!(docs.len() <= 10, "reranker candidate budget exceeded");
        ensure!(
            self.reranker_ready(),
            "reranker artifact is not qualified and loaded"
        );
        #[cfg(feature = "inference")]
        {
            let loaded = self.loaded.clone().context("inference unavailable")?;
            let permit = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.permits.clone().acquire_owned(),
            )
            .await??;
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let mut loaded = loaded
                    .lock()
                    .map_err(|_| anyhow::anyhow!("inference lock poisoned"))?;
                let model = loaded.reranker.as_mut().context("reranker unavailable")?;
                let mut check = model.tokenizer.clone();
                check
                    .with_truncation(None)
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                check.with_padding(None);
                for doc in &docs {
                    ensure!(
                        check
                            .encode((query.as_str(), doc.as_str()), true)
                            .map_err(|e| anyhow::anyhow!(e.to_string()))?
                            .len()
                            <= 1024,
                        "reranker input would be truncated"
                    );
                }
                Ok(model
                    .rerank(query, docs, false, Some(1))?
                    .into_iter()
                    .map(|r| (r.index, f64::from(r.score)))
                    .collect())
            })
            .await?
        }
        #[cfg(not(feature = "inference"))]
        {
            let _ = (query, docs);
            bail!("binary compiled without inference");
        }
    }
}
