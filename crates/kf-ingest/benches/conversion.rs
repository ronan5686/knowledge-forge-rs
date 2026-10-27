//! Synthetic deterministic parsing and full UTF-8 pagination only.
//! These measurements do not exercise embedding, retrieval or reranking.
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use kf_core::{CaptionKind, Metadata, ReadRequest, read_page};
use kf_ingest::prepare;
use std::hint::black_box;
use std::time::Duration;

fn metadata() -> Metadata {
    Metadata {
        external_id: "synthetic-benchmark".into(),
        source_id: "synthetic-publisher".into(),
        title: "Synthetic UTF-8 benchmark".into(),
        publisher: "Synthetic Publisher".into(),
        source_url: None,
        published_at: None,
        language: None,
        caption_kind: CaptionKind::Unknown,
        themes: vec![],
        speakers: vec![],
    }
}

fn benchmark(c: &mut Criterion) {
    let sentence = "Énergie, agents et systèmes fiables. Engineering evidence 😀 日本語.";
    let segments: Vec<_> = (0..1000)
        .map(|i| serde_json::json!({"text":sentence,"start":i as f64,"duration":1.0}))
        .collect();
    let json = serde_json::to_vec(&segments).unwrap();
    let markdown = (0..1000)
        .map(|i| format!("## Synthetic section {i}\n\n{sentence}\n\n"))
        .collect::<String>();
    let srt = (0..1000)
        .map(|i| {
            let timestamp = |second: usize| {
                format!(
                    "{:02}:{:02}:{:02},000",
                    second / 3600,
                    second / 60 % 60,
                    second % 60
                )
            };
            format!(
                "{}\n{} --> {}\n{}\n\n",
                i + 1,
                timestamp(i),
                timestamp(i + 1),
                sentence
            )
        })
        .collect::<String>();
    let mut group = c.benchmark_group("synthetic_parse_and_render");
    for (format, bytes) in [
        ("json", json.as_slice()),
        ("md", markdown.as_bytes()),
        ("srt", srt.as_bytes()),
    ] {
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(format), bytes, |b, raw| {
            b.iter(|| prepare(black_box(raw), format, metadata(), false).unwrap());
        });
    }
    group.finish();

    let document = prepare(markdown.as_bytes(), "md", metadata(), false).unwrap();
    let mut group = c.benchmark_group("synthetic_full_utf8_pagination");
    group.throughput(Throughput::Bytes(document.markdown.len() as u64));
    for limit in [4096, 32000] {
        group.bench_with_input(BenchmarkId::from_parameter(limit), &limit, |b, &limit| {
            b.iter(|| {
                let mut cursor = 0;
                let mut bytes = 0;
                loop {
                    let page = read_page(
                        black_box(&document),
                        &ReadRequest {
                            document_id: document.metadata.document_id(),
                            cursor,
                            limit,
                            expected_hash: Some(document.document_hash.clone()),
                        },
                    )
                    .unwrap();
                    bytes += page.markdown.len();
                    match page.next_cursor {
                        Some(next) => cursor = next,
                        None => {
                            assert!(page.eof);
                            break;
                        }
                    }
                }
                assert_eq!(bytes, document.markdown.len());
                black_box(bytes)
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10).warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3));
    targets = benchmark
}
criterion_main!(benches);
