use super::*;
use sqlx::Row;

pub async fn is_entry_sent(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    entry_id: &str,
) -> anyhow::Result<bool> {
    let row = sqlx::query(
        "SELECT 1 FROM sent_entries WHERE chat_id = ? AND feed_url = ? AND entry_id = ?",
    )
    .bind(chat_id)
    .bind(feed_url)
    .bind(entry_id)
    .fetch_optional(db)
    .await?;
    Ok(row.is_some())
}

pub async fn mark_entry_sent(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    entry_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO sent_entries (chat_id, feed_url, entry_id) VALUES (?, ?, ?)",
    )
    .bind(chat_id)
    .bind(feed_url)
    .bind(entry_id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn cleanup_sent_entries(db: &Db, retention_days: i64) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"DELETE FROM sent_entries
           WHERE sent_at < datetime('now', ?)
             AND NOT EXISTS (
                 SELECT 1 FROM subscriptions s
                 WHERE s.chat_id = sent_entries.chat_id
                   AND s.feed_url = sent_entries.feed_url
             )"#,
    )
    .bind(format!("-{} days", retention_days))
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

pub async fn get_cached_file_id(db: &Db, content_hash: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT file_id FROM file_id_cache WHERE content_hash = ?")
        .bind(content_hash)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("file_id")))
}

pub async fn put_cached_file_id(db: &Db, content_hash: &str, file_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO file_id_cache (content_hash, file_id)
           VALUES (?1, ?2)
           ON CONFLICT(content_hash) DO UPDATE SET
             file_id = excluded.file_id,
             created_at = datetime('now')"#,
    )
    .bind(content_hash)
    .bind(file_id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn invalidate_cached_file_id(db: &Db, content_hash: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM file_id_cache WHERE content_hash = ?")
        .bind(content_hash)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn cleanup_file_id_cache(db: &Db, retention_days: i64) -> anyhow::Result<u64> {
    let result = sqlx::query("DELETE FROM file_id_cache WHERE created_at < datetime('now', ?)")
        .bind(format!("-{} days", retention_days))
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

pub async fn cleanup_stale_settings(
    db: &Db,
    prefix: &str,
    retention_days: i64,
) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM settings WHERE instr(key, ?) = 1 AND updated_at < datetime('now', ?)",
    )
    .bind(prefix)
    .bind(format!("-{} days", retention_days))
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

pub async fn record_sent_message(
    db: &Db,
    chat_id: &str,
    message_id: i64,
    feed_url: &str,
    entry_id: &str,
    entry_link: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO sent_messages (chat_id, message_id, feed_url, entry_id, entry_link)
           VALUES (?1, ?2, ?3, ?4, ?5)
           ON CONFLICT(chat_id, message_id) DO UPDATE SET
             feed_url = excluded.feed_url,
             entry_id = excluded.entry_id,
             entry_link = excluded.entry_link,
             created_at = datetime('now')"#,
    )
    .bind(chat_id)
    .bind(message_id)
    .bind(feed_url)
    .bind(entry_id)
    .bind(entry_link)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn trim_sent_messages(db: &Db, max_rows: i64) -> anyhow::Result<u64> {
    if max_rows <= 0 {
        return Ok(0);
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sent_messages")
        .fetch_one(db)
        .await?;
    if count <= max_rows {
        return Ok(0);
    }
    let result = sqlx::query(
        r#"DELETE FROM sent_messages
           WHERE id NOT IN (SELECT id FROM sent_messages ORDER BY id DESC LIMIT ?)"#,
    )
    .bind(max_rows)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

pub struct SentMessageRow {
    pub feed_url: String,
    pub entry_id: String,
    pub entry_link: Option<String>,
    pub created_at: String,
}

pub async fn find_sent_message(
    db: &Db,
    chat_id: &str,
    message_id: i64,
) -> anyhow::Result<Option<SentMessageRow>> {
    let row = sqlx::query(
        "SELECT feed_url, entry_id, entry_link, created_at FROM sent_messages WHERE chat_id = ? AND message_id = ?",
    )
    .bind(chat_id)
    .bind(message_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| SentMessageRow {
        feed_url: r.get("feed_url"),
        entry_id: r.get("entry_id"),
        entry_link: r.get("entry_link"),
        created_at: r.get("created_at"),
    }))
}

pub const SENT_ENTRY_LOOKBACK_DAYS: i64 = 180;
pub const SENT_ENTRY_LOOKBACK_ROWS: i64 = 5000;

pub async fn sent_entry_ids_recent(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    retention_days: i64,
    limit: i64,
) -> anyhow::Result<std::collections::HashSet<String>> {
    let rows = sqlx::query(
        "SELECT entry_id FROM sent_entries
          WHERE chat_id = ? AND feed_url = ? AND sent_at >= datetime('now', ?)
          ORDER BY sent_at DESC LIMIT ?",
    )
    .bind(chat_id)
    .bind(feed_url)
    .bind(format!("-{} days", retention_days))
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|r| r.get("entry_id")).collect())
}

pub async fn sent_entry_ids_in_window(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    ids: &[String],
) -> anyhow::Result<std::collections::HashSet<String>> {
    const CHUNK: usize = 300;
    let mut found = std::collections::HashSet::new();
    for chunk in ids.chunks(CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT entry_id FROM sent_entries
              WHERE chat_id = ? AND feed_url = ? AND entry_id IN ({placeholders})"
        );
        let mut query = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql));
        query = query.bind(chat_id).bind(feed_url);
        for id in chunk {
            query = query.bind(id);
        }
        found.extend(query.fetch_all(db).await?);
    }
    Ok(found)
}
