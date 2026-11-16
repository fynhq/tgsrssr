//! Shared runtime state: clients, semaphores, cookies and SSRF policy.
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, Semaphore};

use crate::config::Config;
use crate::db::Db;

pub const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/605.1.15 (KHTML, like Gecko) Version/27.0 Safari/605.1.15";

pub const MOBILE_BROWSER_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 27_0 like Mac OS X) \
    AppleWebKit/605.1.15 (KHTML, like Gecko) Version/27.0 Mobile/15E148 Safari/604.1";

pub const PLATFORM_COMMAND_CONCURRENCY: usize = 2;

enum RouteKind {
    Insecure,
    Direct,
    Proxy,
}

#[derive(Debug, Clone)]
pub enum PendingOp {
    RenameSub {
        sub_id: i64,
        prompt_msg_id: i32,
        panel_msg_id: i32,
        ch_page: usize,
        chlist_page: usize,
        created_at: std::time::Instant,
    },
    SetCookie {
        platform: String,
        prompt_msg_id: i32,
        panel_msg_id: i32,
        created_at: std::time::Instant,
    },
    SetFilterKeywords {
        sub_id: i64,
        prompt_msg_id: i32,
        panel_msg_id: i32,
        ch_page: usize,
        chlist_page: usize,
        created_at: std::time::Instant,
    },
}

impl PendingOp {
    pub fn prompt_msg_id(&self) -> i32 {
        match self {
            PendingOp::RenameSub { prompt_msg_id, .. } => *prompt_msg_id,
            PendingOp::SetCookie { prompt_msg_id, .. } => *prompt_msg_id,
            PendingOp::SetFilterKeywords { prompt_msg_id, .. } => *prompt_msg_id,
        }
    }

    pub fn created_at(&self) -> std::time::Instant {
        match self {
            PendingOp::RenameSub { created_at, .. } => *created_at,
            PendingOp::SetCookie { created_at, .. } => *created_at,
            PendingOp::SetFilterKeywords { created_at, .. } => *created_at,
        }
    }
}

pub const PENDING_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

pub const HOST_DOWNLOAD_CONCURRENCY: usize = 3;

const MAX_HOST_BUCKETS: usize = 512;

fn cap_host_buckets(map: &mut HashMap<String, Arc<Semaphore>>, host: &str) {
    if map.contains_key(host) || map.len() < MAX_HOST_BUCKETS {
        return;
    }
    map.retain(|k, sem| k == host || sem.available_permits() < HOST_DOWNLOAD_CONCURRENCY);
    if map.len() >= MAX_HOST_BUCKETS {
        if let Some(k) = map.keys().find(|k| *k != host).cloned() {
            map.remove(&k);
        }
    }
}

pub struct ActiveFlagGuard(pub Arc<AtomicBool>);

impl Drop for ActiveFlagGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    pub media: reqwest::Client,
    pub media_direct: reqwest::Client,
    pub media_insecure: reqwest::Client,
    pub media_trusted: reqwest::Client,
    pub media_trusted_insecure: reqwest::Client,
    pub bot_username: String,
    pub platform_command_semaphore: Arc<Semaphore>,
    pub command_rate_limiter: Arc<crate::limits::CommandRateLimiter>,
    pub user_sub_locks: Arc<Mutex<HashMap<i64, Arc<Mutex<()>>>>>,
    pub fetch_semaphore: Arc<Semaphore>,
    pub download_semaphore: Arc<Semaphore>,
    pub media_budget: Arc<crate::media::MediaBudget>,
    pub host_download_semaphores: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    pub fetch_cycle_active: Arc<AtomicBool>,
    pub outbox_retry_active: Arc<AtomicBool>,
    pub cleanup_active: Arc<AtomicBool>,
    pub pending_ops: Arc<Mutex<HashMap<(i64, i64), PendingOp>>>,
    pub unsuball_inflight: Arc<std::sync::Mutex<std::collections::HashSet<(i64, i32)>>>,
    pub cookies: Arc<RwLock<HashMap<String, String>>>,
    pub weibo_visitor_cookie: Arc<Mutex<Option<(String, std::time::Instant)>>>,
    pub weibo_visitor_cookie_fetch: Arc<tokio::sync::Mutex<()>>,
}

impl AppState {
    pub fn new(config: Config, db: Db, bot_username: String) -> anyhow::Result<Self> {
        fn build_client(
            proxy: Option<&str>,
            insecure: bool,
            follow_redirects: bool,
            ssrf_guard: bool,
        ) -> anyhow::Result<reqwest::Client> {
            let mut b = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .read_timeout(std::time::Duration::from_secs(30))
                .pool_max_idle_per_host(2)
                .pool_idle_timeout(std::time::Duration::from_secs(30))
                .user_agent(BROWSER_UA);
            if insecure {
                b = b.danger_accept_invalid_certs(true);
            }
            if !follow_redirects {
                b = b.redirect(reqwest::redirect::Policy::none());
            }
            if ssrf_guard {
                b = b.dns_resolver(crate::ssrf::GuardedResolver);
            }
            if let Some(p) = proxy {
                b = b.proxy(reqwest::Proxy::all(p)?);
            } else {
                b = b.no_proxy();
            }
            #[cfg(target_os = "android")]
            {
                let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
                    .iter()
                    .map(|der| reqwest::Certificate::from_der(der.as_ref()))
                    .collect::<Result<Vec<_>, _>>()?;
                b = b.tls_certs_only(roots);
            }
            Ok(b.build()?)
        }

        let proxy = config.rss_proxy.as_deref();
        let has_proxy = proxy.is_some();

        let media_direct = build_client(None, false, false, true)?;
        let media = if has_proxy {
            build_client(proxy, false, false, false)?
        } else {
            media_direct.clone()
        };
        let media_insecure = build_client(proxy, true, false, !has_proxy)?;
        let media_trusted = build_client(None, false, false, false)?;
        let media_trusted_insecure = build_client(None, true, false, false)?;

        Ok(Self {
            fetch_semaphore: Arc::new(Semaphore::new(crate::config::FETCH_CONCURRENCY)),
            download_semaphore: Arc::new(Semaphore::new(crate::config::DOWNLOAD_CONCURRENCY)),
            media_budget: Arc::new(crate::media::MediaBudget::new()),
            host_download_semaphores: Arc::new(Mutex::new(HashMap::new())),
            fetch_cycle_active: Arc::new(AtomicBool::new(false)),
            outbox_retry_active: Arc::new(AtomicBool::new(false)),
            cleanup_active: Arc::new(AtomicBool::new(false)),
            cookies: Arc::new(RwLock::new(HashMap::new())),
            weibo_visitor_cookie: Arc::new(Mutex::new(None)),
            weibo_visitor_cookie_fetch: Arc::new(tokio::sync::Mutex::new(())),
            config: Arc::new(config),
            db,
            bot_username,
            media,
            media_direct,
            media_insecure,
            media_trusted,
            media_trusted_insecure,
            platform_command_semaphore: Arc::new(Semaphore::new(PLATFORM_COMMAND_CONCURRENCY)),
            command_rate_limiter: Arc::new(crate::limits::CommandRateLimiter::new()),
            user_sub_locks: Arc::new(Mutex::new(HashMap::new())),
            pending_ops: Arc::new(Mutex::new(HashMap::new())),
            unsuball_inflight: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        })
    }

    pub fn is_trusted_private_url(&self, url: &str) -> bool {
        trusted_private_match(&self.config.rss_trusted_private_hosts, url)
    }

    pub async fn user_sub_lock(&self, user_id: i64) -> Arc<Mutex<()>> {
        let mut m = self.user_sub_locks.lock().await;
        if m.len() > 1024 {
            m.retain(|_, lock| Arc::strong_count(lock) > 1);
        }
        m.entry(user_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub async fn check_target(
        &self,
        url: &str,
        allow_trusted: bool,
        kind: &str,
    ) -> anyhow::Result<()> {
        if allow_trusted && self.is_trusted_private_url(url) {
            return Ok(());
        }
        let via_proxy = self.is_routed_via_proxy(url);
        match crate::ssrf::ssrf_check(url).await {
            crate::ssrf::SsrfOutcome::Allowed => Ok(()),
            crate::ssrf::SsrfOutcome::Private => {
                anyhow::bail!("blocked: private/loopback address: {}", url)
            }
            crate::ssrf::SsrfOutcome::LookupFailed if via_proxy => {
                let host = url::Url::parse(url)
                    .ok()
                    .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
                    .unwrap_or_default();
                if crate::ssrf::proxy_host_denied(&host) {
                    anyhow::bail!("blocked: internal-looking host via proxy: {}", host)
                }
                tracing::warn!(
                    url = %url,
                    kind = kind,
                    "local DNS pre-check failed; proceeding via proxy (veto policy)"
                );
                Ok(())
            }
            crate::ssrf::SsrfOutcome::LookupFailed => {
                anyhow::bail!("dns resolve failed: {}", url)
            }
            crate::ssrf::SsrfOutcome::InvalidUrl => {
                anyhow::bail!("invalid {} URL: {}", kind, url)
            }
        }
    }

    pub fn media_client_for_url_trusted(&self, url: &str, allow_trusted: bool) -> &reqwest::Client {
        self.media_route_for_url_trusted(url, allow_trusted).0
    }

    pub fn media_route_for_url_trusted(
        &self,
        url: &str,
        allow_trusted: bool,
    ) -> (&reqwest::Client, bool) {
        if allow_trusted && self.is_trusted_private_url(url) {
            let host = url::Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
                .unwrap_or_default();
            if crate::config::host_matches_exact(&host, &self.config.rss_insecure_hosts) {
                return (&self.media_trusted_insecure, false);
            }
            return (&self.media_trusted, false);
        }
        self.media_route_for_url(url)
    }

    pub async fn host_download_semaphore(&self, url: &str) -> Arc<Semaphore> {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
            .unwrap_or_else(|| "default".to_string());
        let mut m = self.host_download_semaphores.lock().await;
        cap_host_buckets(&mut m, &host);
        m.entry(host)
            .or_insert_with(|| Arc::new(Semaphore::new(HOST_DOWNLOAD_CONCURRENCY)))
            .clone()
    }

    pub async fn get_cookie(&self, platform: &str) -> Option<String> {
        self.cookies.read().await.get(platform).cloned()
    }

    pub async fn set_cookie(&self, platform: &str, value: &str) {
        let mut w = self.cookies.write().await;
        if value.trim().is_empty() {
            w.remove(platform);
        } else {
            w.insert(platform.to_string(), value.trim().to_string());
        }
    }

    fn route_kind(&self, url: &str) -> (RouteKind, bool) {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
            .unwrap_or_default();

        if crate::config::host_matches_exact(&host, &self.config.rss_insecure_hosts) {
            return (RouteKind::Insecure, self.config.rss_proxy.is_some());
        }

        if self.config.rss_proxy.is_none() {
            return (RouteKind::Direct, false);
        }
        if crate::config::host_in_list(&host, &self.config.rss_no_proxy) {
            (RouteKind::Direct, false)
        } else {
            (RouteKind::Proxy, true)
        }
    }

    pub fn media_client_for_url(&self, url: &str) -> &reqwest::Client {
        self.media_route_for_url(url).0
    }

    pub fn media_route_for_url(&self, url: &str) -> (&reqwest::Client, bool) {
        let (kind, via_proxy) = self.route_kind(url);
        let client = match kind {
            RouteKind::Insecure => &self.media_insecure,
            RouteKind::Direct => &self.media_direct,
            RouteKind::Proxy => &self.media,
        };
        (client, via_proxy)
    }

    pub fn is_routed_via_proxy(&self, url: &str) -> bool {
        self.route_kind(url).1
    }
}

// The trusted private allowlist is exact host:port; subdomains never inherit an entry.
fn trusted_private_match(hosts: &[String], url: &str) -> bool {
    if hosts.is_empty() {
        return false;
    }
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    if u.scheme() != "http" && u.scheme() != "https" {
        return false;
    }
    let Some(raw_host) = u.host_str() else {
        return false;
    };
    let host = raw_host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    let port = u.port_or_known_default().unwrap_or(80);
    let target = format!("{}:{}", host, port);
    hosts.iter().any(|entry| entry == &target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket() -> Arc<Semaphore> {
        Arc::new(Semaphore::new(HOST_DOWNLOAD_CONCURRENCY))
    }

    #[tokio::test]
    async fn platform_command_semaphore_capacity_is_two() {
        let sem = Arc::new(Semaphore::new(PLATFORM_COMMAND_CONCURRENCY));
        let first = sem.acquire().await.unwrap();
        let second = sem.acquire().await.unwrap();
        assert!(sem.try_acquire().is_err(), "third platform fetch must wait");
        drop(first);
        assert!(
            sem.try_acquire().is_ok(),
            "released permit must be reusable"
        );
        drop(second);
    }

    #[test]
    fn trusted_private_match_is_exact_host_port() {
        let hosts = vec![
            "127.0.0.1:1200".to_string(),
            "rsshub:1200".to_string(),
            "host.docker.internal:1200".to_string(),
        ];
        assert!(trusted_private_match(
            &hosts,
            "http://127.0.0.1:1200/weibo/user/1"
        ));
        assert!(trusted_private_match(&hosts, "http://RSSHUB:1200/feed"));
        assert!(trusted_private_match(
            &hosts,
            "https://host.docker.internal:1200/x"
        ));
        assert!(!trusted_private_match(&hosts, "http://127.0.0.1:1201/x"));
        assert!(!trusted_private_match(&hosts, "http://127.0.0.2:1200/x"));
        assert!(!trusted_private_match(&hosts, "ftp://127.0.0.1:1200/x"));
        assert!(!trusted_private_match(&hosts, "http://sub.rsshub:1200/x"));
        let default_ports = vec![
            "example.internal:443".to_string(),
            "plain.internal:80".to_string(),
        ];
        assert!(trusted_private_match(
            &default_ports,
            "https://example.internal/x"
        ));
        assert!(trusted_private_match(
            &default_ports,
            "http://plain.internal/x"
        ));
        assert!(!trusted_private_match(&[], "http://127.0.0.1:1200/x"));
        assert!(!trusted_private_match(&hosts, "not a url"));
    }

    fn fill(map: &mut HashMap<String, Arc<Semaphore>>, n: usize, busy: bool) {
        for i in 0..n {
            let sem = bucket();
            if busy {
                let permit = sem.try_acquire().unwrap();
                std::mem::forget(permit);
            }
            map.insert(format!("host-{}", i), sem);
        }
    }

    #[test]
    fn cap_below_limit_is_noop() {
        let mut m = HashMap::new();
        fill(&mut m, 4, false);
        cap_host_buckets(&mut m, "new-host");
        assert_eq!(m.len(), 4, "below-cap calls must not evict");
    }

    #[test]
    fn cap_evicts_only_idle_buckets() {
        let mut m = HashMap::new();
        for i in 0..MAX_HOST_BUCKETS / 2 {
            let sem = bucket();
            let permit = sem.try_acquire().unwrap();
            std::mem::forget(permit);
            m.insert(format!("busy-{}", i), sem);
        }
        fill(&mut m, MAX_HOST_BUCKETS / 2, false);
        assert_eq!(m.len(), MAX_HOST_BUCKETS);
        cap_host_buckets(&mut m, "new-host");
        assert_eq!(m.len(), MAX_HOST_BUCKETS / 2);
        assert!(m
            .values()
            .all(|s| s.available_permits() < HOST_DOWNLOAD_CONCURRENCY));
        assert!(m.len() < MAX_HOST_BUCKETS);
    }

    #[test]
    fn cap_all_busy_evicts_exactly_one() {
        let mut m = HashMap::new();
        fill(&mut m, MAX_HOST_BUCKETS, true);
        cap_host_buckets(&mut m, "new-host");
        assert_eq!(m.len(), MAX_HOST_BUCKETS - 1);
    }

    #[test]
    fn cap_existing_host_never_evicts() {
        let mut m = HashMap::new();
        fill(&mut m, MAX_HOST_BUCKETS, false);
        cap_host_buckets(&mut m, "host-0");
        assert_eq!(
            m.len(),
            MAX_HOST_BUCKETS,
            "requests to cached hosts must not evict"
        );
    }
}
