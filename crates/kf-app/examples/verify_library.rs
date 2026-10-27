//! Read-only operator qualification. No Google calls, writes or model inference.

#[cfg(unix)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use kf_core::{PreparedDocument, ReadPage, ReadRequest, sha256};
    use serde_json::json;
    use std::{fs, path::PathBuf, time::Duration};

    let args: Vec<_> = std::env::args_os().collect();
    ensure!(
        args.len() == 4,
        "usage: verify_library DATA_ROOT SOCKET EXPECTED_VERSIONS"
    );
    let root = PathBuf::from(&args[1]);
    let socket = PathBuf::from(&args[2]);
    let expected: usize = args[3].to_str().context("invalid count")?.parse()?;
    ensure!(
        root.is_absolute() && !root.symlink_metadata()?.file_type().is_symlink(),
        "use an absolute, non-symlink data root"
    );
    let http = reqwest::Client::builder()
        .unix_socket(socket)
        .timeout(Duration::from_secs(15))
        .build()?;
    let mut records = Vec::new();
    for directory in fs::read_dir(root.join("documents"))? {
        let directory = directory?;
        ensure!(
            !directory.file_type()?.is_symlink(),
            "document directory is a symlink"
        );
        if !directory.file_type()?.is_dir() {
            continue;
        }
        for item in fs::read_dir(directory.path())? {
            let item = item?;
            ensure!(
                !item.file_type()?.is_symlink(),
                "document file is a symlink"
            );
            if item.path().extension().is_some_and(|v| v == "json") {
                records.push(item.path());
            }
        }
    }
    records.sort();
    ensure!(
        records.len() == expected,
        "unexpected archive version count"
    );
    let mut report = Vec::new();
    for path in records {
        ensure!(
            path.metadata()?.len() <= 250_000_000,
            "oversized document record"
        );
        let doc: PreparedDocument = serde_json::from_slice(&fs::read(&path)?)?;
        doc.metadata.validate()?;
        for hash in [&doc.source_hash, &doc.document_hash] {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid archive fingerprint"
            );
        }
        let raw_path = root.join("raw").join(format!("{}.bin", doc.source_hash));
        ensure!(
            !raw_path.symlink_metadata()?.file_type().is_symlink(),
            "raw source is a symlink"
        );
        let raw = fs::read(raw_path)?;
        ensure!(
            sha256(&raw) == doc.source_hash,
            "raw source fingerprint differs"
        );
        let format = match doc.capture_format.as_str() {
            "browser_youtube_export" | "plain_text" => "txt",
            "obsidian_web_clipper_native_transcript" | "plain_markdown" => "md",
            "json_segments" => "json",
            "srt" => "srt",
            "vtt" => "vtt",
            _ => anyhow::bail!("unsupported stored capture format"),
        };
        let rebuilt = kf_ingest::prepare(&raw, format, doc.metadata.clone(), doc.complete)?;
        ensure!(
            serde_json::to_value(&rebuilt)? == serde_json::to_value(&doc)?,
            "raw source does not reproduce the entire document"
        );
        let markdown = fs::read(path.with_extension("md"))?;
        ensure!(
            markdown == doc.markdown.as_bytes() && sha256(&markdown) == doc.document_hash,
            "Markdown fingerprint differs"
        );
        let mut cursor = 0;
        let mut joined = String::new();
        let mut pages = 0usize;
        loop {
            let request = ReadRequest {
                document_id: doc.metadata.document_id(),
                cursor,
                limit: 4096,
                expected_hash: Some(doc.document_hash.clone()),
            };
            let page: ReadPage = http
                .post("http://localhost/v1/read")
                .json(&request)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            ensure!(
                page.document_hash == doc.document_hash
                    && page.total_bytes == markdown.len()
                    && page.complete == doc.complete,
                "service version or completeness differs"
            );
            joined.push_str(&page.markdown);
            pages += 1;
            ensure!(
                joined.len() <= markdown.len(),
                "service returned excess text"
            );
            if page.eof {
                ensure!(page.next_cursor.is_none(), "EOF contains another cursor");
                break;
            }
            let next = page.next_cursor.context("missing next cursor")?;
            ensure!(
                next > cursor && next == joined.len() && next < markdown.len(),
                "invalid pagination progress"
            );
            cursor = next;
        }
        ensure!(
            joined == doc.markdown,
            "service omitted or altered Markdown bytes"
        );
        let spoken: Vec<_> = doc.segments.iter().filter(|s| !s.heading).collect();
        report.push(json!({"external_id":doc.metadata.external_id,"publisher":doc.metadata.publisher,"segments":spoken.len(),"first_timestamp_ms":spoken.first().and_then(|s|s.start_ms),"last_timestamp_ms":spoken.last().and_then(|s|s.start_ms),"markdown_bytes":markdown.len(),"source_hash":doc.source_hash,"document_hash":doc.document_hash,"raw_to_markdown_exact":true,"service_pages":pages,"service_eof":true,"service_bytes_exact":true,"complete":doc.complete}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"versions_verified":report.len(),"all_stored_versions_passed":true,"audio_completeness_verified":false,"new_acquisition_test":false,"documents":report})
        )?
    );
    Ok(())
}

#[cfg(not(unix))]
fn main() {
    eprintln!("verify_library requires a Unix socket and a private local archive");
    std::process::exit(1);
}
