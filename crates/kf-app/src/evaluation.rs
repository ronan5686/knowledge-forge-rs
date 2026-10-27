//! Private retrieval evaluation, not a truth or abstention certificate.
//!
//! Generic input (replace identifiers with actual private corpus documents):
//! ```json
//! {"questions":[{"id":"q01","query":"How are safe retries defined?",
//! "filters":{"theme":"agents"},"answerable":true,
//! "expected":[{"document_id":"00000000-0000-0000-0000-000000000001",
//! "quote":"A quotation copied exactly from the selected source."}]}]}
//! ```
//! Expected quotations are optional passage targets. They do not automatically
//! establish that a retrieved passage supports a generated answer.

use crate::{Config, api::Client};
use anyhow::{Context, Result, ensure};
use kf_core::{
    Evidence, ReadPage, ReadRequest, SearchFilters, SearchHit, SearchMode, SearchRequest,
    SearchResponse,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Component, Path};
use std::time::Instant;
use uuid::Uuid;

const MAX_DATASET_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SOURCE_BYTES: usize = 100 * 1024 * 1024;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    document_id: Uuid,
    document_hash: Option<String>,
    quote: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Question {
    id: Option<String>,
    query: String,
    #[serde(default)]
    filters: SearchFilters,
    #[serde(default)]
    expected: Vec<Expected>,
    answerable: Option<bool>,
}

#[derive(Clone)]
struct PinnedExpected {
    document_id: Uuid,
    document_hash: Option<String>,
    quote: Option<String>,
}

#[derive(Default)]
struct Measures {
    completed: usize,
    failed: usize,
    elapsed_ms: Vec<f64>,
    recalls: Vec<f64>,
    expected_documents: usize,
    recovered_documents: usize,
    expected_quotes: usize,
    recovered_quotes: usize,
    returned_chunks: usize,
    verified_ranges: usize,
    verified_provenance: usize,
}

type ProvenanceCache = HashMap<Uuid, Option<Value>>;

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn parse_questions(bytes: &[u8]) -> Result<Vec<Question>> {
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_DATASET_BYTES,
        "benchmark dataset size is out of bounds"
    );
    let value: Value = serde_json::from_slice(bytes).context("invalid benchmark JSON")?;
    let questions: Vec<Question> =
        serde_json::from_value(value.get("questions").unwrap_or(&value).clone())
            .context("benchmark requires a questions array")?;
    ensure!(
        !questions.is_empty() && questions.len() <= 1000,
        "benchmark requires 1..=1000 questions"
    );
    let mut identifiers = HashSet::new();
    let mut unique_queries = HashSet::new();
    for (index, question) in questions.iter().enumerate() {
        SearchRequest {
            query: question.query.clone(),
            filters: question.filters.clone(),
            limit: 10,
            mode: SearchMode::Lexical,
        }
        .validate()?;
        let id = question
            .id
            .clone()
            .unwrap_or_else(|| format!("q{:04}", index + 1));
        ensure!(
            !id.trim().is_empty() && id.len() <= 128 && identifiers.insert(id),
            "question identifiers must be unique and bounded"
        );
        ensure!(
            unique_queries.insert((
                question.query.trim().to_string(),
                serde_json::to_string(&question.filters)?
            )),
            "duplicate query and filters in evaluation dataset"
        );
        ensure!(
            question.expected.len() <= 100,
            "too many expected documents for one question"
        );
        ensure!(
            question.answerable != Some(false) || question.expected.is_empty(),
            "unanswerable questions cannot declare expected documents"
        );
        ensure!(
            question.answerable != Some(true) || !question.expected.is_empty(),
            "answerable questions require expected documents"
        );
        let mut documents = HashSet::new();
        for expected in &question.expected {
            ensure!(
                documents.insert(expected.document_id),
                "expected document identifiers must be unique per question"
            );
            ensure!(
                expected
                    .document_hash
                    .as_ref()
                    .is_none_or(|h| valid_hash(h)),
                "invalid expected document hash"
            );
            ensure!(
                expected
                    .quote
                    .as_ref()
                    .is_none_or(|q| !q.trim().is_empty() && q.len() <= 32768 && !q.contains('\0')),
                "expected quotation is empty or excessive"
            );
        }
    }
    Ok(questions)
}

fn percentile(samples: &[f64], fraction: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let index = ((sorted.len() as f64 * fraction).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    Some(sorted[index])
}

fn ratio(numerator: usize, denominator: usize) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

fn recall(expected: &[PinnedExpected], hits: &[SearchHit]) -> (usize, usize, Option<f64>) {
    let recovered = expected
        .iter()
        .filter(|expected| {
            hits.iter().take(10).any(|hit| {
                hit.document_id == expected.document_id
                    && expected.document_hash.as_deref() == Some(hit.document_hash.as_str())
            })
        })
        .count();
    (recovered, expected.len(), ratio(recovered, expected.len()))
}

async fn provenance(client: &Client, cache: &mut ProvenanceCache, id: Uuid) -> Option<Value> {
    if let Some(value) = cache.get(&id) {
        return value.clone();
    }
    let value = client
        .get(&format!("/v1/provenance/{id}"))
        .await
        .ok()
        .filter(|value| {
            value["document_id"].as_str() == Some(id.to_string().as_str())
                && value["versions"].is_array()
        });
    cache.insert(id, value.clone());
    value
}

fn source_version(value: &Value, expected_hash: Option<&str>) -> Option<Value> {
    value["versions"]
        .as_array()?
        .iter()
        .find(|version| match expected_hash {
            Some(hash) => version["document_hash"].as_str() == Some(hash),
            None => version["current"] == true,
        })
        .filter(|version| {
            version["document_hash"].as_str().is_some_and(valid_hash)
                && version["source_hash"].as_str().is_some_and(valid_hash)
                && version["metadata"]["source_id"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
        })
        .cloned()
}

async fn verify_hit_provenance(
    client: &Client,
    cache: &mut ProvenanceCache,
    hit: &SearchHit,
    filters: &SearchFilters,
) -> bool {
    let Some(value) = provenance(client, cache, hit.document_id).await else {
        return false;
    };
    let Some(version) = source_version(&value, Some(&hit.document_hash)) else {
        return false;
    };
    let metadata = &version["metadata"];
    metadata["source_id"].as_str() == Some(hit.source_id.as_str())
        && metadata["title"].as_str() == Some(hit.title.as_str())
        && metadata["source_url"].as_str() == hit.source_url.as_deref()
        && metadata_matches_filters(metadata, filters)
}

fn metadata_matches_filters(metadata: &Value, filters: &SearchFilters) -> bool {
    for (field, expected) in [
        ("source_id", &filters.source_id),
        ("language", &filters.language),
    ] {
        if expected
            .as_ref()
            .is_some_and(|value| metadata[field].as_str() != Some(value.as_str()))
        {
            return false;
        }
    }
    for (field, expected) in [("speakers", &filters.speaker), ("themes", &filters.theme)] {
        if let Some(value) = expected
            && !metadata[field].as_array().is_some_and(|values| {
                values
                    .iter()
                    .any(|entry| entry.as_str() == Some(value.as_str()))
            })
        {
            return false;
        }
    }
    for (bound, after) in [
        (&filters.published_after, true),
        (&filters.published_before, false),
    ] {
        if let Some(bound) = bound {
            let published = metadata["published_at"].as_str().and_then(|value| {
                time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                    .ok()
            });
            let boundary =
                time::OffsetDateTime::parse(bound, &time::format_description::well_known::Rfc3339)
                    .ok();
            if !published
                .zip(boundary)
                .is_some_and(|(published, boundary)| {
                    if after {
                        published >= boundary
                    } else {
                        published <= boundary
                    }
                })
            {
                return false;
            }
        }
    }
    true
}

async fn verify_full_markdown(client: &Client, id: Uuid, hash: &str) -> bool {
    let mut cursor = 0;
    let mut digest = Sha256::new();
    loop {
        let response = client
            .post(
                "/v1/read",
                &ReadRequest {
                    document_id: id,
                    cursor,
                    limit: 32000,
                    expected_hash: Some(hash.into()),
                },
            )
            .await;
        let Some(page) = response
            .ok()
            .and_then(|value| serde_json::from_value::<ReadPage>(value).ok())
        else {
            return false;
        };
        if page.document_id != id
            || page.document_hash != hash
            || page.total_bytes > MAX_SOURCE_BYTES
            || page.markdown.is_empty()
            || cursor + page.markdown.len() > page.total_bytes
        {
            return false;
        }
        digest.update(page.markdown.as_bytes());
        cursor += page.markdown.len();
        if page.eof {
            return page.next_cursor.is_none()
                && cursor == page.total_bytes
                && hex::encode(digest.finalize()) == hash;
        }
        if page.next_cursor != Some(cursor) {
            return false;
        }
    }
}

fn available(status: &Value, mode: SearchMode) -> Option<bool> {
    match mode {
        SearchMode::Lexical => status["capabilities"]["lexical"].as_bool(),
        SearchMode::Semantic => status["capabilities"]["embeddings"].as_bool(),
        SearchMode::Hybrid => status["capabilities"]["hybrid"].as_bool(),
        SearchMode::Deep => status["capabilities"]["embeddings"]
            .as_bool()
            .zip(status["capabilities"]["reranker"].as_bool())
            .map(|(a, b)| a && b),
    }
}

fn summary(
    mode: SearchMode,
    measures: &Measures,
    question_count: usize,
    mode_available: Option<bool>,
    warmup: Value,
    trials: Vec<Value>,
) -> Value {
    json!({
        "mode":mode,"mode_available":mode_available,"state":if mode_available==Some(false){"unavailable"}else if measures.completed==question_count{"complete"}else if measures.completed==0{"failed"}else{"partial"},
        "queries_total":question_count,"queries_completed":measures.completed,"queries_failed":measures.failed,
        "warmup":warmup,"latency_condition":"sequential_after_one_warmup_not_controlled_cold_start",
        "search_latency_ms":{"p50":percentile(&measures.elapsed_ms,0.50),"p95":percentile(&measures.elapsed_ms,0.95),"samples":measures.elapsed_ms.len(),"includes_provenance_reads":false},
        "document_recall_at_10":{"macro":if measures.recalls.is_empty(){None}else{Some(measures.recalls.iter().sum::<f64>()/measures.recalls.len()as f64)},"micro":ratio(measures.recovered_documents,measures.expected_documents),"recovered":measures.recovered_documents,"expected":measures.expected_documents},
        "expected_quote_recovery_at_10":ratio(measures.recovered_quotes,measures.expected_quotes),
        "returned_chunks":measures.returned_chunks,"exact_range_verified_chunks":measures.verified_ranges,"provenance_verified_chunks":measures.verified_provenance,
        "semantic_support_reviewed":false,"abstention_reviewed":false,"fallback_attempted":false,"trials":trials
    })
}

fn atomic_report(output: &Path, value: &Value) -> Result<()> {
    ensure!(
        output.is_absolute()
            && !output
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
        "report path must be absolute without traversal"
    );
    for path in output.ancestors() {
        match fs::symlink_metadata(path) {
            Ok(meta) => ensure!(
                !meta.file_type().is_symlink(),
                "report path contains a symbolic link"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot inspect private report path"),
        }
    }
    let parent = output.parent().context("report parent missing")?;
    ensure!(
        parent.is_dir(),
        "private report parent must be provisioned before evaluation"
    );
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).context("cannot create private report")?;
    serde_json::to_writer_pretty(&mut temporary, value)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(output)
        .context("cannot atomically publish private report")?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Exercise the real service endpoints. Model unavailability, failed requests
/// and incomplete verification remain visible; none are silently downgraded.
pub async fn benchmark(config: &Config, questions: &Path, output: &Path) -> Result<Value> {
    ensure!(
        fs::metadata(questions)
            .context("cannot inspect private question set")?
            .len()
            <= MAX_DATASET_BYTES,
        "question set exceeds size limit"
    );
    let bytes = fs::read(questions).context("cannot read private question set")?;
    let dataset = parse_questions(&bytes)?;
    let client = Client::new(&config.socket)?;
    let status = client
        .get("/v1/status")
        .await
        .context("knowledge service is unavailable")?;
    let started = Instant::now();
    let mut cache = ProvenanceCache::new();
    let mut gold_documents = HashSet::new();
    let mut gold_sources = HashSet::new();
    let mut missing_gold = 0;
    let mut incomplete_gold = HashSet::new();
    let mut markdown_checks = HashMap::new();
    let mut pinned = Vec::new();
    for question in &dataset {
        let mut expected_pinned = Vec::new();
        for expected in &question.expected {
            gold_documents.insert(expected.document_id);
            let version = match provenance(&client, &mut cache, expected.document_id).await {
                Some(value) => source_version(&value, expected.document_hash.as_deref()),
                None => None,
            };
            let hash = version
                .as_ref()
                .and_then(|v| v["document_hash"].as_str())
                .map(str::to_owned);
            if let Some(version) = &version {
                gold_sources.insert(
                    version["metadata"]["source_id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                );
                if version["complete"] != true {
                    incomplete_gold.insert((expected.document_id, hash.clone()));
                }
            } else {
                missing_gold += 1;
            }
            if let Some(hash) = &hash {
                let key = (expected.document_id, hash.clone());
                if let std::collections::hash_map::Entry::Vacant(entry) = markdown_checks.entry(key)
                {
                    entry.insert(verify_full_markdown(&client, expected.document_id, hash).await);
                }
            }
            expected_pinned.push(PinnedExpected {
                document_id: expected.document_id,
                document_hash: hash,
                quote: expected.quote.clone(),
            });
        }
        pinned.push(expected_pinned);
    }
    let mut reports = Vec::new();
    for mode in [
        SearchMode::Lexical,
        SearchMode::Semantic,
        SearchMode::Hybrid,
        SearchMode::Deep,
    ] {
        let can_run = available(&status, mode);
        let mut measures = Measures::default();
        let mut trials = Vec::new();
        let warmup = if can_run != Some(false) {
            let warm_started = Instant::now();
            let result = client
                .post(
                    "/v1/search",
                    &SearchRequest {
                        query: dataset[0].query.clone(),
                        filters: dataset[0].filters.clone(),
                        limit: 10,
                        mode,
                    },
                )
                .await;
            json!({"attempted":true,"succeeded":result.is_ok(),"elapsed_ms":warm_started.elapsed().as_secs_f64()*1000.0,"controlled_cold_start":false})
        } else {
            json!({"attempted":false,"succeeded":false,"reason":"capability_unavailable"})
        };
        for (index, question) in dataset.iter().enumerate() {
            let expected = &pinned[index];
            measures.expected_documents += expected.len();
            measures.expected_quotes += expected.iter().filter(|e| e.quote.is_some()).count();
            let id = question
                .id
                .clone()
                .unwrap_or_else(|| format!("q{:04}", index + 1));
            if can_run == Some(false) {
                if !expected.is_empty() {
                    measures.recalls.push(0.0);
                }
                trials.push(json!({"id":id,"state":"unavailable","query_sha256":kf_core::sha256(question.query.as_bytes()),"reason":"capability_unavailable"}));
                continue;
            }
            let timer = Instant::now();
            let response = client
                .post(
                    "/v1/search",
                    &SearchRequest {
                        query: question.query.clone(),
                        filters: question.filters.clone(),
                        limit: 10,
                        mode,
                    },
                )
                .await;
            let elapsed = timer.elapsed().as_secs_f64() * 1000.0;
            measures.elapsed_ms.push(elapsed);
            let parsed = response
                .ok()
                .and_then(|value| serde_json::from_value::<SearchResponse>(value).ok());
            let Some(response) = parsed.filter(|r| r.mode_executed == mode && r.hits.len() <= 10)
            else {
                measures.failed += 1;
                if !expected.is_empty() {
                    measures.recalls.push(0.0);
                }
                trials.push(json!({"id":id,"state":"failed","reason":"search_request_or_mode_validation_failed","search_elapsed_ms":elapsed}));
                continue;
            };
            measures.completed += 1;
            measures.returned_chunks += response.hits.len();
            let evidence: Vec<_> = response
                .hits
                .iter()
                .map(|hit| Evidence {
                    document_id: hit.document_id,
                    document_hash: hit.document_hash.clone(),
                    start_byte: hit.start_byte,
                    end_byte: hit.end_byte,
                    quote: hit.text.clone(),
                })
                .collect();
            let verification_started = Instant::now();
            let exact = if evidence.is_empty() {
                true
            } else {
                client
                    .post("/v1/evidence-check", &json!({"evidence":evidence}))
                    .await
                    .ok()
                    .is_some_and(|v| {
                        v["valid"] == true
                            && v["count"].as_u64() == Some(response.hits.len() as u64)
                    })
            };
            let mut verified_hits = Vec::new();
            for hit in &response.hits {
                let source_ok =
                    verify_hit_provenance(&client, &mut cache, hit, &question.filters).await;
                if source_ok {
                    measures.verified_provenance += 1;
                }
                if exact {
                    measures.verified_ranges += 1;
                }
                if exact && source_ok {
                    verified_hits.push(hit.clone());
                }
            }
            let (recovered, total, question_recall) = recall(expected, &verified_hits);
            measures.recovered_documents += recovered;
            if let Some(value) = question_recall {
                measures.recalls.push(value);
            }
            let recovered_quotes = expected
                .iter()
                .filter(|e| {
                    e.quote.as_ref().is_some_and(|quote| {
                        verified_hits.iter().any(|hit| {
                            hit.document_id == e.document_id
                                && e.document_hash.as_deref() == Some(hit.document_hash.as_str())
                                && hit.text.contains(quote)
                        })
                    })
                })
                .count();
            measures.recovered_quotes += recovered_quotes;
            trials.push(json!({"id":id,"query_sha256":kf_core::sha256(question.query.as_bytes()),"state":"complete","index_generation":response.index_generation,"search_elapsed_ms":elapsed,
                "verification_elapsed_ms":verification_started.elapsed().as_secs_f64()*1000.0,"returned_chunks":response.hits.len(),"exact_ranges_verified":exact,"verified_document_matches":recovered,"expected_documents":total,"document_recall_at_10":question_recall,"expected_quotes_recovered":recovered_quotes,
                "negative_question_hit_count":if question.answerable==Some(false){Some(response.hits.len())}else{None},"abstention_assessed":false}));
        }
        reports.push(summary(
            mode,
            &measures,
            dataset.len(),
            can_run,
            warmup,
            trials,
        ));
    }
    let dataset_large_enough =
        dataset.len() >= 100 && gold_documents.len() >= 20 && gold_sources.len() >= 10;
    let all_markdown_verified =
        !markdown_checks.is_empty() && markdown_checks.values().all(|valid| *valid);
    let all_modes_complete = reports.iter().all(|mode| mode["state"] == "complete");
    let returned_total: u64 = reports
        .iter()
        .filter_map(|mode| mode["returned_chunks"].as_u64())
        .sum();
    let range_integrity = returned_total > 0
        && reports.iter().all(|mode| {
            mode["returned_chunks"] == mode["exact_range_verified_chunks"]
                && mode["returned_chunks"] == mode["provenance_verified_chunks"]
        });
    let index_generation_consistent =
        reports
            .iter()
            .filter(|mode| mode["mode"] != "lexical")
            .all(|mode| {
                mode["trials"].as_array().is_some_and(|trials| {
                    trials
                        .iter()
                        .filter(|trial| trial["state"] == "complete")
                        .all(|trial| {
                            trial["index_generation"].is_string()
                                && trial["index_generation"] == status["model_generation"]
                        })
                })
            });
    let recall_target = reports.iter().all(|mode| {
        mode["document_recall_at_10"]["macro"]
            .as_f64()
            .is_some_and(|v| v >= 0.90)
    });
    let latency_target = reports.iter().all(|mode| {
        mode["search_latency_ms"]["p95"].as_f64().is_some_and(|v| {
            v <= if mode["mode"] == "deep" {
                15000.0
            } else {
                3000.0
            }
        })
    });
    let result = json!({
        "schema_version":1,"dataset_sha256":kf_core::sha256(&bytes),"created_at":time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?,
        "measured":true,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0,"question_count":dataset.len(),"gold_document_count":gold_documents.len(),"gold_source_count":gold_sources.len(),"missing_gold_references":missing_gold,"incomplete_gold_versions":incomplete_gold.len(),
        "full_markdown_versions_checked":markdown_checks.len(),"full_markdown_sha256_verified":all_markdown_verified,"canonical_offsets_verified_via_service":range_integrity,"index_generation_consistent":index_generation_consistent,
        "qualification":dataset_large_enough&&missing_gold==0&&incomplete_gold.is_empty()&&all_markdown_verified&&all_modes_complete&&range_integrity&&index_generation_consistent&&recall_target&&latency_target,
        "qualification_scope":"retrieval and byte-level source integrity only","requirements":{"questions_min":100,"documents_min":20,"sources_min":10,"document_recall_at_10_macro_min":0.90,"search_p95_ms_max":3000,"deep_p95_ms_max":15000},
        "checks":{"dataset_size":dataset_large_enough,"all_modes_complete":all_modes_complete,"exact_source_ranges":range_integrity,"recall_target":recall_target,"latency_target":latency_target},
        "truth_certification":false,"semantic_support_reviewed":false,"abstention_reviewed":false,"raw_archive_bytes_rechecked":false,"hnsw_vs_exact_recall_measured":false,"cold_start_benchmark_performed":false,
        "model_generation_at_start":status["model_generation"],"modes":reports
    });
    atomic_report(output, &result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles_and_empty_metrics_are_explicit() {
        assert_eq!(percentile(&[], 0.95), None);
        assert_eq!(percentile(&[10.0], 0.95), Some(10.0));
        let values: Vec<_> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&values, 0.95), Some(95.0));
        assert_eq!(ratio(0, 0), None);
        assert_eq!(ratio(1, 2), Some(0.5));
    }

    #[test]
    fn empty_and_contradictory_datasets_never_qualify() {
        assert!(parse_questions(br#"{"questions":[]}"#).is_err());
        assert!(
            parse_questions(br#"[{"query":"Question","answerable":true,"expected":[]}]"#).is_err()
        );
        assert!(parse_questions(br#"[{"query":"Question","answerable":false,"expected":[{"document_id":"00000000-0000-0000-0000-000000000001"}]}]"#).is_err());
        assert!(parse_questions(br#"[{"query":"Question"},{"query":"Question"}]"#).is_err());
        let small = parse_questions(br#"[{"query":"Synthetic unknown topic","answerable":false}]"#)
            .unwrap();
        assert_eq!(small.len(), 1);
        let report = summary(
            SearchMode::Deep,
            &Measures::default(),
            1,
            Some(false),
            Value::Null,
            vec![],
        );
        assert_eq!(report["state"], "unavailable");
        assert!(report["search_latency_ms"]["p95"].is_null());
        assert!(report["document_recall_at_10"]["macro"].is_null());
    }

    #[test]
    fn document_recall_deduplicates_chunks_and_requires_pinned_version() {
        let id = Uuid::new_v4();
        let expected = vec![PinnedExpected {
            document_id: id,
            document_hash: Some("a".repeat(64)),
            quote: None,
        }];
        let hit = SearchHit {
            document_id: id,
            document_hash: "a".repeat(64),
            chunk_index: 0,
            title: "Synthetic".into(),
            source_id: "example".into(),
            source_url: None,
            text: "Synthetic source".into(),
            start_byte: 0,
            end_byte: 16,
            start_ms: None,
            end_ms: None,
            score: 0.0,
        };
        assert_eq!(
            recall(&expected, &[hit.clone(), hit.clone()]),
            (1, 1, Some(1.0))
        );
        let mut stale = hit;
        stale.document_hash = "b".repeat(64);
        assert_eq!(recall(&expected, &[stale]), (0, 1, Some(0.0)));
        assert_eq!(recall(&[], &[]), (0, 0, None));
    }

    #[test]
    fn unavailable_summary_does_not_fabricate_latency_or_success() {
        let report = summary(
            SearchMode::Semantic,
            &Measures::default(),
            100,
            Some(false),
            Value::Null,
            vec![],
        );
        assert_eq!(report["state"], "unavailable");
        assert_eq!(report["queries_completed"], 0);
        assert_eq!(report["fallback_attempted"], false);
        assert_eq!(report["semantic_support_reviewed"], false);
    }

    #[test]
    fn report_is_written_as_private_atomic_json() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synthetic-report.json");
        atomic_report(&path, &json!({"qualification":false,"synthetic":true})).unwrap();
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["qualification"], false);
        atomic_report(
            &path,
            &json!({"qualification":false,"synthetic":true,"revision":2}),
        )
        .unwrap();
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        assert!(atomic_report(Path::new("relative.json"), &Value::Null).is_err());
    }

    #[test]
    fn source_filters_do_not_confuse_publisher_and_speaker() {
        let metadata = json!({"source_id":"publisher-alpha","title":"Synthetic talk","speakers":["Ada Example"],"themes":["agents"],"language":"en","published_at":"2026-10-27T00:00:00Z"});
        let filters = SearchFilters {
            source_id: Some("publisher-alpha".into()),
            speaker: Some("Ada Example".into()),
            theme: Some("agents".into()),
            language: Some("en".into()),
            published_after: Some("2026-10-26T00:00:00Z".into()),
            published_before: Some("2026-10-28T00:00:00Z".into()),
        };
        assert!(metadata_matches_filters(&metadata, &filters));
        let mut wrong = filters.clone();
        wrong.speaker = Some("publisher-alpha".into());
        assert!(!metadata_matches_filters(&metadata, &wrong));
        let mut wrong = filters;
        wrong.published_after = Some("2026-10-28T00:00:00Z".into());
        assert!(!metadata_matches_filters(&metadata, &wrong));
    }
}
