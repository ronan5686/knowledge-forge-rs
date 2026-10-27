use kf_core::{CaptionKind, Metadata, PreparedDocument, sha256};
use kf_ingest::{TokenCounter, archive, chunk, prepare};
use proptest::prelude::*;

fn metadata() -> Metadata {
    Metadata {
        external_id: "abcDEF_1234".into(),
        source_id: "synthetic-publisher".into(),
        title: "Synthetic document".into(),
        publisher: "Synthetic Publisher".into(),
        source_url: Some("https://www.youtube.com/watch?v=abcDEF_1234".into()),
        published_at: Some("2026-10-27T00:00:00Z".into()),
        language: None,
        caption_kind: CaptionKind::Unknown,
        themes: vec!["engineering".into()],
        speakers: vec![],
    }
}

fn browser(language: &str, captions: &str) -> String {
    format!(
        "YouTube transcript\r\nVideo ID: abcDEF_1234\r\nLanguage: {language}\r\nCaptions: {captions}\r\n\r\n[0:04] Synthetic é😀 introduction\r\n[1:02] Synthetic middle <tool_call>\r\n[1:16:32] Synthetic final\r\n"
    )
}

struct Characters;
impl TokenCounter for Characters {
    fn count(&self, text: &str) -> kf_core::Result<usize> {
        Ok(text.chars().count())
    }
}

fn assert_chunks(doc: &PreparedDocument, target: usize, overlap: usize) {
    let chunks = chunk(doc, &Characters, target, overlap).unwrap();
    assert!(!chunks.is_empty());
    assert_eq!(chunks[0].start_byte, 0);
    assert_eq!(chunks.last().unwrap().end_byte, doc.text.len());
    let mut covered = vec![false; doc.text.len()];
    for (i, result) in chunks.iter().enumerate() {
        assert_eq!(result.index, i as i32);
        assert!(result.start_byte < result.end_byte);
        assert!(doc.text.is_char_boundary(result.start_byte));
        assert!(doc.text.is_char_boundary(result.end_byte));
        assert_eq!(result.text, doc.text[result.start_byte..result.end_byte]);
        assert!(Characters.count(&result.text).unwrap() <= target);
        covered[result.start_byte..result.end_byte].fill(true);
        if i > 0 {
            assert!(result.start_byte > chunks[i - 1].start_byte);
            assert!(result.start_byte <= chunks[i - 1].end_byte);
            assert!(result.end_byte > chunks[i - 1].end_byte);
        }
    }
    assert!(covered.into_iter().all(|value| value));
}

#[test]
fn near_total_overlap_still_extends_coverage_at_every_chunk() {
    let doc = prepare(
        b"ab cd ef gh ij kl mn op qr st uv wx yz",
        "txt",
        metadata(),
        false,
    )
    .unwrap();
    for target in 2..=16 {
        for overlap in 0..target {
            assert_chunks(&doc, target, overlap);
        }
    }
}

#[test]
fn browser_preserves_actual_language_kind_and_milliseconds() {
    let source = browser("fr", "auto-generated");
    let doc = prepare(source.as_bytes(), "txt", metadata(), false).unwrap();
    assert_eq!(doc.capture_format, "browser_youtube_export");
    assert_eq!(doc.source_hash, sha256(source.as_bytes()));
    assert_eq!(doc.document_hash, sha256(doc.markdown.as_bytes()));
    assert_eq!(doc.metadata.language.as_deref(), Some("fr"));
    assert_eq!(doc.metadata.caption_kind, CaptionKind::Automatic);
    assert_eq!(doc.segments[0].start_ms, Some(4000));
    assert_eq!(doc.segments[0].duration_ms, Some(58000));
    assert_eq!(doc.segments[2].start_ms, Some(4_592_000));
    assert_eq!(doc.segments[2].duration_ms, None);
    assert!(!doc.complete);
    assert!(doc.text.contains("<tool_call>"));
}

#[test]
fn uncertain_authorship_is_not_mislabeled_human() {
    let doc = prepare(
        browser("en", "authored or unspecified").as_bytes(),
        "txt",
        metadata(),
        true,
    )
    .unwrap();
    assert_eq!(doc.metadata.caption_kind, CaptionKind::Unknown);
    assert!(doc.complete);
    assert_eq!(
        prepare(b"Synthetic ordinary prose.", "txt", metadata(), false)
            .unwrap()
            .metadata
            .language,
        None
    );
}

#[test]
fn completeness_promotion_keeps_source_bytes_and_invalidates_old_read_version() {
    let raw = b"Synthetic source with exact quotation.";
    let partial = prepare(raw, "txt", metadata(), false).unwrap();
    let reviewed = prepare(raw, "txt", metadata(), true).unwrap();
    assert_eq!(partial.source_hash, reviewed.source_hash);
    assert_eq!(partial.text, reviewed.text);
    assert_eq!(
        partial.metadata.document_id(),
        reviewed.metadata.document_id()
    );
    assert_ne!(partial.document_hash, reviewed.document_hash);
    let request = kf_core::ReadRequest {
        document_id: reviewed.metadata.document_id(),
        cursor: 0,
        limit: 1000,
        expected_hash: Some(partial.document_hash),
    };
    assert!(matches!(
        kf_core::read_page(&reviewed, &request),
        Err(kf_core::Error::VersionChanged)
    ));
}

#[test]
fn browser_rejects_mismatch_unclocked_text_missing_and_bad_fields() {
    let valid = browser("en", "auto-generated");
    for source in [
        valid.replace("abcDEF_1234", "xyzDEF_5678"),
        valid.replace("Language: en\r\n", ""),
        valid.replace("[0:04]", "[0:99]"),
        valid.replace("[1:02]", "[0:01]"),
        valid.replace("auto-generated", "invented"),
        valid.clone() + "Unclocked last line\n",
        valid.replace("Synthetic final", ""),
    ] {
        assert!(prepare(source.as_bytes(), "txt", metadata(), false).is_err());
    }
    let mut meta = metadata();
    meta.language = Some("fr".into());
    assert!(prepare(valid.as_bytes(), "txt", meta, false).is_err());
}

#[test]
fn json_segments_array_and_object_validate_actual_metadata() {
    let source = br#"{"video_id":"abcDEF_1234","language":"fr","is_generated":true,"segments":[{"text":"Synthetic first","start":0.125,"duration":1.25},{"text":"Synthetic last","start":2,"duration":0}]}"#;
    let doc = prepare(source, "json", metadata(), false).unwrap();
    assert_eq!(doc.metadata.language.as_deref(), Some("fr"));
    assert_eq!(doc.metadata.caption_kind, CaptionKind::Automatic);
    assert_eq!(doc.segments[0].start_ms, Some(125));
    assert_eq!(doc.segments[0].duration_ms, Some(1250));
    let array = br#"[{"text":"Synthetic","start":0,"duration":1}]"#;
    assert_eq!(
        prepare(array, "json", metadata(), false)
            .unwrap()
            .metadata
            .language,
        None
    );
    for bad in [
        br#"[]"#.as_slice(),
        br#"[{"text":"","start":0,"duration":1}]"#,
        br#"[{"text":"x","start":-1,"duration":1}]"#,
        br#"[{"text":"x","start":true,"duration":1}]"#,
        br#"[{"text":"x","start":1e300,"duration":1}]"#,
        br#"{"video_id":"other","segments":[{"text":"x","start":0,"duration":1}]}"#,
    ] {
        assert!(prepare(bad, "json", metadata(), false).is_err());
    }
}

#[test]
fn native_markdown_preserves_chapters_without_spoken_attribution() {
    let source = "---\nvideo_id: abcDEF_1234\npage_language: fr\n---\n# Capture\n\n### Chapter one\n\n**0:04** · Synthetic 😀 words\ncontinuation\n### Chapter two\n**1:00:03** · Synthetic end\n";
    let doc = prepare(source.as_bytes(), "md", metadata(), false).unwrap();
    assert_eq!(doc.metadata.language, None);
    assert_eq!(doc.metadata.caption_kind, CaptionKind::Unknown);
    assert_eq!(doc.capture_format, "obsidian_web_clipper_native_transcript");
    assert!(
        doc.segments
            .iter()
            .filter(|s| s.heading)
            .all(|s| s.start_ms.is_none() && s.duration_ms.is_none())
    );
    assert_eq!(doc.segments[2].text, "Synthetic 😀 words\ncontinuation");
    assert_eq!(doc.segments[2].duration_ms, Some(3_599_000));
    assert_chunks(&doc, 35, 6);
}

#[test]
fn native_markdown_metadata_is_attested_and_unresolved_capture_rejected() {
    let source = "---\nlanguage: fr\ncaption_type: human\n---\n**0:00** · Synthetic words.\n";
    let doc = prepare(source.as_bytes(), "md", metadata(), false).unwrap();
    assert_eq!(doc.metadata.language.as_deref(), Some("fr"));
    assert_eq!(doc.metadata.caption_kind, CaptionKind::Human);
    for bad in [
        "{{transcript}}",
        "---\nvideo_id: other\n---\n**0:00** · Words.",
        "---\nurl: https://www.youtube.com/watch?v=other\n---\n**0:00** · Words.",
        "---\nurl: https://evil.example/watch?v=abcDEF_1234\n---\n**0:00** · Words.",
        "**0:00** ·Words.",
        "**0:99** · Words.",
        "**0:00** · ",
        "**0:00** · Words.\n### Trailing chapter",
    ] {
        assert!(prepare(bad.as_bytes(), "md", metadata(), false).is_err());
    }
}

#[test]
fn plain_markdown_and_text_preserve_complete_unicode_body() {
    let source = "# Synthetic title\n\nTexte é😀 Rust\nline two\n";
    for format in ["txt", "md"] {
        let doc = prepare(source.as_bytes(), format, metadata(), false).unwrap();
        assert_eq!(doc.text, source);
        assert_eq!(doc.metadata.language, None);
        assert_chunks(&doc, 8, 2);
    }
}

#[test]
fn clipper_cannot_import_a_title_or_description_as_a_transcript() {
    for body in [
        "",
        "\n",
        "# Capture native YouTube à vérifier\n\n",
        "# Video\nDescription only.",
    ] {
        let source =
            format!("---\ncapture_source: obsidian_web_clipper_native_transcript\n---\n{body}");
        for complete in [false, true] {
            assert!(prepare(source.as_bytes(), "md", metadata(), complete).is_err());
        }
    }
    assert!(
        prepare(
            "# Capture native YouTube à vérifier\n".as_bytes(),
            "md",
            metadata(),
            false
        )
        .is_err()
    );
}

#[test]
fn clipper_page_language_is_not_caption_language_even_for_legacy_templates() {
    for properties in [
        "language: fr\n",
        "page_language: fr\ncaption_language: unknown\n",
        "page_language: fr\ncaption_language: und\n",
    ] {
        let source = format!(
            "---\n{properties}capture_source: obsidian_web_clipper_native_transcript\n---\n# Capture\n**0:00** · Synthetic English.\n"
        );
        let doc = prepare(source.as_bytes(), "md", metadata(), false).unwrap();
        assert_eq!(doc.metadata.language, None);
        assert_eq!(doc.metadata.caption_kind, CaptionKind::Unknown);
        assert_eq!(doc.segments[1].text, "Synthetic English.");
    }
    let source = "---\nlanguage: fr\ncapture_source: obsidian_web_clipper_native_transcript\ncaption_language: en\n---\n**0:00** · Synthetic English.\n";
    let doc = prepare(source.as_bytes(), "md", metadata(), false).unwrap();
    assert_eq!(doc.metadata.language.as_deref(), Some("en"));
    let mut conflicting = metadata();
    conflicting.language = Some("fr".into());
    assert!(prepare(source.as_bytes(), "md", conflicting, false).is_err());
}

#[test]
fn bundled_clipper_template_keeps_unknown_caption_language_and_rejects_empty_capture() {
    let template: serde_json::Value = serde_json::from_str(include_str!(
        "../../../templates/obsidian-web-clipper-youtube.json"
    ))
    .unwrap();
    let properties = template["properties"].as_array().unwrap();
    assert!(!properties.iter().any(|p| p["name"] == "language"));
    assert!(
        properties
            .iter()
            .any(|p| p["name"] == "page_language" && p["value"] == "{{language}}")
    );
    assert!(
        properties
            .iter()
            .any(|p| p["name"] == "caption_language" && p["value"] == "unknown")
    );
    let body = template["noteContentFormat"]
        .as_str()
        .unwrap()
        .replace("{{transcript}}", "");
    assert!(prepare(body.as_bytes(), "md", metadata(), false).is_err());
}

#[test]
fn srt_and_vtt_cues_keep_precise_timing_and_text() {
    let srt = "1\r\n00:00:01,125 --> 00:00:03,500\r\nSynthetic first\r\nsecond line\r\n \r\n2\r\n00:00:04,000 --> 00:00:06,000\r\nSynthetic end\r\n";
    let doc = prepare(srt.as_bytes(), "srt", metadata(), false).unwrap();
    assert_eq!(doc.segments.len(), 2);
    assert_eq!(doc.segments[0].start_ms, Some(1125));
    assert_eq!(doc.segments[0].duration_ms, Some(2375));
    assert_eq!(doc.segments[0].text, "Synthetic first\nsecond line");
    let vtt = "WEBVTT\nLanguage: en\n\nNOTE synthetic note\nnot spoken\n\ncue-a\n00:01.125 --> 00:03.500 align:start\nSynthetic cue\n\nSTYLE\n::cue { color: red; }\n\n00:04.000 --> 00:06.000\nSynthetic end\n";
    let doc = prepare(vtt.as_bytes(), "vtt", metadata(), false).unwrap();
    assert_eq!(doc.segments.len(), 2);
    assert!(!doc.text.contains("not spoken"));
    assert_eq!(doc.metadata.language, None);
    assert_chunks(&doc, 10, 3);
}

#[test]
fn subtitles_never_silently_ignore_unparsed_cue_blocks() {
    for source in [
        "1\n00:00:02,000 --> 00:00:01,000\nSynthetic",
        "1\n00:00:01,000 --> 00:00:02,000\n",
        "Unparsed noncue text",
        "1\n00:00:01,000 --> 00:00:02,000\nA\n2\n00:00:03,000 --> 00:00:04,000\nB",
    ] {
        assert!(prepare(source.as_bytes(), "srt", metadata(), false).is_err());
    }
    assert!(prepare(b"00:00.000 --> 00:01.000\nWords", "vtt", metadata(), false).is_err());
}

#[test]
fn chunk_last_time_remains_unknown_and_counter_errors_propagate() {
    let doc = prepare(
        browser("en", "auto-generated").as_bytes(),
        "txt",
        metadata(),
        false,
    )
    .unwrap();
    let chunks = chunk(&doc, &Characters, 350, 60).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].start_ms, Some(4000));
    assert_eq!(chunks[0].end_ms, None);
    struct Failing;
    impl TokenCounter for Failing {
        fn count(&self, _: &str) -> kf_core::Result<usize> {
            Err(kf_core::Error::Unavailable(
                "synthetic tokenizer failure".into(),
            ))
        }
    }
    assert!(chunk(&doc, &Failing, 350, 60).is_err());
    for (target, overlap) in [(0, 0), (10, 10), (8193, 0)] {
        assert!(chunk(&doc, &Characters, target, overlap).is_err());
    }
}

#[test]
fn archive_is_atomic_immutable_and_verifies_both_hashes() {
    let temp = tempfile::tempdir().unwrap();
    let raw = b"\xef\xbb\xbfSynthetic source \xc3\xa9\r\n";
    let doc = prepare(raw, "txt", metadata(), false).unwrap();
    archive(temp.path(), raw, &doc).unwrap();
    archive(temp.path(), raw, &doc).unwrap();
    let raw_path = temp
        .path()
        .join("raw")
        .join(format!("{}.bin", doc.source_hash));
    assert_eq!(std::fs::read(&raw_path).unwrap(), raw);
    let path = temp
        .path()
        .join("documents")
        .join(doc.metadata.document_id().to_string())
        .join(format!("{}.json", doc.document_hash));
    let saved: PreparedDocument = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(saved.text, doc.text);
    assert_eq!(saved.markdown, doc.markdown);
    assert!(archive(temp.path(), b"changed", &doc).is_err());
    let mut tampered = doc.clone();
    tampered.markdown.push('x');
    assert!(archive(temp.path(), raw, &tampered).is_err());
    std::fs::write(&raw_path, b"tampered").unwrap();
    assert!(archive(temp.path(), raw, &doc).is_err());
    assert!(archive(&temp.path().join("child").join(".."), raw, &doc).is_err());
    assert!(archive(std::path::Path::new("relative"), raw, &doc).is_err());
}

#[cfg(unix)]
#[test]
fn archive_rejects_symlink_root_component_and_destination() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let raw = b"Synthetic source";
    let doc = prepare(raw, "txt", metadata(), false).unwrap();
    symlink(elsewhere.path(), temp.path().join("linked")).unwrap();
    assert!(archive(&temp.path().join("linked"), raw, &doc).is_err());
    symlink(elsewhere.path(), temp.path().join("raw")).unwrap();
    assert!(archive(temp.path(), raw, &doc).is_err());
}

#[test]
fn input_and_metadata_limits_are_explicit() {
    for raw in [b"".as_slice(), b" \n", b"bad\0text", &[0xff]] {
        assert!(prepare(raw, "txt", metadata(), false).is_err());
    }
    assert!(prepare(b"Synthetic", "html", metadata(), false).is_err());
    let mut meta = metadata();
    meta.external_id = "../bad".into();
    assert!(prepare(b"Synthetic", "txt", meta, false).is_err());
}

#[test]
fn twenty_question_synthetic_ground_truth_has_real_source_ranges() {
    // This validates the future evaluation fixture, not a retrieval score. No
    // search model or database is invoked and no quality percentage is emitted.
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/retrieval-golden-synthetic.json")).unwrap();
    let documents = fixture["documents"].as_array().unwrap();
    let questions = fixture["questions"].as_array().unwrap();
    assert_eq!(questions.len(), 20);
    let mut prepared = std::collections::HashMap::new();
    for document in documents {
        let mut meta = metadata();
        meta.external_id = document["id"].as_str().unwrap().into();
        meta.source_id = document["source_id"].as_str().unwrap().into();
        meta.publisher = document["publisher"].as_str().unwrap().into();
        meta.source_url = None;
        meta.language = Some(document["language"].as_str().unwrap().into());
        meta.themes = vec![document["theme"].as_str().unwrap().into()];
        meta.speakers = vec![document["speaker"].as_str().unwrap().into()];
        let doc = prepare(
            document["text"].as_str().unwrap().as_bytes(),
            "txt",
            meta,
            true,
        )
        .unwrap();
        assert_chunks(&doc, 350, 60);
        assert!(
            prepared
                .insert(doc.metadata.external_id.clone(), doc)
                .is_none()
        );
    }
    let mut identifiers = std::collections::HashSet::new();
    let mut unanswerable = 0;
    for question in questions {
        assert!(identifiers.insert(question["id"].as_str().unwrap()));
        assert!(!question["query"].as_str().unwrap().trim().is_empty());
        let evidence = question["evidence"].as_array().unwrap();
        if question["unanswerable"] == true {
            unanswerable += 1;
            assert!(evidence.is_empty());
        } else {
            assert!(!evidence.is_empty());
        }
        for citation in evidence {
            let doc = &prepared[citation["id"].as_str().unwrap()];
            let quote = citation["quote"].as_str().unwrap();
            let start = doc
                .text
                .find(quote)
                .expect("fixture quote must exist verbatim");
            assert_eq!(&doc.text[start..start + quote.len()], quote);
            for (field, expected) in question["filters"].as_object().into_iter().flatten() {
                let expected = expected.as_str().unwrap();
                assert!(match field.as_str() {
                    "source_id" => doc.metadata.source_id == expected,
                    "language" => doc.metadata.language.as_deref() == Some(expected),
                    "speaker" => doc.metadata.speakers.iter().any(|s| s == expected),
                    "theme" => doc.metadata.themes.iter().any(|s| s == expected),
                    _ => false,
                });
            }
        }
    }
    assert_eq!(unanswerable, 2);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]
    #[test]
    fn unicode_chunks_cover_every_byte_without_exceeding_budget(chars in prop::collection::vec(any::<char>().prop_filter("no NUL", |c| *c != '\0'), 1..500), target in 1usize..80, ratio in 0usize..100) {
        let source: String = chars.into_iter().collect();
        prop_assume!(!source.trim().is_empty());
        let doc = prepare(source.as_bytes(), "txt", metadata(), false).unwrap();
        let overlap = ratio * target / 100;
        assert_chunks(&doc, target, overlap);
    }
    #[test]
    fn arbitrary_utf8_never_panics_in_any_parser(source in ".{0,300}", format in prop::sample::select(vec!["txt", "md", "json", "srt", "vtt"])) {
        let _ = prepare(source.as_bytes(), format, metadata(), false);
    }
}
