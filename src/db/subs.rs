//! Subscriptions, due scheduling, feed configs and retry state.

use super::*;
use super::chats::{cleanup_chat_settings_keys, cleanup_feed_settings_keys, clear_auto_disable_notified};
use sqlx::Row;

/// Feed URLs of every active subscription in an enabled chat. Used by the
/// scheduler's platform-health check to distinguish "all active sources ran"
/// from "only the one source that happens to be due this round ran".
pub async fn active_subscription_urls(db: &Db) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT s.feed_url
           FROM subscriptions s
           JOIN chats ch ON ch.chat_id = s.chat_id AND ch.enabled = 1
          WHERE s.active = 1",
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Count subscriptions per chat (for the channel list page display).
pub async fn count_subscriptions_per_chat(db: &Db) -> anyhow::Result<std::collections::HashMap<String, i64>> {
    let rows = sqlx::query("SELECT chat_id, COUNT(*) AS cnt FROM subscriptions GROUP BY chat_id")
        .fetch_all(db)
        .await?;
    let mut map = std::collections::HashMap::new();
    for r in rows {
        map.insert(r.get::<String, _>("chat_id"), r.get::<i64, _>("cnt"));
    }
    Ok(map)
}

/// Count all subscription rows for a chat (active + paused). Used by the
/// `/sub` quota check: re-activating an existing paused row must not count as
/// a new subscription, so the cap applies to rows, not active feeds.
pub async fn count_subscriptions_for_chat(db: &Db, chat_id: &str) -> anyhow::Result<i64> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM subscriptions WHERE chat_id = ?")
        .bind(chat_id)
        .fetch_one(db)
        .await?;
    Ok(n)
}

/// All subscription URLs for a chat (active + paused); quota pre-checks use
/// the set to distinguish new URLs from re-enabled ones.
pub async fn subscription_urls_for_chat(db: &Db, chat_id: &str) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query("SELECT feed_url FROM subscriptions WHERE chat_id = ?")
        .bind(chat_id)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(|r| r.get::<String, _>("feed_url")).collect())
}

/// All subscription URLs across the channels owned by `user_id`
/// (`chats.added_by`). Drives the per-user total / platform-source caps.
pub async fn subscription_urls_for_user_owner(db: &Db, user_id: i64) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query(
        r#"SELECT s.feed_url
           FROM subscriptions s
           JOIN chats ch ON ch.chat_id = s.chat_id
           WHERE ch.added_by = ?"#,
    )
    .bind(user_id.to_string())
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|r| r.get::<String, _>("feed_url")).collect())
}

pub async fn list_chats(db: &Db, user_id: i64) -> anyhow::Result<Vec<ChatRow>> {
    // Pure ownership isolation: everyone (including admins) only sees the
    // channels whose added_by is themselves.
    let rows = sqlx::query_as::<_, ChatRow>(
        "SELECT chat_id, chat_type, title, username, added_by, enabled FROM chats WHERE enabled = 1 AND added_by = ? ORDER BY title",
    )
    .bind(user_id.to_string())
    .fetch_all(db)
    .await?;
    Ok(rows)
}


/// Insert (or re-activate) a subscription and mark its current entries as
/// sent in one transaction. /sub uses this so a failure can neither leave a
/// partially-marked subscription nor roll back a pre-existing (re-enabled)
/// row: the previous two-step add + mark + delete-on-error flow deleted the
/// user's original subscription when the mark step failed.
pub async fn add_subscription_with_sent(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    feed_title: Option<&str>,
    entry_ids: &[String],
) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    // /sub resolves the target chat and fetches the feed before reaching this
    // call; the bot can be removed from the chat in that window, and purge_chat
    // then deletes the chats row. An orphan subscription would be invisible:
    // the due query INNER JOINs chats, and ownership resolution reads chats
    // too, so no command could list, edit or unsubscribe it. Re-check inside
    // the transaction and fail the add instead of persisting the orphan.
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
    // Re-enabling through /sub must clear the auto-disable notification edge
    // flag, exactly like the panel resume paths. Otherwise a later failure
    // that reaches the threshold stays silent because the stale flag remains.
    clear_auto_disable_notified(&mut tx, chat_id, feed_url).await?;
    tx.commit().await?;
    Ok(())
}

/// Bounded sent-entry snapshot for platform enrichment.
///
/// `sent_entries` is never pruned while a subscription exists, so loading the
/// full history for Weibo detail/vote suppression grows with the source's age.
/// The snapshot is only a hint: a missing old entry costs one extra detail
/// request and is still filtered by the exact window query below.
pub const SENT_ENTRY_LOOKBACK_DAYS: i64 = 180;
/// Row cap for the bounded snapshot (most recent first).
pub const SENT_ENTRY_LOOKBACK_ROWS: i64 = 5000;

/// Recent sent-entry ids for one subscription (bounded by age and row count).
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

/// Sent-entry ids from the current feed window (chunked `IN` query).
///
/// Uses the `UNIQUE(chat_id, feed_url, entry_id)` index so only matching rows
/// are read; the table's full history is never scanned. Chunking keeps the
/// bound count below SQLite's variable limit.
pub async fn sent_entry_ids_in_window(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    ids: &[String],
) -> anyhow::Result<std::collections::HashSet<String>> {
    const CHUNK: usize = 300;
    let mut found = std::collections::HashSet::new();
    for chunk in ids.chunks(CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(",");
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

/// Batch form of the per-entry outbox check for one subscription. The fetch
/// cycle skips entries the outbox already owns (the outbox has sole retry
/// authority). Callers must fail closed: a query error means the outbox state
/// is unknown, so the feed is deferred to the next round instead of treating
/// the set as empty.
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

/// Ask both dedup tables about one entry in a single query. The scheduler
/// rechecks this immediately before sending: the batch snapshots taken before
/// the fetch can go stale while an overlapping outbox retry finishes the same
/// entry, and a duplicate push is worse than one extra indexed lookup.
/// Returns (already_sent, queued_in_outbox). Callers must fail closed on Err.
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
    Ok((row.get::<i64, _>("sent") != 0, row.get::<i64, _>("queued") != 0))
}

pub async fn delete_subscription(db: &Db, sub_id: i64) -> anyhow::Result<()> {
    // Drop pending outbox retries for this subscription first (while the
    // subscriptions row still exists so the correlated lookup works).
    // Wrapped in a transaction: outbox cleanup and subscription deletion are
    // atomic, avoiding intermediate states.
    let mut tx = db.begin().await?;
    sqlx::query(
        "DELETE FROM outbox WHERE EXISTS (SELECT 1 FROM subscriptions s WHERE s.id = ? AND s.chat_id = outbox.chat_id AND s.feed_url = outbox.feed_url)",
    )
    .bind(sub_id)
    .execute(&mut *tx)
    .await?;
    // Cascade-clean the per-feed settings keys (media_dl_fail /
    // auto_disable_notified / stale_zero) while the subscriptions row still
    // exists so the correlated lookups work.
    cleanup_feed_settings_keys(&mut tx, sub_id).await?;
    // Drop per-feed config overrides in the same transaction: a later
    // re-subscribe must start from channel defaults instead of reviving stale
    // interval/content/filter settings (previously this ran outside the
    // transaction and a failure silently kept the old row).
    sqlx::query(
        "DELETE FROM feed_configs WHERE EXISTS (SELECT 1 FROM subscriptions s WHERE s.id = ? AND s.chat_id = feed_configs.chat_id AND s.feed_url = feed_configs.feed_url)",
    )
    .bind(sub_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM subscriptions WHERE id = ?").bind(sub_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn delete_all_subscriptions(db: &Db, chat_id: &str) -> anyhow::Result<u64> {
    // Drop pending outbox retries for this chat (before subscriptions vanish).
    // Wrapped in a transaction: outbox cleanup and subscription deletion are
    // atomic, avoiding intermediate states.
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM outbox WHERE chat_id = ?")
        .bind(chat_id)
        .execute(&mut *tx)
        .await?;
    // Cascade-clean every per-chat settings key family (same list as
    // purge_chat via the shared helper).
    cleanup_chat_settings_keys(&mut tx, chat_id).await?;
    // Drop per-feed config overrides for this chat as well (same contract as
    // single unsubscribe: a later re-subscribe starts from defaults).
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

/// Column list shared by every `SubRow` query (keep in sync with the struct).
const SUB_COLUMNS: &str = "id, chat_id, feed_url, feed_title, title_overridden, original_title, active, error_count, last_fetch_at, last_error, http_etag, http_last_modified, auto_disabled, disabled_at";

pub async fn get_subscriptions(db: &Db, chat_id: &str) -> anyhow::Result<Vec<SubRow>> {
    let sql = format!(
        "SELECT {SUB_COLUMNS} FROM subscriptions WHERE chat_id = ? ORDER BY id DESC"
    );
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

pub async fn get_subscription_by_url(db: &Db, chat_id: &str, feed_url: &str) -> anyhow::Result<Option<SubRow>> {
    let sql = format!("SELECT {SUB_COLUMNS} FROM subscriptions WHERE chat_id = ? AND feed_url = ?");
    let row = sqlx::query_as::<_, SubRow>(sqlx::AssertSqlSafe(sql))
    .bind(chat_id)
    .bind(feed_url)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Rename a subscription manually. Sets the override flag so the scheduler
/// stops auto-syncing the feed title.
///
/// Smart reset (v1.3.3): if the new title equals the recorded original title,
/// the override is cleared (title_overridden=0, original_title=NULL) — the
/// source resumes following feed title updates and the "original name" display
/// disappears. This makes "rename back to original" a true reset.
///
/// NOTE: SET clause order matters — SQLite evaluates left-to-right and later
/// expressions see already-updated values. original_title must be computed
/// before feed_title is overwritten.
pub async fn rename_subscription(db: &Db, sub_id: i64, new_title: &str) -> anyhow::Result<()> {
    sqlx::query(
        // `IS` (not `=`) so a NULL original_title is compared correctly; the
        // right-hand expressions all read the pre-update row, matching the
        // single-statement semantics.
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

/// Auto-update feed title from RSS without setting title_overridden.
pub async fn rename_subscription_auto(db: &Db, sub_id: i64, new_title: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE subscriptions SET feed_title = ? WHERE id = ? AND title_overridden = 0")
        .bind(new_title)
        .bind(sub_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Effective config for one subscription (chat defaults + feed overrides).
pub async fn get_effective_config(db: &Db, chat_id: &str, feed_url: &str) -> anyhow::Result<EffectiveConfig> {
    // Build from a one-row base so a missing chat_configs row does not hide
    // feed_configs overrides. This is the same precedence as the due query
    // below: feed override > channel config > DEFAULT_*.
    let row = sqlx::query(
        r#"SELECT ?1 AS chat_id, ?2 AS feed_url,
                  COALESCE(f.interval_minutes, c.interval_minutes, 30) AS interval_minutes,
                  COALESCE(f.content_mode, c.content_mode, 'title_link') AS content_mode,
                  COALESCE(f.source_display, c.source_display, 'source') AS source_display,
                  COALESCE(f.caption_above_media, c.caption_above_media, 0) AS caption_above_media,
                  COALESCE(f.link_preview, c.link_preview, 1) AS link_preview,
                  f.interval_minutes IS NOT NULL AS iv_overridden,
                  f.content_mode IS NOT NULL AS cm_overridden,
                  f.source_display IS NOT NULL AS sd_overridden,
                  f.caption_above_media IS NOT NULL AS cp_overridden,
                  f.link_preview IS NOT NULL AS lp_overridden
           FROM (SELECT 1) AS base
           LEFT JOIN chat_configs c ON c.chat_id = ?1
           LEFT JOIN feed_configs f ON f.chat_id = ?1 AND f.feed_url = ?2"#,
    )
    .bind(chat_id)
    .bind(feed_url)
    .fetch_optional(db)
    .await?;
    // Defensive: the base row means a row should always exist, but keep a
    // defaults fallback if SQLite behavior changes.
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

/// Manual pause/resume (bot button). Manual operations set auto_disabled=0,
/// guaranteeing that a source the user actively stopped is never auto-restored
/// by the self-heal job.
///
/// Resuming (active=true) also mirrors the auto-heal recovery: it resets the
/// failure counter and clears the auto-disable notification dedup flag, so a
/// source the user manually resumes starts from a clean slate. Without this,
/// a manual resume left the stale error_count in place, so the very next
/// failure could immediately re-trigger auto-disable — and because the
/// notified flag was still set, that re-disable was silent (the user perceives
/// the resume button as "not working"). Pausing leaves the counters untouched.
/// Returns false when the (id, chat_id) pair no longer exists. The chat_id
/// condition closes the TOCTOU window between the callback ownership guard and
/// this update: an id recreated for another chat can never be toggled here.
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
        // Clear the per-feed auto-disable notification dedup flag (exact key
        // match, same shape as the unsubscribe cascade) so the next genuine
        // disable can notify again.
        sqlx::query(
            "DELETE FROM settings WHERE key = (SELECT 'auto_disable_notified:' || chat_id || ':' || feed_url FROM subscriptions WHERE id = ? AND chat_id = ?)",
        )
        .bind(sub_id)
        .bind(chat_id)
        .execute(&mut *tx)
        .await?;
        // A manual resume is a fresh start for send failures too; otherwise a
        // single new failure would immediately hit the threshold again.
        sqlx::query(
            "DELETE FROM settings WHERE key = (SELECT 'send_fail:' || chat_id || ':' || feed_url FROM subscriptions WHERE id = ? AND chat_id = ?)",
        )
        .bind(sub_id)
        .bind(chat_id)
        .execute(&mut *tx)
        .await?;
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

/// System auto-disable (consecutive failures reached the threshold): sets
/// auto_disabled=1 and records the disable moment so the self-heal job can
/// auto-recover it after the cooldown.
/// The `active = 1` guard closes the race against a manual pause made during
/// the fetch round: the scheduler decides from a round-start snapshot, so
/// without the guard a just-paused (auto_disabled=0) source would be flipped
/// to auto-disabled and resurrected by the self-heal cooldown, overriding the
/// user's intent. Returns whether a row was actually disabled (false = the
/// user already paused it this round, caller should skip the notification).
pub async fn auto_disable_subscription(db: &Db, sub_id: i64) -> anyhow::Result<bool> {
    let res = sqlx::query(
        "UPDATE subscriptions SET active = 0, auto_disabled = 1, disabled_at = datetime('now') WHERE id = ? AND active = 1",
    )
    .bind(sub_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Self-heal: re-enable sources that are "auto-disabled and past the cooldown"
/// and reset their error counts to zero. Returns the number of recovered
/// sources. Manually paused (auto_disabled=0) sources are not affected.
pub async fn reenable_due_auto_disabled(db: &Db, cooldown_hours: i64) -> anyhow::Result<u64> {
    let mut tx = db.begin().await?;
    // A source retired by repeated send failures must start with a fresh
    // streak; keeping the old count would make its first post-heal failure
    // re-disable it immediately.
    sqlx::query(
        r#"DELETE FROM settings WHERE key IN (
             SELECT 'send_fail:' || chat_id || ':' || feed_url FROM subscriptions
             WHERE active = 0 AND auto_disabled = 1
               AND disabled_at IS NOT NULL
               AND (julianday('now') - julianday(disabled_at)) * 24 >= ?
           )"#,
    )
    .bind(cooldown_hours)
    .execute(&mut *tx)
    .await?;
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

/// One-click enable all disabled sources under a channel (manual operation):
/// resets error counts and auto-disable flags. Returns the number of enabled
/// sources.
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
    // Clear the per-feed auto-disable notification dedup flags so a source
    // that fails again before any success can notify the owner (same cleanup
    // as the single-subscription resume path).
    sqlx::query("DELETE FROM settings WHERE key LIKE ?")
        .bind(format!("auto_disable_notified:{}:%", chat_id))
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM settings WHERE key LIKE ?")
        .bind(format!("send_fail:{}:%", chat_id))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(res.rows_affected())
}

pub async fn increment_error_count(db: &Db, sub_id: i64, error_msg: &str) -> anyhow::Result<i64> {
    // Wrap UPDATE+SELECT in a transaction: guarantees we read the value after
    // this increment. fetch_optional: the subscription may have been deleted
    // during fetch, in which case return 0 instead of erroring.
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

/// Flat row combining subscription + effective-config columns for
/// `get_due_subscriptions` (avoids manual per-column `row.get` mapping).
#[derive(Debug, sqlx::FromRow)]
struct DueRow {
    #[sqlx(flatten)]
    sub: SubRow,
    #[sqlx(flatten)]
    cfg: EffectiveConfig,
}

/// Maximum subscriptions processed in one fetch cycle. The rest stay due and
/// are picked up by the next cycle (ordered oldest-first), which prevents a
/// backlog of thousands of due rows from monopolizing memory and upstream
/// connections in one round.
pub const MAX_DUE_PER_CYCLE: i64 = 200;

/// Count all subscriptions that are currently due (unbounded). Used only for
/// the cycle summary log (`due_total` vs `selected`).
pub async fn count_due_subscriptions(db: &Db) -> anyhow::Result<i64> {
    let (n,): (i64,) = sqlx::query_as(
        r#"SELECT COUNT(*)
           FROM subscriptions s
           LEFT JOIN chat_configs c ON c.chat_id = s.chat_id
           LEFT JOIN feed_configs f ON f.chat_id = s.chat_id AND f.feed_url = s.feed_url
           JOIN chats ch ON ch.chat_id = s.chat_id AND ch.enabled = 1
           WHERE s.active = 1
             AND (s.last_fetch_at IS NULL OR (julianday('now') - julianday(s.last_fetch_at)) * 1440 >= COALESCE(f.interval_minutes, c.interval_minutes, 30))"#,
    )
    .fetch_one(db)
    .await?;
    Ok(n)
}

/// Get due subscriptions for one fetch cycle (at most MAX_DUE_PER_CYCLE).
/// Ordering: never-fetched sources first, then oldest `last_fetch_at`, then id
/// for a stable tie-break. Sources beyond the cap remain due and rotate in on
/// later cycles instead of starving.
pub async fn get_due_subscriptions(db: &Db) -> anyhow::Result<Vec<(SubRow, EffectiveConfig)>> {
    // The COALESCE fallback defaults must stay in sync with the DEFAULT_*
    // constants (asserted by test_default_constants).
    let rows = sqlx::query_as::<_, DueRow>(
        r#"SELECT s.id, s.chat_id, s.feed_url, s.feed_title, s.title_overridden, s.original_title, s.active, s.error_count, s.last_fetch_at, s.last_error,
                  s.http_etag, s.http_last_modified, s.auto_disabled, s.disabled_at,
                  COALESCE(f.interval_minutes, c.interval_minutes, 30) AS interval_minutes,
                  COALESCE(f.content_mode, c.content_mode, 'title_link') AS content_mode,
                  COALESCE(f.source_display, c.source_display, 'source') AS source_display,
                  COALESCE(f.caption_above_media, c.caption_above_media, 0) AS caption_above_media,
                  COALESCE(f.link_preview, c.link_preview, 1) AS link_preview,
                  f.interval_minutes IS NOT NULL AS iv_overridden,
                  f.content_mode IS NOT NULL AS cm_overridden,
                  f.source_display IS NOT NULL AS sd_overridden,
                  f.caption_above_media IS NOT NULL AS cp_overridden,
                  f.link_preview IS NOT NULL AS lp_overridden
           FROM subscriptions s
           LEFT JOIN chat_configs c ON c.chat_id = s.chat_id
           LEFT JOIN feed_configs f ON f.chat_id = s.chat_id AND f.feed_url = s.feed_url
           JOIN chats ch ON ch.chat_id = s.chat_id AND ch.enabled = 1
           WHERE s.active = 1
             AND (s.last_fetch_at IS NULL OR (julianday('now') - julianday(s.last_fetch_at)) * 1440 >= COALESCE(f.interval_minutes, c.interval_minutes, 30))
           ORDER BY (s.last_fetch_at IS NOT NULL) ASC, s.last_fetch_at ASC, s.id ASC
           LIMIT ?"#,
    )
    .bind(MAX_DUE_PER_CYCLE)
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(|r| (r.sub, r.cfg)).collect())
}

/// Save the HTTP negotiation headers (ETag / Last-Modified) of this fetch
/// response for the next conditional request; passing None means the response
/// did not carry that header (keep the old value, do not overwrite).
pub async fn update_fetch_headers(
    db: &Db,
    sub_id: i64,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> anyhow::Result<()> {
    if etag.is_none() && last_modified.is_none() {
        return Ok(());
    }
    // One statement: committing the two negotiation headers separately could
    // pair this response's ETag with the previous Last-Modified on a partial
    // failure and make the next conditional request inconsistent.
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

