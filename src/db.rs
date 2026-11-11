use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

pub type Db = SqlitePool;

/// Unified constructor for settings-table key names.
///
/// Previously the various runtime keys (cookie / cookie_expired_notified /
/// platform_health / platform_streak / media_dl_fail) were hand-assembled with
/// `format!` scattered across scheduler / callbacks / platforms — a single
/// mistyped character produced no error and silently disabled the key.
/// Centralizing them here means the key formats have a single definition,
/// eliminating cross-file inconsistencies.
///
/// Since v1.11.0 the cookie and platform-health keys are global (no user_id
/// segment): cookies are configured once by admins and shared by all users;
/// platform health state was globalized accordingly. The old per-user keys
/// are cleaned up at startup by migration logic in main.rs.
pub mod setting_key {
    /// Global platform login cookie: `cookie:<platform>`
    pub fn cookie(platform: &str) -> String {
        format!("cookie:{}", platform)
    }
    /// Cookie-expired notification flag: `cookie_expired_notified:<platform>`
    pub fn cookie_expired_notified(platform: &str) -> String {
        format!("cookie_expired_notified:{}", platform)
    }
    /// Platform health state (ok/down): `platform_health:<platform>`
    pub fn platform_health(platform: &str) -> String {
        format!("platform_health:{}", platform)
    }
    /// Platform health hysteresis streak counter: `platform_streak:<platform>`
    pub fn platform_streak(platform: &str) -> String {
        format!("platform_streak:{}", platform)
    }
    /// Media-only mode download-failure retry counter:
    /// `media_dl_fail:<chat_id>:<feed_url_chars>:<feed_url>:<entry_id>`.
    /// The length prefix prevents one source's prefix cleanup from deleting
    /// another source's counter when one URL is a prefix of the other
    /// (e.g. `http://h` vs `http://h:1200/x`).
    pub fn media_dl_fail(chat_id: &str, feed_url: &str, entry_id: &str) -> String {
        format!(
            "media_dl_fail:{}:{}:{}:{}",
            chat_id,
            feed_url.chars().count(),
            feed_url,
            entry_id
        )
    }
    /// Per-feed auto-disable notification flag: `auto_disable_notified:<chat_id>:<feed_url>`
    /// Used for edge deduplication — when the same feed is disabled, later
    /// self-healed, and then fails again, no duplicate notice is sent (#9 fix).
    pub fn auto_disable_notified(chat_id: &str, feed_url: &str) -> String {
        format!("auto_disable_notified:{}:{}", chat_id, feed_url)
    }
    /// Consecutive-round counter of successful full-body fetches (HTTP 200,
    /// not 304) that parsed zero entries: `stale_zero:<chat_id>:<feed_url>`.
    /// Structure-independent silence detector: the per-platform zero-entry
    /// warns need the platform response structure to still be recognized; if
    /// a redesign makes the container keys vanish, the parser returns "valid
    /// empty" and those warns never fire. This counter catches that case after
    /// a sustained streak (warn log only, no user notification).
    pub fn stale_zero(chat_id: &str, feed_url: &str) -> String {
        format!("stale_zero:{}:{}", chat_id, feed_url)
    }
    /// One-shot owner notice flag for media that expired while an outbox row
    /// was waiting for retry: `outbox_media_lost:<outbox_id>`.
    /// Deleted on success/abandon; cleanup_stale_settings removes orphans.
    pub fn outbox_media_lost(outbox_id: i64) -> String {
        format!("outbox_media_lost:{}", outbox_id)
    }
    /// Consecutive send failures for one (chat, feed):
    /// `send_fail:<chat_id>:<feed_url>`. Cleared on the next successful send
    /// and on source resume; reaching the threshold auto-disables the source
    /// instead of letting a broken chat enqueue entries for 72 hours.
    pub fn send_fail(chat_id: &str, feed_url: &str) -> String {
        format!("send_fail:{}:{}", chat_id, feed_url)
    }
}

/// Default config values (single source of truth on the Rust side).
/// The SQL COALESCE literals must stay in sync with these (sqlx 0.9 forbids
/// dynamic SQL so they cannot be injected via format!; consistency is asserted
/// by test_default_constants_match_sql below).
pub const DEFAULT_INTERVAL_MINUTES: i64 = 30;
pub const DEFAULT_CONTENT_MODE: &str = "title_link";
pub const DEFAULT_SOURCE_DISPLAY: &str = "source";
pub const DEFAULT_CAPTION_ABOVE_MEDIA: i64 = 0;
pub const DEFAULT_LINK_PREVIEW: i64 = 1;
/// Upper bound for a per-chat/per-feed fetch interval. A forged or accidental
/// huge value would make the due comparison never true and silently stop the
/// source; one day is a generous ceiling.
pub const MAX_INTERVAL_MINUTES: i64 = 1440;

/// Row types for queries.
// Some fields (chat_type/enabled etc.) only appear in SQL
// WHERE/COALESCE and are never read on the Rust side — kept to preserve the
// complete mapping of the database schema. (Exception: added_by is read on
// the Rust side by get_chat_owner / can_manage_chat.)
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChatRow {
    pub chat_id: String,
    pub chat_type: String,
    pub title: Option<String>,
    pub username: Option<String>,
    pub added_by: Option<String>,
    pub enabled: i64,
}

// error_count/last_fetch_at/last_error/auto_disabled/disabled_at are read and
// written by SQL (due checks / error counting / self-heal cooldown) and never
// read directly on the Rust side; kept for a complete schema mapping.
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SubRow {
    pub id: i64,
    pub chat_id: String,
    pub feed_url: String,
    pub feed_title: Option<String>,
    pub title_overridden: i64,
    pub original_title: Option<String>,
    pub active: i64,
    pub error_count: i64,
    pub last_fetch_at: Option<String>,
    pub last_error: Option<String>,
    /// HTTP conditional-request negotiation headers (server replies 304 when
    /// the source has not changed, saving a fetch)
    pub http_etag: Option<String>,
    pub http_last_modified: Option<String>,
    /// 1 = system auto-disabled after consecutive failures (may auto-recover
    /// after the cooldown); 0 = manually paused (never auto-recovers)
    pub auto_disabled: i64,
    /// Moment of auto-disable (UTC); the cooldown is measured from this point
    pub disabled_at: Option<String>,
}

// chat_id/caption_above_media are not read on the Rust side (the caption
// position is consumed via the COALESCE value in EffectiveConfig); kept for a
// complete schema mapping.
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChatConfigRow {
    pub chat_id: String,
    pub interval_minutes: i64,
    pub content_mode: String,
    pub source_display: String,
    pub caption_above_media: i64,
    pub link_preview: i64,
}

/// Effective config for a subscription (after COALESCE).
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EffectiveConfig {
    pub chat_id: String,
    pub feed_url: String,
    pub interval_minutes: i64,
    pub content_mode: String,
    pub source_display: String,
    pub caption_above_media: bool,
    pub link_preview: bool,
    pub cp_overridden: bool,
    pub iv_overridden: bool,
    pub cm_overridden: bool,
    pub sd_overridden: bool,
    pub lp_overridden: bool,
}

/// Restrict a data path to owner-only permissions on Unix (0600 files,
/// 0700 directories). No-op on Windows. Failures are logged, never fatal:
/// a filesystem without permission support must not stop the bot.
pub(crate) fn restrict_permissions(path: &Path, is_dir: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if is_dir { 0o700 } else { 0o600 };
        match std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
            Ok(()) => {}
            Err(e) => tracing::warn!(
                path = %path.display(),
                mode = format!("{:o}", mode),
                error = %e,
                "failed to restrict data path permissions"
            ),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, is_dir);
    }
}

/// Normalize legacy Telegram entry ids (`<channel>/<msgid>`) to the new stable
/// `<msgid>` form in the dedup/outbox tables. Idempotent; no schema change.
/// Without this, upgrading would make every visible post look new and re-push
/// the current window. Rows that collide with an existing new id are deleted.
pub async fn normalize_telegram_entry_ids(db: &Db) -> anyhow::Result<u64> {
    let mut moved = 0u64;
    for table in ["sent_entries", "outbox", "sent_messages"] {
        let update = format!(
            "UPDATE OR IGNORE {table}
             SET entry_id = substr(entry_id, instr(entry_id, '/') + 1)
             WHERE (feed_url LIKE 'https://t.me/%' OR feed_url LIKE 'http://t.me/%')
               AND instr(entry_id, '/') > 0"
        );
        let res = sqlx::query(sqlx::AssertSqlSafe(update)).execute(db).await?;
        moved += res.rows_affected();
        let cleanup = format!(
            "DELETE FROM {table}
             WHERE (feed_url LIKE 'https://t.me/%' OR feed_url LIKE 'http://t.me/%')
               AND instr(entry_id, '/') > 0"
        );
        sqlx::query(sqlx::AssertSqlSafe(cleanup)).execute(db).await?;
    }
    Ok(moved)
}

pub async fn connect(db_path: &Path) -> anyhow::Result<Db> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
        // The data dir holds the DB (and its backups): owner-only on Unix.
        restrict_permissions(parent, true);
    }

    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .pragma("journal_size_limit", "52428800")
        // Memory bounds. SQLite's default page cache is 2 MB per connection
        // (up to 8 MB for this 4-connection pool); 512 KiB is enough for this
        // workload (a few thousand small rows) and the OS page cache still
        // serves hot pages. Temp b-trees stay on disk instead of in RAM, and
        // mmap is disabled so the DB file does not inflate process RSS.
        .pragma("cache_size", "-512")
        .pragma("temp_store", "1")
        .pragma("mmap_size", "0");

    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        // Reap idle connections so their page caches are returned between
        // command/fetch bursts; reconnect is cheap for a local SQLite file.
        .idle_timeout(Some(Duration::from_secs(60)))
        .connect_with(options)
        .await?;

    // Run migrations
    sqlx::migrate!("./migrations").run(&pool).await?;

    // DB + WAL + SHM: owner-only. SQLite (re)creates the sidecar files on
    // first write; doing this after connect+migrations covers the files that
    // exist at startup, and the backup path re-applies it for a fresh DB.
    restrict_permissions(db_path, false);
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", db_path.display(), suffix));
        if sidecar.exists() {
            restrict_permissions(&sidecar, false);
        }
    }

    Ok(pool)
}

// ===== Settings (key-value store for runtime-mutable config) =====

pub async fn get_setting(db: &Db, key: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("value")))
}

pub async fn set_setting(db: &Db, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, datetime('now'))
           ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')"#,
    )
    .bind(key)
    .bind(value)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete_setting(db: &Db, key: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM settings WHERE key = ?")
        .bind(key)
        .execute(db)
        .await?;
    Ok(())
}

/// Atomically flip a state bit in settings: writes `to` only when the current
/// value != `to`. Returns true when this call performed the flip (the caller
/// sends an edge notification based on it). Used for platform health state
/// (ok/down): when multiple sources fail concurrently only the first flip
/// succeeds, eliminating duplicate notifications (a read-check-write sequence
/// has a race window under concurrency).
pub async fn flip_setting_once(db: &Db, key: &str, to: &str) -> anyhow::Result<bool> {
    let res = sqlx::query(
        r#"INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, datetime('now'))
           ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')
           WHERE settings.value != excluded.value"#,
    )
    .bind(key)
    .bind(to)
    .execute(db)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Get all settings whose key starts with the given prefix.
/// Returns a Result: a transient DB error must not be treated as "no data"
/// (consistent with the project's fail-closed principle); the caller decides
/// explicitly (startup cleanup / cookie-loading scenarios log a warn and skip).
pub async fn get_settings_by_prefix(db: &Db, prefix: &str) -> anyhow::Result<Vec<(String, String)>> {
    // `instr` instead of LIKE: key prefixes such as `platform_health:` contain
    // `_`, which LIKE treats as a single-character wildcard.
    let rows = sqlx::query("SELECT key, value FROM settings WHERE instr(key, ?) = 1")
        .bind(prefix)
        .fetch_all(db)
        .await?;
    Ok(rows.iter()
        .map(|r| (r.get::<String, _>("key"), r.get::<String, _>("value")))
        .collect())
}

/// Get the owner (added_by) of a chat.
/// Ok(None) = chat does not exist or has no owner; Err = DB query failed
/// (callers must log it and must not silently treat it as "no owner" —
/// otherwise owner notifications would be silently lost).
pub async fn get_chat_owner(db: &Db, chat_id: &str) -> anyhow::Result<Option<i64>> {
    Ok(get_chat(db, chat_id)
        .await?
        .and_then(|c| c.added_by)
        .and_then(|s| s.parse::<i64>().ok()))
}

// ===== Chats =====

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
async fn cleanup_chat_settings_keys(
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
async fn cleanup_feed_settings_keys(
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
async fn clear_auto_disable_notified(
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

// ===== Subscriptions =====

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

/// Batch form of `is_entry_sent` for one subscription. The fetch cycle filters
/// entries against this set instead of running two queries per entry (a large
/// feed could hold tens of thousands of entries), and platform fetchers use it
/// to skip enrichment requests for already-pushed entries.
pub async fn sent_entry_ids(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
) -> anyhow::Result<std::collections::HashSet<String>> {
    let rows = sqlx::query("SELECT entry_id FROM sent_entries WHERE chat_id = ? AND feed_url = ?")
        .bind(chat_id)
        .bind(feed_url)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(|r| r.get("entry_id")).collect())
}

/// Stable 64-bit marker hash for an entry id (SHA-256 truncated to 8 bytes).
/// The fetch-cycle filter stores these instead of full id strings. A 64-bit
/// collision is vanishingly rare and would only skip that one entry; the
/// exact `entry_dedup_state` recheck before each send still uses the raw id.
pub fn entry_marker_hash(entry_id: &str) -> u64 {
    let digest = Sha256::digest(entry_id.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

/// Compact form of [`sent_entry_ids`] for the fetch-cycle filter. Rows are
/// streamed one at a time, so loading a long-lived subscription's history does
/// not build a full rows vector plus a full `String` set at once.
///
/// `wanted` optionally restricts the returned set to marker hashes the caller
/// actually needs (the current feed window). The table is still scanned row by
/// row, but memory is bounded by the feed window rather than by the
/// subscription's lifetime history: `sent_entries` rows are never pruned while
/// the subscription exists, so a decades-old source can accumulate hundreds of
/// thousands of markers.
pub async fn sent_entry_id_hashes(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    wanted: Option<&std::collections::HashSet<u64>>,
) -> anyhow::Result<std::collections::HashSet<u64>> {
    use futures_util::TryStreamExt;
    let mut rows = sqlx::query_scalar::<_, String>(
        "SELECT entry_id FROM sent_entries WHERE chat_id = ? AND feed_url = ?",
    )
    .bind(chat_id)
    .bind(feed_url)
    .fetch(db);
    let mut set = std::collections::HashSet::new();
    while let Some(id) = rows.try_next().await? {
        let hash = entry_marker_hash(&id);
        if wanted.is_none_or(|w| w.contains(&hash)) {
            set.insert(hash);
        }
    }
    Ok(set)
}

/// Batch form of the per-entry outbox check for one subscription. The fetch
/// cycle skips entries the outbox already owns (the outbox has sole retry
/// authority). Fail-open semantics: callers treat a query error as empty,
/// because the outbox re-checks is_entry_sent before sending.
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

// ===== Chat configs =====

pub async fn ensure_chat_config(db: &Db, chat_id: &str) -> anyhow::Result<()> {
    // New channels default to a 30-minute fetch interval (migration 002 has
    // already changed existing rows to 30, but the DDL DEFAULT is still 10,
    // so pass the value explicitly here to stay consistent).
    // The value must match DEFAULT_INTERVAL_MINUTES (asserted by
    // test_default_constants).
    sqlx::query(
        "INSERT OR IGNORE INTO chat_configs (chat_id, interval_minutes) VALUES (?, 30)",
    )
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
        let v: i64 = value.parse()
            .map_err(|_| anyhow::anyhow!("interval_minutes must be numeric, got '{}'", value))?;
        if !(1..=MAX_INTERVAL_MINUTES).contains(&v) {
            anyhow::bail!("interval_minutes out of range: {}", v);
        }
        sqlx::query(sql).bind(v).bind(chat_id).execute(db).await?;
    } else {
        validate_config_value(field, value)?;
        sqlx::query(sql).bind(value).bind(chat_id).execute(db).await?;
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
        sqlx::query(sql).bind(chat_id).bind(feed_url).execute(db).await?;
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
            let n: i64 = v.parse()
                .map_err(|_| anyhow::anyhow!("interval_minutes must be numeric, got '{}'", v))?;
            if !(1..=MAX_INTERVAL_MINUTES).contains(&n) {
                anyhow::bail!("interval_minutes out of range: {}", n);
            }
            sqlx::query(sql).bind(chat_id).bind(feed_url).bind(n).execute(db).await?;
        } else {
            validate_config_value(field, v)?;
            sqlx::query(sql).bind(chat_id).bind(feed_url).bind(v).execute(db).await?;
        }
    }
    Ok(())
}

// ===== Sent entries =====

pub async fn is_entry_sent(db: &Db, chat_id: &str, feed_url: &str, entry_id: &str) -> anyhow::Result<bool> {
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

pub async fn mark_entry_sent(db: &Db, chat_id: &str, feed_url: &str, entry_id: &str) -> anyhow::Result<()> {
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

// ===== Cleanup =====

pub async fn cleanup_sent_entries(db: &Db, retention_days: i64) -> anyhow::Result<u64> {
    // Time is not a safe criterion for an *existing* subscription: an entry
    // can stay inside a source's rolling window (or reappear on the next full
    // fetch) long after the retention period. Deleting its sent marker then
    // makes the bot re-push an old entry — for X feeds without conditional
    // requests this can re-post the whole visible window every retention
    // cycle. Keep rows for any subscription that still exists (active,
    // paused, or auto-disabled and waiting for self-heal); only clean up
    // true orphans whose subscription was removed. `/sub` marks the current
    // window as sent on re-subscribe, so keeping these rows cannot double
    // push either.
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

// ===== File-ID cache (upload dedup) =====

/// Cache lookup: whether a reusable Telegram file_id already exists for the
/// same content hash.
pub async fn get_cached_file_id(db: &Db, content_hash: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT file_id FROM file_id_cache WHERE content_hash = ?")
        .bind(content_hash)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("file_id")))
}

/// Cache write: record the file_id after a successful media upload
/// (refreshes the timestamp on conflict).
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

/// Cache invalidation: delete the entry when sending by file_id is rejected by
/// Telegram (bot changed / file cleaned up); the next attempt automatically
/// falls back to re-uploading.
pub async fn invalidate_cached_file_id(db: &Db, content_hash: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM file_id_cache WHERE content_hash = ?")
        .bind(content_hash)
        .execute(db)
        .await?;
    Ok(())
}

/// Clean up expired cache entries (retention in days); returns the number of
/// deleted rows.
pub async fn cleanup_file_id_cache(db: &Db, retention_days: i64) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM file_id_cache WHERE created_at < datetime('now', ?)",
    )
    .bind(format!("-{} days", retention_days))
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// Clean up stale keys with the given prefix in the settings table
/// (updated_at older than the retention period). Used for keys that can linger
/// past an entry's lifecycle (e.g. media_dl_fail: when an entry rolls out of
/// the source's sliding window before reaching the retry cap, neither the
/// download-success nor the abandon deletion point will ever fire again).
/// Prefix matching uses instr instead of LIKE (feed_url may contain the % / _
/// wildcard characters).
pub async fn cleanup_stale_settings(db: &Db, prefix: &str, retention_days: i64) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM settings WHERE instr(key, ?) = 1 AND updated_at < datetime('now', ?)",
    )
    .bind(prefix)
    .bind(format!("-{} days", retention_days))
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

// ===== Sent messages registry (/find reverse lookup) =====

/// Register one sent message: TG message → RSS feed entry. Row-count
/// trimming is done by the periodic cleanup job, not on every insert: the
/// previous COUNT(*) ran once per delivered message.
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

/// Enforce the sent-messages row cap (called from the periodic cleanup job).
/// Returns the number of rows removed.
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

/// /find reverse lookup: find the RSS entry for a channel + message id.
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


// ===== Outbox (persistent retry queue for failed sends) =====

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


// ===== Content filtering (per-feed) =====

/// Per-feed content filter config.
#[derive(Debug, Clone, Default)]
pub struct FilterConfig {
    pub enabled: bool,
    /// "blacklist" (exclude matching) or "whitelist" (only matching).
    pub mode: String,
    /// Comma-separated keywords matched against entry titles (case-insensitive).
    pub keywords: String,
}

impl FilterConfig {
    /// True when filtering should actually be applied (switch on + keywords set).
    pub fn is_active(&self) -> bool {
        self.enabled && !self.keywords.trim().is_empty()
    }

    /// Parsed keyword list (trimmed, lowercased, empty items dropped).
    pub fn keyword_list(&self) -> Vec<String> {
        // Accept the full-width Chinese comma/semicolon too: the UI prompt is
        // localized and Chinese users naturally type `，`. Treating the whole
        // string as one keyword made whitelist filtering mark every entry as
        // sent (total silence) and blacklist filtering a no-op.
        self.keywords
            .split([',', '，', ';', '；'])
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Whether an entry title passes the filter (true = should be pushed).
    /// The scheduler runs
    /// this once per entry; re-splitting/lowercasing the same string for a
    /// large feed is pure overhead.
    pub fn allows_title_with_keywords(&self, title: &str, keywords: &[String]) -> bool {
        if !self.is_active() {
            return true;
        }
        let lower = title.to_lowercase();
        let matched = keywords.iter().any(|kw| lower.contains(kw));
        match self.mode.as_str() {
            "whitelist" => matched,
            _ => !matched, // blacklist: exclude matches
        }
    }
}

/// Read the content filter config for a feed (defaults when no row exists).
pub async fn get_feed_filter(db: &Db, chat_id: &str, feed_url: &str) -> anyhow::Result<FilterConfig> {
    let row = sqlx::query(
        "SELECT filter_enabled, filter_mode, filter_keywords FROM feed_configs WHERE chat_id = ? AND feed_url = ?",
    )
    .bind(chat_id)
    .bind(feed_url)
    .fetch_optional(db)
    .await?;
    Ok(match row {
        Some(r) => FilterConfig {
            enabled: r.get::<i64, _>("filter_enabled") != 0,
            mode: r.get("filter_mode"),
            keywords: r.get("filter_keywords"),
        },
        None => FilterConfig::default(),
    })
}

/// Update one filter field (upsert; creates the feed_configs row if missing).
async fn set_feed_filter_field(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    field: &str,
    value: &str,
) -> anyhow::Result<()> {
    let sql = match field {
        "filter_enabled" => "INSERT INTO feed_configs (chat_id, feed_url, filter_enabled) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET filter_enabled = excluded.filter_enabled, updated_at = datetime('now')",
        "filter_mode" => "INSERT INTO feed_configs (chat_id, feed_url, filter_mode) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET filter_mode = excluded.filter_mode, updated_at = datetime('now')",
        "filter_keywords" => "INSERT INTO feed_configs (chat_id, feed_url, filter_keywords) VALUES (?1, ?2, ?3) ON CONFLICT(chat_id, feed_url) DO UPDATE SET filter_keywords = excluded.filter_keywords, updated_at = datetime('now')",
        _ => anyhow::bail!("unknown filter field: {}", field),
    };
    sqlx::query(sql)
        .bind(chat_id)
        .bind(feed_url)
        .bind(value)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn set_feed_filter_enabled(db: &Db, chat_id: &str, feed_url: &str, enabled: bool) -> anyhow::Result<()> {
    set_feed_filter_field(db, chat_id, feed_url, "filter_enabled", if enabled { "1" } else { "0" }).await
}

pub async fn set_feed_filter_mode(db: &Db, chat_id: &str, feed_url: &str, mode: &str) -> anyhow::Result<()> {
    set_feed_filter_field(db, chat_id, feed_url, "filter_mode", mode).await
}

pub async fn set_feed_filter_keywords(db: &Db, chat_id: &str, feed_url: &str, keywords: &str) -> anyhow::Result<()> {
    set_feed_filter_field(db, chat_id, feed_url, "filter_keywords", keywords).await
}

/// Clear both keywords and the enable switch in one transaction. "Clear"
/// in the UI means filtering is off; clearing only the keywords left
/// `filter_enabled=1`, so the panel showed "off" while the button still
/// offered "disable", and the next keyword entry silently re-enabled it.
pub async fn clear_feed_filter(db: &Db, chat_id: &str, feed_url: &str) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "UPDATE feed_configs SET filter_keywords = '', filter_enabled = 0, updated_at = datetime('now') WHERE chat_id = ? AND feed_url = ?",
    )
    .bind(chat_id)
    .bind(feed_url)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: the DEFAULT_* constants must stay in sync with the SQL
    /// COALESCE literals in get_due_subscriptions / ensure_chat_config
    /// (sqlx 0.9 forbids dynamic SQL, so they cannot be injected via format!).
    /// When changing a constant, the SQL literal must be changed too,
    /// otherwise this test fails.
    #[test]
    fn default_constants_match_sql_literals() {
        assert_eq!(DEFAULT_INTERVAL_MINUTES, 30);
        assert_eq!(DEFAULT_CONTENT_MODE, "title_link");
        assert_eq!(DEFAULT_SOURCE_DISPLAY, "source");
        assert_eq!(DEFAULT_CAPTION_ABOVE_MEDIA, 0);
        assert_eq!(DEFAULT_LINK_PREVIEW, 1);

        // Ensure the SQL COALESCE fallbacks use the same literals. The old
        // version only compared the constants with hardcoded numbers and
        // could never catch SQL drift.
        let src = include_str!("db.rs");
        for (field, value) in [
            ("interval_minutes", "30"),
            ("content_mode", "'title_link'"),
            ("source_display", "'source'"),
            ("caption_above_media", "0"),
            ("link_preview", "1"),
        ] {
            let needle = format!("COALESCE(f.{field}, c.{field}, {value})");
            assert!(src.contains(&needle), "missing SQL fallback: {needle}");
        }
    }

    #[test]
    fn filter_keywords_preparsed_match_single_shot() {
        let blacklist = FilterConfig {
            enabled: true,
            mode: "blacklist".into(),
            keywords: "foo, Bar，中文；third".into(),
        };
        assert_eq!(
            blacklist.keyword_list(),
            vec!["foo", "bar", "中文", "third"],
            "full-width comma/semicolon separators must be accepted"
        );
        let kws = blacklist.keyword_list();
        assert!(!blacklist.allows_title_with_keywords("a FOO b", &kws));
        assert!(blacklist.allows_title_with_keywords("baz", &kws));
        let whitelist = FilterConfig {
            enabled: true,
            mode: "whitelist".into(),
            keywords: "foo".into(),
        };
        let kws = whitelist.keyword_list();
        assert!(whitelist.allows_title_with_keywords("foo", &kws));
        assert!(!whitelist.allows_title_with_keywords("bar", &kws));
        assert!(FilterConfig::default().allows_title_with_keywords("anything", &[]));
    }

    async fn test_pool() -> Db {
        // Single connection: every sqlite::memory: connection would otherwise
        // see its own empty database.
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn set_subscription_active_requires_matching_chat() {
        let pool = test_pool().await;
        let res = sqlx::query(
            "INSERT INTO subscriptions (chat_id, feed_url, active, auto_disabled) VALUES ('1', 'https://example.com/feed', 0, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let sub_id = res.last_insert_rowid();

        // The callback ownership guard may have raced with a deleted/recreated
        // row: a different chat must never toggle this id.
        assert!(!set_subscription_active(&pool, sub_id, "2", true)
            .await
            .unwrap());
        let state: (i64, i64) = sqlx::query_as(
            "SELECT active, auto_disabled FROM subscriptions WHERE id = ?",
        )
        .bind(sub_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state, (0, 1));

        assert!(set_subscription_active(&pool, sub_id, "1", true)
            .await
            .unwrap());
        let state: (i64, i64) = sqlx::query_as(
            "SELECT active, auto_disabled FROM subscriptions WHERE id = ?",
        )
        .bind(sub_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state, (1, 0));
    }

    #[tokio::test]
    async fn normalize_telegram_entry_ids_strips_channel_prefix() {
        let pool = test_pool().await;
        let feed = "https://t.me/chan";
        sqlx::query(
            "INSERT INTO sent_entries (chat_id, feed_url, entry_id) VALUES ('1', ?, 'chan/9')",
        )
        .bind(feed)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO outbox (chat_id, feed_url, entry_id, entry_json, next_retry_at, created_at)
             VALUES ('1', ?, 'chan/10', '{}', 0, 0)",
        )
        .bind(feed)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sent_messages (chat_id, message_id, feed_url, entry_id) VALUES ('1', 5, ?, 'chan/11')",
        )
        .bind(feed)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(normalize_telegram_entry_ids(&pool).await.unwrap(), 3);
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT entry_id FROM sent_entries UNION ALL SELECT entry_id FROM outbox UNION ALL SELECT entry_id FROM sent_messages ORDER BY entry_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(ids, vec!["10", "11", "9"]);
        // Idempotent: a second startup must not touch anything.
        assert_eq!(normalize_telegram_entry_ids(&pool).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn effective_config_keeps_feed_override_without_chat_row() {
        let pool = test_pool().await;
        sqlx::query(
            "INSERT INTO feed_configs (chat_id, feed_url, content_mode) VALUES ('1', 'https://example.com/feed', 'content_only')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let cfg = get_effective_config(&pool, "1", "https://example.com/feed")
            .await
            .unwrap();
        assert_eq!(cfg.content_mode, "content_only");
        assert!(cfg.cm_overridden);
        assert_eq!(cfg.interval_minutes, DEFAULT_INTERVAL_MINUTES);
        assert_eq!(cfg.source_display, DEFAULT_SOURCE_DISPLAY);
    }

    #[tokio::test]
    async fn effective_config_caption_override_and_clear() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO chat_configs (chat_id, caption_above_media) VALUES ('1', 0)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO feed_configs (chat_id, feed_url, caption_above_media) VALUES ('1', 'https://example.com/feed', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let cfg = get_effective_config(&pool, "1", "https://example.com/feed")
            .await
            .unwrap();
        assert!(cfg.caption_above_media);
        assert!(cfg.cp_overridden);

        // A false override must survive COALESCE instead of falling back to the channel value.
        set_feed_config_field(&pool, "1", "https://example.com/feed", "caption_above_media", Some("0"))
            .await
            .unwrap();
        let cfg = get_effective_config(&pool, "1", "https://example.com/feed")
            .await
            .unwrap();
        assert!(!cfg.caption_above_media);
        assert!(cfg.cp_overridden);

        set_feed_config_field(&pool, "1", "https://example.com/feed", "caption_above_media", None)
            .await
            .unwrap();
        let cfg = get_effective_config(&pool, "1", "https://example.com/feed")
            .await
            .unwrap();
        assert!(!cfg.caption_above_media);
        assert!(!cfg.cp_overridden);
    }

    #[tokio::test]
    async fn due_subscriptions_flatten_effective_config() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO chats (chat_id, chat_type, enabled) VALUES ('1', 'channel', 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO subscriptions (chat_id, feed_url) VALUES ('1', 'https://example.com/feed')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO feed_configs (chat_id, feed_url, interval_minutes, content_mode) VALUES ('1', 'https://example.com/feed', 5, 'content_only')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let due = get_due_subscriptions(&pool).await.unwrap();
        assert_eq!(due.len(), 1);
        let (sub, cfg) = &due[0];
        assert_eq!(sub.chat_id, "1");
        assert_eq!(cfg.feed_url, "https://example.com/feed");
        assert_eq!(cfg.interval_minutes, 5);
        assert_eq!(cfg.content_mode, "content_only");
        assert!(cfg.iv_overridden);
        assert!(cfg.cm_overridden);
        assert!(cfg.link_preview);
    }

    #[tokio::test]
    async fn set_chat_config_field_creates_row_with_defaults() {
        let pool = test_pool().await;
        set_chat_config_field(&pool, "1", "content_mode", "content_only")
            .await
            .unwrap();
        let cfg = get_chat_config(&pool, "1").await.unwrap().expect("row created");
        assert_eq!(cfg.interval_minutes, DEFAULT_INTERVAL_MINUTES);
        assert_eq!(cfg.content_mode, "content_only");
    }

    #[tokio::test]
    async fn trim_sent_messages_keeps_newest_rows() {
        let pool = test_pool().await;
        for mid in 1..=3 {
            record_sent_message(&pool, "1", mid, "https://example.com/feed", &format!("e{mid}"), None)
                .await
                .unwrap();
        }
        assert_eq!(trim_sent_messages(&pool, 2).await.unwrap(), 1);
        let rows: Vec<i64> = sqlx::query_scalar("SELECT message_id FROM sent_messages ORDER BY message_id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(rows, vec![2, 3]);
    }

    #[tokio::test]
    async fn cleanup_sent_entries_keeps_active_subscription_rows() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO subscriptions (chat_id, feed_url) VALUES ('1', 'https://example.com/feed')")
            .execute(&pool)
            .await
            .unwrap();
        // Old row for a subscription that still exists: must be kept even
        // though it is past the retention period (re-pushing it after an
        // inactive period is a regression).
        sqlx::query(
            "INSERT INTO sent_entries (chat_id, feed_url, entry_id, sent_at)
             VALUES ('1', 'https://example.com/feed', 'old-active', datetime('now', '-200 days'))",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Old orphan (subscription removed): cleaned up.
        sqlx::query(
            "INSERT INTO sent_entries (chat_id, feed_url, entry_id, sent_at)
             VALUES ('2', 'https://example.com/gone', 'old-orphan', datetime('now', '-200 days'))",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Recent orphan: kept until the retention period expires.
        sqlx::query(
            "INSERT INTO sent_entries (chat_id, feed_url, entry_id)
             VALUES ('2', 'https://example.com/gone', 'recent-orphan')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let removed = cleanup_sent_entries(&pool, 180).await.unwrap();
        assert_eq!(removed, 1, "only the expired orphan should be removed");
        let remaining: Vec<(String, String)> = sqlx::query_as(
            "SELECT chat_id, entry_id FROM sent_entries ORDER BY entry_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            remaining,
            vec![
                ("1".to_string(), "old-active".to_string()),
                ("2".to_string(), "recent-orphan".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn vacuum_into_creates_a_sqlite_snapshot() {
        // The daily backup relies on VACUUM INTO producing a standalone DB
        // file (SQLite >= 3.27). Guard against a downgraded SQLite where the
        // statement is unavailable.
        // Use a real file-backed DB: VACUUM INTO on sqlx's shared in-memory
        // connection does not materialise the target file.
        let db_path = std::env::temp_dir().join(format!(
            "tgsrssr_vacuum_src_{}.db",
            uuid::Uuid::new_v4()
        ));
        let path = std::env::temp_dir().join(format!(
            "tgsrssr_vacuum_{}.db",
            uuid::Uuid::new_v4()
        ));
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&path);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&db_path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::query("CREATE TABLE t (x INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        let literal = path.to_string_lossy().replace('\'', "''");
        sqlx::query(sqlx::AssertSqlSafe(format!("VACUUM INTO '{}'", literal)))
            .execute(&pool)
            .await
            .expect("VACUUM INTO should be supported");
        let head = std::fs::read(&path).expect("snapshot file should exist");
        assert!(head.starts_with(b"SQLite format 3\0"), "not a SQLite database");
        pool.close().await;
        std::fs::remove_file(&db_path).ok();
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn disabled_chat_and_paused_subscription_hold_outbox_rows() {
        let pool = test_pool().await;
        sqlx::query(
            "INSERT INTO chats (chat_id, chat_type, added_by, enabled) VALUES ('1', 'channel', '42', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO subscriptions (chat_id, feed_url, active) VALUES ('1', 'https://example.com/feed', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        outbox_insert(
            &pool,
            "1",
            "https://example.com/feed",
            "e1",
            "Feed",
            "{}",
            "[]",
            "",
            "title_link",
            "source",
            true,
            false,
            None,
            0,
            0,
        )
        .await
        .unwrap();

        // Paused source: queued rows are held.
        assert!(outbox_due_ids(&pool, 1, 10).await.unwrap().is_empty());
        sqlx::query("UPDATE subscriptions SET active = 1 WHERE chat_id = '1'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(outbox_due_ids(&pool, 1, 10).await.unwrap().len(), 1);

        // Disabled chat: queued rows are held again.
        assert!(set_chat_enabled(&pool, "1", false).await.unwrap());
        assert!(outbox_due_ids(&pool, 1, 10).await.unwrap().is_empty());
        assert!(!can_manage_chat(&pool, 42, "1").await.unwrap());
        assert!(set_chat_enabled(&pool, "1", true).await.unwrap());
        assert!(can_manage_chat(&pool, 42, "1").await.unwrap());
        assert_eq!(outbox_due_ids(&pool, 1, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn outbox_retry_records_first_attempt_only() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO chats (chat_id, chat_type, enabled) VALUES ('1', 'channel', 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO subscriptions (chat_id, feed_url, active) VALUES ('1', 'https://example.com/feed', 1)")
            .execute(&pool)
            .await
            .unwrap();
        outbox_insert(
            &pool,
            "1",
            "https://example.com/feed",
            "e1",
            "Feed",
            "{}",
            "[]",
            "",
            "title_link",
            "source",
            true,
            false,
            None,
            0,
            0,
        )
        .await
        .unwrap();
        let due_ids = outbox_due_ids(&pool, 1, 10).await.unwrap();
        assert_eq!(due_ids.len(), 1);
        let row = outbox_get(&pool, due_ids[0]).await.unwrap().unwrap();
        assert_eq!(row.first_attempt_at, None);

        outbox_update_retry(&pool, row.id, 1, 5, 123, "boom")
            .await
            .unwrap();
        let due_ids = outbox_due_ids(&pool, 5, 10).await.unwrap();
        let row = outbox_get(&pool, due_ids[0]).await.unwrap().unwrap();
        assert_eq!(row.attempts, 1);
        assert_eq!(row.first_attempt_at, Some(123));
        assert_eq!(row.last_error.as_deref(), Some("boom"));

        // A later retry must not move the first-attempt anchor; otherwise the
        // 72h window would never expire under continuous failures.
        outbox_update_retry(&pool, row.id, 2, 10, 456, "still boom")
            .await
            .unwrap();
        let due_ids = outbox_due_ids(&pool, 10, 10).await.unwrap();
        let row = outbox_get(&pool, due_ids[0]).await.unwrap().unwrap();
        assert_eq!(row.attempts, 2);
        assert_eq!(row.first_attempt_at, Some(123));
        assert_eq!(row.last_error.as_deref(), Some("still boom"));

        // A removed row is reported as missing rather than an error.
        outbox_delete(&pool, row.id).await.unwrap();
        assert!(outbox_get(&pool, row.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn migrate_chat_id_moves_all_chat_scoped_data() {
        let pool = test_pool().await;
        let old = "-1001";
        let new = "-1002";
        let feed = "https://example.com/feed";

        sqlx::query("INSERT INTO chats (chat_id, chat_type, title, added_by) VALUES (?, 'group', 'old', '42')")
            .bind(old)
            .execute(&pool)
            .await
            .unwrap();
        // Provisional row from an out-of-order my_chat_member update.
        sqlx::query("INSERT INTO chats (chat_id, chat_type, title, added_by) VALUES (?, 'supergroup', 'new', '99')")
            .bind(new)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO subscriptions (chat_id, feed_url, feed_title) VALUES (?, ?, 'feed')")
            .bind(old)
            .bind(feed)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO chat_configs (chat_id, interval_minutes) VALUES (?, 5)")
            .bind(old)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO chat_configs (chat_id, interval_minutes) VALUES (?, 99)")
            .bind(new)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO feed_configs (chat_id, feed_url, content_mode) VALUES (?, ?, 'content_only')")
            .bind(old)
            .bind(feed)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO sent_entries (chat_id, feed_url, entry_id) VALUES (?, ?, 'e1')")
            .bind(old)
            .bind(feed)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO outbox (chat_id, feed_url, entry_id, feed_title, entry_json, next_retry_at, created_at)
             VALUES (?, ?, 'e2', 'feed', '{}', 0, 0)",
        )
        .bind(old)
        .bind(feed)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO sent_messages (chat_id, message_id, feed_url, entry_id) VALUES (?, 77, ?, 'e3')")
            .bind(old)
            .bind(feed)
            .execute(&pool)
            .await
            .unwrap();
        for key in [
            format!("auto_disable_notified:{}:{}", old, feed),
            format!("stale_zero:{}:{}", old, feed),
            setting_key::media_dl_fail(old, feed, "e4"),
        ] {
            set_setting(&pool, &key, "1").await.unwrap();
        }

        migrate_chat_id(&pool, old, new).await.unwrap();
        // Idempotent second call (e.g. duplicate service message).
        migrate_chat_id(&pool, old, new).await.unwrap();

        let chats: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT chat_id, added_by FROM chats ORDER BY chat_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(chats, vec![(new.to_string(), Some("42".to_string()))]);

        for (table, condition) in [
            ("subscriptions", "chat_id = ?"),
            ("feed_configs", "chat_id = ?"),
            ("sent_entries", "chat_id = ?"),
            ("outbox", "chat_id = ?"),
            ("sent_messages", "chat_id = ?"),
        ] {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE {condition}");
            let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .bind(new)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(count, 1, "{table} row not moved");
            let old_count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(
                format!("SELECT COUNT(*) FROM {table} WHERE {condition}"),
            ))
            .bind(old)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(old_count, 0, "{table} old row left behind");
        }

        let interval: i64 = sqlx::query_scalar("SELECT interval_minutes FROM chat_configs WHERE chat_id = ?")
            .bind(new)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(interval, 5, "old chat config must win over provisional defaults");

        let settings: Vec<String> = sqlx::query_scalar(
            "SELECT key FROM settings WHERE instr(key, 'auto_disable_notified:') = 1
                OR instr(key, 'stale_zero:') = 1 OR instr(key, 'media_dl_fail:') = 1 ORDER BY key",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            settings,
            vec![
                format!("auto_disable_notified:{}:{}", new, feed),
                setting_key::media_dl_fail(new, feed, "e4"),
                format!("stale_zero:{}:{}", new, feed),
            ]
        );
    }

    #[tokio::test]
    async fn outbox_media_json_stream_skips_empty_values() {
        use futures_util::TryStreamExt;
        let pool = test_pool().await;
        let item = crate::media::MediaItem {
            path: std::path::PathBuf::from("a.jpg"),
            kind: crate::media::MediaKind::Image,
            width: 1,
            height: 1,
            size: 1,
            is_gif: false,
            thumb_path: None,
            duration_secs: None,
            content_hash: "h".into(),
            is_retweet: false,
        };
        let media_json = serde_json::to_string(&vec![item]).unwrap();
        for (entry, media) in [("with-media", media_json.as_str()), ("empty", "[]")] {
            outbox_insert(
                &pool,
                "1",
                "https://example.com/feed",
                entry,
                "Feed",
                "{}",
                media,
                "",
                "title_link",
                "source",
                true,
                false,
                None,
                0,
                0,
            )
            .await
            .unwrap();
        }

        let mut stream = outbox_media_json_stream(&pool);
        let mut values = Vec::new();
        while let Some(v) = stream.try_next().await.unwrap() {
            values.push(v);
        }
        assert_eq!(values, vec![media_json]);
    }

    #[tokio::test]
    async fn entry_dedup_state_reports_sent_and_queued() {
        let pool = test_pool().await;

        mark_entry_sent(&pool, "1", "https://example.com/feed", "sent-entry")
            .await
            .unwrap();
        outbox_insert(
            &pool,
            "1",
            "https://example.com/feed",
            "queued-entry",
            "Feed",
            "{}",
            "[]",
            "",
            "title_link",
            "source",
            true,
            false,
            None,
            0,
            0,
        )
        .await
        .unwrap();

        assert_eq!(
            entry_dedup_state(&pool, "1", "https://example.com/feed", "sent-entry").await.unwrap(),
            (true, false)
        );
        assert_eq!(
            entry_dedup_state(&pool, "1", "https://example.com/feed", "queued-entry").await.unwrap(),
            (false, true)
        );
        assert_eq!(
            entry_dedup_state(&pool, "1", "https://example.com/feed", "new-entry").await.unwrap(),
            (false, false)
        );
        assert_eq!(
            entry_dedup_state(&pool, "1", "https://example.com/other", "sent-entry").await.unwrap(),
            (false, false)
        );
    }

    #[test]
    fn entry_marker_hash_is_stable_and_distinct() {
        let a = entry_marker_hash("entry-a");
        assert_eq!(a, entry_marker_hash("entry-a"));
        assert_ne!(a, entry_marker_hash("entry-b"));
        assert_ne!(entry_marker_hash(""), entry_marker_hash("x"));
    }

    #[tokio::test]
    async fn sent_entry_id_hashes_matches_sent_ids_for_the_subscription() {
        let pool = test_pool().await;
        let feed = "https://example.com/feed";
        mark_entry_sent(&pool, "1", feed, "e1").await.unwrap();
        mark_entry_sent(&pool, "1", feed, "e2").await.unwrap();
        mark_entry_sent(&pool, "2", feed, "other-chat").await.unwrap();
        mark_entry_sent(&pool, "1", "https://example.com/other", "other-feed")
            .await
            .unwrap();

        let ids = sent_entry_ids(&pool, "1", feed).await.unwrap();
        let hashes = sent_entry_id_hashes(&pool, "1", feed, None).await.unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(hashes.len(), 2);
        assert!(hashes.contains(&entry_marker_hash("e1")));
        assert!(hashes.contains(&entry_marker_hash("e2")));
        assert!(!hashes.contains(&entry_marker_hash("other-chat")));
        assert!(!hashes.contains(&entry_marker_hash("other-feed")));

        // A wanted filter bounds the result to the current feed window while
        // preserving the same membership answers for those ids.
        let wanted = std::collections::HashSet::from([
            entry_marker_hash("e2"),
            entry_marker_hash("not-sent"),
        ]);
        let filtered = sent_entry_id_hashes(&pool, "1", feed, Some(&wanted))
            .await
            .unwrap();
        assert_eq!(filtered, std::collections::HashSet::from([entry_marker_hash("e2")]));
    }

    async fn insert_chat(pool: &Db, chat_id: &str, owner: &str, enabled: i64) {
        sqlx::query(
            "INSERT INTO chats (chat_id, chat_type, added_by, enabled) VALUES (?, 'channel', ?, ?)",
        )
        .bind(chat_id)
        .bind(owner)
        .bind(enabled)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_sub(pool: &Db, chat_id: &str, feed_url: &str, last_fetch_at: Option<&str>) {
        sqlx::query(
            "INSERT INTO subscriptions (chat_id, feed_url, active, last_fetch_at) VALUES (?, ?, 1, ?)",
        )
        .bind(chat_id)
        .bind(feed_url)
        .bind(last_fetch_at)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn subscription_quota_helpers_scope_by_chat_and_owner() {
        let pool = test_pool().await;
        insert_chat(&pool, "1", "7", 1).await;
        insert_chat(&pool, "2", "7", 1).await;
        insert_chat(&pool, "3", "8", 1).await;
        insert_sub(&pool, "1", "https://example.com/a", None).await;
        insert_sub(&pool, "1", "https://weibo.com/u/1234567890", None).await;
        insert_sub(&pool, "2", "https://example.com/b", None).await;
        insert_sub(&pool, "3", "https://example.com/c", None).await;

        assert_eq!(count_subscriptions_for_chat(&pool, "1").await.unwrap(), 2);
        let chat_urls = subscription_urls_for_chat(&pool, "1").await.unwrap();
        assert_eq!(chat_urls.len(), 2);

        let owner7 = subscription_urls_for_user_owner(&pool, 7).await.unwrap();
        assert_eq!(owner7.len(), 3, "both channels owned by user 7 must count");
        assert_eq!(
            owner7.iter().filter(|u| crate::platforms::platform_for_url(u).is_some()).count(),
            1
        );
        let owner8 = subscription_urls_for_user_owner(&pool, 8).await.unwrap();
        assert_eq!(owner8.len(), 1);
        assert!(subscription_urls_for_user_owner(&pool, 9).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn due_subscriptions_are_oldest_first_and_capped() {
        let pool = test_pool().await;
        insert_chat(&pool, "1", "7", 1).await;
        // Disabled chats must not contribute due rows.
        insert_chat(&pool, "2", "7", 0).await;
        insert_sub(&pool, "2", "https://example.com/disabled-chat", None).await;
        insert_sub(&pool, "1", "https://example.com/never", None).await;
        insert_sub(&pool, "1", "https://example.com/old", Some("2020-01-01 00:00:00")).await;
        insert_sub(&pool, "1", "https://example.com/newer", Some("2021-01-01 00:00:00")).await;

        let due = get_due_subscriptions(&pool).await.unwrap();
        let urls: Vec<&str> = due.iter().map(|(s, _)| s.feed_url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://example.com/never",
                "https://example.com/old",
                "https://example.com/newer",
            ],
            "never-fetched first, then oldest, disabled chats excluded"
        );
        assert_eq!(count_due_subscriptions(&pool).await.unwrap(), 3);

        // Backlog above the cap: only MAX_DUE_PER_CYCLE rows are selected.
        for i in 0..MAX_DUE_PER_CYCLE {
            insert_sub(&pool, "1", &format!("https://example.com/bulk/{}", i), None).await;
        }
        let due = get_due_subscriptions(&pool).await.unwrap();
        assert_eq!(due.len() as i64, MAX_DUE_PER_CYCLE);
        assert!(count_due_subscriptions(&pool).await.unwrap() > MAX_DUE_PER_CYCLE);
    }
}
