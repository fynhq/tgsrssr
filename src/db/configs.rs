//! Chat- and feed-level delivery configuration.

use super::*;

pub async fn ensure_chat_config(db: &Db, chat_id: &str) -> anyhow::Result<()> {
    // New channels default to a 30-minute fetch interval. The immutable
    // baseline DDL (001_init.sql) still declares DEFAULT 10, so every insert
    // path must pass 30 explicitly; the value must match
    // DEFAULT_INTERVAL_MINUTES (asserted by test_default_constants).
    sqlx::query("INSERT OR IGNORE INTO chat_configs (chat_id, interval_minutes) VALUES (?, 30)")
        .bind(chat_id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn get_chat_config(db: &Db, chat_id: &str) -> anyhow::Result<Option<ChatConfigRow>> {
    let row = sqlx::query_as::<_, ChatConfigRow>(
        "SELECT chat_id, interval_minutes, content_mode, source_display, caption_above_media, link_preview FROM chat_configs WHERE chat_id = ?",
    )
    .bind(chat_id)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Write-side whitelist for enum-valued chat/feed config fields. Callback
/// data is client-controlled; an unknown value would otherwise be persisted
/// verbatim (readers fall back to defaults, so this was never a crash, but
/// dirty values lingered in the DB and were copied into outbox snapshots).
/// Canonical snake_case only — legacy camel-case spellings are still accepted
/// by readers for old rows, but writers always produce the canonical form.
fn validate_config_value(field: &str, value: &str) -> anyhow::Result<()> {
    let allowed: &[&str] = match field {
        "content_mode" => &[
            "title_link",
            "title_summary",
            "title_content",
            "title_only",
            "content_only",
            "media_only",
        ],
        "source_display" => &["source", "author", "none"],
        _ => return Ok(()),
    };
    if !allowed.contains(&value) {
        anyhow::bail!("invalid {} value: '{}'", field, value);
    }
    Ok(())
}

pub async fn set_chat_config_field(
    db: &Db,
    chat_id: &str,
    field: &str,
    value: &str,
) -> anyhow::Result<()> {
    // Callers usually opened the settings page first, but do not rely on it:
    // an UPDATE on a missing row affects zero rows and silently returns Ok,
    // and the DDL default for a new interval is still 10. Create the row with
    // the documented 30-minute default before updating the requested field.
    ensure_chat_config(db, chat_id).await?;
    // The column name is a compile-time constant, so this does not violate
    // the no-dynamic-SQL convention.
    let sql = match field {
        "interval_minutes" => "UPDATE chat_configs SET interval_minutes = ?, updated_at = datetime('now') WHERE chat_id = ?",
        "content_mode" => "UPDATE chat_configs SET content_mode = ?, updated_at = datetime('now') WHERE chat_id = ?",
        "source_display" => "UPDATE chat_configs SET source_display = ?, updated_at = datetime('now') WHERE chat_id = ?",
        "caption_above_media" => "UPDATE chat_configs SET caption_above_media = ?, updated_at = datetime('now') WHERE chat_id = ?",
        "link_preview" => "UPDATE chat_configs SET link_preview = ?, updated_at = datetime('now') WHERE chat_id = ?",
        _ => anyhow::bail!("unknown field: {}", field),
    };
    if field == "interval_minutes" {
        // Defense: ensure the value bound to an INTEGER column is a valid
        // number (callers usually validate already, but the db layer should
        // not assume so).
        let v: i64 = value
            .parse()
            .map_err(|_| anyhow::anyhow!("interval_minutes must be numeric, got '{}'", value))?;
        if !(1..=MAX_INTERVAL_MINUTES).contains(&v) {
            anyhow::bail!("interval_minutes out of range: {}", v);
        }
        sqlx::query(sql).bind(v).bind(chat_id).execute(db).await?;
    } else {
        validate_config_value(field, value)?;
        sqlx::query(sql)
            .bind(value)
            .bind(chat_id)
            .execute(db)
            .await?;
    }
    Ok(())
}

// ===== Feed configs =====

pub async fn set_feed_config_field(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    field: &str,
    value: Option<&str>,
) -> anyhow::Result<()> {
    if value.is_none() {
        // Reset to inherit: set column to NULL.
        let sql = match field {
            "interval_minutes" => "UPDATE feed_configs SET interval_minutes = NULL, updated_at = datetime('now') WHERE chat_id = ? AND feed_url = ?",
            "content_mode" => "UPDATE feed_configs SET content_mode = NULL, updated_at = datetime('now') WHERE chat_id = ? AND feed_url = ?",
            "source_display" => "UPDATE feed_configs SET source_display = NULL, updated_at = datetime('now') WHERE chat_id = ? AND feed_url = ?",
            "caption_above_media" => "UPDATE feed_configs SET caption_above_media = NULL, updated_at = datetime('now') WHERE chat_id = ? AND feed_url = ?",
            "link_preview" => "UPDATE feed_configs SET link_preview = NULL, updated_at = datetime('now') WHERE chat_id = ? AND feed_url = ?",
            _ => anyhow::bail!("unknown field: {}", field),
        };
        sqlx::query(sql)
            .bind(chat_id)
            .bind(feed_url)
            .execute(db)
            .await?;
        // If all fields are NULL, delete the row.
        // The three filter columns are NOT NULL DEFAULT (0/'blacklist'/'') and
        // are never NULL, so we must judge by "filter completely unconfigured":
        // as long as the user ever touched the filter (enabled the switch or
        // stored keywords) keep the whole row, otherwise resetting other fields
        // would silently lose the content-filter config.
        sqlx::query(
            "DELETE FROM feed_configs WHERE chat_id = ? AND feed_url = ? AND interval_minutes IS NULL AND content_mode IS NULL AND source_display IS NULL AND caption_above_media IS NULL AND link_preview IS NULL AND filter_enabled = 0 AND filter_keywords = '' AND filter_mode = 'blacklist'",
        )
        .bind(chat_id)
        .bind(feed_url)
        .execute(db)
        .await?;
    } else if let Some(v) = value {
        let sql = match field {
            "interval_minutes" => "INSERT INTO feed_configs (chat_id, feed_url, interval_minutes) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET interval_minutes = excluded.interval_minutes, updated_at = datetime('now')",
            "content_mode" => "INSERT INTO feed_configs (chat_id, feed_url, content_mode) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET content_mode = excluded.content_mode, updated_at = datetime('now')",
            "source_display" => "INSERT INTO feed_configs (chat_id, feed_url, source_display) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET source_display = excluded.source_display, updated_at = datetime('now')",
            "caption_above_media" => "INSERT INTO feed_configs (chat_id, feed_url, caption_above_media) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET caption_above_media = excluded.caption_above_media, updated_at = datetime('now')",
            "link_preview" => "INSERT INTO feed_configs (chat_id, feed_url, link_preview) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET link_preview = excluded.link_preview, updated_at = datetime('now')",
            _ => anyhow::bail!("unknown field: {}", field),
        };
        if field == "interval_minutes" {
            // Defense: ensure the value bound to an INTEGER column is a valid
            // number.
            let n: i64 = v
                .parse()
                .map_err(|_| anyhow::anyhow!("interval_minutes must be numeric, got '{}'", v))?;
            if !(1..=MAX_INTERVAL_MINUTES).contains(&n) {
                anyhow::bail!("interval_minutes out of range: {}", n);
            }
            sqlx::query(sql)
                .bind(chat_id)
                .bind(feed_url)
                .bind(n)
                .execute(db)
                .await?;
        } else {
            validate_config_value(field, v)?;
            sqlx::query(sql)
                .bind(chat_id)
                .bind(feed_url)
                .bind(v)
                .execute(db)
                .await?;
        }
    }
    Ok(())
}
