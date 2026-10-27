# Internal implementation contract v0.1

Shared Rust types live in kf-core/src/lib.rs. Changes require coordination.

## Ingest API (kf-ingest)

- `pub fn prepare(raw: &[u8], format: &str, metadata: Metadata, complete: bool) -> kf_core::Result<PreparedDocument>` accepts json,txt,md,srt,vtt; archives are handled by operator app.
- `pub trait TokenCounter { fn count(&self, text: &str) -> kf_core::Result<usize>; }`
- `pub fn chunk(doc: &PreparedDocument, counter: &dyn TokenCounter, target: usize, overlap: usize) -> kf_core::Result<Vec<Chunk>>` returns offsets in PreparedDocument.text, timestamps from source segments. No silent token truncation. Entire text coverage required.
- `pub fn archive(root: &Path, raw: &[u8], doc: &PreparedDocument) -> kf_core::Result<()>` immutable atomic bytes and markdown. Reject symlink components and path escapes.

## Storage API (kf-store)

Storage returns anyhow::Result for infrastructure errors and preserves kf_core::Error in the error chain.

- `Store::connect(url: &str, max_connections: u32) -> Result<Store>` async; Store Clone.
- `Store::migrate(&self) -> Result<()>` async, separate operator invocation.
- `Store::put_document(&self, doc: &PreparedDocument, chunks: &[Chunk]) -> Result<serde_json::Value>` async; preserve complete current version against partial input.
- `Store::get_document(&self, id: Uuid) -> Result<PreparedDocument>` async; current version.
- `Store::status(&self) -> Result<serde_json::Value>` async.
- `Store::sources(&self) -> Result<serde_json::Value>` async.
- `Store::provenance(&self, id: Uuid) -> Result<serde_json::Value>` async.
- `Store::lexical(&self, request: &SearchRequest) -> Result<Vec<SearchHit>>` async.
- `Store::semantic(&self, request: &SearchRequest, vector: &[f32], generation: &str) -> Result<Vec<SearchHit>>` async; exact search for small sets, generation isolation.
- `Store::pending_embeddings(&self, generation: &str, limit: i64) -> Result<Vec<(Uuid,String,i32,String)>>` async.
- `Store::put_embedding(&self, id: Uuid, hash: &str, chunk_index: i32, generation: &str, vector: &[f32]) -> Result<()>` async; dimensions/finiteness enforced.
- `Store::submit_proposal(&self, proposal: &WikiProposal) -> Result<serde_json::Value>` async; verify every source version/byte range and exact quote. No truth certification.
- `Store::enqueue(&self, kind: &str, key: &str, payload: &Value) -> Result<Value>` async.
- `Store::claim_job(&self, worker: &str) -> Result<Option<Value>>` async, lease/retries.
- `Store::finish_job(&self, id: Uuid, worker: &str, success: bool, detail: &Value) -> Result<()>` async.

## Runtime

App owns CLI, persistent inference, Unix socket HTTP, thin rmcp stdio adapter, Google connector, migration and backup operator commands. Store never downloads models or contacts Google. Ingest never calls a model or PostgreSQL. No real data or secrets in this public workspace.
