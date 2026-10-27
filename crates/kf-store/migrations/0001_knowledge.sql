CREATE EXTENSION IF NOT EXISTS vector;
DO $$
BEGIN
    IF string_to_array((SELECT extversion FROM pg_extension WHERE extname='vector'),'.')::integer[] < ARRAY[0,8,7] THEN
        RAISE EXCEPTION 'pgvector 0.8.7 or later is required';
    END IF;
END $$;

CREATE TABLE kf_documents (
    id uuid PRIMARY KEY,
    current_hash text,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE kf_versions (
    document_id uuid NOT NULL REFERENCES kf_documents(id),
    hash text NOT NULL CHECK (hash ~ '^[a-f0-9]{64}$'),
    payload jsonb NOT NULL,
    complete boolean NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(document_id, hash)
);
ALTER TABLE kf_documents ADD CONSTRAINT kf_current_version
    FOREIGN KEY(id,current_hash) REFERENCES kf_versions(document_id,hash)
    DEFERRABLE INITIALLY DEFERRED;
CREATE TABLE kf_chunks (
    document_id uuid NOT NULL,
    hash text NOT NULL,
    chunk_index integer NOT NULL CHECK(chunk_index >= 0),
    text text NOT NULL,
    start_byte bigint NOT NULL CHECK(start_byte >= 0),
    end_byte bigint NOT NULL CHECK(end_byte > start_byte),
    start_ms bigint,
    end_ms bigint,
    search_simple tsvector GENERATED ALWAYS AS(to_tsvector('simple'::regconfig,text)) STORED,
    search_english tsvector GENERATED ALWAYS AS(to_tsvector('english'::regconfig,text)) STORED,
    search_french tsvector GENERATED ALWAYS AS(to_tsvector('french'::regconfig,text)) STORED,
    PRIMARY KEY(document_id,hash,chunk_index),
    FOREIGN KEY(document_id,hash) REFERENCES kf_versions(document_id,hash)
);
CREATE INDEX kf_chunks_simple ON kf_chunks USING gin(search_simple);
CREATE INDEX kf_chunks_english ON kf_chunks USING gin(search_english);
CREATE INDEX kf_chunks_french ON kf_chunks USING gin(search_french);
CREATE TABLE kf_embedding_generations (
    name text PRIMARY KEY,
    dimensions integer NOT NULL CHECK(dimensions=1024),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE kf_embeddings (
    document_id uuid NOT NULL,
    hash text NOT NULL,
    chunk_index integer NOT NULL,
    generation text NOT NULL REFERENCES kf_embedding_generations(name),
    embedding vector(1024) NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(document_id,hash,chunk_index,generation),
    FOREIGN KEY(document_id,hash,chunk_index) REFERENCES kf_chunks(document_id,hash,chunk_index)
);
CREATE INDEX kf_embedding_cosine_hnsw ON kf_embeddings USING hnsw(embedding vector_cosine_ops);
CREATE TABLE kf_proposals (
    id uuid PRIMARY KEY,
    digest text NOT NULL UNIQUE,
    payload jsonb NOT NULL,
    status text NOT NULL DEFAULT 'proposed' CHECK(status='proposed'),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE kf_jobs (
    id uuid PRIMARY KEY,
    kind text NOT NULL,
    job_key text NOT NULL,
    payload jsonb NOT NULL,
    state text NOT NULL DEFAULT 'queued' CHECK(state IN('queued','running','succeeded','failed','needs_reconciliation')),
    worker text,
    lease_until timestamptz,
    attempts integer NOT NULL DEFAULT 0,
    max_attempts integer NOT NULL DEFAULT 3,
    result jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE(kind,job_key)
);
CREATE INDEX kf_jobs_claim ON kf_jobs(state,created_at);
