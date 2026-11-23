use super::chats::{
    cleanup_chat_settings_keys, cleanup_feed_settings_keys, clear_auto_disable_notified,
};
use super::*;
use sqlx::Row;

pub async fn active_subscription_urls(
    db: &Db,
    allowed_owner_ids: &[i64],
) -> anyhow::Result<Vec<String>> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT s.feed_url
           FROM subscriptions s
           JOIN chats ch ON ch.chat_id = s.chat_id AND ch.enabled = 1
          WHERE s.active = 1 AND ",
    );
    super::push_delivery_owner_filter(&mut qb, "ch", allowed_owner_ids);
    qb.build_query_scalar::<String>()
        .fetch_all(db)
        .await
        .map_err(Into::into)
}

pub async fn count_subscriptions_per_chat(
    db: &Db,
) -> anyhow::Result<std::collections::HashMap<String, i64>> {
    let rows = sqlx::query("SELECT chat_id, COUNT(*) AS cnt FROM subscriptions GROUP BY chat_id")
        .fetch_all(db)
        .await?;
    let mut map = std::collections::HashMap::new();
    for r in rows {
        map.insert(r.get::<String, _>("chat_id"), r.get::<i64, _>("cnt"));
    }
    Ok(map)
}

pub async fn count_subscriptions_for_chat(db: &Db, chat_id: &str) -> anyhow::Result<i64> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM subscriptions WHERE chat_id = ?")
        .bind(chat_id)
        .fetch_one(db)
        .await?;
    Ok(n)
}

pub async fn subscription_urls_for_chat(db: &Db, chat_id: &str) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query("SELECT feed_url FROM subscriptions WHERE chat_id = ?")
        .bind(chat_id)
        .fetch_all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| r.get::<String, _>("feed_url"))
        .collect())
}

pub async fn subscription_urls_for_user_owner(
    db: &Db,
    user_id: i64,
) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query(
        r#"SELECT s.feed_url
           FROM subscriptions s
           JOIN chats ch ON ch.chat_id = s.chat_id
           WHERE ch.added_by = ?"#,
    )
    .bind(user_id.to_string())
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| r.get::<String, _>("feed_url"))
        .collect())
}

pub async fn list_chats(db: &Db, user_id: i64) -> anyhow::Result<Vec<ChatRow>> {
    let rows = sqlx::query_as::<_, ChatRow>(
        "SELECT chat_id, chat_type, title, username, added_by, enabled FROM chats WHERE enabled = 1 AND added_by = ? ORDER BY title",
    )
    .bind(user_id.to_string())
    .fetch_all(db)
    .await?;
    Ok(rows)
}

pub async fn add_subscription_with_sent(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    feed_title: Option<&str>,
    entry_ids: &[String],
) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    let chat_exists: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM chats WHERE chat_id = ? AND enabled = 1")
            .bind(chat_id)
            .fetch_optional(&mut *tx)
            .await?;
    if chat_exists.is_none() {
        anyhow::bail!("chat no longer registered or is disabled (bot removed before the subscription was saved)");
    }
    sqlx::query(
        r#"INSERT INTO subscriptions (chat_id, feed_url, feed_title, title_overridden, active, error_count)
           VALUES (?1, ?2, ?3, 0, 1, 0)
           ON CONFLICT(chat_id, feed_url) DO UPDATE SET active = 1, error_count = 0, last_error = NULL,
           auto_disabled = 0, disabled_at = NULL,
           feed_title = CASE WHEN title_overridden = 1 THEN feed_title ELSE excluded.feed_title END"#,
    )
    .bind(chat_id)
    .bind(feed_url)
    .bind(feed_title)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM settings WHERE key = ?")
        .bind(setting_key::send_fail(chat_id, feed_url))
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM outbox WHERE chat_id = ? AND feed_url = ?")
        .bind(chat_id)
        .bind(feed_url)
        .execute(&mut *tx)
        .await?;
    for eid in entry_ids {
        sqlx::query(
            "INSERT OR IGNORE INTO sent_entries (chat_id, feed_url, entry_id) VALUES (?, ?, ?)",
        )
        .bind(chat_id)
        .bind(feed_url)
        .bind(eid)
        .execute(&mut *tx)
        .await?;
    }
    clear_auto_disable_notified(&mut tx, chat_id, feed_url).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn entry_dedup_state(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    entry_id: &str,
) -> anyhow::Result<(bool, bool)> {
    let row = sqlx::query(
        r#"SELECT
             EXISTS(SELECT 1 FROM sent_entries WHERE chat_id = ?1 AND feed_url = ?2 AND entry_id = ?3) AS sent,
             EXISTS(SELECT 1 FROM outbox WHERE chat_id = ?1 AND feed_url = ?2 AND entry_id = ?3) AS queued"#,
    )
    .bind(chat_id)
    .bind(feed_url)
    .bind(entry_id)
    .fetch_one(db)
    .await?;
    Ok((
        row.get::<i64, _>("sent") != 0,
        row.get::<i64, _>("queued") != 0,
    ))
}

pub async fn delete_subscription(db: &Db, sub_id: i64) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT chat_id, feed_url FROM subscriptions WHERE id = ?")
            .bind(sub_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((chat_id, feed_url)) = row else {
        tx.commit().await?;
        return Ok(());
    };
    sqlx::query("DELETE FROM outbox WHERE chat_id = ? AND feed_url = ?")
        .bind(&chat_id)
        .bind(&feed_url)
        .execute(&mut *tx)
        .await?;
    cleanup_feed_settings_keys(&mut tx, &chat_id, &feed_url).await?;
    sqlx::query("DELETE FROM feed_configs WHERE chat_id = ? AND feed_url = ?")
        .bind(&chat_id)
        .bind(&feed_url)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM subscriptions WHERE id = ?")
        .bind(sub_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn delete_all_subscriptions(db: &Db, chat_id: &str) -> anyhow::Result<u64> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM outbox WHERE chat_id = ?")
        .bind(chat_id)
        .execute(&mut *tx)
        .await?;
    cleanup_chat_settings_keys(&mut tx, chat_id).await?;
    sqlx::query("DELETE FROM feed_configs WHERE chat_id = ?")
        .bind(chat_id)
        .execute(&mut *tx)
        .await?;
    let result = sqlx::query("DELETE FROM subscriptions WHERE chat_id = ?")
        .bind(chat_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected())
}

const SUB_COLUMNS: &str = "id, chat_id, feed_url, feed_title, title_overridden, original_title, active, error_count, last_fetch_at, last_error, http_etag, http_last_modified, auto_disabled, disabled_at";

pub async fn get_subscriptions(db: &Db, chat_id: &str) -> anyhow::Result<Vec<SubRow>> {
    let sql = format!("SELECT {SUB_COLUMNS} FROM subscriptions WHERE chat_id = ? ORDER BY id DESC");
    let rows = sqlx::query_as::<_, SubRow>(sqlx::AssertSqlSafe(sql))
        .bind(chat_id)
        .fetch_all(db)
        .await?;
    Ok(rows)
}

pub async fn get_subscription_by_id(db: &Db, sub_id: i64) -> anyhow::Result<Option<SubRow>> {
    let sql = format!("SELECT {SUB_COLUMNS} FROM subscriptions WHERE id = ?");
    let row = sqlx::query_as::<_, SubRow>(sqlx::AssertSqlSafe(sql))
        .bind(sub_id)
        .fetch_optional(db)
        .await?;
    Ok(row)
}

pub async fn get_subscription_by_url(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
) -> anyhow::Result<Option<SubRow>> {
    let sql = format!("SELECT {SUB_COLUMNS} FROM subscriptions WHERE chat_id = ? AND feed_url = ?");
    let row = sqlx::query_as::<_, SubRow>(sqlx::AssertSqlSafe(sql))
        .bind(chat_id)
        .bind(feed_url)
        .fetch_optional(db)
        .await?;
    Ok(row)
}

pub async fn rename_subscription(db: &Db, sub_id: i64, new_title: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE subscriptions SET
           title_overridden = CASE WHEN ? IS original_title THEN 0 ELSE 1 END,
           original_title = CASE WHEN ? IS original_title THEN NULL ELSE COALESCE(original_title, feed_title) END,
           feed_title = ?
         WHERE id = ?",
    )
    .bind(new_title)
    .bind(new_title)
    .bind(new_title)
    .bind(sub_id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn rename_subscription_auto(db: &Db, sub_id: i64, new_title: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE subscriptions SET feed_title = ? WHERE id = ? AND title_overridden = 0")
        .bind(new_title)
        .bind(sub_id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn get_effective_config(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
) -> anyhow::Result<EffectiveConfig> {
    let sql = format!(
        r#"SELECT ?1 AS chat_id, ?2 AS feed_url,
                  {EFFECTIVE_CONFIG_COLUMNS}
           FROM (SELECT 1) AS base
           LEFT JOIN chat_configs c ON c.chat_id = ?1
           LEFT JOIN feed_configs f ON f.chat_id = ?1 AND f.feed_url = ?2"#
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(chat_id)
        .bind(feed_url)
        .fetch_optional(db)
        .await?;
    let Some(row) = row else {
        return Ok(EffectiveConfig {
            chat_id: chat_id.to_string(),
            feed_url: feed_url.to_string(),
            interval_minutes: DEFAULT_INTERVAL_MINUTES,
            content_mode: DEFAULT_CONTENT_MODE.to_string(),
            source_display: DEFAULT_SOURCE_DISPLAY.to_string(),
            caption_above_media: DEFAULT_CAPTION_ABOVE_MEDIA != 0,
            link_preview: DEFAULT_LINK_PREVIEW != 0,
            cp_overridden: false,
            iv_overridden: false,
            cm_overridden: false,
            sd_overridden: false,
            lp_overridden: false,
        });
    };
    Ok(EffectiveConfig {
        chat_id: row.get("chat_id"),
        feed_url: row.get("feed_url"),
        interval_minutes: row.get("interval_minutes"),
        content_mode: row.get("content_mode"),
        source_display: row.get("source_display"),
        caption_above_media: row.get::<i64, _>("caption_above_media") != 0,
        link_preview: row.get::<i64, _>("link_preview") != 0,
        cp_overridden: row.get::<i64, _>("cp_overridden") != 0,
        iv_overridden: row.get::<i64, _>("iv_overridden") != 0,
        cm_overridden: row.get::<i64, _>("cm_overridden") != 0,
        sd_overridden: row.get::<i64, _>("sd_overridden") != 0,
        lp_overridden: row.get::<i64, _>("lp_overridden") != 0,
    })
}

// The UPDATE must also match chat_id: closes the TOCTOU window between the callback ownership guard and this write.
pub async fn set_subscription_active(
    db: &Db,
    sub_id: i64,
    chat_id: &str,
    active: bool,
) -> anyhow::Result<bool> {
    let mut tx = db.begin().await?;
    if active {
        let res = sqlx::query(
            "UPDATE subscriptions SET active = 1, auto_disabled = 0, disabled_at = NULL, error_count = 0, last_error = NULL WHERE id = ? AND chat_id = ?",
        )
        .bind(sub_id)
        .bind(chat_id)
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() == 0 {
            tx.commit().await?;
            return Ok(false);
        }
        let feed_url: Option<String> =
            sqlx::query_scalar("SELECT feed_url FROM subscriptions WHERE id = ? AND chat_id = ?")
                .bind(sub_id)
                .bind(chat_id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(feed_url) = feed_url {
            for key in [
                setting_key::auto_disable_notified(chat_id, &feed_url),
                setting_key::send_fail(chat_id, &feed_url),
            ] {
                sqlx::query("DELETE FROM settings WHERE key = ?")
                    .bind(key)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    } else {
        let res = sqlx::query(
            "UPDATE subscriptions SET active = 0, auto_disabled = 0, disabled_at = NULL WHERE id = ? AND chat_id = ?",
        )
        .bind(sub_id)
        .bind(chat_id)
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() == 0 {
            tx.commit().await?;
            return Ok(false);
        }
    }
    tx.commit().await?;
    Ok(true)
}

// The active = 1 guard is required: without it an auto-disable can override a manual pause made during the fetch round.
pub async fn auto_disable_subscription(db: &Db, sub_id: i64) -> anyhow::Result<bool> {
    let res = sqlx::query(
        "UPDATE subscriptions SET active = 0, auto_disabled = 1, disabled_at = datetime('now') WHERE id = ? AND active = 1",
    )
    .bind(sub_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

pub async fn reenable_due_auto_disabled(db: &Db, cooldown_hours: i64) -> anyhow::Result<u64> {
    let mut tx = db.begin().await?;
    let due: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT chat_id, feed_url FROM subscriptions
           WHERE active = 0 AND auto_disabled = 1
             AND disabled_at IS NOT NULL
             AND (julianday('now') - julianday(disabled_at)) * 24 >= ?"#,
    )
    .bind(cooldown_hours)
    .fetch_all(&mut *tx)
    .await?;
    for (chat_id, feed_url) in &due {
        for key in [
            setting_key::send_fail(chat_id, feed_url),
            setting_key::auto_disable_notified(chat_id, feed_url),
        ] {
            sqlx::query("DELETE FROM settings WHERE key = ?")
                .bind(key)
                .execute(&mut *tx)
                .await?;
        }
    }
    let res = sqlx::query(
        r#"UPDATE subscriptions
           SET active = 1, auto_disabled = 0, disabled_at = NULL, error_count = 0, last_error = NULL
           WHERE active = 0 AND auto_disabled = 1
             AND disabled_at IS NOT NULL
             AND (julianday('now') - julianday(disabled_at)) * 24 >= ?"#,
    )
    .bind(cooldown_hours)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(res.rows_affected())
}

pub async fn resume_all_subscriptions(db: &Db, chat_id: &str) -> anyhow::Result<u64> {
    let mut tx = db.begin().await?;
    let res = sqlx::query(
        r#"UPDATE subscriptions
           SET active = 1, auto_disabled = 0, disabled_at = NULL, error_count = 0, last_error = NULL
           WHERE chat_id = ? AND active = 0"#,
    )
    .bind(chat_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM settings WHERE key LIKE ?")
        .bind(format!(
            "{}{}:%",
            setting_key::AUTO_DISABLE_NOTIFIED_PREFIX,
            chat_id
        ))
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM settings WHERE key LIKE ?")
        .bind(format!("{}{}:%", setting_key::SEND_FAIL_PREFIX, chat_id))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(res.rows_affected())
}

pub async fn increment_error_count(db: &Db, sub_id: i64, error_msg: &str) -> anyhow::Result<i64> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "UPDATE subscriptions SET error_count = error_count + 1, last_error = ?, last_fetch_at = datetime('now') WHERE id = ?",
    )
    .bind(error_msg)
    .bind(sub_id)
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query("SELECT error_count FROM subscriptions WHERE id = ?")
        .bind(sub_id)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(row.map(|r| r.get::<i64, _>("error_count")).unwrap_or(0))
}

pub async fn reset_error_count(db: &Db, sub_id: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE subscriptions SET error_count = 0, last_error = NULL, last_fetch_at = datetime('now') WHERE id = ?")
        .bind(sub_id)
        .execute(db)
        .await?;
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct DueRow {
    due_total: i64,
    #[sqlx(flatten)]
    sub: SubRow,
    #[sqlx(flatten)]
    cfg: EffectiveConfig,
}

pub const MAX_DUE_PER_CYCLE: i64 = 200;

pub async fn get_due_subscriptions(
    db: &Db,
    allowed_owner_ids: &[i64],
) -> anyhow::Result<(i64, Vec<(SubRow, EffectiveConfig)>)> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
        r#"SELECT COUNT(*) OVER() AS due_total,
                  s.id, s.chat_id, s.feed_url, s.feed_title, s.title_overridden, s.original_title, s.active, s.error_count, s.last_fetch_at, s.last_error,
                  s.http_etag, s.http_last_modified, s.auto_disabled, s.disabled_at,
                  {EFFECTIVE_CONFIG_COLUMNS}
           FROM subscriptions s
           LEFT JOIN chat_configs c ON c.chat_id = s.chat_id
           LEFT JOIN feed_configs f ON f.chat_id = s.chat_id AND f.feed_url = s.feed_url
           JOIN chats ch ON ch.chat_id = s.chat_id AND ch.enabled = 1
           WHERE s.active = 1 AND "#
    ));
    super::push_delivery_owner_filter(&mut qb, "ch", allowed_owner_ids);
    qb.push(
        r#" AND (s.last_fetch_at IS NULL OR (julianday('now') - julianday(s.last_fetch_at)) * 1440 >= COALESCE(f.interval_minutes, c.interval_minutes, 30))
           ORDER BY (s.last_fetch_at IS NOT NULL) ASC, s.last_fetch_at ASC, s.id ASC
           LIMIT "#,
    );
    qb.push_bind(MAX_DUE_PER_CYCLE);
    let rows = qb.build_query_as::<DueRow>().fetch_all(db).await?;

    let total = rows.first().map(|r| r.due_total).unwrap_or(0);
    Ok((total, rows.into_iter().map(|r| (r.sub, r.cfg)).collect()))
}

pub async fn update_fetch_headers(
    db: &Db,
    sub_id: i64,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> anyhow::Result<()> {
    if etag.is_none() && last_modified.is_none() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE subscriptions
            SET http_etag = COALESCE(?, http_etag),
                http_last_modified = COALESCE(?, http_last_modified)
          WHERE id = ?",
    )
    .bind(etag)
    .bind(last_modified)
    .bind(sub_id)
    .execute(db)
    .await?;
    Ok(())
}
