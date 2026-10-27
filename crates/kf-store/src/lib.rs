//! Versioned knowledge storage. Sources are evidence, never instructions or truth certificates.
use anyhow::{Context, Result, bail};
use kf_core::{
    Chunk, Error, Evidence, PreparedDocument, SearchFilters, SearchHit, SearchRequest, WikiProposal,
};
use pgvector::Vector;
use serde_json::{Value, json};
use sqlx::{
    PgPool, Postgres, QueryBuilder, Row,
    postgres::{PgPoolOptions, PgRow},
};
use uuid::Uuid;

const DIMENSIONS: usize = 1024;
const EXACT_LIMIT: i64 = 2048;
const HIT_COLUMNS: &str = "c.document_id,c.hash,c.chunk_index,c.text,c.start_byte,c.end_byte,c.start_ms,c.end_ms,v.payload->'metadata' AS metadata";

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

fn invalid(message: &str) -> anyhow::Error {
    Error::Invalid(message.into()).into()
}

fn validate_generation(generation: &str) -> Result<()> {
    if generation.is_empty()
        || generation.len() > 128
        || !generation
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
    {
        return Err(invalid("invalid index generation"));
    }
    Ok(())
}

fn verify_evidence_payload(evidence: &Evidence, payload: &Value) -> Result<()> {
    let metadata: kf_core::Metadata = serde_json::from_value(payload["metadata"].clone())
        .context("invalid stored source metadata")?;
    let text = payload["text"]
        .as_str()
        .context("invalid stored source text")?;
    if metadata.document_id() != evidence.document_id
        || payload["document_hash"].as_str() != Some(evidence.document_hash.as_str())
        || evidence.document_hash.len() != 64
        || !evidence
            .document_hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || evidence.quote.is_empty()
        || evidence.quote.len() > 32_768
        || evidence.quote.contains('\0')
        || evidence.end_byte <= evidence.start_byte
        || evidence.end_byte > text.len()
        || !text.is_char_boundary(evidence.start_byte)
        || !text.is_char_boundary(evidence.end_byte)
        || text[evidence.start_byte..evidence.end_byte] != evidence.quote
    {
        return Err(invalid(
            "evidence does not match the cited immutable document version and UTF-8 byte range",
        ));
    }
    Ok(())
}

fn checked_vector(vector: &[f32]) -> Result<Vector> {
    if vector.len() != DIMENSIONS
        || vector.iter().any(|x| !x.is_finite())
        || vector.iter().all(|x| *x == 0.0)
    {
        return Err(invalid(
            "embedding requires 1024 finite values and a nonzero norm",
        ));
    }
    Ok(Vector::from(vector.to_vec()))
}

fn wiki_page(
    id: Uuid,
    digest: &str,
    payload: &Value,
    cursor: usize,
    limit: usize,
    expected_hash: Option<&str>,
) -> Result<Value> {
    let text = serde_json::to_string(payload)?;
    if kf_core::sha256(text.as_bytes()) != digest {
        return Err(invalid("stored proposal digest does not match its payload"));
    }
    if expected_hash.is_some_and(|hash| hash != digest) {
        return Err(Error::VersionChanged.into());
    }
    if !(1..=8000).contains(&limit)
        || cursor > text.len()
        || !text.is_char_boundary(cursor)
        || (cursor > 0 && expected_hash.is_none())
    {
        return Err(invalid("invalid proposal cursor, limit or expected hash"));
    }
    let mut end = cursor.saturating_add(limit).min(text.len());
    while end > cursor && !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == cursor && end < text.len() {
        return Err(invalid("limit smaller than next UTF-8 character"));
    }
    let eof = end == text.len();
    let result = json!({"proposal_id":id,"proposal_hash":digest,"format":"json","text":&text[cursor..end],"next_cursor":(!eof).then_some(end),"eof":eof,"total_bytes":text.len(),"generated":true,"independent_evidence":false,"truth_certification":false,"status":"proposed"});
    if serde_json::to_vec(&result)?.len() > 32768 {
        return Err(invalid("proposal response exceeds bound"));
    }
    Ok(result)
}

fn validate_document(doc: &PreparedDocument, chunks: &[Chunk]) -> Result<()> {
    doc.metadata.validate()?;
    let hash_valid = |s: &str| {
        s.len() == 64
            && s.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    };
    if !hash_valid(&doc.document_hash)
        || !hash_valid(&doc.source_hash)
        || kf_core::sha256(doc.markdown.as_bytes()) != doc.document_hash
        || doc.text.is_empty()
        || doc.text.len() > 100 * 1024 * 1024
        || chunks.is_empty()
        || chunks.len() > 100_000
    {
        return Err(invalid("invalid document hashes, size or chunks"));
    }
    let mut covered = 0;
    for (index, c) in chunks.iter().enumerate() {
        if c.index != index as i32
            || c.start_byte > covered
            || c.end_byte <= covered
            || c.end_byte <= c.start_byte
            || c.end_byte > doc.text.len()
            || !doc.text.is_char_boundary(c.start_byte)
            || !doc.text.is_char_boundary(c.end_byte)
            || doc.text[c.start_byte..c.end_byte] != c.text
            || c.start_ms.zip(c.end_ms).is_some_and(|(a, b)| b < a)
            || c.start_ms
                .into_iter()
                .chain(c.end_ms)
                .any(|v| v > i64::MAX as u64)
        {
            return Err(invalid(
                "invalid chunk coverage, text, UTF-8 offsets or timestamps",
            ));
        }
        covered = covered.max(c.end_byte);
    }
    if covered != doc.text.len() {
        return Err(invalid("chunks must cover the entire text"));
    }
    Ok(())
}

fn filters<'a>(q: &mut QueryBuilder<'a, Postgres>, f: &'a SearchFilters) {
    if let Some(x) = &f.source_id {
        q.push(" AND v.payload #>> '{metadata,source_id}' = ")
            .push_bind(x);
    }
    if let Some(x) = &f.language {
        q.push(" AND v.payload #>> '{metadata,language}' = ")
            .push_bind(x);
    }
    for (key, value) in [("speakers", &f.speaker), ("themes", &f.theme)] {
        if let Some(x) = value {
            q.push(" AND EXISTS(SELECT 1 FROM jsonb_array_elements_text(v.payload->'metadata'->'")
                .push(key)
                .push("') AS label(value) WHERE label.value=")
                .push_bind(x)
                .push(")");
        }
    }
    if let Some(x) = &f.published_after {
        q.push(" AND (v.payload #>> '{metadata,published_at}')::timestamptz >= ")
            .push_bind(x)
            .push("::timestamptz");
    }
    if let Some(x) = &f.published_before {
        q.push(" AND (v.payload #>> '{metadata,published_at}')::timestamptz <= ")
            .push_bind(x)
            .push("::timestamptz");
    }
}

fn hit(row: PgRow) -> Result<SearchHit> {
    let meta: Value = row.try_get("metadata")?;
    let offset = |name: &str| -> Result<usize> {
        usize::try_from(row.try_get::<i64, _>(name)?).context("invalid stored offset")
    };
    let stamp = |name: &str| -> Result<Option<u64>> {
        row.try_get::<Option<i64>, _>(name)?
            .map(u64::try_from)
            .transpose()
            .context("invalid stored timestamp")
    };
    Ok(SearchHit {
        document_id: row.try_get("document_id")?,
        document_hash: row.try_get("hash")?,
        chunk_index: row.try_get("chunk_index")?,
        title: meta["title"].as_str().unwrap_or_default().into(),
        source_id: meta["source_id"].as_str().unwrap_or_default().into(),
        source_url: meta["source_url"].as_str().map(str::to_owned),
        text: row.try_get("text")?,
        start_byte: offset("start_byte")?,
        end_byte: offset("end_byte")?,
        start_ms: stamp("start_ms")?,
        end_ms: stamp("end_ms")?,
        score: row.try_get("score")?,
    })
}

impl Store {
    /// Operator integration only; agent-facing tools never expose arbitrary SQL.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn resolve_external(&self, external_id: &str) -> Result<Uuid> {
        if external_id.is_empty() || external_id.len() > 128 {
            return Err(invalid("invalid external document identity"));
        }
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT d.id FROM kf_documents d JOIN kf_versions v ON v.document_id=d.id AND v.hash=d.current_hash WHERE v.payload #>> '{metadata,external_id}'=$1 LIMIT 2")
            .bind(external_id).fetch_all(&self.pool).await?;
        match ids.as_slice() {
            [id] => Ok(*id),
            [] => Err(Error::NotFound.into()),
            _ => Err(invalid(
                "external identity is ambiguous; use the document UUID",
            )),
        }
    }

    pub async fn connect(url: &str, max_connections: u32) -> Result<Self> {
        if !(1..=64).contains(&max_connections) {
            return Err(invalid("pool size out of bounds"));
        }
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await
            .context("knowledge database connection failed")?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<()> {
        let mut connection = self.pool.acquire().await?;
        let pg: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::integer")
            .fetch_one(&mut *connection)
            .await?;
        if pg < 160000 {
            bail!("PostgreSQL 16 or later is required");
        }
        let role = std::env::var("KF_MIGRATION_ROLE").ok();
        if role.as_deref().is_some_and(|r| r != "kf_owner") {
            return Err(invalid("only kf_owner is an allowed migration role"));
        }
        if role.is_some() {
            sqlx::query("SET ROLE kf_owner")
                .execute(&mut *connection)
                .await?;
        }
        let migrated = sqlx::migrate!("./migrations").run(&mut *connection).await;
        if role.is_some()
            && sqlx::query("RESET ROLE")
                .execute(&mut *connection)
                .await
                .is_err()
        {
            connection.close().await?;
            bail!("failed to reset migration role; database connection closed");
        }
        migrated.context("knowledge migration failed")
    }

    pub async fn put_document(&self, doc: &PreparedDocument, chunks: &[Chunk]) -> Result<Value> {
        validate_document(doc, chunks)?;
        let id = doc.metadata.document_id();
        let payload = serde_json::to_value(doc)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO kf_documents(id) VALUES($1) ON CONFLICT DO NOTHING")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let current: Option<String> =
            sqlx::query_scalar("SELECT current_hash FROM kf_documents WHERE id=$1 FOR UPDATE")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        let existing: Option<Value> =
            sqlx::query_scalar("SELECT payload FROM kf_versions WHERE document_id=$1 AND hash=$2")
                .bind(id)
                .bind(&doc.document_hash)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(existing) = existing {
            if existing != payload {
                return Err(invalid(
                    "immutable version hash collides with different document content",
                ));
            }
            let saved=sqlx::query("SELECT chunk_index,text,start_byte,end_byte,start_ms,end_ms FROM kf_chunks WHERE document_id=$1 AND hash=$2 ORDER BY chunk_index").bind(id).bind(&doc.document_hash).fetch_all(&mut *tx).await?;
            if saved.len() != chunks.len()
                || saved.iter().zip(chunks).any(|(r, c)| {
                    r.get::<i32, _>("chunk_index") != c.index
                        || r.get::<String, _>("text") != c.text
                        || r.get::<i64, _>("start_byte") != c.start_byte as i64
                        || r.get::<i64, _>("end_byte") != c.end_byte as i64
                        || r.get::<Option<i64>, _>("start_ms") != c.start_ms.map(|v| v as i64)
                        || r.get::<Option<i64>, _>("end_ms") != c.end_ms.map(|v| v as i64)
                })
            {
                return Err(invalid(
                    "immutable version was already indexed with different chunks",
                ));
            }
        } else {
            sqlx::query(
                "INSERT INTO kf_versions(document_id,hash,payload,complete) VALUES($1,$2,$3,$4)",
            )
            .bind(id)
            .bind(&doc.document_hash)
            .bind(&payload)
            .bind(doc.complete)
            .execute(&mut *tx)
            .await?;
            for c in chunks {
                sqlx::query("INSERT INTO kf_chunks(document_id,hash,chunk_index,text,start_byte,end_byte,start_ms,end_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8)").bind(id).bind(&doc.document_hash).bind(c.index).bind(&c.text).bind(c.start_byte as i64).bind(c.end_byte as i64).bind(c.start_ms.map(|v|v as i64)).bind(c.end_ms.map(|v|v as i64)).execute(&mut *tx).await?;
            }
        }
        let current_complete = if let Some(hash) = &current {
            sqlx::query_scalar::<_, bool>(
                "SELECT complete FROM kf_versions WHERE document_id=$1 AND hash=$2",
            )
            .bind(id)
            .bind(hash)
            .fetch_one(&mut *tx)
            .await?
        } else {
            false
        };
        let activated = doc.complete || !current_complete;
        if activated {
            sqlx::query("UPDATE kf_documents SET current_hash=$2 WHERE id=$1")
                .bind(id)
                .bind(&doc.document_hash)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(
            json!({"document_id":id,"document_hash":doc.document_hash,"current":activated,"complete":doc.complete,"preserved_complete_current":!activated}),
        )
    }

    pub async fn get_document(&self, id: Uuid) -> Result<PreparedDocument> {
        let payload:Option<Value>=sqlx::query_scalar("SELECT v.payload FROM kf_documents d JOIN kf_versions v ON v.document_id=d.id AND v.hash=d.current_hash WHERE d.id=$1").bind(id).fetch_optional(&self.pool).await?;
        Ok(serde_json::from_value(payload.ok_or(Error::NotFound)?)?)
    }

    pub async fn lexical(&self, request: &SearchRequest) -> Result<Vec<SearchHit>> {
        request.validate()?;
        let mut q =
            QueryBuilder::<Postgres>::new("WITH queries AS(SELECT plainto_tsquery('simple',");
        q.push_bind(&request.query).push(") AS s,plainto_tsquery('english',").push_bind(&request.query).push(") AS e,plainto_tsquery('french',").push_bind(&request.query).push(") AS f) SELECT ").push(HIT_COLUMNS).push(",GREATEST(ts_rank_cd(c.search_simple,queries.s),ts_rank_cd(c.search_english,queries.e),ts_rank_cd(c.search_french,queries.f))::float8 AS score FROM kf_chunks c JOIN kf_documents d ON d.id=c.document_id AND d.current_hash=c.hash JOIN kf_versions v ON v.document_id=c.document_id AND v.hash=c.hash CROSS JOIN queries WHERE (c.search_simple@@queries.s OR c.search_english@@queries.e OR c.search_french@@queries.f)");
        filters(&mut q, &request.filters);
        q.push(" ORDER BY score DESC,c.document_id,c.chunk_index LIMIT ")
            .push_bind(request.limit as i64);
        q.build()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(hit)
            .collect()
    }

    pub async fn semantic(
        &self,
        request: &SearchRequest,
        vector: &[f32],
        generation: &str,
    ) -> Result<Vec<SearchHit>> {
        request.validate()?;
        validate_generation(generation)?;
        let vector = checked_vector(vector)?;
        let mut tx = self.pool.begin().await?;
        let mut count = QueryBuilder::<Postgres>::new(
            "SELECT count(*) FROM (SELECT 1 FROM kf_embeddings e JOIN kf_documents d ON d.id=e.document_id AND d.current_hash=e.hash JOIN kf_versions v ON v.document_id=e.document_id AND v.hash=e.hash WHERE e.generation=",
        );
        count.push_bind(generation);
        filters(&mut count, &request.filters);
        count.push(" LIMIT 2049) AS bounded_candidates");
        let n: i64 = count.build_query_scalar().fetch_one(&mut *tx).await?;
        if n == 0 {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        let exact = n <= EXACT_LIMIT;
        if !exact {
            sqlx::query("SET LOCAL hnsw.iterative_scan='strict_order'")
                .execute(&mut *tx)
                .await?;
            sqlx::query("SET LOCAL hnsw.ef_search=100")
                .execute(&mut *tx)
                .await?;
        }
        let mut q = QueryBuilder::<Postgres>::new(if exact {
            "WITH candidates AS MATERIALIZED(SELECT "
        } else {
            "SELECT "
        });
        q.push(HIT_COLUMNS).push(",e.embedding ");
        if !exact {
            q.push(", (1-(e.embedding <=> ")
                .push_bind(&vector)
                .push("::vector))::float8 AS score ");
        }
        q.push("FROM kf_embeddings e JOIN kf_chunks c ON c.document_id=e.document_id AND c.hash=e.hash AND c.chunk_index=e.chunk_index JOIN kf_documents d ON d.id=c.document_id AND d.current_hash=c.hash JOIN kf_versions v ON v.document_id=c.document_id AND v.hash=c.hash WHERE e.generation=").push_bind(generation);
        filters(&mut q, &request.filters);
        if exact {
            q.push(") SELECT *, (1-(embedding <=> ")
                .push_bind(&vector)
                .push("::vector))::float8 AS score FROM candidates ORDER BY embedding <=> ")
                .push_bind(&vector)
                .push("::vector,document_id,chunk_index LIMIT ")
                .push_bind(request.limit as i64);
        } else {
            q.push(" ORDER BY e.embedding <=> ")
                .push_bind(&vector)
                .push("::vector LIMIT ")
                .push_bind(request.limit as i64);
        }
        let rows = q.build().fetch_all(&mut *tx).await?;
        tx.commit().await?;
        rows.into_iter().map(hit).collect()
    }

    pub async fn pending_embeddings(
        &self,
        generation: &str,
        limit: i64,
    ) -> Result<Vec<(Uuid, String, i32, String)>> {
        validate_generation(generation)?;
        if !(1..=1000).contains(&limit) {
            return Err(invalid("embedding batch out of bounds"));
        }
        Ok(sqlx::query_as("SELECT c.document_id,c.hash,c.chunk_index,c.text FROM kf_chunks c JOIN kf_documents d ON d.id=c.document_id AND d.current_hash=c.hash LEFT JOIN kf_embeddings e ON e.document_id=c.document_id AND e.hash=c.hash AND e.chunk_index=c.chunk_index AND e.generation=$1 WHERE e.document_id IS NULL ORDER BY c.document_id,c.chunk_index LIMIT $2").bind(generation).bind(limit).fetch_all(&self.pool).await?)
    }

    pub async fn put_embedding(
        &self,
        id: Uuid,
        hash: &str,
        chunk_index: i32,
        generation: &str,
        vector: &[f32],
    ) -> Result<()> {
        validate_generation(generation)?;
        let vector = checked_vector(vector)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO kf_embedding_generations(name,dimensions) VALUES($1,1024) ON CONFLICT DO NOTHING").bind(generation).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO kf_embeddings(document_id,hash,chunk_index,generation,embedding) VALUES($1,$2,$3,$4,$5::vector) ON CONFLICT DO NOTHING").bind(id).bind(hash).bind(chunk_index).bind(generation).bind(&vector).execute(&mut *tx).await?;
        let same:bool=sqlx::query_scalar("SELECT embedding=$5::vector FROM kf_embeddings WHERE document_id=$1 AND hash=$2 AND chunk_index=$3 AND generation=$4").bind(id).bind(hash).bind(chunk_index).bind(generation).bind(&vector).fetch_one(&mut *tx).await?;
        if !same {
            return Err(invalid(
                "embedding generation is immutable; choose a new generation",
            ));
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn status(&self) -> Result<Value> {
        let row=sqlx::query("SELECT (SELECT count(*) FROM kf_documents) AS documents,(SELECT count(*) FROM kf_versions) AS versions,(SELECT count(*) FROM kf_chunks) AS chunks,(SELECT count(*) FROM kf_embeddings) AS embeddings,(SELECT count(*) FROM kf_proposals) AS proposals,(SELECT count(*) FROM kf_jobs WHERE state IN('queued','running','needs_reconciliation')) AS open_jobs,(SELECT extversion FROM pg_extension WHERE extname='vector') AS vector_version,EXISTS(SELECT 1 FROM pg_indexes WHERE indexname='kf_embedding_cosine_hnsw') AS hnsw_installed").fetch_one(&self.pool).await?;
        let inventory=sqlx::query("SELECT count(*) AS videos,count(*) FILTER(WHERE availability='public' AND published_at>='2026-07-26T22:00:00Z'::timestamptz) AS eligible,count(*) FILTER(WHERE availability='public' AND published_at>='2026-10-27T23:00:00Z'::timestamptz) AS future_eligible,(SELECT count(*) FROM kf_sources) AS catalog_sources,(SELECT count(*) FROM kf_documents d JOIN kf_versions v ON v.document_id=d.id AND v.hash=d.current_hash WHERE v.complete) AS complete_current,COALESCE((SELECT jsonb_object_agg(state,n) FROM(SELECT state,count(*) AS n FROM kf_videos GROUP BY state) AS x),'{}'::jsonb) AS inventory_states,COALESCE((SELECT jsonb_object_agg(availability,n) FROM(SELECT availability,count(*) AS n FROM kf_videos GROUP BY availability) AS x),'{}'::jsonb) AS availability_states,COALESCE((SELECT jsonb_object_agg(state,n) FROM(SELECT state,count(*) AS n FROM kf_jobs WHERE kind LIKE 'capture.%' OR kind LIKE 'caption.%' GROUP BY state) AS x),'{}'::jsonb) AS capture_states FROM kf_videos").fetch_one(&self.pool).await?;
        let generations=sqlx::query("SELECT g.name,g.dimensions,count(e.document_id) AS total,count(d.id) AS current FROM kf_embedding_generations g LEFT JOIN kf_embeddings e ON e.generation=g.name LEFT JOIN kf_documents d ON d.id=e.document_id AND d.current_hash=e.hash GROUP BY g.name,g.dimensions ORDER BY g.name").fetch_all(&self.pool).await?;
        let google = sqlx::query("SELECT (SELECT count(*) FROM kf_google_writes WHERE state='uncertain') AS uncertain_writes,(SELECT value FROM kf_settings WHERE key='last_google_sync') AS last_sync")
            .fetch_one(&self.pool).await?;
        Ok(
            json!({"documents":row.get::<i64,_>("documents"),"versions":row.get::<i64,_>("versions"),"chunks":row.get::<i64,_>("chunks"),"embeddings":row.get::<i64,_>("embeddings"),"proposals":row.get::<i64,_>("proposals"),"open_jobs":row.get::<i64,_>("open_jobs"),"pgvector_version":row.get::<Option<String>,_>("vector_version"),"hnsw_installed":row.get::<bool,_>("hnsw_installed"),"semantic_exact_candidate_limit":EXACT_LIMIT,"embedding_dimensions":DIMENSIONS,"lexical_rank":"ts_rank_cd (simple, english, french)","truth_certification":false,
            "google_uncertain_writes":google.get::<i64,_>("uncertain_writes"),"last_google_sync":google.get::<Option<Value>,_>("last_sync"),"catalog_sources":inventory.get::<i64,_>("catalog_sources"),"inventory_videos":inventory.get::<i64,_>("videos"),"eligible_since_cutoff":inventory.get::<i64,_>("eligible"),"historical_cutoff":"2026-07-26T22:00:00Z","automatic_playlist_cutoff":"2026-10-27T23:00:00Z","future_playlist_eligible":inventory.get::<i64,_>("future_eligible"),"complete_current_documents":inventory.get::<i64,_>("complete_current"),"inventory_state_counts":inventory.get::<Value,_>("inventory_states"),"inventory_availability_counts":inventory.get::<Value,_>("availability_states"),"capture_jobs_by_state":inventory.get::<Value,_>("capture_states"),"index_generations":generations.into_iter().map(|r|json!({"generation":r.get::<String,_>("name"),"dimensions":r.get::<i32,_>("dimensions"),"embeddings_total":r.get::<i64,_>("total"),"embeddings_current":r.get::<i64,_>("current")})).collect::<Vec<_>>(),"inventory_does_not_prove_transcription":true}),
        )
    }

    pub async fn sources(&self) -> Result<Value> {
        let rows=sqlx::query(r#"WITH active AS MATERIALIZED(
            SELECT v.payload->'metadata' AS metadata,v.payload #>> '{metadata,source_id}' AS source_id,v.complete
            FROM kf_documents d JOIN kf_versions v ON v.document_id=d.id AND v.hash=d.current_hash
        ), counts AS(
            SELECT source_id,count(*) AS documents,count(*) FILTER(WHERE complete) AS complete_documents,
                COALESCE(jsonb_agg(DISTINCT metadata->>'language') FILTER(WHERE metadata->>'language' IS NOT NULL),'[]'::jsonb) AS languages,
                COALESCE(jsonb_agg(DISTINCT metadata->>'publisher') FILTER(WHERE metadata->>'publisher' IS NOT NULL),'[]'::jsonb) AS publishers
            FROM active GROUP BY source_id
        ), speakers AS(
            SELECT source_id,jsonb_agg(DISTINCT label) AS labels FROM active CROSS JOIN LATERAL jsonb_array_elements_text(metadata->'speakers') AS t(label) GROUP BY source_id
        ), themes AS(
            SELECT source_id,jsonb_agg(DISTINCT label) AS labels FROM active CROSS JOIN LATERAL jsonb_array_elements_text(metadata->'themes') AS t(label) GROUP BY source_id
        ), ids AS(SELECT source_id FROM kf_sources UNION SELECT source_id FROM active)
        SELECT ids.source_id,COALESCE(s.catalog,'{}'::jsonb) AS catalog,COALESCE(c.documents,0) AS documents,COALESCE(c.complete_documents,0) AS complete_documents,
            COALESCE(c.languages,'[]'::jsonb) AS languages,COALESCE(c.publishers,'[]'::jsonb) AS publishers,COALESCE(sp.labels,'[]'::jsonb) AS speakers,COALESCE(th.labels,'[]'::jsonb) AS themes
        FROM ids LEFT JOIN kf_sources s USING(source_id) LEFT JOIN counts c USING(source_id) LEFT JOIN speakers sp USING(source_id) LEFT JOIN themes th USING(source_id) ORDER BY ids.source_id"#).fetch_all(&self.pool).await?;
        Ok(Value::Array(rows.into_iter().map(|r|json!({"source_id":r.get::<String,_>("source_id"),"catalog":r.get::<Value,_>("catalog"),"documents":r.get::<i64,_>("documents"),"complete_documents":r.get::<i64,_>("complete_documents"),"languages":r.get::<Value,_>("languages"),"publishers":r.get::<Value,_>("publishers"),"speakers":r.get::<Value,_>("speakers"),"themes":r.get::<Value,_>("themes"),"labels_are_declared_metadata":true})).collect()))
    }

    pub async fn provenance(&self, id: Uuid) -> Result<Value> {
        let rows=sqlx::query("SELECT v.hash,v.payload->'metadata' AS metadata,v.payload->>'source_hash' AS source_hash,v.complete,(d.current_hash=v.hash) AS current FROM kf_versions v JOIN kf_documents d ON d.id=v.document_id WHERE v.document_id=$1 ORDER BY v.created_at,v.hash").bind(id).fetch_all(&self.pool).await?;
        if rows.is_empty() {
            return Err(Error::NotFound.into());
        }
        Ok(
            json!({"document_id":id,"versions":rows.into_iter().map(|r|json!({"document_hash":r.get::<String,_>("hash"),"source_hash":r.get::<String,_>("source_hash"),"metadata":r.get::<Value,_>("metadata"),"complete":r.get::<bool,_>("complete"),"current":r.get::<bool,_>("current")})).collect::<Vec<_>>(),"truth_certification":false}),
        )
    }

    /// Verify a quotation against an immutable stored version. This is a
    /// read-only integrity check, never certification of semantic truth.
    pub async fn check_evidence(&self, evidence: &Evidence) -> Result<()> {
        if evidence.document_hash.len() != 64 || evidence.quote.len() > 32_768 {
            return Err(invalid("evidence size or version hash is invalid"));
        }
        let payload: Option<Value> =
            sqlx::query_scalar("SELECT payload FROM kf_versions WHERE document_id=$1 AND hash=$2")
                .bind(evidence.document_id)
                .bind(&evidence.document_hash)
                .fetch_optional(&self.pool)
                .await?;
        verify_evidence_payload(evidence, &payload.ok_or(Error::NotFound)?)
    }

    pub async fn submit_proposal(&self, proposal: &WikiProposal) -> Result<Value> {
        if proposal.kind.trim().is_empty()
            || proposal.kind.len() > 64
            || proposal.title.trim().is_empty()
            || proposal.title.len() > 4096
            || proposal.claims.is_empty()
            || proposal.claims.len() > 100
        {
            return Err(invalid("invalid proposal"));
        }
        let mut tx = self.pool.begin().await?;
        for claim in &proposal.claims {
            if claim.text.trim().is_empty()
                || claim.text.len() > 32000
                || claim.evidence.is_empty()
                || claim.evidence.len() > 100
            {
                return Err(invalid("each claim requires bounded text and evidence"));
            }
            for e in &claim.evidence {
                let payload: Option<Value> = sqlx::query_scalar(
                    "SELECT payload FROM kf_versions WHERE document_id=$1 AND hash=$2",
                )
                .bind(e.document_id)
                .bind(&e.document_hash)
                .fetch_optional(&mut *tx)
                .await?;
                let payload = payload.ok_or(Error::NotFound)?;
                let text = payload["text"].as_str().context("invalid stored text")?;
                if e.end_byte <= e.start_byte
                    || e.end_byte > text.len()
                    || !text.is_char_boundary(e.start_byte)
                    || !text.is_char_boundary(e.end_byte)
                    || text[e.start_byte..e.end_byte] != e.quote
                {
                    return Err(invalid(
                        "evidence quote or UTF-8 byte range does not match the cited immutable version",
                    ));
                }
            }
        }
        let payload = serde_json::to_value(proposal)?;
        let digest = kf_core::sha256(&serde_json::to_vec(&payload)?);
        let id = Uuid::new_v4();
        let stored:Uuid=sqlx::query_scalar("INSERT INTO kf_proposals(id,digest,payload) VALUES($1,$2,$3) ON CONFLICT(digest) DO UPDATE SET digest=EXCLUDED.digest RETURNING id").bind(id).bind(digest).bind(payload).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(
            json!({"proposal_id":stored,"status":"proposed","quote_references_verified":true,"truth_certification":false}),
        )
    }

    /// Read generated analysis, never an independent source. No identifier lists
    /// only the latest twenty summaries; an identifier enables hash-bound pages.
    pub async fn read_wiki(
        &self,
        proposal_id: Option<Uuid>,
        cursor: usize,
        limit: usize,
        expected_hash: Option<&str>,
    ) -> Result<Value> {
        if !(1..=8000).contains(&limit) {
            return Err(invalid("proposal read accepts a limit in 1..=8000"));
        }
        if expected_hash.is_some_and(|hash| {
            hash.len() != 64
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            return Err(invalid("invalid proposal hash"));
        }
        if let Some(id) = proposal_id {
            let row = sqlx::query("SELECT digest,payload FROM kf_proposals WHERE id=$1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
                .ok_or(Error::NotFound)?;
            return wiki_page(
                id,
                &row.get::<String, _>("digest"),
                &row.get::<Value, _>("payload"),
                cursor,
                limit,
                expected_hash,
            );
        }
        if cursor != 0 || expected_hash.is_some() {
            return Err(invalid(
                "listing latest proposals does not accept a cursor or version hash",
            ));
        }
        let rows = sqlx::query("SELECT id,digest,left(payload->>'title',128) AS title,char_length(payload->>'title')>128 AS title_truncated,payload->>'kind' AS kind,created_at::text AS created_at FROM kf_proposals ORDER BY created_at DESC,id DESC LIMIT 21")
            .fetch_all(&self.pool).await?;
        let more = rows.len() > 20;
        let items:Vec<Value> = rows.into_iter().take(20).map(|row| json!({"proposal_id":row.get::<Uuid,_>("id"),"proposal_hash":row.get::<String,_>("digest"),"title":row.get::<Option<String>,_>("title"),"title_truncated":row.get::<Option<bool>,_>("title_truncated"),"kind":row.get::<Option<String>,_>("kind"),"created_at":row.get::<String,_>("created_at"),"generated":true,"independent_evidence":false,"truth_certification":false})).collect();
        let result = json!({"items":items,"listing_scope":"latest_20","has_more":more,"list_is_complete":!more,"eof":true,"generated":true,"independent_evidence":false,"truth_certification":false});
        if serde_json::to_vec(&result)?.len() > 32768 {
            return Err(invalid("proposal listing exceeds response bound"));
        }
        Ok(result)
    }

    pub async fn enqueue(&self, kind: &str, key: &str, payload: &Value) -> Result<Value> {
        if kind.is_empty()
            || kind.len() > 64
            || key.is_empty()
            || key.len() > 256
            || serde_json::to_vec(payload)?.len() > 1024 * 1024
        {
            return Err(invalid("invalid job kind, key or payload"));
        }
        let id = Uuid::new_v4();
        let row=sqlx::query("INSERT INTO kf_jobs(id,kind,job_key,payload) VALUES($1,$2,$3,$4) ON CONFLICT(kind,job_key) DO UPDATE SET job_key=EXCLUDED.job_key RETURNING id,payload,state").bind(id).bind(kind).bind(key).bind(payload).fetch_one(&self.pool).await?;
        if row.get::<Value, _>("payload") != *payload {
            return Err(invalid("job key reused with different payload"));
        }
        Ok(json!({"id":row.get::<Uuid,_>("id"),"state":row.get::<String,_>("state")}))
    }

    pub async fn claim_job(&self, worker: &str) -> Result<Option<Value>> {
        if worker.trim().is_empty() || worker.len() > 128 {
            return Err(invalid("invalid worker identity"));
        }
        let mut tx = self.pool.begin().await?;
        // A lost external mutation is never blindly replayed. Operator reconciles it first.
        sqlx::query("WITH expired AS(SELECT id FROM kf_jobs WHERE state='running' AND lease_until<now() FOR UPDATE SKIP LOCKED LIMIT 1000) UPDATE kf_jobs j SET state=CASE WHEN j.kind LIKE 'google.%' OR j.kind LIKE 'external.%' THEN 'needs_reconciliation' WHEN j.attempts>=j.max_attempts THEN 'failed' ELSE 'queued' END,worker=NULL,lease_until=NULL,updated_at=now() FROM expired WHERE j.id=expired.id").execute(&mut *tx).await?;
        let row=sqlx::query("WITH picked AS(SELECT id FROM kf_jobs WHERE state='queued' AND attempts<max_attempts ORDER BY created_at,id FOR UPDATE SKIP LOCKED LIMIT 1) UPDATE kf_jobs j SET state='running',worker=$1,lease_until=now()+interval '5 minutes',attempts=attempts+1,updated_at=now() FROM picked WHERE j.id=picked.id RETURNING j.id,j.kind,j.job_key,j.payload,j.attempts").bind(worker).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        Ok(row.map(|r|json!({"id":r.get::<Uuid,_>("id"),"kind":r.get::<String,_>("kind"),"key":r.get::<String,_>("job_key"),"payload":r.get::<Value,_>("payload"),"attempts":r.get::<i32,_>("attempts"),"worker":worker,"lease_seconds":300})))
    }

    pub async fn finish_job(
        &self,
        id: Uuid,
        worker: &str,
        success: bool,
        detail: &Value,
    ) -> Result<()> {
        if worker.trim().is_empty()
            || worker.len() > 128
            || serde_json::to_vec(detail)?.len() > 1024 * 1024
        {
            return Err(invalid("invalid worker or result size"));
        }
        let n=sqlx::query("UPDATE kf_jobs SET state=CASE WHEN $3 THEN 'succeeded' WHEN kind LIKE 'google.%' OR kind LIKE 'external.%' THEN 'needs_reconciliation' WHEN attempts<max_attempts THEN 'queued' ELSE 'failed' END,result=$4,worker=NULL,lease_until=NULL,updated_at=now() WHERE id=$1 AND worker=$2 AND state='running' AND lease_until>now()").bind(id).bind(worker).bind(success).bind(detail).execute(&self.pool).await?.rows_affected();
        if n != 1 {
            return Err(invalid(
                "job lease is absent, expired or owned by another worker",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> (PreparedDocument, Vec<Chunk>) {
        let document: PreparedDocument = serde_json::from_value(json!({
            "metadata":{"external_id":"synthetic","source_id":"unit","title":"Example","publisher":"Example","source_url":null,"published_at":null,"language":"fr","themes":[],"speakers":[]},
            "segments":[],"text":"Énergie","markdown":"Énergie","source_hash":kf_core::sha256("Énergie".as_bytes()),"document_hash":kf_core::sha256("Énergie".as_bytes()),"complete":true,"capture_format":"txt"
        })).unwrap();
        let chunk = Chunk {
            index: 0,
            text: document.text.clone(),
            start_byte: 0,
            end_byte: document.text.len(),
            start_ms: Some(0),
            end_ms: Some(10),
        };
        (document, vec![chunk])
    }

    #[test]
    fn document_chunks_require_exact_complete_utf8_coverage() {
        let (document, mut chunks) = sample();
        assert!(validate_document(&document, &chunks).is_ok());
        chunks[0].start_byte = 1;
        assert!(validate_document(&document, &chunks).is_err());
        chunks[0].start_byte = 0;
        chunks[0].end_byte -= 1;
        chunks[0].text.pop();
        assert!(validate_document(&document, &chunks).is_err());
    }

    #[test]
    fn document_chunks_reject_text_and_time_fabrication() {
        let (document, mut chunks) = sample();
        chunks[0].text = "invented".into();
        assert!(validate_document(&document, &chunks).is_err());
        chunks[0].text = document.text.clone();
        chunks[0].end_ms = Some(0);
        chunks[0].start_ms = Some(10);
        assert!(validate_document(&document, &chunks).is_err());
    }

    #[test]
    fn document_rejects_markdown_hash_tampering() {
        let (mut document, chunks) = sample();
        document.markdown.push_str(" changed");
        assert!(validate_document(&document, &chunks).is_err());
    }
    #[test]
    fn vectors_require_dimension_finite_and_nonzero() {
        assert!(checked_vector(&[1.0; 1024]).is_ok());
        assert!(checked_vector(&[0.0; 1024]).is_err());
        assert!(checked_vector(&[f32::NAN; 1024]).is_err());
        assert!(checked_vector(&[1.0]).is_err());
    }
    #[test]
    fn generation_is_bounded() {
        assert!(validate_generation("model-v1:1024").is_ok());
        assert!(validate_generation("x';DROP").is_err());
    }
    #[test]
    fn filter_values_are_bound() {
        let f = SearchFilters {
            source_id: Some("' OR TRUE --".into()),
            ..Default::default()
        };
        let mut q = QueryBuilder::<Postgres>::new("SELECT 1 WHERE TRUE");
        filters(&mut q, &f);
        assert!(!q.sql().contains("OR TRUE --"));
        assert!(q.sql().contains("$1"));
    }

    #[test]
    fn evidence_requires_exact_version_identity_and_utf8_bytes() {
        let (document, _) = sample();
        let payload = serde_json::to_value(&document).unwrap();
        let evidence = Evidence {
            document_id: document.metadata.document_id(),
            document_hash: document.document_hash.clone(),
            start_byte: 0,
            end_byte: document.text.len(),
            quote: document.text.clone(),
        };
        assert!(verify_evidence_payload(&evidence, &payload).is_ok());
        let mut wrong = evidence.clone();
        wrong.start_byte = 1;
        assert!(verify_evidence_payload(&wrong, &payload).is_err());
        let mut wrong = evidence.clone();
        wrong.quote = "invented".into();
        assert!(verify_evidence_payload(&wrong, &payload).is_err());
        let mut wrong = evidence.clone();
        wrong.document_hash = "c".repeat(64);
        assert!(verify_evidence_payload(&wrong, &payload).is_err());
        let mut wrong = evidence.clone();
        wrong.document_id = Uuid::new_v4();
        assert!(verify_evidence_payload(&wrong, &payload).is_err());
        let mut wrong = evidence;
        wrong.end_byte = usize::MAX;
        assert!(verify_evidence_payload(&wrong, &payload).is_err());
    }

    #[test]
    fn generated_wiki_pages_preserve_utf8_hash_and_explicit_end() {
        let id = Uuid::new_v4();
        let payload = json!({"title":"Generated résumé 😀","kind":"concept","claims":[{"text":"Analyse générée et attribuée."}]});
        let canonical = serde_json::to_string(&payload).unwrap();
        let hash = kf_core::sha256(canonical.as_bytes());
        let mut cursor = 0;
        let mut recovered = String::new();
        loop {
            let page = wiki_page(id, &hash, &payload, cursor, 17, Some(&hash)).unwrap();
            assert_eq!(page["generated"], true);
            assert_eq!(page["independent_evidence"], false);
            assert_eq!(page["truth_certification"], false);
            recovered.push_str(page["text"].as_str().unwrap());
            match page["next_cursor"].as_u64() {
                Some(next) => {
                    assert!(!page["eof"].as_bool().unwrap());
                    cursor = next as usize;
                }
                None => {
                    assert_eq!(page["eof"], true);
                    break;
                }
            }
        }
        assert_eq!(recovered, canonical);
        assert!(wiki_page(id, &hash, &payload, 1, 17, None).is_err());
        assert!(wiki_page(id, &hash, &payload, 0, 8001, None).is_err());
        assert!(wiki_page(id, &hash, &payload, 0, 17, Some(&"b".repeat(64))).is_err());
        assert!(wiki_page(id, &"b".repeat(64), &payload, 0, 17, None).is_err());
        let broken = canonical.find('😀').unwrap() + 1;
        assert!(wiki_page(id, &hash, &payload, broken, 17, Some(&hash)).is_err());
    }
}
