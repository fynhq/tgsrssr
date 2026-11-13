use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod outbox;
mod sent;
mod settings;

pub use outbox::*;
pub use sent::*;
pub use settings::*;
mod chats;
mod configs;
mod filters;
mod subs;

pub use chats::*;
pub use configs::*;
pub use filters::*;
pub use subs::*;

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
        sqlx::query(sqlx::AssertSqlSafe(cleanup))
            .execute(db)
            .await?;
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
        // could never catch SQL drift. The relevant SQL now lives in the
        // subscriptions/config submodules, so scan them together.
        let src = concat!(
            include_str!("db.rs"),
            include_str!("db/subs.rs"),
            include_str!("db/configs.rs"),
        );
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
        let state: (i64, i64) =
            sqlx::query_as("SELECT active, auto_disabled FROM subscriptions WHERE id = ?")
                .bind(sub_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, (0, 1));

        assert!(set_subscription_active(&pool, sub_id, "1", true)
            .await
            .unwrap());
        let state: (i64, i64) =
            sqlx::query_as("SELECT active, auto_disabled FROM subscriptions WHERE id = ?")
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
        set_feed_config_field(
            &pool,
            "1",
            "https://example.com/feed",
            "caption_above_media",
            Some("0"),
        )
        .await
        .unwrap();
        let cfg = get_effective_config(&pool, "1", "https://example.com/feed")
            .await
            .unwrap();
        assert!(!cfg.caption_above_media);
        assert!(cfg.cp_overridden);

        set_feed_config_field(
            &pool,
            "1",
            "https://example.com/feed",
            "caption_above_media",
            None,
        )
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
        let cfg = get_chat_config(&pool, "1")
            .await
            .unwrap()
            .expect("row created");
        assert_eq!(cfg.interval_minutes, DEFAULT_INTERVAL_MINUTES);
        assert_eq!(cfg.content_mode, "content_only");
    }

    #[tokio::test]
    async fn trim_sent_messages_keeps_newest_rows() {
        let pool = test_pool().await;
        for mid in 1..=3 {
            record_sent_message(
                &pool,
                "1",
                mid,
                "https://example.com/feed",
                &format!("e{mid}"),
                None,
            )
            .await
            .unwrap();
        }
        assert_eq!(trim_sent_messages(&pool, 2).await.unwrap(), 1);
        let rows: Vec<i64> =
            sqlx::query_scalar("SELECT message_id FROM sent_messages ORDER BY message_id")
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
        let remaining: Vec<(String, String)> =
            sqlx::query_as("SELECT chat_id, entry_id FROM sent_entries ORDER BY entry_id")
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
        let db_path =
            std::env::temp_dir().join(format!("tgsrssr_vacuum_src_{}.db", uuid::Uuid::new_v4()));
        let path = std::env::temp_dir().join(format!("tgsrssr_vacuum_{}.db", uuid::Uuid::new_v4()));
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
        assert!(
            head.starts_with(b"SQLite format 3\0"),
            "not a SQLite database"
        );
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
        sqlx::query(
            "INSERT INTO subscriptions (chat_id, feed_url, feed_title) VALUES (?, ?, 'feed')",
        )
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
            let old_count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM {table} WHERE {condition}"
            )))
            .bind(old)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(old_count, 0, "{table} old row left behind");
        }

        let interval: i64 =
            sqlx::query_scalar("SELECT interval_minutes FROM chat_configs WHERE chat_id = ?")
                .bind(new)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            interval, 5,
            "old chat config must win over provisional defaults"
        );

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
            entry_dedup_state(&pool, "1", "https://example.com/feed", "sent-entry")
                .await
                .unwrap(),
            (true, false)
        );
        assert_eq!(
            entry_dedup_state(&pool, "1", "https://example.com/feed", "queued-entry")
                .await
                .unwrap(),
            (false, true)
        );
        assert_eq!(
            entry_dedup_state(&pool, "1", "https://example.com/feed", "new-entry")
                .await
                .unwrap(),
            (false, false)
        );
        assert_eq!(
            entry_dedup_state(&pool, "1", "https://example.com/other", "sent-entry")
                .await
                .unwrap(),
            (false, false)
        );
    }

    #[tokio::test]
    async fn sent_entry_ids_recent_is_bounded_by_age_and_limit() {
        let pool = test_pool().await;
        let feed = "https://example.com/feed";
        mark_entry_sent(&pool, "1", feed, "fresh1").await.unwrap();
        mark_entry_sent(&pool, "1", feed, "fresh2").await.unwrap();
        sqlx::query(
            "INSERT INTO sent_entries (chat_id, feed_url, entry_id, sent_at)
             VALUES ('1', ?, 'old', datetime('now', '-200 days'))",
        )
        .bind(feed)
        .execute(&pool)
        .await
        .unwrap();

        let recent = sent_entry_ids_recent(&pool, "1", feed, 180, 5000)
            .await
            .unwrap();
        assert!(recent.contains("fresh1"));
        assert!(recent.contains("fresh2"));
        assert!(!recent.contains("old"), "age cap must exclude old rows");

        let limited = sent_entry_ids_recent(&pool, "1", feed, 180, 1)
            .await
            .unwrap();
        assert_eq!(limited.len(), 1, "row cap must bound the snapshot");
    }

    #[tokio::test]
    async fn sent_entry_ids_in_window_reads_only_requested_ids() {
        let pool = test_pool().await;
        let feed = "https://example.com/feed";
        mark_entry_sent(&pool, "1", feed, "e1").await.unwrap();
        mark_entry_sent(&pool, "1", feed, "e2").await.unwrap();
        mark_entry_sent(&pool, "2", feed, "other-chat")
            .await
            .unwrap();
        mark_entry_sent(&pool, "1", "https://example.com/other", "other-feed")
            .await
            .unwrap();

        let wanted = vec!["e1".to_string(), "e2".to_string(), "missing".to_string()];
        let found = sent_entry_ids_in_window(&pool, "1", feed, &wanted)
            .await
            .unwrap();
        assert_eq!(
            found,
            std::collections::HashSet::from(["e1".to_string(), "e2".to_string()])
        );
    }

    #[tokio::test]
    async fn sent_entry_ids_in_window_handles_multiple_chunks() {
        let pool = test_pool().await;
        let feed = "https://example.com/feed";
        let ids: Vec<String> = (0..320).map(|i| format!("e{i}")).collect();
        for id in &ids {
            mark_entry_sent(&pool, "1", feed, id).await.unwrap();
        }
        let found = sent_entry_ids_in_window(&pool, "1", feed, &ids)
            .await
            .unwrap();
        assert_eq!(found.len(), ids.len(), "chunk boundary must not drop rows");
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
            owner7
                .iter()
                .filter(|u| crate::platforms::platform_for_url(u).is_some())
                .count(),
            1
        );
        let owner8 = subscription_urls_for_user_owner(&pool, 8).await.unwrap();
        assert_eq!(owner8.len(), 1);
        assert!(subscription_urls_for_user_owner(&pool, 9)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn due_subscriptions_are_oldest_first_and_capped() {
        let pool = test_pool().await;
        insert_chat(&pool, "1", "7", 1).await;
        // Disabled chats must not contribute due rows.
        insert_chat(&pool, "2", "7", 0).await;
        insert_sub(&pool, "2", "https://example.com/disabled-chat", None).await;
        insert_sub(&pool, "1", "https://example.com/never", None).await;
        insert_sub(
            &pool,
            "1",
            "https://example.com/old",
            Some("2020-01-01 00:00:00"),
        )
        .await;
        insert_sub(
            &pool,
            "1",
            "https://example.com/newer",
            Some("2021-01-01 00:00:00"),
        )
        .await;

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
