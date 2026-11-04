-- 001_init.sql — initial schema.
--
-- Baseline schema for a fresh database. Applied migrations must never be
-- edited afterwards; append a new numbered migration for future changes.

-- ===== Chats and subscriptions =====

CREATE TABLE chats (
    chat_id      TEXT PRIMARY KEY,
    chat_type    TEXT NOT NULL,
    title        TEXT,
    username     TEXT,
    is_forum     INTEGER DEFAULT 0,
    added_by     TEXT,
    enabled      INTEGER DEFAULT 1,
    created_at   TEXT DEFAULT (datetime('now')),
    updated_at   TEXT DEFAULT (datetime('now')),
    last_seen_at TEXT
);

CREATE TABLE subscriptions (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id           TEXT NOT NULL,
    feed_url          TEXT NOT NULL,
    feed_title        TEXT,
    title_overridden  INTEGER DEFAULT 0,
    active            INTEGER DEFAULT 1,
    error_count       INTEGER DEFAULT 0,
    last_fetch_at     TEXT,
    last_error        TEXT,
    created_at        TEXT DEFAULT (datetime('now')),
    http_etag         TEXT,
    http_last_modified TEXT,
    auto_disabled     INTEGER NOT NULL DEFAULT 0,
    disabled_at       TEXT,
    original_title    TEXT,
    UNIQUE(chat_id, feed_url)
);

CREATE INDEX idx_subscriptions_chat ON subscriptions(chat_id);

-- Per-chat delivery settings; feed_configs overrides nullable fields.

CREATE TABLE chat_configs (
    chat_id             TEXT PRIMARY KEY,
    interval_minutes    INTEGER DEFAULT 10,
    content_mode        TEXT DEFAULT 'title_link',
    source_display      TEXT DEFAULT 'source',
    caption_above_media INTEGER DEFAULT 0,
    updated_at          TEXT DEFAULT (datetime('now')),
    link_preview        INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE feed_configs (
    chat_id             TEXT NOT NULL,
    feed_url            TEXT NOT NULL,
    interval_minutes    INTEGER,
    content_mode        TEXT,
    source_display      TEXT,
    caption_above_media INTEGER,
    updated_at          TEXT DEFAULT (datetime('now')),
    link_preview        INTEGER,
    filter_enabled      INTEGER NOT NULL DEFAULT 0,
    filter_mode         TEXT NOT NULL DEFAULT 'blacklist',
    filter_keywords     TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (chat_id, feed_url)
);

CREATE INDEX idx_chats_added_by ON chats(added_by);

-- ===== Delivery dedup, cache and outbox =====

CREATE TABLE sent_entries (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id   TEXT NOT NULL,
    feed_url  TEXT NOT NULL,
    entry_id  TEXT NOT NULL,
    sent_at   TEXT DEFAULT (datetime('now')),
    UNIQUE(chat_id, feed_url, entry_id)
);

CREATE INDEX idx_sent_entries_sent_at ON sent_entries(sent_at);

CREATE TABLE sent_messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id     TEXT NOT NULL,
    message_id  INTEGER NOT NULL,
    feed_url    TEXT NOT NULL,
    entry_id    TEXT NOT NULL,
    entry_link  TEXT,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(chat_id, message_id)
);

CREATE TABLE file_id_cache (
    content_hash TEXT PRIMARY KEY,
    file_id      TEXT NOT NULL,
    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id TEXT NOT NULL,
    feed_url TEXT NOT NULL,
    entry_id TEXT NOT NULL,
    feed_title TEXT NOT NULL DEFAULT '',
    entry_json TEXT NOT NULL,
    media_json TEXT NOT NULL DEFAULT '[]',
    content_mode TEXT NOT NULL DEFAULT 'TitleSummary',
    source_display TEXT NOT NULL DEFAULT 'Source',
    link_preview INTEGER NOT NULL DEFAULT 1,
    caption_above_media INTEGER NOT NULL DEFAULT 0,
    author TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_retry_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    last_error TEXT,
    meta_json TEXT NOT NULL DEFAULT '',
    first_attempt_at INTEGER,
    UNIQUE(chat_id, feed_url, entry_id)
);

CREATE INDEX idx_outbox_next_retry ON outbox(next_retry_at);
