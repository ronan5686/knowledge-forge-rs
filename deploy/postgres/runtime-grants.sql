-- Store owns the canonical table privilege list; this is its deployment entry.
-- Invoke psql -f with this file, in the dedicated Knowledge Forge database only.
\set ON_ERROR_STOP on
BEGIN;
\ir ../../crates/kf-store/roles.sql
COMMIT;
