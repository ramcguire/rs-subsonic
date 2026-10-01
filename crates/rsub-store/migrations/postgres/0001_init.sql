-- rs-subsonic schema. Keep in sync with migrations/sqlite/0001_init.sql: the two
-- differ only in id columns (identity columns here, AUTOINCREMENT there, so ids
-- are never reused), bytes (BYTEA vs BLOB) and vectors (pgvector's type vs f32
-- little-endian BLOBs, which sqlite-vec reads).
--
-- Times (`*_at`, `mtime`) are Unix epoch milliseconds in a BIGINT. Backend
-- stamps, sync cursors and file mtimes are compared for equality with their
-- source, so they are stored exactly as given.

-- Sonic similarity is searched with pgvector. It isn't a trusted extension: a
-- superuser creates it once in the database, and this is then a no-op.
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE settings (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);

-- username_key is the username case-folded in Rust: usernames are unique and
-- matched regardless of case. Passwords are encrypted with `username` as context.
CREATE TABLE users (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    username     TEXT NOT NULL,
    username_key TEXT NOT NULL UNIQUE,
    password_enc BYTEA NOT NULL,
    email        TEXT,
    roles        BIGINT NOT NULL,
    max_bitrate  BIGINT NOT NULL DEFAULT 0,
    created_at   BIGINT NOT NULL,
    updated_at   BIGINT NOT NULL
);

-- key_hash: SHA-256 of the key, which is never stored.
CREATE TABLE api_keys (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    key_hash   BYTEA NOT NULL UNIQUE,
    created_at BIGINT NOT NULL
);

CREATE INDEX api_keys_user_id ON api_keys (user_id);

-- name: the backend's id in the config.
CREATE TABLE sources (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    kind       TEXT NOT NULL,
    created_at BIGINT NOT NULL
);

-- The Subsonic music folders.
CREATE TABLE libraries (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source_id         BIGINT NOT NULL REFERENCES sources (id) ON DELETE CASCADE,
    remote_key        TEXT NOT NULL,
    name              TEXT NOT NULL,
    generation        BIGINT NOT NULL DEFAULT 0,
    last_full_sync_at BIGINT,
    -- When the contents last changed here (`getIndexes` lastModified).
    last_sync_at      BIGINT,
    -- The backend's change marker and newest change stamp as of the last
    -- completed sync; incremental syncs start from the stamp.
    change_marker     TEXT,
    changes_cursor    BIGINT,
    deleted_at        BIGINT,
    UNIQUE (source_id, remote_key)
);

-- Catalog rows are soft-deleted (deleted_at) and marked by the sync pass that
-- last saw them (generation). public_id is the id clients see; integer ids stay
-- internal.

-- remote_key NULL: a virtual artist, credited by name only.
CREATE TABLE artists (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    public_id         TEXT NOT NULL,
    library_id        BIGINT NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    remote_key        TEXT,
    guid              TEXT,
    name              TEXT NOT NULL,
    sort_key          TEXT NOT NULL,
    search_norm       TEXT NOT NULL,
    mbid              TEXT,
    summary           TEXT,
    thumb_ref         TEXT,
    album_count       BIGINT NOT NULL DEFAULT 0,
    remote_updated_at BIGINT NOT NULL DEFAULT 0,
    generation        BIGINT NOT NULL,
    deleted_at        BIGINT
);

CREATE UNIQUE INDEX artists_public ON artists (public_id);
CREATE UNIQUE INDEX artists_remote ON artists (library_id, remote_key);
CREATE UNIQUE INDEX artists_virtual ON artists (library_id, name) WHERE remote_key IS NULL;
CREATE INDEX artists_name ON artists (library_id, name);
CREATE INDEX artists_sort ON artists (sort_key);

-- release_types: JSON array.
CREATE TABLE albums (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    public_id         TEXT NOT NULL,
    library_id        BIGINT NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    remote_key        TEXT NOT NULL,
    guid              TEXT,
    title             TEXT NOT NULL,
    sort_key          TEXT NOT NULL,
    search_norm       TEXT NOT NULL,
    display_artist    TEXT NOT NULL,
    artist_sort_key   TEXT NOT NULL,
    year              BIGINT,
    release_date      TEXT,
    orig_release_date TEXT,
    label             TEXT,
    release_types     TEXT NOT NULL DEFAULT '[]',
    is_compilation    BOOLEAN NOT NULL DEFAULT FALSE,
    mbid              TEXT,
    thumb_ref         TEXT,
    song_count        BIGINT NOT NULL DEFAULT 0,
    duration_ms       BIGINT NOT NULL DEFAULT 0,
    added_at          BIGINT NOT NULL,
    remote_updated_at BIGINT NOT NULL,
    generation        BIGINT NOT NULL,
    deleted_at        BIGINT
);

CREATE UNIQUE INDEX albums_public ON albums (public_id);
CREATE UNIQUE INDEX albums_remote ON albums (library_id, remote_key);
CREATE INDEX albums_sort ON albums (sort_key);
CREATE INDEX albums_artist_sort ON albums (artist_sort_key);
CREATE INDEX albums_added ON albums (added_at);
CREATE INDEX albums_year ON albums (year);

-- enriched_at: when the backend's detail view was read; NULL after a rewrite.
CREATE TABLE tracks (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    public_id         TEXT NOT NULL,
    library_id        BIGINT NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    album_id          BIGINT NOT NULL REFERENCES albums (id) ON DELETE CASCADE,
    remote_key        TEXT NOT NULL,
    guid              TEXT,
    part_key          TEXT NOT NULL,
    remote_path       TEXT,
    title             TEXT NOT NULL,
    sort_key          TEXT NOT NULL,
    search_norm       TEXT NOT NULL,
    display_artist    TEXT NOT NULL,
    track_no          BIGINT,
    disc_no           BIGINT,
    year              BIGINT,
    duration_ms       BIGINT NOT NULL,
    bitrate           BIGINT,
    sample_rate       BIGINT,
    bit_depth         BIGINT,
    channels          BIGINT,
    codec             TEXT,
    suffix            TEXT,
    content_type      TEXT,
    size              BIGINT,
    bpm               BIGINT,
    comment           TEXT,
    mbid              TEXT,
    popularity        BIGINT,
    rg_track_gain     REAL,
    rg_track_peak     REAL,
    rg_album_gain     REAL,
    rg_album_peak     REAL,
    has_lyrics        BOOLEAN NOT NULL DEFAULT FALSE,
    added_at          BIGINT NOT NULL,
    remote_updated_at BIGINT NOT NULL,
    enriched_at       BIGINT,
    generation        BIGINT NOT NULL,
    deleted_at        BIGINT
);

CREATE UNIQUE INDEX tracks_public ON tracks (public_id);
CREATE UNIQUE INDEX tracks_remote ON tracks (library_id, remote_key);
CREATE INDEX tracks_album ON tracks (album_id);
-- Joins with file_tags and file_analysis.
CREATE INDEX tracks_path ON tracks (library_id, remote_path);

CREATE TABLE album_artists (
    album_id  BIGINT NOT NULL REFERENCES albums (id) ON DELETE CASCADE,
    artist_id BIGINT NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
    pos       BIGINT NOT NULL,
    PRIMARY KEY (album_id, pos)
);

CREATE INDEX album_artists_artist ON album_artists (artist_id);

-- role: 'artist', 'albumartist', 'composer', 'other'.
CREATE TABLE track_credits (
    track_id  BIGINT NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    artist_id BIGINT NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
    role      TEXT NOT NULL,
    pos       BIGINT NOT NULL,
    PRIMARY KEY (track_id, role, pos)
);

CREATE INDEX track_credits_artist ON track_credits (artist_id);

-- kind: 'genre', 'mood', 'style'.
CREATE TABLE tags (
    id   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    UNIQUE (kind, name)
);

CREATE TABLE album_tags (
    album_id BIGINT NOT NULL REFERENCES albums (id) ON DELETE CASCADE,
    tag_id   BIGINT NOT NULL REFERENCES tags (id) ON DELETE CASCADE,
    PRIMARY KEY (album_id, tag_id)
);

CREATE INDEX album_tags_tag ON album_tags (tag_id);

CREATE TABLE artist_tags (
    artist_id BIGINT NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
    tag_id    BIGINT NOT NULL REFERENCES tags (id) ON DELETE CASCADE,
    PRIMARY KEY (artist_id, tag_id)
);

CREATE INDEX artist_tags_tag ON artist_tags (tag_id);

CREATE TABLE track_tags (
    track_id BIGINT NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    tag_id   BIGINT NOT NULL REFERENCES tags (id) ON DELETE CASCADE,
    PRIMARY KEY (track_id, tag_id)
);

CREATE INDEX track_tags_tag ON track_tags (tag_id);

-- The identity ledger: every identity key seen and the public id it resolves
-- to. Not tied to catalog rows, so an id outlives its row. kind: 'artist',
-- 'album', 'track'.
CREATE TABLE identity_keys (
    kind          TEXT NOT NULL,
    key           TEXT NOT NULL,
    public_id     TEXT NOT NULL,
    first_seen_at BIGINT NOT NULL,
    last_seen_at  BIGINT NOT NULL,
    PRIMARY KEY (kind, key)
);

CREATE INDEX identity_keys_public ON identity_keys (public_id);

-- Tag-pass cache, re-read when size or mtime change. The *_key columns tie a
-- file to its track, album and album artist; lists are JSON arrays.
CREATE TABLE file_tags (
    library_id         BIGINT NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    remote_path        TEXT NOT NULL,
    track_key          TEXT NOT NULL,
    album_key          TEXT NOT NULL,
    artist_key         TEXT,
    size               BIGINT NOT NULL,
    mtime              BIGINT NOT NULL,
    read_at            BIGINT NOT NULL,
    generation         BIGINT NOT NULL,
    release_track_mbid TEXT,
    recording_mbid     TEXT,
    release_mbid       TEXT,
    artist_mbids       TEXT NOT NULL DEFAULT '[]',
    album_artist_mbids TEXT NOT NULL DEFAULT '[]',
    artists            TEXT NOT NULL DEFAULT '[]',
    album_artists      TEXT NOT NULL DEFAULT '[]',
    title              TEXT,
    album              TEXT,
    disc_no            BIGINT,
    track_no           BIGINT,
    PRIMARY KEY (library_id, remote_path)
);

CREATE INDEX file_tags_track ON file_tags (library_id, track_key);
CREATE INDEX file_tags_album ON file_tags (library_id, album_key);
CREATE INDEX file_tags_artist ON file_tags (library_id, artist_key);

-- Audio analyzers. An id names the analyzer, its version and settings, so every
-- vector of one has the same dims (NULL until the first).
CREATE TABLE analyzers (
    id         TEXT PRIMARY KEY NOT NULL,
    dims       BIGINT,
    created_at BIGINT NOT NULL
);

-- Sonic-similarity vectors, one per file and analyzer, redone when size or mtime
-- change. vector: pgvector's, of any dimension; NULL if the file couldn't be
-- analysed.
CREATE TABLE file_analysis (
    library_id  BIGINT NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    remote_path TEXT NOT NULL,
    analyzer    TEXT NOT NULL REFERENCES analyzers (id) ON DELETE CASCADE,
    size        BIGINT NOT NULL,
    mtime       BIGINT NOT NULL,
    vector      vector,
    analyzed_at BIGINT NOT NULL,
    PRIMARY KEY (library_id, remote_path, analyzer)
);

-- A search reads every row's path and vector. Keep both in the row: rows are
-- about 2 KB, and TOAST would move one out of line, costing a fetch per row
-- (a search takes four times as long).
ALTER TABLE file_analysis
    ALTER COLUMN remote_path SET STORAGE MAIN,
    ALTER COLUMN vector SET STORAGE MAIN;

CREATE INDEX file_analysis_analyzer ON file_analysis (analyzer);

-- Ratings of virtual artists, which have no backend counterpart; every other
-- rating lives in the backend. 5 is a star. Keyed by public id, like the
-- identity ledger, so a rating outlives the artist's row.
CREATE TABLE artist_ratings (
    user_id          BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    artist_public_id TEXT NOT NULL,
    rating           BIGINT NOT NULL,
    rated_at         BIGINT NOT NULL,
    PRIMARY KEY (user_id, artist_public_id)
);
