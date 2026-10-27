#![no_main]
use kf_core::{CaptionKind, Metadata, sha256};
use kf_ingest::{TokenCounter, chunk, prepare};
use libfuzzer_sys::fuzz_target;

struct Characters;
impl TokenCounter for Characters {
    fn count(&self, text: &str) -> kf_core::Result<usize> {
        Ok(text.chars().count())
    }
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() || data.len() > 65536 {
        return;
    }
    let Ok(text) = std::str::from_utf8(&data[1..]) else {
        return;
    };
    let plain_line = text.replace(['\r', '\n'], " ");
    let (format, raw) = match data[0] % 9 {
        0 => ("json", text.to_owned()),
        1 => ("srt", text.to_owned()),
        2 => ("vtt", text.to_owned()),
        3 => ("md", text.to_owned()),
        4 => ("txt", text.to_owned()),
        5 => (
            "json",
            serde_json::json!([{"text":text,"start":0.0,"duration":1.25}]).to_string(),
        ),
        6 => (
            "srt",
            format!("1\n00:00:00,000 --> 00:00:01,250\n{plain_line}\n\n"),
        ),
        7 => (
            "vtt",
            format!("WEBVTT\n\n00:00:00.000 --> 00:00:01.250\n{plain_line}\n\n"),
        ),
        _ => (
            "md",
            format!("## Synthetic heading\n\n**0:04** · {plain_line}\n"),
        ),
    };
    let metadata = Metadata {
        external_id: "synthetic-fuzz".into(),
        source_id: "synthetic-publisher".into(),
        title: "Synthetic fuzz document".into(),
        publisher: "Synthetic Publisher".into(),
        source_url: None,
        published_at: None,
        language: None,
        caption_kind: CaptionKind::Unknown,
        themes: vec![],
        speakers: vec![],
    };
    if let Ok(document) = prepare(raw.as_bytes(), format, metadata, false) {
        assert_eq!(document.source_hash, sha256(raw.as_bytes()));
        assert_eq!(document.document_hash, sha256(document.markdown.as_bytes()));
        assert!(!document.complete);
        let chunks = chunk(&document, &Characters, 350, 60).unwrap();
        let mut covered = 0;
        for part in &chunks {
            assert!(part.start_byte <= covered);
            assert!(part.end_byte > covered);
            assert_eq!(part.text, document.text[part.start_byte..part.end_byte]);
            assert!(Characters.count(&part.text).unwrap() <= 350);
            covered = part.end_byte;
        }
        assert_eq!(covered, document.text.len());
    }
});
