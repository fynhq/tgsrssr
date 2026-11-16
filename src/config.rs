use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustedPrivateScope {
    Admin,
    Allowed,
}

pub const FETCH_CONCURRENCY: usize = 8;
pub const DOWNLOAD_CONCURRENCY: usize = 4;
pub const LONG_IMAGE_ASPECT_RATIO: f64 = 8.0;

#[derive(Debug, Clone)]
pub struct Config {
    pub token: String,
    pub bot_api_url: Option<String>,
    pub bot_proxy: Option<String>,
    pub allowed_user_ids: Vec<i64>,
    pub admin_user_id: i64,

    pub db_path: PathBuf,

    pub media_dir: PathBuf,
    pub media_max_size_gb: u64,
    pub max_media_download_mb: u64,
    pub notify_oversized_media: bool,

    pub max_subscriptions_per_chat: i64,
    pub max_subscriptions_per_user: i64,
    pub max_platform_subscriptions_per_user: i64,

    pub rss_proxy: Option<String>,
    pub rss_no_proxy: Vec<String>,
    pub rss_insecure_hosts: Vec<String>,

    pub rss_trusted_private_hosts: Vec<String>,
    pub rss_trusted_private_scope: TrustedPrivateScope,

    pub bot_api_media_dir: String,

    pub img_relay_base_url: String,
    pub img_relay_domains: Vec<String>,

    pub log_retention_days: i64,
    pub backup_retention_days: i64,

    pub max_fetch_errors: i64,

    pub log_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let token = std::env::var("TELOXIDE_TOKEN")
            .map_err(|_| anyhow::anyhow!("TELOXIDE_TOKEN is required"))?;

        let bot_api_url = std::env::var("BOT_API_URL").ok().filter(|s| !s.is_empty());
        let bot_proxy = std::env::var("BOT_PROXY").ok().filter(|s| !s.is_empty());

        let allowed_user_ids = parse_id_list("ALLOWED_USER_IDS");
        let admin_user_id = validate_single_admin(parse_id_list("ADMIN_USER_IDS"))?;

        let db_path = PathBuf::from(env_or("DB_PATH", "data/rss_bot.db"));
        let media_dir = PathBuf::from(env_or("MEDIA_DIR", "media"));
        let log_dir = PathBuf::from(env_or("LOG_DIR", "logs"));
        let bot_api_media_dir = env_or("BOT_API_MEDIA_DIR", "/var/lib/telegram-bot-api-media");

        let rss_proxy = std::env::var("RSS_PROXY").ok().filter(|s| !s.is_empty());
        let rss_no_proxy = split_host_list("RSS_NO_PROXY", false);
        let rss_insecure_hosts = split_host_list("RSS_INSECURE_HOSTS", true);
        let rss_trusted_private_hosts = split_host_list("RSS_TRUSTED_PRIVATE_HOSTS", false);
        let rss_trusted_private_scope = parse_trusted_private_scope();

        let img_relay_base_url = env_or("IMG_RELAY_BASE_URL", "");
        let img_relay_domains = split_host_list("IMG_RELAY_DOMAINS", true);

        Ok(Self {
            token,
            bot_api_url,
            bot_proxy,
            allowed_user_ids,
            admin_user_id,
            db_path,
            media_dir,
            log_dir,
            bot_api_media_dir,
            media_max_size_gb: clamp_warn(
                "MEDIA_MAX_SIZE_GB",
                env_parse_or("MEDIA_MAX_SIZE_GB", 10u64),
                1,
                1000,
            ),
            max_media_download_mb: clamp_warn(
                "MAX_MEDIA_DOWNLOAD_MB",
                env_parse_or("MAX_MEDIA_DOWNLOAD_MB", 100u64),
                1,
                2048,
            ),
            notify_oversized_media: env_bool_or("NOTIFY_OVERSIZED_MEDIA", true),
            max_subscriptions_per_chat: clamp_warn(
                "MAX_SUBSCRIPTIONS_PER_CHAT",
                env_parse_or("MAX_SUBSCRIPTIONS_PER_CHAT", 99i64),
                0,
                1000,
            ),
            max_subscriptions_per_user: clamp_warn(
                "MAX_SUBSCRIPTIONS_PER_USER",
                env_parse_or("MAX_SUBSCRIPTIONS_PER_USER", 300i64),
                0,
                5000,
            ),
            max_platform_subscriptions_per_user: clamp_warn(
                "MAX_PLATFORM_SUBSCRIPTIONS_PER_USER",
                env_parse_or("MAX_PLATFORM_SUBSCRIPTIONS_PER_USER", 20i64),
                0,
                1000,
            ),
            rss_proxy,
            rss_no_proxy,
            rss_insecure_hosts,
            rss_trusted_private_hosts,
            rss_trusted_private_scope,
            img_relay_base_url,
            img_relay_domains,
            log_retention_days: clamp_warn(
                "LOG_RETENTION_DAYS",
                env_parse_or("LOG_RETENTION_DAYS", 30i64),
                1,
                3650,
            ),
            backup_retention_days: clamp_warn(
                "BACKUP_RETENTION_DAYS",
                env_parse_or("BACKUP_RETENTION_DAYS", 7i64),
                1,
                3650,
            ),
            max_fetch_errors: clamp_warn(
                "MAX_FETCH_ERRORS",
                env_parse_or("MAX_FETCH_ERRORS", 20i64),
                1,
                100,
            ),
        })
    }

    pub fn media_max_bytes(&self) -> u64 {
        self.media_max_size_gb * 1024 * 1024 * 1024
    }

    pub fn is_allowed(&self, user_id: i64) -> bool {
        is_allowed_by_ids(self.admin_user_id, &self.allowed_user_ids, user_id)
    }

    pub fn is_admin(&self, user_id: i64) -> bool {
        self.admin_user_id == user_id
    }

    pub fn trusted_private_allowed(&self, user_id: i64) -> bool {
        match self.rss_trusted_private_scope {
            TrustedPrivateScope::Admin => self.is_admin(user_id),
            TrustedPrivateScope::Allowed => self.is_allowed(user_id),
        }
    }
}

fn is_allowed_by_ids(admin_user_id: i64, allowed: &[i64], user_id: i64) -> bool {
    admin_user_id == user_id || allowed.contains(&user_id)
}

fn parse_trusted_private_scope() -> TrustedPrivateScope {
    match std::env::var("RSS_TRUSTED_PRIVATE_SCOPE")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "admin" => TrustedPrivateScope::Admin,
        "allowed" | "whitelist" => TrustedPrivateScope::Allowed,
        other => {
            eprintln!(
                "WARNING: RSS_TRUSTED_PRIVATE_SCOPE has invalid value '{}', using default 'admin'",
                other
            );
            TrustedPrivateScope::Admin
        }
    }
}

fn split_host_list(key: &str, strip_leading_dot: bool) -> Vec<String> {
    std::env::var(key)
        .unwrap_or_default()
        .split(',')
        .map(|s| {
            let s = s.trim();
            if strip_leading_dot {
                s.trim_start_matches('.').to_lowercase()
            } else {
                s.to_lowercase()
            }
        })
        .filter(|s| !s.is_empty())
        .collect()
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

pub(crate) fn host_in_list(host: &str, list: &[String]) -> bool {
    let host = host.trim_end_matches('.').to_lowercase();
    list.iter().any(|entry| {
        let entry = entry
            .trim()
            .trim_start_matches('.')
            .trim_end_matches('.')
            .to_lowercase();
        !entry.is_empty() && (host == entry || host.ends_with(&format!(".{}", entry)))
    })
}

// TLS-verification exemptions are exact-host only; listing example.com must not silently disable validation for *.example.com.
pub(crate) fn host_matches_exact(host: &str, list: &[String]) -> bool {
    let host = host.trim_end_matches('.').to_lowercase();
    list.iter().any(|entry| {
        let entry = entry
            .trim()
            .trim_start_matches('.')
            .trim_end_matches('.')
            .to_lowercase();
        !entry.is_empty() && host == entry
    })
}

fn env_bool_or(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Err(_) => default,
        Ok(v) => match parse_bool(&v) {
            Some(b) => b,
            None => {
                eprintln!(
                    "WARNING: {} has invalid value '{}', using default {}",
                    key, v, default
                );
                default
            }
        },
    }
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_parse_or<T: std::str::FromStr + Copy>(key: &str, default: T) -> T {
    match std::env::var(key) {
        Err(_) => default,
        Ok(v) => match v.parse() {
            Ok(x) => x,
            Err(_) => {
                eprintln!("WARNING: {} has invalid value '{}', using default", key, v);
                default
            }
        },
    }
}

fn clamp_warn<T: Copy + PartialOrd + std::fmt::Display>(key: &str, value: T, min: T, max: T) -> T {
    if value < min {
        eprintln!(
            "WARNING: {} = {} is below the supported minimum {}, clamping",
            key, value, min
        );
        min
    } else if value > max {
        eprintln!(
            "WARNING: {} = {} is above the supported maximum {}, clamping",
            key, value, max
        );
        max
    } else {
        value
    }
}

fn parse_id_list(key: &str) -> Vec<i64> {
    let raw = std::env::var(key).unwrap_or_default();
    let mut ids = Vec::new();
    for s in raw.split(',') {
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        match s.parse::<i64>() {
            Ok(id) => ids.push(id),
            Err(_) => eprintln!("WARNING: {} contains invalid id '{}', skipped", key, s),
        }
    }
    ids
}

fn validate_single_admin(ids: Vec<i64>) -> anyhow::Result<i64> {
    match ids.len() {
        1 if ids[0] > 0 => Ok(ids[0]),
        1 => Err(anyhow::anyhow!(
            "ADMIN_USER_IDS must be a positive Telegram user ID"
        )),
        0 => Err(anyhow::anyhow!(
            "ADMIN_USER_IDS is required and must contain exactly one user ID"
        )),
        n => Err(anyhow::anyhow!(
            "ADMIN_USER_IDS must contain exactly one user ID, got {}",
            n
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        host_in_list, host_matches_exact, is_allowed_by_ids, parse_bool, validate_single_admin,
    };

    #[test]
    fn bool_values_parse_with_common_spellings() {
        for raw in ["1", "true", "TRUE", " yes ", "on"] {
            assert_eq!(parse_bool(raw), Some(true), "raw = {:?}", raw);
        }
        for raw in ["0", "false", "False", "no", "off"] {
            assert_eq!(parse_bool(raw), Some(false), "raw = {:?}", raw);
        }
        assert_eq!(parse_bool("maybe"), None);
        assert_eq!(parse_bool(""), None);
    }

    #[test]
    fn insecure_host_matching_is_exact_but_no_proxy_inherits_subdomains() {
        let list = vec!["example.com".to_string()];
        assert!(host_matches_exact("example.com", &list));
        assert!(host_matches_exact("EXAMPLE.COM.", &list));
        assert!(!host_matches_exact("cdn.example.com", &list));
        assert!(host_in_list("cdn.example.com", &list));
        assert!(host_in_list("example.com.", &list));
    }

    #[test]
    fn single_admin_accepted() {
        assert_eq!(validate_single_admin(vec![42]).unwrap(), 42);
    }

    #[test]
    fn empty_admin_rejected() {
        let err = validate_single_admin(vec![]).unwrap_err().to_string();
        assert!(err.contains("required"), "unexpected message: {}", err);
    }

    #[test]
    fn multiple_admins_rejected() {
        let err = validate_single_admin(vec![1, 2]).unwrap_err().to_string();
        assert!(err.contains("exactly one"), "unexpected message: {}", err);
        assert!(err.contains("got 2"), "unexpected message: {}", err);
    }

    #[test]
    fn non_positive_admin_rejected() {
        for id in [0, -1] {
            let err = validate_single_admin(vec![id]).unwrap_err().to_string();
            assert!(err.contains("positive"), "unexpected message: {}", err);
        }
    }

    #[test]
    fn access_is_admin_or_whitelist_only() {
        assert!(is_allowed_by_ids(42, &[7, 8], 42));
        assert!(is_allowed_by_ids(42, &[7, 8], 7));
        assert!(!is_allowed_by_ids(42, &[7, 8], 9));
        assert!(!is_allowed_by_ids(42, &[], 9));
    }

    #[test]
    fn host_in_list_matches_exact_and_subdomains() {
        let list = vec!["example.com".to_string(), ".internal".to_string()];
        assert!(host_in_list("example.com", &list));
        assert!(host_in_list("www.example.com", &list));
        assert!(host_in_list("a.internal", &list));
        assert!(!host_in_list("badexample.com", &list));
        assert!(!host_in_list("example.com.evil.org", &list));
    }
}
