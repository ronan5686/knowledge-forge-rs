use kf_core::{
    CaptionKind, Chunk, Claim, Evidence, Metadata, PreparedDocument, SearchFilters, SearchMode,
    SearchRequest, WikiProposal,
};
use kf_store::Store;
use serde_json::json;
use uuid::Uuid;

fn fixture(source: &str, text: &str, complete: bool) -> (PreparedDocument, Vec<Chunk>) {
    let metadata = Metadata {
        external_id: format!("episode-{source}"),
        source_id: source.into(),
        title: "Synthetic engineering".into(),
        publisher: "Synthetic publisher".into(),
        source_url: Some("https://example.invalid/watch".into()),
        published_at: Some("2026-10-27T12:00:00Z".into()),
        language: Some("fr".into()),
        caption_kind: CaptionKind::Human,
        themes: vec!["systems".into()],
        speakers: vec!["Ada".into()],
    };
    let markdown = format!("source={source}; complete={complete}\n{text}");
    let hash = kf_core::sha256(markdown.as_bytes());
    let document = PreparedDocument {
        metadata,
        segments: vec![],
        text: text.into(),
        markdown,
        source_hash: kf_core::sha256(text.as_bytes()),
        document_hash: hash,
        complete,
        capture_format: "txt".into(),
    };
    let chunks = vec![Chunk {
        index: 0,
        text: text.into(),
        start_byte: 0,
        end_byte: text.len(),
        start_ms: Some(1000),
        end_ms: Some(3000),
    }];
    (document, chunks)
}

#[tokio::test]
#[ignore = "requires an explicitly configured disposable PostgreSQL 16+ / pgvector 0.8.7+ database"]
async fn version_search_evidence_and_queue_contracts() -> anyhow::Result<()> {
    let url = std::env::var("KF_TEST_DATABASE_URL")
        .expect("KF_TEST_DATABASE_URL is mandatory for this explicit integration test");
    // This test creates synthetic rows only and never truncates a database.
    assert!(
        url.split('?')
            .next()
            .unwrap_or_default()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .contains("kf_test"),
        "integration database name must contain kf_test"
    );
    let store = Store::connect(&url, 5).await?;
    store.migrate().await?;
    let source = format!("test-{}", Uuid::new_v4());
    let (original, chunks) = fixture(&source, "Énergie maison ventilation engineering", true);
    let id = original.metadata.document_id();
    store.put_document(&original, &chunks).await?;
    store.put_document(&original, &chunks).await?;
    let (partial, partial_chunks) = fixture(&source, "partial capture", false);
    assert_eq!(
        store.put_document(&partial, &partial_chunks).await?["preserved_complete_current"],
        true
    );
    assert_eq!(
        store.get_document(id).await?.document_hash,
        original.document_hash
    );
    let mut wrong = original.clone();
    wrong.text = "different bytes".into();
    assert!(store.put_document(&wrong, &chunks).await.is_err());
    let request = SearchRequest {
        query: "ventilation".into(),
        filters: SearchFilters {
            source_id: Some(source.clone()),
            speaker: Some("Ada".into()),
            theme: Some("systems".into()),
            language: Some("fr".into()),
            published_after: Some("2026-10-27T00:00:00Z".into()),
            published_before: Some("2026-10-28T00:00:00Z".into()),
        },
        limit: 10,
        mode: SearchMode::Lexical,
    };
    let hits = store.lexical(&request).await?;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].document_hash, original.document_hash);
    let mut injected = request.clone();
    injected.filters.source_id = Some("' OR TRUE --".into());
    assert!(store.lexical(&injected).await?.is_empty());
    let vector = vec![1.0; 1024];
    let generation = format!("test-{}", Uuid::new_v4());
    store
        .put_embedding(id, &original.document_hash, 0, &generation, &vector)
        .await?;
    store
        .put_embedding(id, &original.document_hash, 0, &generation, &vector)
        .await?;
    let mut changed = vector.clone();
    changed[0] = 0.2;
    assert!(
        store
            .put_embedding(id, &original.document_hash, 0, &generation, &changed)
            .await
            .is_err()
    );
    assert_eq!(
        store.semantic(&request, &vector, &generation).await?.len(),
        1
    );
    assert!(
        store
            .semantic(&request, &vector, "missing-generation")
            .await?
            .is_empty()
    );
    assert!(
        store
            .pending_embeddings(&generation, 1000)
            .await?
            .iter()
            .all(|(document, hash, _, _)| *document != id || hash != &original.document_hash)
    );
    let (replacement, replacement_chunks) = fixture(&source, "Nouveau texte isolation", true);
    store
        .put_document(&replacement, &replacement_chunks)
        .await?;
    assert!(store.lexical(&request).await?.is_empty());
    assert!(
        store
            .semantic(&request, &vector, &generation)
            .await?
            .is_empty()
    );
    let mut proposal = WikiProposal {
        kind: "wiki".into(),
        title: "Synthetic proposal".into(),
        claims: vec![Claim {
            text: "The source mentions energy".into(),
            evidence: vec![Evidence {
                document_id: id,
                document_hash: original.document_hash.clone(),
                start_byte: 0,
                end_byte: "Énergie".len(),
                quote: "Énergie".into(),
            }],
        }],
    };
    let proposal_receipt = store.submit_proposal(&proposal).await?;
    assert_eq!(proposal_receipt["truth_certification"], false);
    let proposal_id = serde_json::from_value(proposal_receipt["proposal_id"].clone())?;
    let summaries = store.read_wiki(None, 0, 8000, None).await?;
    assert_eq!(summaries["generated"], true);
    assert_eq!(summaries["independent_evidence"], false);
    assert!(
        summaries["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["proposal_id"] == proposal_receipt["proposal_id"])
    );
    let first = store.read_wiki(Some(proposal_id), 0, 17, None).await?;
    let hash = first["proposal_hash"].as_str().unwrap().to_string();
    let mut rendered = first["text"].as_str().unwrap().to_string();
    let mut next = first["next_cursor"].as_u64();
    while let Some(cursor) = next {
        let page = store
            .read_wiki(Some(proposal_id), cursor as usize, 17, Some(&hash))
            .await?;
        rendered.push_str(page["text"].as_str().unwrap());
        next = page["next_cursor"].as_u64();
        if next.is_none() {
            assert_eq!(page["eof"], true);
        }
    }
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&rendered)?,
        serde_json::to_value(&proposal)?
    );
    assert_eq!(kf_core::sha256(rendered.as_bytes()), hash);
    assert!(
        store
            .read_wiki(Some(proposal_id), 1, 17, None)
            .await
            .is_err()
    );
    assert!(
        store
            .read_wiki(Some(proposal_id), 0, 8001, None)
            .await
            .is_err()
    );
    assert!(store.read_wiki(None, 1, 8000, None).await.is_err());
    store
        .check_evidence(&proposal.claims[0].evidence[0])
        .await?;
    proposal.claims[0].evidence[0].end_byte = 1;
    assert!(store.submit_proposal(&proposal).await.is_err());
    assert!(
        store
            .check_evidence(&proposal.claims[0].evidence[0])
            .await
            .is_err()
    );
    proposal.claims[0].evidence[0].end_byte = "Énergie".len();
    proposal.claims[0].evidence[0].document_hash = replacement.document_hash.clone();
    assert!(store.submit_proposal(&proposal).await.is_err());
    assert_eq!(
        store.provenance(id).await?["versions"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let key = Uuid::new_v4().to_string();
    let job = store
        .enqueue("local.synthetic", &key, &json!({"synthetic":true}))
        .await?;
    assert_eq!(
        store
            .enqueue("local.synthetic", &key, &json!({"synthetic":true}))
            .await?["id"],
        job["id"]
    );
    assert!(
        store
            .enqueue("local.synthetic", &key, &json!({"synthetic":false}))
            .await
            .is_err()
    );
    let worker = format!("test-{}", Uuid::new_v4());
    let claimed = store
        .claim_job(&worker)
        .await?
        .expect("queued synthetic job");
    let claim_id = Uuid::parse_str(claimed["id"].as_str().unwrap())?;
    assert!(
        store
            .finish_job(claim_id, "other-worker", true, &json!({}))
            .await
            .is_err()
    );
    store
        .finish_job(claim_id, &worker, true, &json!({"synthetic":true}))
        .await?;
    assert!(
        store
            .finish_job(claim_id, &worker, true, &json!({}))
            .await
            .is_err()
    );
    assert!(store.status().await?["hnsw_installed"].as_bool().unwrap());
    let catalog_only = format!("catalog-{}", Uuid::new_v4());
    sqlx::query("INSERT INTO kf_sources(source_id,catalog) VALUES($1,$2)")
        .bind(&catalog_only)
        .bind(json!({"name":"Synthetic catalog only"}))
        .execute(store.pool())
        .await?;
    let sources = store.sources().await?;
    let empty_source = sources
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source_id"] == catalog_only)
        .expect("catalog source without a transcript remains visible");
    assert_eq!(empty_source["documents"], 0);
    assert_eq!(empty_source["speakers"], json!([]));
    assert_eq!(
        store
            .resolve_external(&original.metadata.external_id)
            .await?,
        id
    );
    let retry_key = Uuid::new_v4().to_string();
    store
        .enqueue("local.retry", &retry_key, &json!({"synthetic":true}))
        .await?;
    let retry = store.claim_job("old-unique-run").await?.unwrap();
    let retry_id = Uuid::parse_str(retry["id"].as_str().unwrap())?;
    sqlx::query("UPDATE kf_jobs SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(retry_id)
        .execute(store.pool())
        .await?;
    let recovered = store.claim_job("new-unique-run").await?.unwrap();
    assert_eq!(recovered["id"], retry["id"]);
    assert_eq!(recovered["attempts"], 2);
    assert!(
        store
            .finish_job(retry_id, "old-unique-run", true, &json!({}))
            .await
            .is_err()
    );
    store
        .finish_job(retry_id, "new-unique-run", true, &json!({}))
        .await?;
    store
        .enqueue(
            "google.synthetic",
            &Uuid::new_v4().to_string(),
            &json!({"synthetic":true}),
        )
        .await?;
    let external = store.claim_job("external-unique-run").await?.unwrap();
    let external_id = Uuid::parse_str(external["id"].as_str().unwrap())?;
    sqlx::query("UPDATE kf_jobs SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(external_id)
        .execute(store.pool())
        .await?;
    assert!(store.claim_job("recovery-unique-run").await?.is_none());
    let state: String = sqlx::query_scalar("SELECT state FROM kf_jobs WHERE id=$1")
        .bind(external_id)
        .fetch_one(store.pool())
        .await?;
    assert_eq!(state, "needs_reconciliation");

    // Exercise the >2,048-candidate path, without calling an embedding model.
    let large_source = format!("large-{}", Uuid::new_v4());
    let (large, _) = fixture(&large_source, &"x".repeat(2050), true);
    let large_chunks: Vec<_> = (0..2050)
        .map(|i| Chunk {
            index: i as i32,
            text: "x".into(),
            start_byte: i,
            end_byte: i + 1,
            start_ms: None,
            end_ms: None,
        })
        .collect();
    store.put_document(&large, &large_chunks).await?;
    sqlx::query("INSERT INTO kf_embeddings(document_id,hash,chunk_index,generation,embedding) SELECT document_id,hash,chunk_index,$3,$4 FROM kf_chunks WHERE document_id=$1 AND hash=$2").bind(large.metadata.document_id()).bind(&large.document_hash).bind(&generation).bind(pgvector::Vector::from(vector.clone())).execute(store.pool()).await?;
    let mut large_request = request.clone();
    large_request.filters.source_id = Some(large_source);
    assert_eq!(
        store
            .semantic(&large_request, &vector, &generation)
            .await?
            .len(),
        10
    );
    Ok(())
}
