use std::path::PathBuf;

/// Scope of the trusted private feed allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustedPrivateScope {
    /// Only the admin may use trusted private hosts.
    Admin,
    /// Admin and whitelisted users may use trusted private hosts.
    Allowed,
}

/// Fixed concurrency for RSS fetches and per-source processing.
pub const FETCH_CONCURRENCY: usize = 8;
/// Fixed concurrency for media downloads.
pub const DOWNLOAD_CONCURRENCY: usize = 4;
/// Fixed aspect-ratio threshold above which images are sent as documents.
pub const LONG_IMAGE_ASPECT_RATIO: f64 = 8.0;

/// Application configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    // Bot
    pub token: String,
    pub bot_api_url: Option<String>,
    /// Bot API proxy (http/socks5/socks5h). Ignored when `bot_api_url` is set:
    /// a custom/local Bot API endpoint is contacted directly.
    pub bot_proxy: Option<String>,
    pub allowed_user_ids: Vec<i64>,
    /// The single admin user ID: can manage cookies, fetch logs, and receive
    /// platform health notifications. Exactly one is required.
    pub admin_user_id: i64,

    // Database
    pub db_path: PathBuf,

    // Media
    pub media_dir: PathBuf,
    pub media_max_size_gb: u64,
    /// Maximum size of one downloaded media item (MB). Applies to all media
    /// types, not just videos; official/local Bot API transport caps still
    /// apply on top of it.
    pub max_media_download_mb: u64,
    /// Notify the channel owner when media is skipped for exceeding the
    /// per-file download limit (dedicated video notice plus oversized image/
    /// file entries in the media warning notice). Defaults to true.
    pub notify_oversized_media: bool,

    // Subscription limits
    /// 0 = unlimited. Applies to every user.
    pub max_subscriptions_per_chat: i64,
    /// 0 = unlimited. Applies to non-admin users only.
    pub max_subscriptions_per_user: i64,
    /// 0 = unlimited. Applies to non-admin users only.
    pub max_platform_subscriptions_per_user: i64,

    // Proxy
    pub rss_proxy: Option<String>,
    pub rss_no_proxy: Vec<String>,
    /// Hosts whose TLS certificate chain is rejected by rustls (e.g. legacy
    /// X.509 versions or intercepting CAs). For these hosts only, certificate
    /// verification is skipped. Empty by default = strict verification everywhere.
    pub rss_insecure_hosts: Vec<String>,

    // Trusted private feeds (local RSSHub etc.)
    /// Exact `host:port` entries that may be fetched directly even though they
    /// resolve to a private/loopback address. Empty by default.
    pub rss_trusted_private_hosts: Vec<String>,
    /// Who may use `rss_trusted_private_hosts`.
    pub rss_trusted_private_scope: TrustedPrivateScope,

    // Platform login cookies are stored globally in the DB settings table
    // (key = "cookie:<platform>"); configured by admins via /cookies.

    // Media transport: where the Bot API server sees our media directory.
    // BOT_API_URL controls the 2GB upload cap; this path only enables
    // rich-message file:// server-side reads. Empty => multipart uploads even
    // with a local Bot API.
    pub bot_api_media_dir: String,

    // Image relay (enabled when the base URL is non-empty and a host matches
    // img_relay_domains).
    pub img_relay_base_url: String,
    pub img_relay_domains: Vec<String>,

    // Cleanup
    pub log_retention_days: i64,
    /// Daily DB backups kept this many days.
    pub backup_retention_days: i64,

    // Error handling
    pub max_fetch_errors: i64,

    // Logging
    pub log_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let token = std::env::var("TELOXIDE_TOKEN")
            .map_err(|_| anyhow::anyhow!("TELOXIDE_TOKEN is required"))?;

        let bot_api_url = std::env::var("BOT_API_URL").ok().filter(|s| !s.is_empty());
        let bot_proxy = std::env::var("BOT_PROXY").ok().filter(|s| !s.is_empty());

        let allowed_user_ids = parse_id_list("ALLOWED_USER_IDS");
        // Exactly one admin is required. Missing (unset/empty/all-invalid) or
        // more than one is a fatal misconfiguration: cookie/log management,
        // command-menu elevation and platform alerts all key off this one user.
        let admin_user_id = validate_single_admin(parse_id_list("ADMIN_USER_IDS"))?;

        let db_path = PathBuf::from(env_or("DB_PATH", "data/rss_bot.db"));
        let media_dir = PathBuf::from(env_or("MEDIA_DIR", "media"));
        let log_dir = PathBuf::from(env_or("LOG_DIR", "logs"));
        // Path where the Bot API server sees our media dir (shared mount).
        // Default matches telegram-bot-api compose mount; empty disables
        // local Bot API mode (official-API 49MB cap).
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
            // Retention-day floor of 1: a misconfigured negative/0 value would
            // make the datetime('-N days') modifier invalid (cleanup silently
            // stops working and the table grows unboundedly).
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
            // Floor of 1: MAX_FETCH_ERRORS=0 makes the `count >= max` check in
            // the scheduler true on the very first failure, auto-disabling a
            // source after a single transient error.
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

    /// Whether a user may interact with the bot at all.
    /// Only the admin + ALLOWED_USER_IDS may use the bot.
    pub fn is_allowed(&self, user_id: i64) -> bool {
        is_allowed_by_ids(self.admin_user_id, &self.allowed_user_ids, user_id)
    }

    /// Whether a user is the admin (cookies / loget / platform alerts).
    pub fn is_admin(&self, user_id: i64) -> bool {
        self.admin_user_id == user_id
    }

    /// Whether a user may use trusted private feed hosts.
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

/// True when `host` equals a list entry or is a subdomain of it. Entries are
/// normalized like the env parsers (trim whitespace, drop a leading/trailing
/// dot, lowercase). Shared by proxy/no_proxy/img-relay host matching.
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

/// Exact host match (no subdomain inheritance), normalized like
/// [`host_in_list`]. Used for TLS-verification exemptions: listing
/// `example.com` must not silently disable certificate validation for every
/// `*.example.com`.
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

/// Boolean env flag. Accepts 1/0, true/false, yes/no and on/off
/// (case-insensitive); an invalid value warns and keeps the default.
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
                // Silently falling back to the default on an invalid value is a
                // debugging trap (e.g. MAX_FETCH_ERRORS=abc), so leave a trace.
                // Logging isn't initialized yet, so use eprintln.
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

/// The admin list must resolve to exactly one user ID. Split out from
/// `from_env` so it is unit-testable without touching process env.
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
