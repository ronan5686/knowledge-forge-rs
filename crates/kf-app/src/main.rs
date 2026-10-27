mod api;
mod backup;
mod evaluation;
mod google;
mod legacy;
mod mcp;
mod models;
mod operator;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kf_core::{Metadata, ReadRequest, SearchFilters, SearchMode, SearchRequest};
use kf_store::Store;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Source-grounded knowledge for agents. No implicit model downloads."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Migrate,
    Serve,
    Mcp {
        #[arg(long, default_value = "reader", value_parser = ["reader", "curator"])]
        role: String,
    },
    Status,
    Sources,
    Doctor,
    Import {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        metadata: PathBuf,
        #[arg(long)]
        format: Option<String>,
        #[arg(long, default_value_t = false)]
        complete: bool,
    },
    Search {
        query: String,
        #[arg(long, default_value = "hybrid")]
        mode: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        source: Option<String>,
    },
    Read {
        document_id: uuid::Uuid,
        #[arg(long, default_value_t = 0)]
        cursor: usize,
        #[arg(long, default_value_t = 8000)]
        limit: usize,
        #[arg(long)]
        expected_hash: Option<String>,
    },
    Models {
        #[command(subcommand)]
        command: ModelCommand,
    },
    Reindex {
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
    MigrateLegacy {
        #[arg(long)]
        root: PathBuf,
    },
    ExportLegacy {
        #[arg(long)]
        output: PathBuf,
    },
    Sync {
        #[arg(long, default_value_t = 0)]
        max_writes: usize,
    },
    Worker {
        #[arg(long, default_value_t = false)]
        once: bool,
    },
    Curate {
        #[arg(long, default_value_t = 1)]
        limit: usize,
    },
    Backup {
        #[arg(long, default_value_t = false)]
        restore_test: bool,
    },
    Benchmark {
        #[arg(long)]
        questions: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Subcommand)]
enum ModelCommand {
    Fetch {
        #[arg(long, default_value_t = false)]
        reranker: bool,
    },
    Verify,
    QualifyReranker,
}

#[derive(Clone)]
pub struct Config {
    data_root: PathBuf,
    socket: PathBuf,
    curator_socket: PathBuf,
    model_manifest: PathBuf,
}

impl Config {
    fn from_env() -> Self {
        let data_root = PathBuf::from(
            std::env::var("KF_DATA_ROOT")
                .unwrap_or_else(|_| "/srv/hermes-connaissance/data".into()),
        );
        Self {
            socket: PathBuf::from(
                std::env::var("KF_SOCKET")
                    .unwrap_or_else(|_| "/run/hermes-connaissance/query/query.sock".into()),
            ),
            curator_socket: PathBuf::from(
                std::env::var("KF_CURATOR_SOCKET")
                    .unwrap_or_else(|_| "/run/hermes-connaissance/curator/curator.sock".into()),
            ),
            model_manifest: PathBuf::from(std::env::var("KF_MODEL_MANIFEST").unwrap_or_else(
                |_| {
                    data_root
                        .join("models/manifest.json")
                        .to_string_lossy()
                        .into_owned()
                },
            )),
            data_root,
        }
    }

    async fn store(&self) -> Result<Store> {
        let url = std::env::var("KF_DATABASE_URL")
            .context("KF_DATABASE_URL is required for this operator command")?;
        Store::connect(&url, 4).await
    }
}

fn print_json(v: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .json()
        .init();
    if let Err(error) = run(Cli::parse()).await {
        tracing::error!(error = %error, "command failed");
        eprintln!("Knowledge Forge: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let config = Config::from_env();
    match cli.command {
        Command::Migrate => {
            config.store().await?.migrate().await?;
            print_json(&serde_json::json!({"migrated":true}))
        }
        Command::Serve => api::serve(config).await,
        Command::Mcp { role } => mcp::serve(config, role).await,
        Command::Status => print_json(&api::Client::new(&config.socket)?.get("/v1/status").await?),
        Command::Sources => {
            print_json(&api::Client::new(&config.socket)?.get("/v1/sources").await?)
        }
        Command::Doctor => print_json(&operator::doctor(&config).await?),
        Command::Import {
            file,
            metadata,
            format,
            complete,
        } => {
            let metadata: Metadata = serde_json::from_slice(&std::fs::read(metadata)?)?;
            let format = format
                .or_else(|| file.extension().and_then(|s| s.to_str()).map(str::to_owned))
                .context("format is required")?;
            operator::import(&config, &file, &format, metadata, complete).await
        }
        Command::Search {
            query,
            mode,
            limit,
            source,
        } => {
            let mode: SearchMode = serde_json::from_value(serde_json::Value::String(mode))?;
            let request = SearchRequest {
                query,
                mode,
                limit,
                filters: SearchFilters {
                    source_id: source,
                    ..Default::default()
                },
            };
            print_json(
                &api::Client::new(&config.socket)?
                    .post("/v1/search", &request)
                    .await?,
            )
        }
        Command::Read {
            document_id,
            cursor,
            limit,
            expected_hash,
        } => print_json(
            &api::Client::new(&config.socket)?
                .post(
                    "/v1/read",
                    &ReadRequest {
                        document_id,
                        cursor,
                        limit,
                        expected_hash,
                    },
                )
                .await?,
        ),
        Command::Models {
            command: ModelCommand::Fetch { reranker },
        } => models::fetch(&config, reranker).await,
        Command::Models {
            command: ModelCommand::Verify,
        } => print_json(&models::verify(&config.model_manifest)?),
        Command::Models {
            command: ModelCommand::QualifyReranker,
        } => models::qualify_reranker(&config).await,
        Command::Reindex { limit } => print_json(&operator::reindex(&config, limit).await?),
        Command::MigrateLegacy { root } => {
            print_json(&operator::migrate_legacy(&config, &root).await?)
        }
        Command::ExportLegacy { output } => operator::export_legacy(&config, &output).await,
        Command::Sync { max_writes } => print_json(&google::sync(&config, max_writes).await?),
        Command::Worker { once } => operator::worker(&config, once).await,
        Command::Curate { limit } => print_json(&operator::curate(&config, limit).await?),
        Command::Backup { restore_test } => {
            print_json(&operator::backup(&config, restore_test).await?)
        }
        Command::Benchmark { questions, output } => {
            operator::benchmark(&config, &questions, &output).await
        }
    }
}
