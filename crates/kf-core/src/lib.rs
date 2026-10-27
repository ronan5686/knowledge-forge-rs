use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("document version changed; restart reading")]
    VersionChanged,
    #[error("capability unavailable: {0}")]
    Unavailable(String),
    #[error("not found")]
    NotFound,
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptionKind {
    Human,
    Automatic,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Metadata {
    pub external_id: String,
    pub source_id: String,
    pub title: String,
    pub publisher: String,
    pub source_url: Option<String>,
    pub published_at: Option<String>,
    pub language: Option<String>,
    #[serde(default)]
    pub caption_kind: CaptionKind,
    #[serde(default)]
    pub themes: Vec<String>,
    #[serde(default)]
    pub speakers: Vec<String>,
}

impl Metadata {
    pub fn document_id(&self) -> Uuid {
        Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("{}\0{}", self.source_id, self.external_id).as_bytes(),
        )
    }

    pub fn validate(&self) -> Result<()> {
        for (name, s) in [
            ("external_id", &self.external_id),
            ("source_id", &self.source_id),
        ] {
            if s.is_empty()
                || s.len() > 128
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
            {
                return Err(Error::Invalid(format!("invalid {name}")));
            }
        }
        if self.title.trim().is_empty() || self.title.len() > 4096 || self.publisher.len() > 1024 {
            return Err(Error::Invalid("invalid title or publisher".into()));
        }
        if let Some(url) = &self.source_url {
            let u =
                url::Url::parse(url).map_err(|_| Error::Invalid("invalid source URL".into()))?;
            if !matches!(u.scheme(), "http" | "https")
                || !u.username().is_empty()
                || u.password().is_some()
            {
                return Err(Error::Invalid("unsupported source URL".into()));
            }
        }
        if let Some(date) = &self.published_at {
            time::OffsetDateTime::parse(date, &time::format_description::well_known::Rfc3339)
                .map_err(|_| Error::Invalid("invalid publication timestamp".into()))?;
        }
        if let Some(lang) = &self.language
            && (lang.is_empty()
                || lang.len() > 35
                || !lang.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        {
            return Err(Error::Invalid("invalid language".into()));
        }
        if self.themes.len() > 32
            || self.speakers.len() > 32
            || self
                .themes
                .iter()
                .chain(self.speakers.iter())
                .any(|s| s.trim().is_empty() || s.len() > 256)
        {
            return Err(Error::Invalid("invalid theme or speaker list".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub start_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub heading: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedDocument {
    pub metadata: Metadata,
    pub segments: Vec<Segment>,
    pub text: String,
    pub markdown: String,
    pub source_hash: String,
    pub document_hash: String,
    pub complete: bool,
    pub capture_format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub index: i32,
    pub text: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SearchFilters {
    pub source_id: Option<String>,
    pub speaker: Option<String>,
    pub theme: Option<String>,
    pub published_after: Option<String>,
    pub published_before: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Default, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Lexical,
    Semantic,
    #[default]
    Hybrid,
    Deep,
}

fn default_limit() -> usize {
    20
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub filters: SearchFilters,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub mode: SearchMode,
}

impl SearchRequest {
    pub fn validate(&self) -> Result<()> {
        if self.query.trim().is_empty()
            || self.query.len() > 8192
            || !(1..=50).contains(&self.limit)
        {
            return Err(Error::Invalid("query or limit out of bounds".into()));
        }
        for date in [
            &self.filters.published_after,
            &self.filters.published_before,
        ]
        .into_iter()
        .flatten()
        {
            time::OffsetDateTime::parse(date, &time::format_description::well_known::Rfc3339)
                .map_err(|_| Error::Invalid("invalid date filter".into()))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchHit {
    pub document_id: Uuid,
    pub document_hash: String,
    pub chunk_index: i32,
    pub title: String,
    pub source_id: String,
    pub source_url: Option<String>,
    pub text: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchResponse {
    pub mode_executed: SearchMode,
    pub index_generation: Option<String>,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReadRequest {
    pub document_id: Uuid,
    #[serde(default)]
    pub cursor: usize,
    #[serde(default = "default_read_limit")]
    pub limit: usize,
    pub expected_hash: Option<String>,
}

fn default_read_limit() -> usize {
    8000
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReadPage {
    pub document_id: Uuid,
    pub metadata: Metadata,
    pub document_hash: String,
    pub markdown: String,
    pub next_cursor: Option<usize>,
    pub eof: bool,
    pub total_bytes: usize,
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Evidence {
    pub document_id: Uuid,
    pub document_hash: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub quote: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Claim {
    pub text: String,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WikiProposal {
    pub kind: String,
    pub title: String,
    pub claims: Vec<Claim>,
}

pub fn read_page(doc: &PreparedDocument, request: &ReadRequest) -> Result<ReadPage> {
    if !(1..=32000).contains(&request.limit)
        || request.cursor > doc.markdown.len()
        || !doc.markdown.is_char_boundary(request.cursor)
    {
        return Err(Error::Invalid("invalid read cursor or limit".into()));
    }
    if request.document_id != doc.metadata.document_id() {
        return Err(Error::NotFound);
    }
    if request
        .expected_hash
        .as_deref()
        .is_some_and(|s| s != doc.document_hash)
    {
        return Err(Error::VersionChanged);
    }
    if request.cursor > 0 && request.expected_hash.is_none() {
        return Err(Error::Invalid(
            "expected_hash required after first page".into(),
        ));
    }
    let mut end = request
        .cursor
        .saturating_add(request.limit)
        .min(doc.markdown.len());
    while end > request.cursor && !doc.markdown.is_char_boundary(end) {
        end -= 1;
    }
    if end == request.cursor && end < doc.markdown.len() {
        return Err(Error::Invalid(
            "limit smaller than next UTF-8 character".into(),
        ));
    }
    let eof = end == doc.markdown.len();
    Ok(ReadPage {
        document_id: request.document_id,
        metadata: doc.metadata.clone(),
        document_hash: doc.document_hash.clone(),
        markdown: doc.markdown[request.cursor..end].to_string(),
        next_cursor: (!eof).then_some(end),
        eof,
        total_bytes: doc.markdown.len(),
        complete: doc.complete,
    })
}

pub fn rrf(left: &[SearchHit], right: &[SearchHit], limit: usize) -> Vec<SearchHit> {
    let mut map = std::collections::HashMap::new();
    for list in [left, right] {
        for (rank, hit) in list.iter().enumerate() {
            let key = (hit.document_id, hit.document_hash.clone(), hit.chunk_index);
            let entry = map.entry(key).or_insert_with(|| {
                let mut h = hit.clone();
                h.score = 0.0;
                h
            });
            entry.score += 1.0 / (61.0 + rank as f64);
        }
    }
    let mut hits: Vec<_> = map.into_values().collect();
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.document_id.cmp(&b.document_id))
            .then(a.chunk_index.cmp(&b.chunk_index))
    });
    hits.truncate(limit);
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hashes_are_stable() {
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
    #[test]
    fn reject_invalid_search() {
        assert!(
            SearchRequest {
                query: " ".into(),
                filters: Default::default(),
                limit: 20,
                mode: Default::default()
            }
            .validate()
            .is_err()
        );
    }
}
