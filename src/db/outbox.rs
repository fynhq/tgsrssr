//! Persistent retry queue (outbox) storage.

use super::*;


// next_retry_at/last_error are only used in SQL WHERE/UPDATE and never read
// directly on the Rust side — kept to preserve the complete mapping of the
// database schema. `first_attempt_at` is read by the retry loop to measure the
// 72h abandonment window from the first actual attempt instead of enqueue time.
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OutboxRow {
    pub id: i64,
    pub chat_id: String,
    pub feed_url: String,
    pub entry_id: String,
    pub feed_title: String,
    pub entry_json: String,
    pub media_json: String,
    /// Group-C sidecar: serialized EntryMeta JSON (empty string means no
    /// metadata).
    pub meta_json: String,
    pub content_mode: String,
    pub source_display: String,
    pub link_preview: bool,
    pub caption_above_media: bool,
    pub author: Option<String>,
    pub attempts: i64,
    pub next_retry_at: i64,
    pub created_at: i64,
    /// Unix seconds of the first real retry attempt; NULL until the first
    /// attempt (legacy rows may also be NULL).
    pub first_attempt_at: Option<i64>,
    pub last_error: Option<String>,
}

/// Insert a failed entry into the outbox for later retry.
/// Uses INSERT OR IGNORE to avoid duplicates (same entry already queued).
#[allow(clippy::too_many_arguments)]
pub async fn outbox_insert(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    entry_id: &str,
    feed_title: &str,
    entry_json: &str,
    media_json: &str,
    meta_json: &str,
    content_mode: &str,
    source_display: &str,
    link_preview: bool,
    caption_above_media: bool,
    author: Option<&str>,
    next_retry_at: i64,
    created_at: i64,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "INSERT OR IGNORE INTO outbox (chat_id, feed_url, entry_id, feed_title, entry_json, media_json, meta_json, content_mode, source_display, link_preview, caption_above_media, author, next_retry_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(chat_id)
    .bind(feed_url)
    .bind(entry_id)
    .bind(feed_title)
    .bind(entry_json)
    .bind(media_json)
    .bind(meta_json)
    .bind(content_mode)
    .bind(source_display)
    .bind(link_preview)
    .bind(caption_above_media)
    .bind(author)
    .bind(next_retry_at)
    .bind(created_at)
    .execute(db)
    .await?;
    // INSERT OR IGNORE: rows_affected=0 means the entry is already in the
    // queue (duplicate enqueue is ignored).
    Ok(result.rows_affected() > 0)
}

/// Ids of outbox rows due for retry (oldest first). Rows belonging to a
/// paused/disabled subscription or a disabled chat are held: pausing a source
/// must stop queued deliveries too, and a removed bot must not keep retrying
/// into a chat it can no longer reach.
///
/// The caller fetches each full row one at a time, so a batch of large
/// `entry_json` values is never loaded into memory at once.
pub async fn outbox_due_ids(db: &Db, now_ts: i64, limit: i64) -> anyhow::Result<Vec<i64>> {
    let rows = sqlx::query_scalar::<_, i64>(
        r#"SELECT o.id
             FROM outbox o
             JOIN chats ch
               ON ch.chat_id = o.chat_id AND ch.enabled = 1
             JOIN subscriptions s
               ON s.chat_id = o.chat_id AND s.feed_url = o.feed_url AND s.active = 1
            WHERE o.next_retry_at <= ?
            ORDER BY o.next_retry_at ASC, o.id ASC
            LIMIT ?"#,
    )
    .bind(now_ts)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Number of queued outbox rows for one (chat, feed). Used by the enqueue path
/// to stop an unavailable chat from growing an unbounded 72h retry queue.
pub async fn outbox_count_for_feed(db: &Db, chat_id: &str, feed_url: &str) -> anyhow::Result<i64> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM outbox WHERE chat_id = ? AND feed_url = ?",
    )
    .bind(chat_id)
    .bind(feed_url)
    .fetch_one(db)
    .await?;
    Ok(count)
}

/// Drop every queued outbox row for a chat. Called when the bot is removed or
/// permanently loses posting rights: keeping the rows would either hold them
/// forever (disabled chat) or flood the channel on re-add.
pub async fn delete_outbox_for_chat(db: &Db, chat_id: &str) -> anyhow::Result<u64> {
    let result = sqlx::query("DELETE FROM outbox WHERE chat_id = ?")
        .bind(chat_id)
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

/// Fetch one complete outbox row by id. Returns None when the row was removed
/// between the id listing and this lookup.
pub async fn outbox_get(db: &Db, id: i64) -> anyhow::Result<Option<OutboxRow>> {
    let row = sqlx::query_as::<_, OutboxRow>(
        "SELECT id, chat_id, feed_url, entry_id, feed_title, entry_json, media_json, meta_json, content_mode, source_display, link_preview, caption_above_media, author, attempts, next_retry_at, created_at, first_attempt_at, last_error FROM outbox WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Stream every media_json value still referenced by the outbox (one narrow
/// column, no limit). Used by the media cleanup pass to build the "protected
/// paths" set without holding every row's JSON string in memory at once.
pub fn outbox_media_json_stream(
    db: &Db,
) -> impl futures_util::Stream<Item = sqlx::Result<String>> + '_ {
    sqlx::query_scalar::<_, String>(
        "SELECT media_json FROM outbox WHERE media_json IS NOT NULL AND media_json <> '' AND media_json <> '[]'",
    )
    .fetch(db)
}

/// Update outbox entry after a failed retry attempt.
pub async fn outbox_update_retry(
    db: &Db,
    id: i64,
    attempts: i64,
    next_retry_at: i64,
    attempt_at: i64,
    last_error: &str,
) -> anyhow::Result<()> {
    // COALESCE keeps the first attempt timestamp: the 72h abandonment window
    // is a bounded retry budget, not a timer reset by every retry.
    sqlx::query(
        "UPDATE outbox SET attempts = ?, next_retry_at = ?, first_attempt_at = COALESCE(first_attempt_at, ?), last_error = ? WHERE id = ?",
    )
    .bind(attempts)
    .bind(next_retry_at)
    .bind(attempt_at)
    .bind(last_error)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// Remove an outbox entry (after successful send or abandonment).
pub async fn outbox_delete(db: &Db, id: i64) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM outbox WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}


