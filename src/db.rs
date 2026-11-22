//! SQLite setup, schema helpers and shared row/setting types.
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

pub(crate) fn push_delivery_owner_filter(
    qb: &mut sqlx::QueryBuilder<sqlx::Sqlite>,
    alias: &str,
    owner_ids: &[i64],
) {
    qb.push(format!("({alias}.added_by IS NULL"));
    if !owner_ids.is_empty() {
        qb.push(format!(" OR {alias}.added_by IN ("));
        {
            let mut separated = qb.separated(", ");
            for id in owner_ids {
                separated.push_bind(id.to_string());
            }
        }
        qb.push(")");
    }
    qb.push(")");
}

pub mod setting_key {
    pub const COOKIE_PREFIX: &str = "cookie:";
    pub const COOKIE_EXPIRED_NOTIFIED_PREFIX: &str = "cookie_expired_notified:";
    pub const PLATFORM_HEALTH_PREFIX: &str = "platform_health:";
    pub const PLATFORM_STREAK_PREFIX: &str = "platform_streak:";
    pub const MEDIA_DL_FAIL_PREFIX: &str = "media_dl_fail:";
    pub const AUTO_DISABLE_NOTIFIED_PREFIX: &str = "auto_disable_notified:";
    pub const STALE_ZERO_PREFIX: &str = "stale_zero:";
    pub const OUTBOX_MEDIA_LOST_PREFIX: &str = "outbox_media_lost:";
    pub const SEND_FAIL_PREFIX: &str = "send_fail:";

    pub fn cookie(platform: &str) -> String {
        format!("{}{}", COOKIE_PREFIX, platform)
    }
    pub fn cookie_expired_notified(platform: &str) -> String {
        format!("{}{}", COOKIE_EXPIRED_NOTIFIED_PREFIX, platform)
    }
    pub fn platform_health(platform: &str) -> String {
        format!("{}{}", PLATFORM_HEALTH_PREFIX, platform)
    }
    pub fn platform_streak(platform: &str) -> String {
        format!("{}{}", PLATFORM_STREAK_PREFIX, platform)
    }
    pub fn media_dl_fail_prefix(chat_id: &str, feed_url: &str) -> String {
        format!(
            "{}{}:{}:{}:",
            MEDIA_DL_FAIL_PREFIX,
            chat_id,
            feed_url.chars().count(),
            feed_url
        )
    }
    pub fn media_dl_fail(chat_id: &str, feed_url: &str, entry_id: &str) -> String {
        format!("{}{}", media_dl_fail_prefix(chat_id, feed_url), entry_id)
    }
    pub fn auto_disable_notified(chat_id: &str, feed_url: &str) -> String {
        format!("{}{}:{}", AUTO_DISABLE_NOTIFIED_PREFIX, chat_id, feed_url)
    }
    pub fn stale_zero(chat_id: &str, feed_url: &str) -> String {
        format!("{}{}:{}", STALE_ZERO_PREFIX, chat_id, feed_url)
    }
    pub fn outbox_media_lost(outbox_id: i64) -> String {
        format!("{}{}", OUTBOX_MEDIA_LOST_PREFIX, outbox_id)
    }
    pub fn send_fail(chat_id: &str, feed_url: &str) -> String {
        format!("{}{}:{}", SEND_FAIL_PREFIX, chat_id, feed_url)
    }
}

pub const DEFAULT_INTERVAL_MINUTES: i64 = 30;
pub const DEFAULT_CONTENT_MODE: &str = "title_link";
pub const DEFAULT_SOURCE_DISPLAY: &str = "source";
pub const DEFAULT_CAPTION_ABOVE_MEDIA: i64 = 0;
pub const DEFAULT_LINK_PREVIEW: i64 = 1;
pub const MAX_INTERVAL_MINUTES: i64 = 1440;

const EFFECTIVE_CONFIG_COLUMNS: &str =
    "COALESCE(f.interval_minutes, c.interval_minutes, 30) AS interval_minutes, \
     COALESCE(f.content_mode, c.content_mode, 'title_link') AS content_mode, \
     COALESCE(f.source_display, c.source_display, 'source') AS source_display, \
     COALESCE(f.caption_above_media, c.caption_above_media, 0) AS caption_above_media, \
     COALESCE(f.link_preview, c.link_preview, 1) AS link_preview, \
     f.interval_minutes IS NOT NULL AS iv_overridden, \
     f.content_mode IS NOT NULL AS cm_overridden, \
     f.source_display IS NOT NULL AS sd_overridden, \
     f.caption_above_media IS NOT NULL AS cp_overridden, \
     f.link_preview IS NOT NULL AS lp_overridden";

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
    pub http_etag: Option<String>,
    pub http_last_modified: Option<String>,
    pub auto_disabled: i64,
    pub disabled_at: Option<String>,
}

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
        restrict_permissions(parent, true);
    }

    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .pragma("journal_size_limit", "52428800")
        .pragma("cache_size", "-512")
        .pragma("temp_store", "1")
        .pragma("mmap_size", "0");

    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .idle_timeout(Some(Duration::from_secs(60)))
        .connect_with(options)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

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

    #[test]
    fn default_constants_match_sql_literals() {
        assert_eq!(DEFAULT_INTERVAL_MINUTES, 30);
        assert_eq!(DEFAULT_CONTENT_MODE, "title_link");
        assert_eq!(DEFAULT_SOURCE_DISPLAY, "source");
        assert_eq!(DEFAULT_CAPTION_ABOVE_MEDIA, 0);
        assert_eq!(DEFAULT_LINK_PREVIEW, 1);

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

        let (total, due) = get_due_subscriptions(&pool, &[7, 8]).await.unwrap();
        assert_eq!(total, 1);
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
        sqlx::query(
            "INSERT INTO sent_entries (chat_id, feed_url, entry_id, sent_at)
             VALUES ('1', 'https://example.com/feed', 'old-active', datetime('now', '-200 days'))",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sent_entries (chat_id, feed_url, entry_id, sent_at)
             VALUES ('2', 'https://example.com/gone', 'old-orphan', datetime('now', '-200 days'))",
        )
        .execute(&pool)
        .await
        .unwrap();
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
            "INSERT INTO subscriptions (chat_id, feed_url, active) VALUES ('1', 'https://example.com/feed', 1)",
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
        sqlx::query("UPDATE subscriptions SET active = 0 WHERE chat_id = '1'")
            .execute(&pool)
            .await
            .unwrap();

        assert!(outbox_due_ids(&pool, 1, 10, &[42])
            .await
            .unwrap()
            .is_empty());
        sqlx::query("UPDATE subscriptions SET active = 1 WHERE chat_id = '1'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(outbox_due_ids(&pool, 1, 10, &[42]).await.unwrap().len(), 1);
        assert!(
            outbox_due_ids(&pool, 1, 10, &[7]).await.unwrap().is_empty(),
            "outbox must not deliver after the owner loses whitelist access"
        );

        assert!(set_chat_enabled(&pool, "1", false).await.unwrap());
        assert!(outbox_due_ids(&pool, 1, 10, &[42])
            .await
            .unwrap()
            .is_empty());
        assert!(!can_manage_chat(&pool, 42, "1").await.unwrap());
        assert!(set_chat_enabled(&pool, "1", true).await.unwrap());
        assert!(can_manage_chat(&pool, 42, "1").await.unwrap());
        assert_eq!(outbox_due_ids(&pool, 1, 10, &[42]).await.unwrap().len(), 1);
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
        let due_ids = outbox_due_ids(&pool, 1, 10, &[42]).await.unwrap();
        assert_eq!(due_ids.len(), 1);
        let row = outbox_get(&pool, due_ids[0]).await.unwrap().unwrap();
        assert_eq!(row.first_attempt_at, None);

        outbox_update_retry(&pool, row.id, 1, 5, 123, "boom")
            .await
            .unwrap();
        let due_ids = outbox_due_ids(&pool, 5, 10, &[7]).await.unwrap();
        let row = outbox_get(&pool, due_ids[0]).await.unwrap().unwrap();
        assert_eq!(row.attempts, 1);
        assert_eq!(row.first_attempt_at, Some(123));
        assert_eq!(row.last_error.as_deref(), Some("boom"));

        outbox_update_retry(&pool, row.id, 2, 10, 456, "still boom")
            .await
            .unwrap();
        let due_ids = outbox_due_ids(&pool, 10, 10, &[7]).await.unwrap();
        let row = outbox_get(&pool, due_ids[0]).await.unwrap().unwrap();
        assert_eq!(row.attempts, 2);
        assert_eq!(row.first_attempt_at, Some(123));
        assert_eq!(row.last_error.as_deref(), Some("still boom"));

        outbox_delete(&pool, row.id).await.unwrap();
        assert!(outbox_get(&pool, row.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn outbox_insert_requires_active_subscription() {
        let pool = test_pool().await;
        insert_chat(&pool, "1", "7", 1).await;
        sqlx::query(
            "INSERT INTO subscriptions (chat_id, feed_url, active) VALUES ('1', 'https://example.com/paused', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let inserted = outbox_insert(
            &pool,
            "1",
            "https://example.com/paused",
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
        assert!(!inserted);
        assert!(outbox_entry_ids(&pool, "1", "https://example.com/paused")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn stale_outbox_for_ineligible_owner_is_purged() {
        let pool = test_pool().await;
        insert_chat(&pool, "allowed", "7", 1).await;
        insert_chat(&pool, "revoked", "9", 1).await;
        insert_sub(&pool, "allowed", "https://example.com/allowed", None).await;
        insert_sub(&pool, "revoked", "https://example.com/revoked", None).await;
        for (chat_id, entry_id, created_at) in [
            ("allowed", "old-allowed", 1),
            ("revoked", "old-revoked", 1),
            ("revoked", "new-revoked", 101),
        ] {
            sqlx::query(
                "INSERT INTO outbox (chat_id, feed_url, entry_id, feed_title, entry_json, media_json, meta_json, content_mode, source_display, next_retry_at, created_at) \
                 VALUES (?, ?, ?, 'Feed', '{}', '[]', '', 'title_link', 'source', 0, ?)",
            )
            .bind(chat_id)
            .bind(format!("https://example.com/{chat_id}"))
            .bind(entry_id)
            .bind(created_at)
            .execute(&pool)
            .await
            .unwrap();
        }

        let deleted = delete_ineligible_owner_outbox_before(&pool, 100, &[7])
            .await
            .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(
            outbox_entry_ids(&pool, "allowed", "https://example.com/allowed")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            outbox_entry_ids(&pool, "revoked", "https://example.com/revoked")
                .await
                .unwrap()
                .contains("new-revoked")
        );
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
            setting_key::send_fail(old, feed),
        ] {
            set_setting(&pool, &key, "1").await.unwrap();
        }

        migrate_chat_id(&pool, old, new).await.unwrap();
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
                OR instr(key, 'stale_zero:') = 1 OR instr(key, 'media_dl_fail:') = 1
                OR instr(key, 'send_fail:') = 1 ORDER BY key",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            settings,
            vec![
                format!("auto_disable_notified:{}:{}", new, feed),
                setting_key::media_dl_fail(new, feed, "e4"),
                setting_key::send_fail(new, feed),
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
        insert_chat(&pool, "1", "7", 1).await;
        insert_sub(&pool, "1", "https://example.com/feed", None).await;
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
        insert_chat(&pool, "1", "7", 1).await;
        insert_sub(&pool, "1", "https://example.com/feed", None).await;

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

        let (total, due) = get_due_subscriptions(&pool, &[7, 8]).await.unwrap();
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
        assert_eq!(total, 3);

        for i in 0..MAX_DUE_PER_CYCLE {
            insert_sub(&pool, "1", &format!("https://example.com/bulk/{}", i), None).await;
        }
        let (total, due) = get_due_subscriptions(&pool, &[7, 8]).await.unwrap();
        assert_eq!(due.len() as i64, MAX_DUE_PER_CYCLE);
        assert!(total > MAX_DUE_PER_CYCLE);
    }

    #[tokio::test]
    async fn auto_heal_clears_both_source_notification_keys() {
        let pool = test_pool().await;
        insert_chat(&pool, "1", "7", 1).await;
        sqlx::query(
            "INSERT INTO subscriptions (chat_id, feed_url, active, auto_disabled, disabled_at) \
             VALUES ('1', 'https://example.com/feed', 0, 1, '2020-01-01 00:00:00')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let send_fail = setting_key::send_fail("1", "https://example.com/feed");
        let notified = setting_key::auto_disable_notified("1", "https://example.com/feed");
        set_setting(&pool, &send_fail, "1").await.unwrap();
        set_setting(&pool, &notified, "1").await.unwrap();

        assert_eq!(reenable_due_auto_disabled(&pool, 3).await.unwrap(), 1);
        assert!(get_setting(&pool, &send_fail).await.unwrap().is_none());
        assert!(get_setting(&pool, &notified).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn resubscribe_clears_old_outbox_and_marks_current_window() {
        let pool = test_pool().await;
        insert_chat(&pool, "1", "7", 1).await;
        insert_sub(&pool, "1", "https://example.com/feed", None).await;
        outbox_insert(
            &pool,
            "1",
            "https://example.com/feed",
            "old",
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

        add_subscription_with_sent(
            &pool,
            "1",
            "https://example.com/feed",
            Some("Feed"),
            &["new".to_string()],
        )
        .await
        .unwrap();
        assert!(outbox_entry_ids(&pool, "1", "https://example.com/feed")
            .await
            .unwrap()
            .is_empty());
        assert!(is_entry_sent(&pool, "1", "https://example.com/feed", "new")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn delivery_queries_exclude_revoked_chat_owners() {
        let pool = test_pool().await;
        insert_chat(&pool, "allowed", "7", 1).await;
        insert_chat(&pool, "revoked", "9", 1).await;
        insert_sub(&pool, "allowed", "https://example.com/allowed", None).await;
        insert_sub(&pool, "revoked", "https://example.com/revoked", None).await;

        let (_, due) = get_due_subscriptions(&pool, &[7]).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0.chat_id, "allowed");

        let active = active_subscription_urls(&pool, &[7]).await.unwrap();
        assert_eq!(active, vec!["https://example.com/allowed"]);
    }
}
