-- Operator runs this in the dedicated Knowledge Forge database after migration.
-- Roles are provisioned independently; no password, LOGIN role or database is created here.
-- The migrator runs SET ROLE kf_owner on the same connection as migrate().
GRANT SELECT ON kf_documents,kf_versions,kf_chunks,kf_embeddings,kf_embedding_generations,kf_proposals,kf_jobs TO kf_reader,kf_ingest,kf_curator;
GRANT INSERT ON kf_documents,kf_versions,kf_chunks,kf_embeddings,kf_embedding_generations,kf_jobs TO kf_ingest;
GRANT UPDATE(current_hash) ON kf_documents TO kf_ingest;
GRANT UPDATE(job_key,state,worker,lease_until,attempts,result,updated_at) ON kf_jobs TO kf_ingest;
GRANT INSERT ON kf_proposals TO kf_curator;
GRANT UPDATE(digest) ON kf_proposals TO kf_curator;
GRANT SELECT ON kf_sources,kf_videos,kf_settings,kf_google_quota,kf_google_writes TO kf_reader,kf_ingest,kf_curator;
GRANT INSERT,UPDATE ON kf_sources,kf_videos,kf_settings,kf_google_quota,kf_google_writes TO kf_ingest;
-- No application role can UPDATE or DELETE immutable versions, chunks or embeddings.
