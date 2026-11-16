use super::*;
use sqlx::Row;

#[derive(Debug, Clone, Default)]
pub struct FilterConfig {
    pub enabled: bool,
    pub mode: String,
    pub keywords: String,
}

impl FilterConfig {
    pub fn is_active(&self) -> bool {
        self.enabled && !self.keywords.trim().is_empty()
    }

    pub fn keyword_list(&self) -> Vec<String> {
        self.keywords
            .split([',', '，', ';', '；'])
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    }

    pub fn allows_title_with_keywords(&self, title: &str, keywords: &[String]) -> bool {
        if !self.is_active() {
            return true;
        }
        let lower = title.to_lowercase();
        let matched = keywords.iter().any(|kw| lower.contains(kw));
        match self.mode.as_str() {
            "whitelist" => matched,
            _ => !matched,
        }
    }
}

pub async fn get_feed_filter(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
) -> anyhow::Result<FilterConfig> {
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

pub async fn set_feed_filter_enabled(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    enabled: bool,
) -> anyhow::Result<()> {
    set_feed_filter_field(
        db,
        chat_id,
        feed_url,
        "filter_enabled",
        if enabled { "1" } else { "0" },
    )
    .await
}

pub async fn set_feed_filter_mode(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    mode: &str,
) -> anyhow::Result<()> {
    set_feed_filter_field(db, chat_id, feed_url, "filter_mode", mode).await
}

pub async fn set_feed_filter_keywords(
    db: &Db,
    chat_id: &str,
    feed_url: &str,
    keywords: &str,
) -> anyhow::Result<()> {
    set_feed_filter_field(db, chat_id, feed_url, "filter_keywords", keywords).await
}

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
