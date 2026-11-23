use super::*;
use sqlx::Row;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OutboxRow {
    pub id: i64,
    pub chat_id: String,
    pub feed_url: String,
    pub entry_id: String,
    pub feed_title: String,
    pub entry_json: String,
    pub media_json: String,
    pub meta_json: String,
    pub content_mode: String,
    pub source_display: String,
    pub link_preview: bool,
    pub caption_above_media: bool,
    pub author: Option<String>,
    pub attempts: i64,
    pub created_at: i64,
    pub first_attempt_at: Option<i64>,
    // Read by the retry-bookkeeping tests only; the runtime keeps it in SQL.
    #[allow(dead_code)]
    pub last_error: Option<String>,
}

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
        r#"INSERT OR IGNORE INTO outbox
             (chat_id, feed_url, entry_id, feed_title, entry_json, media_json, meta_json,
              content_mode, source_display, link_preview, caption_above_media, author,
              next_retry_at, created_at)
           SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14
            WHERE EXISTS (
              SELECT 1
                FROM chats ch
                JOIN subscriptions s ON s.chat_id = ch.chat_id AND s.feed_url = ?2
               WHERE ch.chat_id = ?1 AND ch.enabled = 1 AND s.active = 1
            )"#,
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
    Ok(result.rows_affected() > 0)
}

pub async fn outbox_due_ids(
    db: &Db,
    now_ts: i64,
    limit: i64,
    allowed_owner_ids: &[i64],
) -> anyhow::Result<Vec<i64>> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        r#"SELECT o.id
             FROM outbox o
             JOIN chats ch
               ON ch.chat_id = o.chat_id AND ch.enabled = 1
             JOIN subscriptions s
               ON s.chat_id = o.chat_id AND s.feed_url = o.feed_url AND s.active = 1
            WHERE o.next_retry_at <= "#,
    );
    qb.push_bind(now_ts).push(" AND ");
    super::push_delivery_owner_filter(&mut qb, "ch", allowed_owner_ids);
    qb.push(" ORDER BY o.next_retry_at ASC, o.id ASC LIMIT ")
        .push_bind(limit);
    qb.build_query_scalar::<i64>()
        .fetch_all(db)
        .await
        .map_err(Into::into)
}

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

pub async fn delete_outbox_for_chat(db: &Db, chat_id: &str) -> anyhow::Result<u64> {
    let result = sqlx::query("DELETE FROM outbox WHERE chat_id = ?")
        .bind(chat_id)
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

pub async fn delete_ineligible_owner_outbox_before(
    db: &Db,
    before: i64,
    allowed_owner_ids: &[i64],
) -> anyhow::Result<u64> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new("DELETE FROM outbox WHERE created_at < ");
    qb.push_bind(before)
        .push(" AND NOT EXISTS (SELECT 1 FROM chats ch WHERE ch.chat_id = outbox.chat_id AND ");
    super::push_delivery_owner_filter(&mut qb, "ch", allowed_owner_ids);
    qb.push(")");
    let result = qb.build().execute(db).await?;
    Ok(result.rows_affected())
}

pub async fn outbox_get(db: &Db, id: i64) -> anyhow::Result<Option<OutboxRow>> {
    let row = sqlx::query_as::<_, OutboxRow>(
        "SELECT id, chat_id, feed_url, entry_id, feed_title, entry_json, media_json, meta_json, content_mode, source_display, link_preview, caption_above_media, author, attempts, next_retry_at, created_at, first_attempt_at, last_error FROM outbox WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

pub fn outbox_media_json_stream(
    db: &Db,
) -> impl futures_util::Stream<Item = sqlx::Result<String>> + '_ {
    sqlx::query_scalar::<_, String>(
        "SELECT media_json FROM outbox WHERE media_json IS NOT NULL AND media_json <> '' AND media_json <> '[]'",
    )
    .fetch(db)
}

pub async fn outbox_update_retry(
    db: &Db,
    id: i64,
    attempts: i64,
    next_retry_at: i64,
    attempt_at: i64,
    last_error: &str,
) -> anyhow::Result<()> {
    // COALESCE keeps the first-attempt anchor: the 72h window is a bounded retry budget, not a per-retry timer.
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

pub async fn outbox_delete(db: &Db, id: i64) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM outbox WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn outbox_entry_ids(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
) -> anyhow::Result<std::collections::HashSet<String>> {
    let rows = sqlx::query("SELECT entry_id FROM outbox WHERE chat_id = ? AND feed_url = ?")
        .bind(chat_id)
        .bind(feed_url)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(|r| r.get("entry_id")).collect())
}
