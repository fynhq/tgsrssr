use std::sync::LazyLock;

use anyhow::Context;
use feed_rs::model::Entry;
use url::Url;

use crate::htmltext::{truncate_text, unescape_html, RE_TAG};
use crate::i18n::{self, Lang};
use crate::state::AppState;

/// Static regexes: the HTML→Telegram-HTML conversion runs on every entry,
/// so compile once and reuse uniformly (previously every call recompiled a
/// dozen or so regexes).
/// RE_ENTITY_NUM/RE_ENTITY_HEX moved into crate::htmltext together with
/// unescape_html. RE_TAG is likewise shared from htmltext (single compiled
/// copy of `<[^>]+>` across the project).
static RE_AUTHOR_PAREN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\(([^)]+)\)").unwrap());
pub static RE_BLANK_LINES: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\n{3,}").unwrap());
/// Line-leading indentation whitespace (HTML formatting indentation from
/// sources like RSSHub, left over after strip_tags).
static RE_LINE_INDENT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?m)^[ \t]+").unwrap());
/// Fold runs of horizontal whitespace into a single space.
static RE_MULTI_SPACE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"[ \t]{2,}").unwrap());
pub static RE_VIDEO_BLOCK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?is)<video\b[^>]*>.*?</video>").unwrap());
pub static RE_IMG_ALT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?is)<img\b[^>]*?\balt\s*=\s*["']([^"']*)["'][^>]*>"#).unwrap());
pub static RE_IMG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?is)<img\b[^>]*>").unwrap());
static RE_SOURCE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?is)<source\b[^>]*>").unwrap());
static RE_VIDEO_FALLBACK_TEXT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"视频无法显示[，,]?\s*请前往.*?观看[。.]?").unwrap());
/// Non-rendered elements whose text is source code, not content: `<style>`
/// CSS and `<script>` JS. Some feeds (e.g. Bear Blog) inline per-post style
/// blocks; plain tag stripping used to leave the CSS in the visible message
/// and in the 140-char summary.
/// A complete block is removed as a whole; an unterminated block is removed to
/// the end of the input (mirroring HTML parsing), because `entry_summary` cuts
/// the raw body at 256KB and can drop the closing tag. The open-tag tail
/// excludes a trailing `/` so a self-closing `<style/>` falls through to the
/// ordinary tag strip instead of swallowing the rest of the body.
static RE_NON_RENDERED_BLOCK: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"(?is)<style\b[^>]*>.*?</style\s*>",
        r"|<script\b[^>]*>.*?</script\s*>",
        r"|<style\b(?:[^>]*[^/])?\s*>.*",
        r"|<script\b(?:[^>]*[^/])?\s*>.*",
    ))
    .unwrap()
});
static RE_WEIBO_VIDEO_CARD_TEXT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"[\w\u4e00-\u9fff]+的微博视频").unwrap());
pub static RE_BR: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)<br\s*/?>").unwrap());
/// Block-level closing tags whose boundaries must survive tag stripping.
/// Without this, `<li>a</li><li>b</li>` flattens to `ab` and table rows run
/// together. `<blockquote>`/`<pre>` are intentionally excluded: they have
/// their own sentinel/atomic handling.
pub static RE_BLOCK_CLOSE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)</(?:p|div|li|tr|td|th|h[1-6]|table|thead|tbody|ul|ol|section|article|header|footer|figure|figcaption)\s*>",
    )
    .unwrap()
});
static RE_BLOCKQUOTE_OPEN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)<blockquote\b[^>]*>").unwrap());
static RE_BLOCKQUOTE_CLOSE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)</blockquote\s*>").unwrap());
static RE_ANCHOR: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?is)<a\b[^>]*?href\s*=\s*["']([^"']*)["'][^>]*>(.*?)</a>"#).unwrap());
/// Bare URLs that Telegram would otherwise auto-linkify. The character class
/// is deliberately ASCII-only so it stops before CJK text/punctuation even
/// when the source text has no space after the URL.
const BARE_URL_CHARS: &str = r"A-Za-z0-9\-._~:/?#\[\]@!$&()*+,;=%";
/// Bare URLs that Telegram would otherwise auto-linkify. Domain-only strings
/// such as `example.com` are intentionally not matched (they are also common
/// in file/module names); `www.` and `http(s)://` forms remain supported.
static RE_BARE_URL: LazyLock<regex::Regex> = LazyLock::new(|| {
    let pattern = format!(
        r"(?i)(?:https?://[{c}]+|www\.[{c}]+|t\.cn/[{c}]+|(?:[a-z0-9-]+\.)+(?:[a-z]{{2,}})(?::[0-9]{{1,5}})?/[{c}]*)",
        c = BARE_URL_CHARS
    );
    regex::Regex::new(&pattern).unwrap()
});
/// Host suffixes that are much more likely to be file names than domains.
/// Bare `foo.txt/bar` must not become a link; explicit `https://foo.txt/bar`
/// is still respected because the scheme is an unambiguous URL intent.
const BARE_URL_FILE_EXT: &[&str] = &[
    "txt", "md", "markdown", "rs", "js", "mjs", "cjs", "ts", "tsx", "jsx", "css", "html",
    "htm", "xml", "json", "toml", "yaml", "yml", "png", "jpg", "jpeg", "gif", "webp", "svg",
    "bmp", "ico", "mp4", "mov", "webm", "mkv", "mp3", "wav", "flac", "ogg", "pdf", "zip",
    "tar", "gz", "tgz", "bz2", "xz", "exe", "dll", "so", "dylib", "apk", "ipa", "csv",
    "tsv", "log", "lock", "db", "sqlite",
];

/// Return the prefix of a regex candidate that should become the link, trimming
/// prose punctuation that commonly sticks to the URL. CJK punctuation never
/// reaches this point because it is excluded from BARE_URL_CHARS.
fn trim_bare_url_tail(raw: &str) -> &str {
    fn count(s: &str, needle: char) -> usize {
        s.chars().filter(|c| *c == needle).count()
    }
    let mut end = raw.len();
    loop {
        let s = &raw[..end];
        let Some(last) = s.chars().next_back() else {
            break;
        };
        let trim = match last {
            '.' | ',' | ';' | ':' | '!' | '&' => true,
            ')' => count(s, ')') > count(s, '('),
            ']' => count(s, ']') > count(s, '['),
            '}' => count(s, '}') > count(s, '{'),
            _ => false,
        };
        if !trim {
            break;
        }
        end -= last.len_utf8();
    }
    &raw[..end]
}

/// Avoid linkifying a suffix of an email, another URL, or a word: for example
/// the `example.com/path` part after `@` in `user@example.com/path`.
fn bare_url_can_start(text: &str, start: usize) -> bool {
    match text[..start].chars().next_back() {
        Some(c) => !(c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '.' | '/' | '-')),
        None => true,
    }
}

/// Normalize a bare URL candidate: prefix `https://` when the source omits the
/// scheme, validate it, and reject common file-looking host suffixes.
fn normalize_bare_url(raw: &str) -> Option<(String, String)> {
    let lower = raw.to_ascii_lowercase();
    let explicit = lower.starts_with("http://") || lower.starts_with("https://");
    let candidate = if explicit {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let parsed = Url::parse(&candidate).ok()?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return None;
    }
    let host = parsed.host_str()?;
    if host.is_empty() {
        return None;
    }
    if !explicit {
        let last = host.rsplit('.').next()?.to_ascii_lowercase();
        if BARE_URL_FILE_EXT.iter().any(|ext| *ext == last) {
            return None;
        }
    }
    Some((candidate, raw.to_string()))
}

/// Replace bare URLs with internal link tokens (indices into `links`), so the
/// existing anchor restore pass turns them into explicit Telegram `<a>` tags.
/// Operating before escaping also keeps `&` in query strings correct.
fn linkify_bare_urls(text: &str, links: &mut Vec<(String, String)>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for m in RE_BARE_URL.find_iter(text) {
        if !bare_url_can_start(text, m.start()) {
            continue;
        }
        let raw = m.as_str();
        let trimmed = trim_bare_url_tail(raw);
        if trimmed.is_empty() {
            continue;
        }
        let Some((href, display)) = normalize_bare_url(trimmed) else {
            continue;
        };
        out.push_str(&text[last..m.start()]);
        let idx = links.len();
        links.push((href, display));
        out.push_str(&format!("\u{FFFC}{idx}\u{FFFC}"));
        // Leave trailing prose punctuation outside the anchor.
        last = m.start() + trimmed.len();
    }
    out.push_str(&text[last..]);
    out
}

/// Result of fetching a single feed.
pub struct FetchResult {    pub feed_title: Option<String>,
    /// Entries sorted newest-first.
    pub entries: Vec<Entry>,
    /// Server returned 304 (source not updated): nothing to push; callers
    /// treat this as success.
    pub not_modified: bool,
    /// Negotiation headers carried by this response, stored in the DB for the
    /// next conditional request.
    pub http_etag: Option<String>,
    pub http_last_modified: Option<String>,
    /// Structured entry metadata (sidecar, group-C refactor):
    /// entry_id → EntryMeta. Replaces the in-body data-vote/data-rt markers
    /// and the content_type MIME secret handshake; populated on demand by
    /// platform sources, always empty for RSS sources.
    pub metas: crate::entrymeta::MetaMap,
}

impl FetchResult {
    /// Platform-source constructor (F6 convergence): native fetching does not
    /// go through HTTP conditional requests, so the negotiation headers are
    /// always empty. The three platforms previously duplicated the same
    /// literal verbatim; funneling them here prevents field drift.
    pub fn platform(feed_title: Option<String>, entries: Vec<Entry>) -> Self {
        Self {
            feed_title,
            entries,
            not_modified: false,
            http_etag: None,
            http_last_modified: None,
            metas: std::collections::HashMap::new(),
        }
    }
}

/// Platform-only enrichment depth. RSS sources ignore this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enrichment {
    /// Fetch per-entry detail/vote data when the platform supports it.
    Full,
    /// Timeline data only. Used by `/sub`, which marks the whole visible
    /// window as sent and therefore does not need detail enrichment.
    None,
}

impl Enrichment {
    pub fn is_full(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// One fetch request. Bundling the knobs keeps the platform fetch signature
/// stable and lets `/sub`/`/test`/scheduler express their differences
/// explicitly (conditional headers, sent-entry hint, enrichment depth).
pub struct FetchOptions<'a> {
    pub etag: Option<&'a str>,
    pub last_modified: Option<&'a str>,
    pub known_sent: Option<&'a std::collections::HashSet<String>>,
    pub lang: Lang,
    pub allow_trusted_private: bool,
    pub enrichment: Enrichment,
}

/// Normalize a feed URL to a canonical form so that logically identical
/// sources (different host/path variants) deduplicate correctly at subscribe
/// time. Platform sources are rewritten via the platform registry's
/// `canonical_url`; plain RSS URLs get a conservative normalization (see
/// `normalize_plain_feed_url`).
///
/// Canonical forms:
/// - weibo: `https://weibo.com/u/<uid>` (m./www./weibo.cn variants collapse)
/// - x:     `https://x.com/<name>` (twitter.com / www / mobile collapse)
/// - tg:    `https://t.me/<name>` (telegram.me and /s/<name> collapse)
/// - other: scheme/host lowercased, scheme's default port removed
pub fn normalize_feed_url(url: &str) -> String {
    match crate::platforms::platform_for_url(url) {
        Some(p) => (p.canonical_url)(url).unwrap_or_else(|| url.to_string()),
        None => normalize_plain_feed_url(url),
    }
}

/// Conservative normalization for non-platform feed URLs: lowercase the
/// scheme and host and drop the scheme's default port (`http:80` /
/// `https:443`). Path, query and fragment are preserved byte-for-byte:
/// paths can be case-sensitive on the server, and trailing-slash or query
/// reordering can change the served resource, so only spellings that cannot
/// denote different resources are merged. Existing subscription rows keep
/// their stored form; this only dedups new /sub input.
fn normalize_plain_feed_url(url: &str) -> String {
    let Some(sep) = url.find("://") else {
        return url.to_string();
    };
    let scheme = url[..sep].to_ascii_lowercase();
    let after = &url[sep + 3..];
    // The authority ends at the first '/', '?' or '#'.
    let auth_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let authority = &after[..auth_end];
    let rest = &after[auth_end..];
    // Keep userinfo verbatim; normalize only scheme/host/port.
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (&authority[..=i], &authority[i + 1..]),
        None => ("", authority),
    };
    let (host, port) = if hostport.starts_with('[') {
        // IPv6 literal: the port, if present, follows the closing bracket.
        match hostport.find(']') {
            Some(i) => (&hostport[..=i], hostport[i + 1..].strip_prefix(':').unwrap_or("")),
            None => (hostport, ""),
        }
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h, p),
            None => (hostport, ""),
        }
    };
    let host = host.to_ascii_lowercase();
    let is_default_port =
        (scheme == "http" && port == "80") || (scheme == "https" && port == "443");
    let mut out = String::with_capacity(url.len());
    out.push_str(&scheme);
    out.push_str("://");
    out.push_str(userinfo);
    out.push_str(&host);
    if !port.is_empty() && !is_default_port {
        out.push(':');
        out.push_str(port);
    }
    out.push_str(rest);
    out
}

/// Response body size cap (20MB): prevents abnormal/malicious sources from
/// exhausting memory with huge bodies. RSS fetching and Telegram channel page
/// fetching share this same criterion.
pub const MAX_BODY_BYTES: u64 = 20 * 1024 * 1024;

/// Upper bound on entries retained from one RSS/Atom body. A hostile or
/// excessively large feed otherwise keeps a huge Vec alive through the whole
/// media/send loop (up to `FETCH_CONCURRENCY` feeds at once). Newest
/// entries are kept after the normal newest-first sort.
pub const MAX_ENTRIES_PER_FEED: usize = 500;

/// Stream-read a response body while enforcing the size cap (M8 fix: the
/// fetcher and telegram.rs previously duplicated the same bytes_stream loop
/// verbatim in two places). Checks while reading and aborts as soon as the
/// limit is exceeded — does not rely on a Content-Length pre-check (that
/// pre-check is ineffective for chunked responses without the header).
/// `ctx` identifies the source in error messages (e.g. feed URL or channel
/// name).
pub async fn read_body_capped(resp: reqwest::Response, ctx: &str) -> anyhow::Result<Vec<u8>> {
    let content_length = resp.content_length().unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        anyhow::bail!("response too large ({} bytes) for {}", content_length, ctx);
    }
    // Pre-size from Content-Length (checked against the cap above) so a large
    // body does not pay the repeated reallocation/doubling peak. Chunked
    // responses without the header still grow dynamically and are checked
    // while reading below.
    let mut body_buf: Vec<u8> = Vec::with_capacity(content_length as usize);
    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("read body chunk failed for {}", ctx))?;
        body_buf.extend_from_slice(&chunk);
        if body_buf.len() as u64 > MAX_BODY_BYTES {
            anyhow::bail!("response too large (>{} bytes) for {}", MAX_BODY_BYTES, ctx);
        }
    }
    // Chunked responses without Content-Length grow by doubling, so capacity
    // can be up to ~2x the body. Trim it before the parser runs: the raw body
    // and the parsed feed are alive together, and the excess capacity would
    // otherwise multiply across concurrent fetches.
    body_buf.shrink_to_fit();
    Ok(body_buf)
}

/// Read a response body under the size cap (via `read_body_capped`) and parse
/// it as JSON. Platform fetch code previously called `resp.json()` directly,
/// which bypasses MAX_BODY_BYTES and lets a hostile/oversized API response
/// buffer unbounded. This routes every platform JSON read through the same cap
/// the RSS pipeline uses — keep it that way: new platform JSON calls must use
/// this helper, never bare `resp.json()`. `ctx` identifies the source in
/// error messages.
pub async fn read_json_capped(resp: reqwest::Response, ctx: &str) -> anyhow::Result<serde_json::Value> {
    let bytes = read_body_capped(resp, ctx).await?;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("JSON parse failed for {}", ctx))?;
    Ok(json)
}

/// Fetch and parse an RSS/Atom/JSON feed.
/// `opts` carries the saved ETag / Last-Modified for a conditional request
/// (a 304 means the source has not updated) plus platform-routing hints
/// (`known_sent`, `lang`, `enrichment`).
pub async fn fetch_feed(
    state: &AppState,
    feed_url: &str,
    opts: &FetchOptions<'_>,
) -> anyhow::Result<FetchResult> {
    // Native platform fetch (weibo / X / Telegram): bypass the RSS/HTTP
    // pipeline entirely. Registry-driven — new platforms route automatically.
    // `opts.known_sent` lets platform fetchers skip enrichment requests for
    // entries the scheduler already pushed (currently Weibo long-text/vote
    // detail; X ignores the snapshot and always walks the cursor).
    // `opts.lang` selects the owner's language for user-visible fallback
    // labels; `opts.enrichment` lets /sub skip detail enrichment entirely.
    if let Some(p) = crate::platforms::platform_for_url(feed_url) {
        return (p.fetch)(state, feed_url.to_string(), opts).await;
    }

    let _permit = state.fetch_semaphore.acquire().await
        .context("fetch semaphore closed")?;

    // SSRF protection (entry point): /sub and /test accept arbitrary URLs,
    // and the bot runs in host network mode; without validation, whitelisted
    // users could use the bot to probe the host's intranet ports / cloud
    // metadata.
    //
    // Policy (state::check_target):
    // - URL on the admin-scoped trusted private allowlist (exact host:port,
    //   allowed callers only): skip all checks and use the dedicated direct
    //   client that also bypasses the connection-layer resolver.
    // - Direct route: literal + local DNS checks; a DNS failure is fatal
    //   (`dns resolve failed`, matched by sendpolicy::is_net_error).
    // - Proxy route (veto): literal private/loopback targets are rejected
    //   (the proxy may reach the host's loopback/LAN), a local DNS lookup
    //   failure is logged and the proxy is still attempted. This covers the
    //   case where the local resolver cannot resolve a domain that the proxy
    //   can (v1.10.14: avoid false blocks on CDN hostnames).
    //
    // The check runs on the entry URL and on every redirect hop, immediately
    // before each request (inside the shared httpfollow helper below).

    // RSS feed fetching does NOT go through img-relay (that's only for media).
    // Disable client auto-redirects (SSRF protection): the shared helper
    // follows manually hop by hop and re-runs the target check on every hop,
    // preventing a malicious source from 302-ing to an intranet address to
    // bypass the entry check.
    const MAX_REDIRECTS: u32 = 5;
    let resp = crate::httpfollow::get_follow_redirects(
        state,
        feed_url,
        opts.allow_trusted_private,
        None,
        "feed",
        MAX_REDIRECTS,
        |client, url, _hop| {
            let mut req = client.get(url).header(
                "Accept",
                "application/rss+xml, application/atom+xml, application/xml, text/xml, */*",
            );
            // The saved ETag/Last-Modified belong to the final resource, so
            // they must be sent on every hop: a redirected feed (http→https,
            // short links, RSSHub front pages) would otherwise never receive
            // its conditional headers and would always transfer the full body.
            if let Some(e) = opts.etag {
                req = req.header("If-None-Match", e);
            }
            if let Some(lm) = opts.last_modified {
                req = req.header("If-Modified-Since", lm);
            }
            req
        },
    )
    .await?;

    // 304 Not Modified: the source has no updates
    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(FetchResult {
            feed_title: None,
            entries: Vec::new(),
            not_modified: true,
            // A 304 response may not carry the headers; keep the old values
            http_etag: None,
            http_last_modified: None,
            metas: std::collections::HashMap::new(),
        });
    }

    if !resp.status().is_success() {
        anyhow::bail!("HTTP {} for {}", resp.status(), feed_url);
    }

    // Capture this response's negotiation headers (only overwrite the old
    // values when non-empty)
    let http_etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    let http_last_modified = resp
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());

    // Response body size cap (20MB): stream-read and abort as soon as the
    // limit is exceeded (read_body_capped).
    let bytes = read_body_capped(resp, feed_url).await?;

    let feed = feed_rs::parser::parse(&bytes[..])
        .with_context(|| format!("parse failed: {}", feed_url))?;
    // The parser returns an owned model; release the raw body now instead of
    // keeping it alive alongside the parsed entries for the rest of the call.
    drop(bytes);

    let feed_title = feed.title.map(|t| t.content).filter(|s| !s.trim().is_empty());

    // Sort entries newest first by published/updated date, then bound the
    // retained set. The parser has already built the full Vec (feed-rs is not
    // streaming), but truncating here keeps the send loop and the pending
    // batch small instead of retaining every historical entry.
    let mut entries = feed.entries;
    crate::platforms::sort_entries_newest_first(&mut entries);
    if entries.len() > MAX_ENTRIES_PER_FEED {
        tracing::warn!(
            feed = %feed_url,
            total = entries.len(),
            kept = MAX_ENTRIES_PER_FEED,
            "feed has too many entries; keeping the newest window"
        );
        entries.truncate(MAX_ENTRIES_PER_FEED);
    }

    Ok(FetchResult {
        feed_title,
        entries,
        not_modified: false,
        http_etag,
        http_last_modified,
        metas: std::collections::HashMap::new(),
    })
}

/// Apply image-relay / proxy resolution to a media URL if it matches the whitelist.
pub fn resolve_url(state: &AppState, raw: &str) -> String {
    let cfg = &state.config;
    if cfg.img_relay_base_url.is_empty() {
        return raw.to_string();
    }
    if let Ok(u) = Url::parse(raw) {
        let host = u.host_str().unwrap_or_default().to_lowercase();
        // Single host-matching implementation shared with proxy/insecure
        // routing (trim leading dot, lowercase, exact-or-subdomain).
        if crate::config::host_in_list(&host, &cfg.img_relay_domains) {
            return format!("{}/{}", cfg.img_relay_base_url.trim_end_matches('/'), raw);
        }
    }
    raw.to_string()
}

/// Extract a stable unique id for an entry.
pub fn entry_id(entry: &Entry) -> String {
    if !entry.id.trim().is_empty() {
        return entry.id.clone();
    }
    if let Some(l) = &entry.links.first() {
        return l.href.clone();
    }
    // Fallback: title + published timestamp + body hash. The hash keeps
    // distinct entries with the same title and no timestamp from sharing a
    // dedup key (the old `t:title:` key made one of them skip forever).
    let title = entry.title.as_ref().map(|t| t.content.clone()).unwrap_or_default();
    let ts = entry.published.or(entry.updated)
        .map(|d| d.timestamp().to_string())
        .unwrap_or_default();
    // Borrow the body: cloning it here would double a potentially large entry
    // body just to compute the fallback id (called several times per entry in
    // the fetch filter loop).
    let body: &str = entry
        .content
        .as_ref()
        .and_then(|c| c.body.as_deref())
        .or_else(|| entry.summary.as_ref().map(|s| s.content.as_str()))
        .unwrap_or("");
    let hash: String = {
        use sha2::{Digest, Sha256};
        Sha256::digest(body.as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{:02x}", b))
            .collect()
    };
    format!("t:{}:{}:{}", title, ts, hash)
}

/// Extract entry link.
pub fn entry_link(entry: &Entry) -> Option<String> {
    // Some feeds carry an empty/whitespace href; treating it as a link would
    // render `<a href="">` (an invalid Telegram anchor) and defeat the
    // no-link fallbacks.
    entry
        .links
        .iter()
        .map(|l| l.href.trim())
        .find(|href| !href.is_empty())
        .map(|href| href.to_string())
}

/// Extract entry title.
pub fn entry_title(entry: &Entry, lang: Lang) -> String {
    entry
        .title
        .as_ref()
        .map(|t| t.content.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| i18n::t(lang, "title_untitled").to_string())
}

/// Extract entry author name (first author), if any.
pub fn entry_author(entry: &Entry) -> Option<String> {
    entry.authors.iter().find_map(|a| {
        // Per the spec, RSS2's <author> is an email field; feed-rs puts the
        // tag content into Person.email, leaving name with only the role
        // placeholder (the literal "author"). So prefer a meaningful name,
        // and in the placeholder case take the real name from email instead.
        // Since feed-rs 3.0, Person.name is Option<String>.
        let name = a.name.as_deref().unwrap_or("").trim();
        if !name.is_empty()
            && !matches!(name, "author" | "creator" | "managingEditor" | "webMaster")
        {
            return Some(name.to_string());
        }
        a.email.as_ref().and_then(|e| {
            let e = e.trim();
            if e.is_empty() {
                return None;
            }
            // Standard forms like "bob@example.com (Bob Smith)": extract the
            // name inside the parentheses
            if let Some(cap) = RE_AUTHOR_PAREN.captures(e) {
                let inner = cap[1].trim();
                if !inner.is_empty() {
                    return Some(inner.to_string());
                }
            }
            // A bare email address is not shown as the author name (avoid
            // pushing email addresses into the channel)
            if e.contains('@') {
                return None;
            }
            Some(e.to_string())
        })
    })
}

/// Extract entry summary/content as Telegram-HTML (media stripped, links kept
/// as `<a>` hyperlinks), trimmed and truncated to `max`. See
/// `html_to_telegram_html`. On overflow falls back to truncated plain text so
/// the result is always valid Telegram HTML.
///
/// IMPORTANT: `max` is compared against the VISIBLE text length (tags
/// stripped), not the raw HTML length. Weibo search URLs inside <a href>
/// are very long but the visible text is short — using HTML length would
/// incorrectly trigger the plain-text fallback and lose all hyperlinks.
/// Note: every push-path caller passes 100_000 deliberately (splitting into
/// multiple messages happens at send time via split_html), so the overflow
/// plain-text fallback is a safety net for absurdly large bodies rather than a
/// routine branch; do not lower these call-site values without checking that.
///
/// When `meta.prerendered_html` is true (weibo markdown-rendered posts,
/// group-C sidecar, replacing the former content_type `text/x-tg-html` MIME
/// secret handshake), the body
/// is already valid Telegram HTML — only media tags are stripped,
/// formatting tags (<b>/<i>/<s>/<code>/<a>/<blockquote>) are preserved.
pub fn entry_body_html(entry: &Entry, max: usize, meta: Option<&crate::entrymeta::EntryMeta>) -> String {
    entry_body_html_from_source(entry_body_source(entry), max, meta)
}

/// The raw body source used for rendering and media extraction: the entry
/// content body when present, otherwise the summary. Borrowed so callers do
/// not clone the (possibly huge) body just to inspect it.
pub fn entry_body_source(entry: &Entry) -> &str {
    // Prefer content, but fall through to summary when content exists yet is
    // empty/whitespace: otherwise a feed that sets both would render a blank
    // body even though the summary has the text. Same rule for summary itself.
    let non_blank = |s: &str| !s.trim().is_empty();
    if let Some(body) = entry.content.as_ref().and_then(|c| c.body.as_deref()) {
        if non_blank(body) {
            return body;
        }
    }
    if let Some(summary) = entry.summary.as_ref().map(|s| s.content.as_str()) {
        if non_blank(summary) {
            return summary;
        }
    }
    ""
}

/// Render a raw body source into Telegram HTML. Split out from
/// `entry_body_html` so callers that only need a short prefix (title+summary)
/// can cap the input before conversion.
pub(crate) fn entry_body_html_from_source(
    html: &str,
    max: usize,
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> String {
    // Pre-rendered Telegram HTML (weibo markdown posts): skip the generic
    // html_to_telegram_html conversion which would strip formatting tags.
    // Only remove media tags (<img>/<video>/<source>) — media is sent as
    // separate attachments by the media pipeline.
    // Group C: explicitly marked via EntryMeta.prerendered_html (replacing
    // the text/x-tg-html MIME secret handshake).
    let is_prerendered = meta.map(|m| m.prerendered_html).unwrap_or(false);

    let tg = if is_prerendered {
        let s = RE_VIDEO_BLOCK.replace_all(html, "").to_string();
        let s = RE_IMG.replace_all(&s, "").to_string();
        let s = RE_SOURCE.replace_all(&s, "").to_string();
        collapse_blank_lines(&s)
    } else {
        html_to_telegram_html(html)
    };

    // Check visible text length (strip all tags) against the limit.
    // Visible text uses the text decoder (`decode_entities`); only URLs keep
    // the URL decoder (`unescape_html`) semantics.
    let visible = crate::htmltext::decode_entities(&strip_tags(&tg));
    if visible.chars().count() <= max {
        return tg;
    }
    // Release the converted/visible copies before building the plain fallback
    // (large bodies otherwise keep several full-size strings alive at once).
    drop(tg);
    drop(visible);
    // Overflow: emit truncated plain text (always parseable, no links).
    let plain = crate::htmltext::decode_entities(&strip_tags(html));
    let plain = collapse_blank_lines(&plain);
    crate::htmltext::escape_html(&truncate_text(plain.trim(), max))
}

/// Strip all HTML tags, dropping non-rendered element contents first
/// (`<style>` CSS, `<script>` JS). Visible-length checks, plain-text
/// fallbacks and title/label extraction all funnel through here, so that
/// source text never reaches the message.
pub fn strip_tags(s: &str) -> String {
    // Keep the non-rendered-block removal borrowed when it does not match;
    // only the final tag strip needs an owned result.
    let s = RE_NON_RENDERED_BLOCK.replace_all(s, "");
    RE_TAG.replace_all(&s, "").to_string()
}

/// Collapse runs of 3+ newlines down to 2.
fn collapse_blank_lines(s: &str) -> String {
    RE_BLANK_LINES.replace_all(s, "\n\n").to_string()
}

/// Strip HTML tags, unescape entities, and trim. Produces plain text
/// suitable for titles and summaries.
pub fn html_to_plain(html: &str) -> String {
    // Visible-text decoder, not the URL decoder: titles, author names and
    // document labels must decode named entities like `&nbsp;`/`&mdash;` too.
    // Block boundaries become spaces, otherwise list items/table cells would
    // concatenate ("firstsecond").
    let s = RE_BR.replace_all(html, " ");
    let s = RE_BLOCK_CLOSE.replace_all(&s, " ");
    let s = strip_tags(&s);
    let s = crate::htmltext::decode_entities(&s);
    s.trim().to_string()
}

/// True when an anchor href points at a media resource (a video page or an
/// image/video file). Media extraction drops these anchors entirely so card
/// icons are not sent as standalone photos. Text rendering keeps video hrefs
/// (see `is_video_href`) but still drops image ones, because those images are
/// sent as attachments.
pub fn is_media_href(href: &str) -> bool {
    let lower = href.to_lowercase();
    // Weibo video page / card links. Note: wblive (livestream) links are NOT
    // treated as media — live posts have no media to send, so the link must
    // stay in the text as a clickable hyperlink for users to watch.
    if lower.contains("video.weibo.com") || lower.contains("weibo.com/tv/") {
        return true;
    }
    // Direct media file extensions.
    let path = lower.split('?').next().unwrap_or(&lower);
    const MEDIA_EXT: [&str; 9] = [".mp4", ".mov", ".webm", ".m4v", ".jpg", ".jpeg", ".png", ".gif", ".webp"];
    MEDIA_EXT.iter().any(|e| path.ends_with(e))
}

/// True when an anchor href points at a video resource (a video page or a
/// direct video file). Such links stay in the rendered text even though the
/// video is sent as an attachment: when it is oversized/failed the link is the
/// only way to reach the source, and when it is delivered the link is still a
/// useful entry point. Image media links remain filtered by `is_media_href`.
pub(crate) fn is_video_href(href: &str) -> bool {
    let lower = href.to_lowercase();
    if lower.contains("video.weibo.com") || lower.contains("weibo.com/tv/") {
        return true;
    }
    let path = lower.split('?').next().unwrap_or(&lower);
    const VIDEO_EXT: [&str; 4] = [".mp4", ".mov", ".webm", ".m4v"];
    VIDEO_EXT.iter().any(|e| path.ends_with(e))
}

/// Convert an HTML fragment into Telegram HTML:
/// - `<img>`/`<video>`/`<source>` media tags are dropped entirely — media is
///   sent as separate photo/video attachments;
/// - video hyperlinks (video pages / direct video files) are kept so users can
///   open the source when the video is skipped or delivered; image media links
///   are still dropped with their text;
/// - other hyperlinks (including Weibo `#topic#` hashtag search links) become
///   `<a href>…</a>` so hashtags render as clickable hyperlinks, not markdown
///   source;
/// - `<br>` / block closes become newlines;
/// - all other markup is flattened to escaped plain text.
///
/// Mirrors rsstt's approach (strip media, keep hashtag/topic links).
pub fn html_to_telegram_html(html: &str) -> String {
    use std::borrow::Cow;

    // Most transformation steps below are conditional. Keeping the working
    // string as a Cow avoids copying a multi-megabyte body once per step when
    // the regex found nothing; only an actual match allocates.
    macro_rules! replace_cow {
        ($cur:ident, $re:expr, $rep:expr) => {
            if let Cow::Owned(v) = $re.replace_all(&$cur, $rep) {
                $cur = Cow::Owned(v);
            }
        };
    }

    let mut s: Cow<'_, str> = Cow::Borrowed(html);
    // Normalize line endings up front: split_html/blockquote folding split on
    // "\n\n", so a JSON Feed (or any source that keeps CRLF) would otherwise
    // never match paragraph boundaries and would retain stray '\r' bytes.
    if s.contains('\r') {
        s = Cow::Owned(s.replace("\r\n", "\n").replace('\r', "\n"));
    }

    // 0) Strip sentinel characters already present in the input (U+FFFC object
    //    replacement character / U+FFFE noncharacter). These two characters
    //    are used as internal markers by this function (hyperlink
    //    placeholders / blockquote boundaries); if the feed body itself
    //    contains them they would be wrongly restored as markers, causing
    //    layout injection.
    if s.contains('\u{FFFC}') || s.contains('\u{FFFE}') {
        let mut owned = s.into_owned();
        owned.retain(|c| c != '\u{FFFC}' && c != '\u{FFFE}');
        s = Cow::Owned(owned);
    }

    // 1) Drop media entirely. For <img>, preserve the alt text ONLY when it
    //    looks like a weibo emoji icon (e.g. <img alt="[酷]">) so it survives
    //    as plain text instead of vanishing. Regular images are sent as media
    //    attachments, so their alt text (e.g. Unsplash filenames like
    //    "peter-burdon-5aJi4Dw2_Gs-unsplash") is noise and must not leak.
    replace_cow!(s, RE_VIDEO_BLOCK, "");
    replace_cow!(s, RE_IMG_ALT, |caps: &regex::Captures| {
        let alt = &caps[1];
        if alt.starts_with('[') && alt.ends_with(']') {
            alt.to_string()
        } else {
            String::new()
        }
    });
    replace_cow!(s, RE_IMG, "");
    replace_cow!(s, RE_SOURCE, "");
    // Safety: remove any leftover video fallback text that may have leaked
    // (e.g. the "video cannot be displayed, please go to Weibo Video to
    // watch" text from RSSHub <video> blocks).
    replace_cow!(s, RE_VIDEO_FALLBACK_TEXT, "");

    // 2) Line breaks and block boundaries. Other block closers (</li>, </tr>,
    //    headings, ...) also become newlines; otherwise their text runs
    //    together after tag stripping.
    replace_cow!(s, RE_BR, "\n");
    replace_cow!(s, RE_BLOCK_CLOSE, "\n");
    if s.contains("</br>") {
        s = Cow::Owned(s.replace("</br>", "\n"));
    }

    // 2.5) Preserve <blockquote> boundaries (RSSHub wraps retweeted content in
    //      <blockquote>). Replace with sentinel tokens that survive strip_tags
    //      and escape_tg, then restore as Telegram <blockquote expandable>.
    replace_cow!(s, RE_BLOCKQUOTE_OPEN, "\u{FFFE}BQO\u{FFFE}");
    replace_cow!(s, RE_BLOCKQUOTE_CLOSE, "\u{FFFE}BQC\u{FFFE}");

    // 3) Extract anchors to placeholders, remembering (href, text).
    //    Anchors whose href is a non-video media resource (image files) are
    //    dropped entirely — that image is sent as an attachment, so its link
    //    must not leak into the text. Video hrefs are kept for source access.
    let mut links: Vec<(String, String)> = Vec::new();
    let mut buf = String::new();
    let mut last = 0usize;
    for caps in RE_ANCHOR.captures_iter(&s) {
        let whole = caps.get(0).unwrap();
        buf.push_str(&s[last..whole.start()]);
        let href = unescape_html(&caps[1]);
        if is_media_href(&href) && !is_video_href(&href) {
            // Drop the image media link (text + href).
            last = whole.end();
            continue;
        }
        let text = crate::htmltext::decode_entities(&strip_tags(&caps[2]));
        let text = text.trim().to_string();
        let idx = links.len();
        links.push((href, text));
        // Object-replacement char as a marker that survives tag-strip/escape.
        buf.push_str(&format!("\u{FFFC}{}\u{FFFC}", idx));
        last = whole.end();
    }
    buf.push_str(&s[last..]);

    // Remove the "XX's Weibo Video" card text that may leak if the <a> block
    // wasn't caught by is_media_href (e.g. unusual href format). This must run
    // after anchor extraction: a kept video anchor's label lives in `links`,
    // and removing it earlier would leave an empty anchor that is then
    // dropped.
    let mut buf: Cow<'_, str> = Cow::Owned(buf);
    replace_cow!(buf, RE_WEIBO_VIDEO_CARD_TEXT, "");

    // 4) Flatten remaining markup, decode entities, escape for Telegram.
    //    `decode_entities` is the visible-text decoder: it maps HTML5
    //    named entities like `&nbsp;`/`&mdash;` and numeric references in a
    //    single pass (so `&amp;lt;` decodes once, not twice). The URL decoder
    //    `unescape_html` leaves named entities literal, and the following
    //    `escape_html` would then re-escape the `&` into a visible
    //    `&amp;nbsp;` leak.
    let plain = crate::htmltext::decode_entities(&strip_tags(&buf));
    // Explicitly linkify bare URLs before escaping. This bounds Telegram's
    // auto-detection to the actual URL (important for CJK text glued to the
    // end of a URL without whitespace).
    let plain = linkify_bare_urls(&plain, &mut links);
    let mut out = crate::htmltext::escape_html(&collapse_blank_lines(&plain));

    // 5) Restore anchors as Telegram hyperlinks (skip empty-text links).
    for (i, (href, text)) in links.iter().enumerate() {
        let token = format!("\u{FFFC}{}\u{FFFC}", i);
        // Media anchors are dropped before this point, so their tokens never
        // exist; skip the full-string replace in that case.
        if !out.contains(&token) {
            continue;
        }
        let replacement = if text.is_empty() {
            String::new()
        } else if href.is_empty() || !crate::htmltext::is_safe_href(href) {
            // Unsupported scheme (javascript:/data:/relative): keep the
            // visible text but never emit an unsafe Telegram anchor.
            crate::htmltext::escape_html(text)
        } else {
            format!("<a href=\"{}\">{}</a>", crate::htmltext::escape_attr(href), crate::htmltext::escape_html(text))
        };
        out = out.replace(&token, &replacement);
    }

    // 6) Restore blockquote sentinels as Telegram expandable blockquote.
    if out.contains("\u{FFFE}BQO\u{FFFE}") {
        out = out.replace("\u{FFFE}BQO\u{FFFE}", "<blockquote expandable>");
    }
    if out.contains("\u{FFFE}BQC\u{FFFE}") {
        out = out.replace("\u{FFFE}BQC\u{FFFE}", "</blockquote>");
    }

    // Collapse HTML formatting whitespace: strip line-leading indentation and
    // fold runs of spaces/tabs (RSSHub etc. indent tags heavily; after tag
    // stripping these become visible blank runs).
    let mut out: Cow<'_, str> = Cow::Owned(out);
    replace_cow!(out, RE_LINE_INDENT, "");
    replace_cow!(out, RE_MULTI_SPACE, " ");

    let out = collapse_blank_lines(&out);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weibo_hashtag_and_video_link_kept_media_stripped() {
        // Real RSSHub/Weibo fragment: hashtag wrapped in <a><span class="surl-text">,
        // plus <img>/<video> media tags that must NOT leak into the text.
        // The video card anchor itself stays clickable.
        let html = r#"#话题A#<a href="https://m.weibo.cn/search?containerid=231522type%3D1&amp;q=%23%E8%AF%9D%E9%A2%98B%23" data-hide=""><span class="surl-text">#话题B#</span></a> 正文内容。<a href="https://video.weibo.com/show?fid=1034:1" data-hide=""><span class="url-icon"><img src="https://h5.sinaimg.cn/x.png"></span><span class="surl-text">微博视频</span></a><br clear="both"><img src="https://tvax4.sinaimg.cn/mw2000/abc.jpg" referrerpolicy="no-referrer"><video controls="controls" poster="https://tvax1.sinaimg.cn/p.jpg"><source src="https://f.video.weibocdn.com/v.mp4"></video>"#;

        let out = html_to_telegram_html(html);

        // Hashtag kept as a clickable hyperlink with decoded href.
        assert!(out.contains(r#"<a href="https://m.weibo.cn/search?containerid=231522type%3D1&amp;q=%23%E8%AF%9D%E9%A2%98B%23">#话题B#</a>"#), "out: {}", out);
        // Video card link kept as a clickable hyperlink.
        assert!(out.contains(r#"<a href="https://video.weibo.com/show?fid=1034:1">微博视频</a>"#), "video link lost: {}", out);
        // Plain hashtag text preserved.
        assert!(out.contains("#话题A#"));
        // Media tag URLs must not leak.
        assert!(!out.contains("sinaimg.cn"), "out: {}", out);
        assert!(!out.contains("weibocdn.com"), "out: {}", out);
        assert!(!out.contains("![") && !out.contains("]("), "md syntax leaked: {}", out);
        // Body text preserved.
        assert!(out.contains("正文内容。"));
    }

    #[test]
    fn weibo_video_post_keeps_topic_and_video_anchors() {
        // Exact shape of a native Weibo video post: two topic anchors plus a
        // trailing video-card anchor. Topic hrefs are absolute and
        // percent-encoded; the video card icon must not leak.
        let html = r#"【听！<a  href="https://m.weibo.cn/search?containerid=231522type%3D1%26t%3D10%26q%3D%23%E8%BF%99%E6%98%AF%E5%8C%97%E4%BA%AC%E4%B8%8A%E7%A9%BA%E6%B2%BB%E6%84%88%E7%9A%84%E8%87%AA%E7%84%B6%E4%B9%8B%E5%A3%B0%23&extparam=%23%E8%BF%99%E6%98%AF%E5%8C%97%E4%BA%AC%E4%B8%8A%E7%A9%BA%E6%B2%BB%E6%84%88%E7%9A%84%E8%87%AA%E7%84%B6%E4%B9%8B%E5%A3%B0%23&luicode=20000061&lfid=1000000000000001&launchid=10000360-page_H5" data-hide=""><span class="surl-text">#这是北京上空治愈的自然之声#</span></a>】<a  href="https://m.weibo.cn/search?containerid=231522type%3D1%26t%3D10%26q%3D%23%E5%8C%97%E4%BA%AC%E5%B8%822026%E8%A7%82%E9%B8%9F%E5%9C%B0%E5%9B%BE%E8%AF%B7%E6%94%B6%E5%A5%BD%23&extparam=%23%E5%8C%97%E4%BA%AC%E5%B8%822026%E8%A7%82%E9%B8%9F%E5%9C%B0%E5%9B%BE%E8%AF%B7%E6%94%B6%E5%A5%BD%23&luicode=20000061&lfid=1000000000000001&launchid=10000360-page_H5" data-hide=""><span class="surl-text">#北京市2026观鸟地图请收好#</span></a> 作为东亚-澳大利西亚候鸟迁徙的关键节点.<a  href="https://video.weibo.com/show?fid=1034:1000000000000002" data-hide=""><span class='url-icon'><img style='width: 1rem;height: 1rem' src='https://h5.sinaimg.cn/upload/2015/09/25/3/timeline_card_small_video_default.png'></span><span class="surl-text">人民日报的微博视频</span></a> "#;
        let out = html_to_telegram_html(html);
        assert!(
            out.contains(">#这是北京上空治愈的自然之声#</a>"),
            "first topic lost: {out}"
        );
        assert!(
            out.contains(">#北京市2026观鸟地图请收好#</a>"),
            "second topic lost: {out}"
        );
        assert!(
            out.contains(r#"<a href="https://video.weibo.com/show?fid=1034:1000000000000002">人民日报的微博视频</a>"#),
            "video card lost: {out}"
        );
        assert!(!out.contains("h5.sinaimg.cn"), "card icon leaked: {out}");
    }

    #[test]
    fn weibo_video_card_label_with_owner_prefix_kept() {
        // The "XX的微博视频" label is also the anchor text; the cleanup must
        // only remove leaked copies outside anchors, not this one.
        let html = r#"正文<a href="https://video.weibo.com/show?fid=1034:123" data-hide=""><span class="url-icon"><img src="https://h5.sinaimg.cn/x.png"></span><span class="surl-text">某某的微博视频</span></a>"#;
        let out = html_to_telegram_html(html);
        assert!(out.contains(r#"<a href="https://video.weibo.com/show?fid=1034:123">某某的微博视频</a>"#), "out: {}", out);
        assert!(!out.contains("sinaimg.cn"), "icon leaked: {}", out);
        assert!(out.contains("正文"), "body lost: {}", out);
    }

    #[test]
    fn leaked_weibo_video_card_text_still_removed() {
        // Text-shaped residue outside an anchor is still cleaned up.
        let out = html_to_telegram_html("正文 某某的微博视频");
        assert_eq!(out, "正文");
    }

    #[test]
    fn video_anchor_without_visible_text_is_dropped() {
        let html = r#"正文<a href="https://video.weibo.com/show?fid=1"><img src="https://h5.sinaimg.cn/x.png"></a>结尾"#;
        let out = html_to_telegram_html(html);
        assert!(!out.contains("<a"), "empty anchor kept: {}", out);
        assert!(!out.contains("video.weibo.com"), "href leaked: {}", out);
        assert!(out.contains("正文") && out.contains("结尾"), "out: {}", out);
    }

    #[test]
    fn image_media_anchor_still_dropped() {
        let html = r#"正文<a href="https://example.com/a.jpg">查看图片</a>结尾"#;
        let out = html_to_telegram_html(html);
        assert!(!out.contains("example.com"), "image href leaked: {}", out);
        assert!(!out.contains("查看图片"), "image link text leaked: {}", out);
        assert!(out.contains("正文") && out.contains("结尾"), "out: {}", out);
    }

    #[test]
    fn direct_video_file_anchor_kept() {
        let html = r#"<a href="https://cdn.example.com/v.mp4">视频</a>"#;
        let out = html_to_telegram_html(html);
        assert_eq!(out, r#"<a href="https://cdn.example.com/v.mp4">视频</a>"#);
    }

    #[test]
    fn weibo_emoji_alt_text_preserved() {
        // Weibo emoji are icon <img alt="[酷]"> tags; the image is dropped
        // but the alt text must survive as plain text.
        let html = r#"今天心情不错<span class="url-icon"><img alt="[酷]" src="https://face.t.sinajs.cn/t4/appstyle/expression/ext/normal/cool.png"></span>继续加油<span class="url-icon"><img alt="[doge]" src="https://face.t.sinajs.cn/t4/doge.png"></span>"#;
        let out = html_to_telegram_html(html);
        assert!(out.contains("[酷]"), "out: {}", out);
        assert!(out.contains("[doge]"), "out: {}", out);
        assert!(!out.contains("sinajs.cn"), "img src leaked: {}", out);
        assert!(!out.contains("<img"), "img tag leaked: {}", out);
    }

    #[test]
    fn plain_text_escaped_and_br_to_newline() {
        let out = html_to_telegram_html("a &lt;b&gt;<br>c");
        assert_eq!(out, "a &lt;b&gt;\nc");
    }

    #[test]
    fn block_elements_are_separated_not_concatenated() {
        let out = html_to_telegram_html("<ul><li>first</li><li>second</li></ul>");
        assert!(out.contains("first\nsecond"), "list items concatenated: {out:?}");
        let title = html_to_plain("<h1>Head</h1><p>Body</p>");
        assert_eq!(title, "Head Body");
        let cells = html_to_plain("<table><tr><td>A</td><td>B</td></tr></table>");
        assert_eq!(cells, "A B");
    }

    #[test]
    fn non_rendered_style_and_script_blocks_do_not_leak_source() {
        // Bear Blog inlines a dark-mode <style> block before each image group.
        // Only the tags used to be stripped, so the CSS stayed in the visible
        // text and filled the latter part of the 140-char summary.
        let html = concat!(
            "<p>第一段正文。</p>",
            "<style>\n",
            "    @media (prefers-color-scheme: dark) {\n",
            "        .#img-group-x .desc-text { color: rgba(255, 255, 255, 0.7); }\n",
            "    }\n",
            "</style>",
            "<p>第二段正文。</p>",
            "<script>var x = 1 < 2;</script>",
            "<p>第三段正文。</p>",
        );
        let out = html_to_telegram_html(html);
        assert!(out.contains("第一段正文。"), "out: {out}");
        assert!(out.contains("第二段正文。"), "out: {out}");
        assert!(out.contains("第三段正文。"), "out: {out}");
        assert!(!out.contains("@media"), "css leaked: {out}");
        assert!(!out.contains("prefers-color-scheme"), "css leaked: {out}");
        assert!(!out.contains("rgba("), "css leaked: {out}");
        assert!(!out.contains("var x"), "js leaked: {out}");
    }

    #[test]
    fn non_rendered_blocks_match_case_and_attributes() {
        let html = r#"<STYLE type="text/css">.a{color:red}</STYLE><p>正文</p><script defer src="x.js">init()</script>"#;
        let out = html_to_telegram_html(html);
        assert_eq!(out, "正文");
        assert_eq!(html_to_plain(html), "正文");
    }

    #[test]
    fn unterminated_style_block_drops_to_end() {
        // The summary path cuts the raw body at 256KB, which can remove the
        // closing tag; the leftover CSS must still be dropped like HTML
        // parsing does, not shown as body text.
        let html = "<p>正文</p><style>@media (prefers-color-scheme: dark) { .x { color: red; } }";
        let out = html_to_telegram_html(html);
        assert_eq!(out, "正文");
        assert_eq!(html_to_plain(html), "正文");
    }

    #[test]
    fn self_closing_style_tag_does_not_swallow_body() {
        let html = "<style/><p>正文</p><script src=\"x.js\"/><p>结尾</p>";
        let out = html_to_telegram_html(html);
        assert!(out.contains("正文"), "body lost: {out}");
        assert!(out.contains("结尾"), "tail lost: {out}");
    }

    #[test]
    fn bare_url_stops_before_cjk_punctuation_and_text() {
        // Exact shape from Weibo: a bare GitHub URL glued to `。` and the
        // following Chinese sentence. Telegram auto-detection would otherwise
        // absorb the trailing text into the link.
        let out = html_to_telegram_html("解锁：github.com/example/repo。这个项目同样使用漏洞。");
        assert!(
            out.contains(r#"<a href="https://github.com/example/repo">github.com/example/repo</a>。这个项目"#),
            "out: {out}"
        );

        // Also stop at CJK text without punctuation.
        let out = html_to_telegram_html("见 github.com/example/repo这个项目");
        assert!(
            out.contains(r#"<a href="https://github.com/example/repo">github.com/example/repo</a>这个项目"#),
            "out: {out}"
        );
    }

    #[test]
    fn bare_url_trims_ascii_prose_punctuation_and_keeps_balanced_parens() {
        let out = html_to_telegram_html("(https://example.com/a)");
        assert_eq!(
            out,
            r#"(<a href="https://example.com/a">https://example.com/a</a>)"#
        );
        let out = html_to_telegram_html("https://example.com/a_(b)");
        assert_eq!(
            out,
            r#"<a href="https://example.com/a_(b)">https://example.com/a_(b)</a>"#
        );
        let out = html_to_telegram_html("https://example.com/a.");
        assert_eq!(
            out,
            r#"<a href="https://example.com/a">https://example.com/a</a>."#
        );
    }

    #[test]
    fn bare_url_supports_scheme_www_tcn_and_domain_path() {
        let out = html_to_telegram_html("http://example.com/a www.example.com/b t.cn/AX123 example.com/c");
        assert!(out.contains(r#"<a href="http://example.com/a">http://example.com/a</a>"#), "out: {out}");
        assert!(
            out.contains(r#"<a href="https://www.example.com/b">www.example.com/b</a>"#),
            "out: {out}"
        );
        assert!(out.contains(r#"<a href="https://t.cn/AX123">t.cn/AX123</a>"#), "out: {out}");
        assert!(
            out.contains(r#"<a href="https://example.com/c">example.com/c</a>"#),
            "out: {out}"
        );
    }

    #[test]
    fn bare_url_query_ampersand_is_escaped_once() {
        let out = html_to_telegram_html("https://example.com/a?b=1&c=2，下一句");
        assert_eq!(
            out,
            r#"<a href="https://example.com/a?b=1&amp;c=2">https://example.com/a?b=1&amp;c=2</a>，下一句"#
        );
    }

    #[test]
    fn existing_anchor_is_not_double_linkified() {
        let out = html_to_telegram_html(
            r#"<a href="https://example.com/x">https://example.com/x</a>"#,
        );
        assert_eq!(out.matches("<a ").count(), 1, "out: {out}");
        assert!(out.contains(r#"<a href="https://example.com/x">https://example.com/x</a>"#));
    }

    #[test]
    fn file_like_tokens_are_not_linkified() {
        let out = html_to_telegram_html("foo.txt/bar v1.2.3 user@example.com");
        assert!(!out.contains("<a "), "out: {out}");
        assert!(out.contains("foo.txt/bar"));
        assert!(out.contains("v1.2.3"));
        assert!(out.contains("user@example.com"));
    }

    #[test]
    fn html_to_plain_uses_visible_text_decoder() {
        assert_eq!(html_to_plain("A&nbsp;B &mdash; C&hellip;"), "A B — C…");
        assert_eq!(html_to_plain("<b>x</b>&amp;lt;"), "x&lt;");
    }

    #[test]
    fn nbsp_and_named_entities_decoded_not_double_escaped() {
        // huxiu (RSSHub) bodies carry &nbsp; inside plain text. The visible
        // path must run the text decoder: nbsp becomes a real space. The URL
        // decoder left it literal and the following escape turned the & into
        // &amp; — users saw "张三&amp;nbsp;李四".
        let out = html_to_telegram_html("<p>作者：张三&nbsp;李四 &amp; 伙伴</p>");
        assert!(out.contains("张三 李四"), "nbsp not decoded: {out}");
        assert!(out.contains("&amp; 伙伴"), "ampersand not re-escaped: {out}");
        assert!(!out.contains("nbsp"), "entity leaked: {out}");
    }

    #[test]
    fn blockquote_becomes_expandable_tg_blockquote() {
        // RSSHub non-readable retweet: <blockquote> - retweet @author: text</blockquote>
        let html = r#"转发语<br><blockquote> - 转发 <a href="https://weibo.com/n/某人">@某人</a>: 原博内容</blockquote>"#;
        let out = html_to_telegram_html(html);
        assert!(out.contains("<blockquote expandable>"), "out: {}", out);
        assert!(out.contains("</blockquote>"), "out: {}", out);
        assert!(out.contains("- 转发"), "out: {}", out);
        assert!(out.contains("原博内容"), "out: {}", out);
        assert!(out.contains(r#"<a href="https://weibo.com/n/某人">@某人</a>"#), "out: {}", out);
    }

    #[test]
    fn blockquote_readable_mode_with_style() {
        // RSSHub readable mode: <blockquote style="background: ...">
        let html = r#"正文<br clear="both" /><div style="clear: both"></div><blockquote style="background: #80808010;border-top:1px solid #80808030;">原博文字</blockquote>"#;
        let out = html_to_telegram_html(html);
        assert!(out.contains("<blockquote expandable>"), "out: {}", out);
        assert!(out.contains("原博文字"), "out: {}", out);
        assert!(out.contains("</blockquote>"), "out: {}", out);
    }

    // ===== entry_body_html prerendered path (group-C sidecar) =====

    fn entry_with_body_html(body: &str) -> feed_rs::model::Entry {
        use feed_rs::model::{Content, Entry};
        Entry {
            id: "test".to_string(),
            title: None,
            updated: None,
            published: None,
            authors: Vec::new(),
            contributors: Vec::new(),
            content: Some(Content {
                body: Some(body.to_string()),
                content_type: "text/html".parse().unwrap(),
                length: None,
                src: None,
            }),
            links: Vec::new(),
            summary: None,
            categories: Vec::new(),
            media: Vec::new(),
            rights: None,
            source: None,
            language: None,
            base: None,
        }
    }

    #[test]
    fn entry_body_source_falls_back_when_content_is_blank() {
        use feed_rs::model::{Content, Entry, Text};
        let entry = Entry {
            id: "t".into(),
            title: None,
            updated: None,
            published: None,
            authors: Vec::new(),
            contributors: Vec::new(),
            content: Some(Content {
                body: Some("   \n\t ".into()),
                content_type: "text/html".parse().unwrap(),
                length: None,
                src: None,
            }),
            links: Vec::new(),
            summary: Some(Text {
                content_type: "text/plain".parse().unwrap(),
                src: None,
                content: "summary-text".into(),
            }),
            categories: Vec::new(),
            media: Vec::new(),
            rights: None,
            source: None,
            language: None,
            base: None,
        };
        assert_eq!(entry_body_source(&entry), "summary-text");
    }

    #[test]
    fn entry_link_skips_blank_hrefs() {
        use feed_rs::model::{Entry, Link};
        let empty_link = |href: &str| Link {
            href: href.into(),
            target: None,
            rel: None,
            media_type: None,
            href_lang: None,
            title: None,
            length: None,
        };
        let entry = Entry {
            id: "t".into(),
            title: None,
            updated: None,
            published: None,
            authors: Vec::new(),
            contributors: Vec::new(),
            content: None,
            links: vec![empty_link("  "), empty_link(""), empty_link("https://example.com/x")],
            summary: None,
            categories: Vec::new(),
            media: Vec::new(),
            rights: None,
            source: None,
            language: None,
            base: None,
        };
        assert_eq!(entry_link(&entry).as_deref(), Some("https://example.com/x"));
        assert!(entry_link(&Entry { links: vec![empty_link(" ")], ..entry }).is_none());
    }

    #[test]
    fn entry_body_html_prerendered_preserves_formatting_strips_media() {
        // prerendered_html=true: preserve <b>/<i>/<a>/<blockquote>, strip
        // only the media tags.
        let body = r#"<b>粗体</b> <i>斜体</i> <a href="https://x.com">链接</a><img src="https://img.example.com/a.jpg"><video src="https://v.example.com/b.mp4"></video><blockquote>引用</blockquote>"#;
        let entry = entry_with_body_html(body);
        let meta = crate::entrymeta::EntryMeta {
            prerendered_html: true,
            poll: None,
            retweet_images: Vec::new(),
        };
        let out = entry_body_html(&entry, 100_000, Some(&meta));
        assert!(out.contains("<b>粗体</b>"), "out: {}", out);
        assert!(out.contains("<i>斜体</i>"), "out: {}", out);
        assert!(out.contains(r#"<a href="https://x.com">链接</a>"#), "out: {}", out);
        assert!(out.contains("<blockquote>引用</blockquote>"), "out: {}", out);
        assert!(!out.contains("img.example.com"), "img leaked: {}", out);
        assert!(!out.contains("v.example.com"), "video leaked: {}", out);
    }

    #[test]
    fn entry_body_html_no_meta_runs_generic_conversion() {
        // meta=None (RSS source): goes through the generic
        // html_to_telegram_html conversion.
        let body = r#"普通文本<br>第二行<img src="https://img.example.com/a.jpg">"#;
        let entry = entry_with_body_html(body);
        let out = entry_body_html(&entry, 100_000, None);
        assert!(out.contains("普通文本"), "out: {}", out);
        assert!(out.contains("第二行"), "out: {}", out);
        assert!(!out.contains("img.example.com"), "img leaked: {}", out);
    }

    #[test]
    fn entry_title_fallback_is_localized() {
        let entry = crate::platforms::make_entry(
            "id".to_string(),
            String::new(),
            "author".to_string(),
            String::new(),
            "text/html",
            "https://example.com/1".to_string(),
            None,
        );
        assert_eq!(entry_title(&entry, Lang::Zh), "(无标题)");
        assert_eq!(entry_title(&entry, Lang::En), "(Untitled)");
    }

    #[test]
    fn plain_feed_url_normalization_is_conservative() {
        // Scheme/host case and default ports are the only variants merged.
        assert_eq!(
            normalize_plain_feed_url("HTTP://Example.COM/Feed.xml"),
            "http://example.com/Feed.xml"
        );
        assert_eq!(
            normalize_plain_feed_url("https://example.com:443/feed"),
            "https://example.com/feed"
        );
        assert_eq!(
            normalize_plain_feed_url("http://example.com:80/feed"),
            "http://example.com/feed"
        );
        // Non-default port and path/query casing are preserved.
        assert_eq!(
            normalize_plain_feed_url("https://Example.com:8443/Feed?Q=A"),
            "https://example.com:8443/Feed?Q=A"
        );
        // Trailing slash and query order keep their identity (no merging).
        assert_eq!(
            normalize_plain_feed_url("https://example.com/feed/"),
            "https://example.com/feed/"
        );
        // IPv6 literal host and userinfo survive normalization.
        assert_eq!(
            normalize_plain_feed_url("http://user:pw@[::1]:8080/feed"),
            "http://user:pw@[::1]:8080/feed"
        );
        // Non-URL input passes through untouched.
        assert_eq!(normalize_plain_feed_url("not a url"), "not a url");
    }
}
