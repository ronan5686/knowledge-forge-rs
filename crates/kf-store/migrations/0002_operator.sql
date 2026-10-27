-- Operator inventories are separate from actual source documents and transcriptions.
CREATE TABLE kf_sources (
    source_id text PRIMARY KEY,
    catalog jsonb NOT NULL,
    channel_id text,
    uploads_id text,
    history_cursor text,
    history_done boolean NOT NULL DEFAULT false,
    poll_cursor text,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE kf_videos (
    video_id text PRIMARY KEY,
    source_id text NOT NULL REFERENCES kf_sources(source_id),
    published_at timestamptz,
    title text NOT NULL DEFAULT '',
    metadata jsonb NOT NULL DEFAULT '{}',
    availability text NOT NULL DEFAULT 'unknown',
    playlist_item_id text,
    state text NOT NULL DEFAULT 'new',
    uncertain boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX kf_videos_source_date ON kf_videos(source_id,published_at);
CREATE TABLE kf_settings (
    key text PRIMARY KEY,
    value jsonb NOT NULL
);
CREATE TABLE kf_google_quota (
    day date PRIMARY KEY,
    units integer NOT NULL CHECK(units>=0)
);
CREATE TABLE kf_google_writes (
    id uuid PRIMARY KEY,
    op_key text NOT NULL UNIQUE,
    kind text NOT NULL,
    payload jsonb NOT NULL,
    state text NOT NULL,
    external_id text,
    updated_at timestamptz NOT NULL DEFAULT now()
);
