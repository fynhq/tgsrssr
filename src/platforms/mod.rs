//! Platform registry, canonical URLs, cookie handling and API HTTP.

pub mod telegram;
pub mod weibo;
pub mod x;

use std::future::Future;
use std::pin::Pin;

use crate::state::AppState;
use feed_rs::model::{Content, Entry, Link, Person, Text};
use jiff::Timestamp;

pub fn make_entry(
    id: String,
    title: String,
    author: String,
    body: String,
    content_type: &str,
    link: String,
    published: Option<Timestamp>,
) -> Entry {
    Entry {
        id,
        title: Some(Text {
            content_type: "text/plain".parse().unwrap(),
            src: None,
            content: title,
        }),
        updated: published.map(crate::timeutil::to_chrono),
        published: published.map(crate::timeutil::to_chrono),
        authors: vec![Person {
            name: Some(author),
            uri: None,
            email: None,
            role: None,
        }],
        contributors: Vec::new(),
        content: Some(Content {
            body: Some(body),
            content_type: content_type
                .parse()
                .unwrap_or_else(|_| "text/html".parse().unwrap()),
            length: None,
            src: None,
        }),
        links: vec![Link {
            href: link,
            target: None,
            rel: None,
            media_type: None,
            href_lang: None,
            title: None,
            length: None,
        }],
        summary: None,
        categories: Vec::new(),
        media: Vec::new(),
        rights: None,
        source: None,
        language: None,
        base: None,
    }
}

#[derive(Debug)]
pub enum PlatformError {
    AuthExpired {
        platform: &'static str,
        detail: String,
    },
    RateLimited {
        platform: &'static str,
        status: u16,
    },
    SourceForbidden {
        platform: &'static str,
        status: u16,
        detail: String,
    },
}

impl std::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlatformError::AuthExpired { platform, detail } => {
                write!(
                    f,
                    "{} cookie expired: {}; please update via /cookies",
                    platform, detail
                )
            }
            PlatformError::RateLimited { platform, status } => {
                write!(
                    f,
                    "{} rate limited (HTTP {}); will retry later",
                    platform, status
                )
            }
            PlatformError::SourceForbidden {
                platform,
                status,
                detail,
            } => {
                write!(
                    f,
                    "{} source request forbidden (HTTP {}): {}",
                    platform, status, detail
                )
            }
        }
    }
}

impl std::error::Error for PlatformError {}

pub fn auth_expired(platform: &'static str, detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(PlatformError::AuthExpired {
        platform,
        detail: detail.into(),
    })
}

pub fn rate_limited(platform: &'static str, status: u16) -> anyhow::Error {
    anyhow::Error::new(PlatformError::RateLimited { platform, status })
}

pub fn source_forbidden(
    platform: &'static str,
    status: u16,
    detail: impl Into<String>,
) -> anyhow::Error {
    anyhow::Error::new(PlatformError::SourceForbidden {
        platform,
        status,
        detail: detail.into(),
    })
}

pub type PlatformFetchFn = for<'a> fn(
    &'a AppState,
    String,
    &'a crate::fetcher::FetchOptions<'a>,
) -> Pin<
    Box<dyn Future<Output = anyhow::Result<crate::fetcher::FetchResult>> + Send + 'a>,
>;

pub type PlatformSelfSourceFn =
    for<'a> fn(
        &'a AppState,
        &'a str,
        &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<bool>> + Send + 'a>>;

pub type PlatformMediaEnrichFn = for<'a> fn(
    &'a AppState,
    Option<&'a str>,
    &'a str,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>>;

// Platform-specific media enrichment. `matches_url` keys on media/CDN hosts, not feed URLs.
pub struct MediaEnrich {
    pub matches_url: fn(&str) -> bool,
    pub referer: Option<&'static str>,
    pub upgrade_image_url: fn(&str) -> String,
    pub enrich_video_url: PlatformMediaEnrichFn,
}

pub struct PlatformDef {
    pub id: &'static str,
    pub label: &'static str,
    pub needs_cookie: bool,
    pub known_sent: bool,
    pub enrichment: bool,
    pub match_url: fn(&str) -> bool,
    pub canonical_url: fn(&str) -> Option<String>,
    pub fetch: PlatformFetchFn,
    pub self_source: Option<PlatformSelfSourceFn>,
    pub media_enrich: Option<&'static MediaEnrich>,
}

pub const PLATFORMS: &[PlatformDef] = &[
    PlatformDef {
        id: "weibo",
        label: "Weibo",
        needs_cookie: true,
        known_sent: true,
        enrichment: true,
        match_url: weibo::is_weibo_url,
        canonical_url: |url| {
            weibo::parse_uid(url).map(|uid| format!("https://weibo.com/u/{}", uid))
        },
        fetch: |state, url, opts| Box::pin(weibo::fetch_by_url(state, url, opts)),
        self_source: None,
        media_enrich: Some(&weibo::MEDIA_ENRICH),
    },
    PlatformDef {
        id: "x",
        label: "X",
        needs_cookie: true,
        known_sent: false,
        enrichment: false,
        match_url: x::is_x_url,
        canonical_url: |url| {
            x::parse_screen_name(url)
                .map(|name| format!("https://x.com/{}", name.to_ascii_lowercase()))
        },
        fetch: |state, url, opts| Box::pin(x::fetch_by_url(state, url, opts)),
        self_source: None,
        media_enrich: None,
    },
    PlatformDef {
        id: "telegram",
        label: "Telegram",
        needs_cookie: false,
        known_sent: false,
        enrichment: false,
        match_url: telegram::is_telegram_url,
        canonical_url: |url| {
            telegram::parse_channel_name(url)
                .map(|ch| format!("https://t.me/{}", ch.to_ascii_lowercase()))
        },
        fetch: |state, url, opts| Box::pin(telegram::fetch_by_url(state, url, opts)),
        self_source: Some(|state, chat_id, url| {
            Box::pin(telegram::is_self_source(state, chat_id, url))
        }),
        media_enrich: None,
    },
];

pub fn find_platform(id: &str) -> Option<&'static PlatformDef> {
    PLATFORMS.iter().find(|p| p.id == id)
}

fn api_authority(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?.to_lowercase();
    let port = u.port_or_known_default().unwrap_or(80);
    Some(format!("{}://{}:{}", u.scheme(), host, port))
}

fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "cookie" | "authorization" | "x-csrf-token"
    )
}

pub(crate) async fn api_get(
    state: &AppState,
    url: &str,
    headers: &[(&str, String)],
    max_redirects: u32,
) -> anyhow::Result<reqwest::Response> {
    use anyhow::Context as _;

    let initial_authority = api_authority(url);
    let mut cur_url = url.to_string();
    let mut hops = 0u32;
    loop {
        match crate::ssrf::ssrf_check_literal(&cur_url) {
            crate::ssrf::SsrfOutcome::Private => {
                anyhow::bail!("blocked: private/loopback address: {}", cur_url);
            }
            crate::ssrf::SsrfOutcome::InvalidUrl => {
                anyhow::bail!("invalid platform URL: {}", cur_url);
            }
            _ => {}
        }
        if !state.is_routed_via_proxy(&cur_url) {
            match crate::ssrf::ssrf_check(&cur_url).await {
                crate::ssrf::SsrfOutcome::Private => {
                    anyhow::bail!("blocked: private/loopback address: {}", cur_url);
                }
                crate::ssrf::SsrfOutcome::LookupFailed => {
                    anyhow::bail!("dns resolve failed: {}", cur_url);
                }
                crate::ssrf::SsrfOutcome::InvalidUrl => {
                    anyhow::bail!("invalid platform URL: {}", cur_url);
                }
                crate::ssrf::SsrfOutcome::Allowed => {}
            }
        }

        let client = state.media_client_for_url(&cur_url);
        let same_authority = api_authority(&cur_url) == initial_authority;
        let mut req = client.get(&cur_url);
        for (name, value) in headers {
            if !same_authority && is_sensitive_header(name) {
                continue;
            }
            req = req.header(*name, value);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("platform request failed: {}", cur_url))?;

        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(resp);
        }
        if resp.status().is_redirection() && max_redirects > 0 {
            hops += 1;
            if hops > max_redirects {
                anyhow::bail!("too many redirects (>{}): {}", max_redirects, url);
            }
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow::anyhow!("redirect without Location header"))?;
            let next = url::Url::parse(&cur_url)
                .and_then(|base| base.join(loc))
                .map_err(|e| anyhow::anyhow!("bad redirect Location: {}", e))?;
            cur_url = next.to_string();
            continue;
        }
        return Ok(resp);
    }
}

pub fn platform_for_url(url: &str) -> Option<&'static PlatformDef> {
    PLATFORMS.iter().find(|p| (p.match_url)(url))
}

pub fn sort_entries_newest_first(entries: &mut [feed_rs::model::Entry]) {
    entries.sort_by(|a, b| {
        let ta = crate::timeutil::entry_timestamp(a);
        let tb = crate::timeutil::entry_timestamp(b);
        tb.cmp(&ta)
    });
}

pub fn push_unique_entry(
    entries: &mut Vec<feed_rs::model::Entry>,
    seen: &mut std::collections::HashSet<String>,
    entry: feed_rs::model::Entry,
) -> bool {
    if entry.id.is_empty() || !seen.insert(entry.id.clone()) {
        return false;
    }
    entries.push(entry);
    true
}

pub fn make_title(plain: &str, fallback: &str) -> String {
    let title: String = plain.chars().take(50).collect();
    let title = if plain.chars().count() > 50 {
        format!("{}…", title)
    } else {
        title
    };
    if title.trim().is_empty() {
        fallback.to_string()
    } else {
        title
    }
}

pub fn cookie_setting_key(platform: &str) -> String {
    crate::db::setting_key::cookie(platform)
}

pub async fn load_persisted_cookies(state: &AppState) {
    let rows = match crate::db::get_settings_by_prefix(
        &state.db,
        crate::db::setting_key::COOKIE_PREFIX,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "load_persisted_cookies: prefix query failed, cookies not loaded");
            return;
        }
    };
    for (key, value) in rows {
        let parts: Vec<&str> = key.splitn(2, ':').collect();
        if parts.len() == 2 {
            let platform = parts[1];
            if platform.contains(':') {
                continue;
            }
            if !value.trim().is_empty() {
                state.set_cookie(platform, &value).await;
            }
        }
    }
}

pub async fn migrate_legacy_cookies(state: &AppState) {
    let rows =
        match crate::db::get_settings_by_prefix(&state.db, crate::db::setting_key::COOKIE_PREFIX)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "migrate_legacy_cookies: prefix query failed");
                return;
            }
        };

    let mut legacy: Vec<(i64, String, String)> = Vec::new();
    for (key, value) in &rows {
        let parts: Vec<&str> = key.splitn(3, ':').collect();
        if parts.len() == 3 {
            if let Ok(uid) = parts[1].parse::<i64>() {
                legacy.push((uid, parts[2].to_string(), value.clone()));
            }
        }
    }

    if legacy.is_empty() {
        tracing::debug!(
            "migrate_legacy_cookies: no legacy cookies, running orphan-key cleanup only"
        );
    } else {
        tracing::info!(
            count = legacy.len(),
            "migrating legacy per-user cookies to global format"
        );

        let mut best: std::collections::HashMap<String, (i64, String)> =
            std::collections::HashMap::new();
        for (uid, platform, value) in &legacy {
            let is_admin = state.config.is_admin(*uid);
            let entry = best.entry(platform.clone());
            match entry {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((*uid, value.clone()));
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let cur_uid = e.get().0;
                    if is_admin && !state.config.is_admin(cur_uid) {
                        e.insert((*uid, value.clone()));
                    }
                }
            }
        }

        let mut migrated: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (platform, (_uid, value)) in &best {
            let global_key = cookie_setting_key(platform);
            let existing = crate::db::get_setting(&state.db, &global_key)
                .await
                .ok()
                .flatten();
            if existing.is_some() {
                migrated.insert(platform.clone());
            } else if !value.trim().is_empty() {
                match crate::db::set_setting(&state.db, &global_key, value).await {
                    Ok(()) => {
                        state.set_cookie(platform, value).await;
                        migrated.insert(platform.clone());
                        tracing::info!(platform = %platform, "migrated cookie to global format");
                    }
                    Err(e) => {
                        tracing::warn!(
                            platform = %platform,
                            error = %e,
                            "cookie migration write failed; keeping legacy key for retry"
                        );
                    }
                }
            } else {
                migrated.insert(platform.clone());
            }
        }

        for (key, _) in &rows {
            let parts: Vec<&str> = key.splitn(3, ':').collect();
            if parts.len() == 3 && migrated.contains(parts[2]) {
                crate::db::delete_setting(&state.db, key).await.ok();
            }
        }
    }

    for (key, _) in &rows {
        let parts: Vec<&str> = key.splitn(3, ':').collect();
        if parts.len() != 3 {
            continue;
        }
        let platform = parts[2];
        if crate::db::get_setting(&state.db, &cookie_setting_key(platform))
            .await
            .ok()
            .flatten()
            .is_some()
        {
            crate::db::delete_setting(&state.db, key).await.ok();
        }
    }

    let known_platforms: Vec<&str> = PLATFORMS.iter().map(|p| p.id).collect();
    for prefix in [
        crate::db::setting_key::PLATFORM_HEALTH_PREFIX,
        crate::db::setting_key::PLATFORM_STREAK_PREFIX,
        crate::db::setting_key::COOKIE_EXPIRED_NOTIFIED_PREFIX,
    ] {
        if let Ok(hrows) = crate::db::get_settings_by_prefix(&state.db, prefix).await {
            for (key, _) in &hrows {
                let rest = &key[prefix.len()..];
                if rest.contains(':') || !known_platforms.contains(&rest) {
                    crate::db::delete_setting(&state.db, key).await.ok();
                    tracing::info!(key = %key, "removed orphan settings key");
                }
            }
        }
    }
}

pub async fn update_cookie(state: &AppState, platform: &str, value: &str) -> anyhow::Result<()> {
    let value = value.trim();
    let cookie_key = cookie_setting_key(platform);
    let notified_key = crate::db::setting_key::cookie_expired_notified(platform);
    crate::db::set_settings_atomic(&state.db, &[(&cookie_key, value), (&notified_key, "0")])
        .await?;
    state.set_cookie(platform, value).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{api_authority, is_sensitive_header};

    #[test]
    fn authority_normalizes_case_and_default_ports() {
        assert_eq!(
            api_authority("https://X.com/path"),
            Some("https://x.com:443".to_string())
        );
        assert_eq!(
            api_authority("http://example.com:8080/x"),
            Some("http://example.com:8080".to_string())
        );
        assert_ne!(
            api_authority("https://example.com:8080/x"),
            api_authority("http://example.com:8080/x")
        );
        assert_ne!(
            api_authority("https://x.com/a"),
            api_authority("https://api.x.com/a")
        );
        assert_eq!(api_authority("not a url"), None);
    }

    #[test]
    fn sensitive_headers_are_case_insensitive() {
        for name in [
            "Cookie",
            "cookie",
            "Authorization",
            "X-CSRF-Token",
            "x-csrf-token",
        ] {
            assert!(is_sensitive_header(name), "{name}");
        }
        for name in ["Accept", "Referer", "User-Agent", "x-twitter-auth-type"] {
            assert!(!is_sensitive_header(name), "{name}");
        }
    }
}
