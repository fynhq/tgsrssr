use super::*;

pub fn chat_type_str(is_channel: bool, is_group: bool, is_supergroup: bool) -> &'static str {
    if is_channel {
        "channel"
    } else if is_group || is_supergroup {
        "group"
    } else {
        "private"
    }
}

pub async fn upsert_chat(
    db: &Db,
    chat_id: &str,
    chat_type: &str,
    title: Option<&str>,
    username: Option<&str>,
    added_by: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO chats (chat_id, chat_type, title, username, added_by, enabled, last_seen_at)
           VALUES (?1, ?2, ?3, ?4, ?5, 1, datetime('now'))
           ON CONFLICT(chat_id) DO UPDATE SET
             chat_type = excluded.chat_type,
             title = COALESCE(excluded.title, chats.title),
             username = COALESCE(excluded.username, chats.username),
             added_by = COALESCE(chats.added_by, excluded.added_by),
             enabled = 1,
             last_seen_at = datetime('now'),
             updated_at = datetime('now')"#,
    )
    .bind(chat_id)
    .bind(chat_type)
    .bind(title)
    .bind(username)
    .bind(added_by)
    .execute(db)
    .await?;
    sqlx::query("DELETE FROM settings WHERE key LIKE ?")
        .bind(format!("{}{}:%", setting_key::SEND_FAIL_PREFIX, chat_id))
        .execute(db)
        .await?;
    Ok(())
}

pub(super) async fn cleanup_chat_settings_keys(
    conn: &mut sqlx::SqliteConnection,
    chat_id: &str,
) -> anyhow::Result<()> {
    for prefix in [
        setting_key::MEDIA_DL_FAIL_PREFIX,
        setting_key::AUTO_DISABLE_NOTIFIED_PREFIX,
        setting_key::STALE_ZERO_PREFIX,
        setting_key::SEND_FAIL_PREFIX,
    ] {
        sqlx::query("DELETE FROM settings WHERE key LIKE ?")
            .bind(format!("{}{}:%", prefix, chat_id))
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

pub(super) async fn cleanup_feed_settings_keys(
    conn: &mut sqlx::SqliteConnection,
    chat_id: &str,
    feed_url: &str,
) -> anyhow::Result<()> {
    for key in [
        setting_key::auto_disable_notified(chat_id, feed_url),
        setting_key::stale_zero(chat_id, feed_url),
        setting_key::send_fail(chat_id, feed_url),
    ] {
        sqlx::query("DELETE FROM settings WHERE key = ?")
            .bind(key)
            .execute(&mut *conn)
            .await?;
    }
    sqlx::query("DELETE FROM settings WHERE instr(key, ?) = 1")
        .bind(setting_key::media_dl_fail_prefix(chat_id, feed_url))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub(super) async fn clear_auto_disable_notified(
    conn: &mut sqlx::SqliteConnection,
    chat_id: &str,
    feed_url: &str,
) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM settings WHERE key = ?")
        .bind(setting_key::auto_disable_notified(chat_id, feed_url))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub async fn migrate_chat_id(db: &Db, old_chat_id: &str, new_chat_id: &str) -> anyhow::Result<()> {
    if old_chat_id == new_chat_id {
        return Ok(());
    }
    let mut tx = db.begin().await?;

    let old_exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM chats WHERE chat_id = ?")
        .bind(old_chat_id)
        .fetch_optional(&mut *tx)
        .await?;
    if old_exists.is_some() {
        sqlx::query("DELETE FROM chats WHERE chat_id = ?")
            .bind(new_chat_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE chats SET chat_id = ? WHERE chat_id = ?")
            .bind(new_chat_id)
            .bind(old_chat_id)
            .execute(&mut *tx)
            .await?;
    }

    let old_config_exists: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM chat_configs WHERE chat_id = ?")
            .bind(old_chat_id)
            .fetch_optional(&mut *tx)
            .await?;
    if old_config_exists.is_some() {
        sqlx::query("DELETE FROM chat_configs WHERE chat_id = ?")
            .bind(new_chat_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE chat_configs SET chat_id = ? WHERE chat_id = ?")
            .bind(new_chat_id)
            .bind(old_chat_id)
            .execute(&mut *tx)
            .await?;
    }

    for table in [
        "subscriptions",
        "feed_configs",
        "sent_entries",
        "outbox",
        "sent_messages",
    ] {
        let update = format!("UPDATE OR IGNORE {table} SET chat_id = ? WHERE chat_id = ?");
        sqlx::query(sqlx::AssertSqlSafe(update))
            .bind(new_chat_id)
            .bind(old_chat_id)
            .execute(&mut *tx)
            .await?;
        let delete = format!("DELETE FROM {table} WHERE chat_id = ?");
        sqlx::query(sqlx::AssertSqlSafe(delete))
            .bind(old_chat_id)
            .execute(&mut *tx)
            .await?;
    }

    for prefix in [
        setting_key::AUTO_DISABLE_NOTIFIED_PREFIX,
        setting_key::STALE_ZERO_PREFIX,
        setting_key::MEDIA_DL_FAIL_PREFIX,
        setting_key::SEND_FAIL_PREFIX,
    ] {
        let old_prefix = format!("{}{}:", prefix, old_chat_id);
        let new_prefix = format!("{}{}:", prefix, new_chat_id);
        sqlx::query(
            "UPDATE OR IGNORE settings
             SET key = ? || substr(key, length(?) + 1)
             WHERE instr(key, ?) = 1",
        )
        .bind(&new_prefix)
        .bind(&old_prefix)
        .bind(&old_prefix)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM settings WHERE instr(key, ?) = 1")
            .bind(&old_prefix)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(())
}

pub async fn get_chat(db: &Db, chat_id: &str) -> anyhow::Result<Option<ChatRow>> {
    let row = sqlx::query_as::<_, ChatRow>(
        "SELECT chat_id, chat_type, title, username, added_by, enabled FROM chats WHERE chat_id = ?",
    )
    .bind(chat_id)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

pub async fn set_chat_enabled(db: &Db, chat_id: &str, enabled: bool) -> anyhow::Result<bool> {
    let result =
        sqlx::query("UPDATE chats SET enabled = ?, updated_at = datetime('now') WHERE chat_id = ?")
            .bind(if enabled { 1 } else { 0 })
            .bind(chat_id)
            .execute(db)
            .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn can_manage_chat(db: &Db, user_id: i64, chat_id: &str) -> anyhow::Result<bool> {
    let Some(chat) = get_chat(db, chat_id).await? else {
        return Ok(false);
    };
    Ok(chat.enabled == 1 && chat.added_by.as_deref() == Some(&user_id.to_string()))
}
