//! Chats, ownership and chat lifecycle storage.

use super::*;

/// Classify a Telegram chat for the DB chat_type column.
/// Accepts the three predicates so it works with both `Chat` and `ChatFullInfo`.
/// Defined in db.rs (batch A3: breaks the callbacks→commands reverse
/// dependency edge); commands keeps a re-export to preserve existing call paths.
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
    // Re-adding the bot is a fresh start for send health; otherwise the old
    // failure streak would disable the chat again on the next single failure.
    sqlx::query("DELETE FROM settings WHERE key LIKE ?")
        .bind(format!("send_fail:{}:%", chat_id))
        .execute(db)
        .await?;
    Ok(())
}

/// Cascade-clean every per-chat settings key family (media_dl_fail retries,
/// auto_disable_notified edge flags, stale_zero streak counters) for one
/// chat. Single home for the chat-wide key list: purge_chat once missed a
/// key because the list was hand-copied across cleanup sites.
pub(super) async fn cleanup_chat_settings_keys(
    conn: &mut sqlx::SqliteConnection,
    chat_id: &str,
) -> anyhow::Result<()> {
    for prefix in ["media_dl_fail", "auto_disable_notified", "stale_zero", "send_fail"] {
        sqlx::query("DELETE FROM settings WHERE key LIKE ?")
            .bind(format!("{prefix}:{chat_id}:%"))
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Cascade-clean every per-feed settings key for the subscription identified
/// by `sub_id` (the subscriptions row must still exist when this runs).
/// media_dl_fail keys carry a length-prefixed URL and are matched with
/// `instr` instead of LIKE: feed URLs often contain `%` / `_`, which LIKE
/// would treat as wildcards and wrongly match adjacent sources.
pub(super) async fn cleanup_feed_settings_keys(
    conn: &mut sqlx::SqliteConnection,
    sub_id: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        "DELETE FROM settings WHERE instr(key, (SELECT 'media_dl_fail:' || chat_id || ':' || length(feed_url) || ':' || feed_url || ':' FROM subscriptions WHERE id = ?)) = 1",
    )
    .bind(sub_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "DELETE FROM settings WHERE key = (SELECT 'auto_disable_notified:' || chat_id || ':' || feed_url FROM subscriptions WHERE id = ?)",
    )
    .bind(sub_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "DELETE FROM settings WHERE key = (SELECT 'stale_zero:' || chat_id || ':' || feed_url FROM subscriptions WHERE id = ?)",
    )
    .bind(sub_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "DELETE FROM settings WHERE key = (SELECT 'send_fail:' || chat_id || ':' || feed_url FROM subscriptions WHERE id = ?)",
    )
    .bind(sub_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Clear the auto-disable notification edge flag for one subscription.
/// Re-enabling a source (via /sub or the panel resume paths) must reset it,
/// otherwise a later failure that reaches the threshold stays silent.
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

/// Migrate every record keyed by a chat id when Telegram upgrades a group to a
/// supergroup. The old group keeps its id in the notification payload; the new
/// supergroup has a different id and the bot is moved automatically. Without
/// this, subscriptions remain bound to the dead old id and every push fails.
///
/// Idempotent: rows already under the new id are kept; duplicates created by an
/// out-of-order `my_chat_member` update are ignored. The old chat row wins over
/// a provisional new row (owner/title), while child rows are merged with the
/// new id winning on unique-key conflicts.
pub async fn migrate_chat_id(db: &Db, old_chat_id: &str, new_chat_id: &str) -> anyhow::Result<()> {
    if old_chat_id == new_chat_id {
        return Ok(());
    }
    let mut tx = db.begin().await?;

    // Chat row: preserve the existing registration (added_by/title) and drop a
    // provisional row for the new id if one already exists. If the old row is
    // missing (legacy data), leave the new row untouched.
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

    // Chat-level config: the old row carries the user's settings; a
    // provisional new row (if any) only has defaults. Guard on the old row
    // existing: an idempotent repeat of this migration (the same service
    // message delivered twice) must not delete the already-migrated config.
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

    // Tables keyed by (chat_id, ...): move and drop duplicates already under
    // the new id. The new rows win on conflict; deleting the old copy is safe
    // (it is the same logical subscription/message).
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

    // Settings keys embed the chat id for per-feed retry/dedup counters.
    for prefix in ["auto_disable_notified:", "stale_zero:", "media_dl_fail:"] {
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
        // Conflicts (the new key already exists) keep the new value; remove
        // the old key so it cannot linger as an orphan.
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

/// Enable/disable a chat without deleting its subscriptions or history.
/// Disabled chats are excluded from scheduling and outbox retries; the owner
/// is preserved so a later re-add keeps the original ownership.
pub async fn set_chat_enabled(db: &Db, chat_id: &str, enabled: bool) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE chats SET enabled = ?, updated_at = datetime('now') WHERE chat_id = ?",
    )
    .bind(if enabled { 1 } else { 0 })
    .bind(chat_id)
    .execute(db)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Whether `user_id` can manage `chat_id`. No admin bypass: data always belongs
/// to the individual who added the bot. Unregistered or disabled chats → false.
pub async fn can_manage_chat(db: &Db, user_id: i64, chat_id: &str) -> anyhow::Result<bool> {
    let Some(chat) = get_chat(db, chat_id).await? else {
        return Ok(false);
    };
    Ok(chat.enabled == 1 && chat.added_by.as_deref() == Some(&user_id.to_string()))
}

