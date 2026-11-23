//! X/Twitter native fetcher via the GraphQL timeline API.

use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use feed_rs::model::Entry;
use jiff::{Timestamp, Zoned};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::fetcher::FetchOptions;
use crate::htmltext::extract_cookie_value;
use crate::i18n::{self, Lang};
use crate::state::AppState;

const BEARER: &str = "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";

const BASE_API: &str = "https://x.com/i/api";

const QID_USER_BY_SCREEN_NAME: &str = "Gb-d6r0vxPOADdG62OEBpQ";
const QID_USER_TWEETS: &str = "eoJ5zbv51Z_KVl81v9PmLQ";

const FEATURES_USER: &str = r#"{"hidden_profile_subscriptions_enabled":true,"rweb_tipjar_consumption_enabled":true,"responsive_web_graphql_exclude_directive_enabled":true,"verified_phone_label_enabled":false,"subscriptions_verification_info_is_identity_verified_enabled":true,"subscriptions_verification_info_verified_since_enabled":true,"highlights_tweets_tab_ui_enabled":true,"responsive_web_twitter_article_notes_tab_enabled":true,"subscriptions_feature_can_gift_premium":true,"creator_subscriptions_tweet_preview_api_enabled":true,"responsive_web_graphql_skip_user_profile_image_extensions_enabled":false,"responsive_web_graphql_timeline_navigation_enabled":true}"#;

const FEATURES_FEED: &str = r#"{"rweb_tipjar_consumption_enabled":true,"responsive_web_graphql_exclude_directive_enabled":true,"verified_phone_label_enabled":false,"creator_subscriptions_tweet_preview_api_enabled":true,"responsive_web_graphql_timeline_navigation_enabled":true,"responsive_web_graphql_skip_user_profile_image_extensions_enabled":false,"communities_web_enable_tweet_community_results_fetch":true,"c9s_tweet_anatomy_moderator_badge_enabled":true,"articles_preview_enabled":true,"responsive_web_edit_tweet_api_enabled":true,"graphql_is_translatable_rweb_tweet_is_translatable_enabled":true,"view_counts_everywhere_api_enabled":true,"longform_notetweets_consumption_enabled":true,"responsive_web_twitter_article_tweet_consumption_enabled":true,"tweet_awards_web_tipping_enabled":false,"creator_subscriptions_quote_tweet_preview_enabled":false,"freedom_of_speech_not_reach_fetch_enabled":true,"standardized_nudges_misinfo":true,"tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled":true,"rweb_video_timestamps_enabled":true,"longform_notetweets_rich_text_read_enabled":true,"longform_notetweets_inline_media_enabled":true,"responsive_web_enhance_cards_enabled":false}"#;

const MAX_TIMELINE_PAGES: usize = 3;
const MAX_EMPTY_TIMELINE_PAGES: usize = 2;

const TIMELINE_PAGE_PAUSE: Duration = Duration::from_millis(1100);

pub fn is_x_url(url: &str) -> bool {
    parse_screen_name(url).is_some()
}

pub fn parse_screen_name(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?.to_lowercase();
    let is_x_host = matches!(
        host.as_str(),
        "x.com" | "www.x.com" | "twitter.com" | "www.twitter.com" | "mobile.twitter.com"
    );
    if !is_x_host {
        return None;
    }
    let segs: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return None;
    }
    let name = segs[0];
    const RESERVED: &[&str] = &[
        "i",
        "home",
        "search",
        "explore",
        "notifications",
        "messages",
        "settings",
        "compose",
        "login",
        "signup",
        "logout",
        "hashtag",
    ];
    let name_lower = name.to_ascii_lowercase();
    if RESERVED.contains(&name_lower.as_str()) {
        return None;
    }
    if name.is_empty()
        || name.len() > 15
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    Some(name.to_string())
}

pub async fn fetch_by_url(
    state: &AppState,
    url: String,
    opts: &FetchOptions<'_>,
) -> anyhow::Result<crate::fetcher::FetchResult> {
    let screen_name = parse_screen_name(&url)
        .ok_or_else(|| anyhow::anyhow!("Invalid X user profile URL: {}", url))?;
    fetch_x_user(state, &screen_name, opts.lang).await
}

pub async fn fetch_x_user(
    state: &AppState,
    screen_name: &str,
    lang: Lang,
) -> anyhow::Result<crate::fetcher::FetchResult> {
    let auth_token = match state.get_cookie("x").await {
        Some(c) if !c.trim().is_empty() => c,
        _ => anyhow::bail!(
            "X feeds require an admin-configured login cookie (use the /cookies command)"
        ),
    };

    let mut ct0 = obtain_ct0_cached(state, &auth_token).await?;

    let result = fetch_x_user_with_ct0(state, &auth_token, &ct0, screen_name, lang).await;
    let result = match result {
        Err(e) if e.downcast_ref::<StaleCt0Error>().is_some() => {
            tracing::info!(
                screen_name = screen_name,
                "X ct0 looks stale; refreshing and retrying once"
            );
            clear_ct0_cache(&auth_token).await;
            ct0 = obtain_ct0(state, &auth_token).await?;
            {
                let mut cache = ct0_cache().lock().await;
                cache.insert(
                    auth_token.clone(),
                    Ct0Entry {
                        ct0: ct0.clone(),
                        fetched_at: Instant::now(),
                    },
                );
            }
            fetch_x_user_with_ct0(state, &auth_token, &ct0, screen_name, lang).await
        }
        other => other,
    };
    result.map_err(|e| {
        if e.downcast_ref::<StaleCt0Error>().is_some() {
            crate::platforms::auth_expired("X", "HTTP 403 after a ct0 refresh")
        } else {
            e
        }
    })
}

async fn fetch_x_user_with_ct0(
    state: &AppState,
    auth_token: &str,
    ct0: &str,
    screen_name: &str,
    lang: Lang,
) -> anyhow::Result<crate::fetcher::FetchResult> {
    let (rest_id, display_name) = resolve_user(state, auth_token, ct0, screen_name).await?;

    let tweets = fetch_user_tweets(state, auth_token, ct0, &rest_id, screen_name).await?;

    let mut entries: Vec<Entry> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut dropped = 0usize;
    let mut skipped_replies = 0usize;
    for tweet in &tweets {
        match tweet_to_entry(tweet, screen_name, &display_name, lang) {
            ConvertOutcome::Ok(e) => {
                crate::platforms::push_unique_entry(&mut entries, &mut seen_ids, *e);
            }
            ConvertOutcome::SkippedReply => skipped_replies += 1,
            ConvertOutcome::Failed => dropped += 1,
        }
    }
    if skipped_replies > 0 {
        tracing::debug!(
            screen_name = screen_name,
            skipped = skipped_replies,
            total = tweets.len(),
            "X replies skipped during conversion (expected)"
        );
    }
    if dropped > 0 {
        tracing::warn!(
            screen_name = screen_name,
            dropped = dropped,
            total = tweets.len(),
            "X tweets dropped during conversion (legacy missing?)"
        );
    }

    crate::platforms::sort_entries_newest_first(&mut entries);

    Ok(crate::fetcher::FetchResult::platform(
        Some(display_name),
        entries,
    ))
}

async fn obtain_ct0(state: &AppState, auth_token: &str) -> anyhow::Result<String> {
    let resp = crate::platforms::api_get(
        state,
        "https://x.com",
        &[("Cookie", format!("auth_token={}", auth_token))],
        0,
    )
    .await
    .context("x.com request failed")?;

    let mut ct0 = String::new();
    for value in resp.headers().get_all("set-cookie") {
        if let Ok(s) = value.to_str() {
            if let Some(token) = extract_cookie_value(s, "ct0") {
                ct0 = token;
                break;
            }
        }
    }
    if ct0.is_empty() {
        let status = resp.status();
        if status.is_redirection() || status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(crate::platforms::auth_expired(
                "X",
                format!("ct0 missing (HTTP {})", status),
            ));
        }
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(crate::platforms::rate_limited("X", status.as_u16()));
        }
        anyhow::bail!(
            "X ct0 unavailable (HTTP {}); response shape may have changed",
            status
        );
    }
    Ok(ct0)
}

const CT0_TTL: Duration = Duration::from_secs(3600);

struct Ct0Entry {
    ct0: String,
    fetched_at: Instant,
}

static CT0_CACHE: OnceLock<Mutex<std::collections::HashMap<String, Ct0Entry>>> = OnceLock::new();

fn ct0_cache() -> &'static Mutex<std::collections::HashMap<String, Ct0Entry>> {
    CT0_CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

async fn obtain_ct0_cached(state: &AppState, auth_token: &str) -> anyhow::Result<String> {
    {
        let cache = ct0_cache().lock().await;
        if let Some(entry) = cache.get(auth_token) {
            if entry.fetched_at.elapsed() < CT0_TTL {
                return Ok(entry.ct0.clone());
            }
        }
    }
    let ct0 = obtain_ct0(state, auth_token).await?;
    let mut cache = ct0_cache().lock().await;
    cache.retain(|_, entry| entry.fetched_at.elapsed() < CT0_TTL);
    cache.insert(
        auth_token.to_string(),
        Ct0Entry {
            ct0: ct0.clone(),
            fetched_at: Instant::now(),
        },
    );
    Ok(ct0)
}

async fn clear_ct0_cache(auth_token: &str) {
    let mut cache = ct0_cache().lock().await;
    cache.remove(auth_token);
}

const QUERY_ID_DOC_URL: &str =
    "https://cdn.jsdelivr.net/gh/fa0311/TwitterInternalAPIDocument@master/docs/json/API.json";

const QUERY_ID_TTL: Duration = Duration::from_secs(6 * 3600);
const QUERY_ID_NEGATIVE_TTL: Duration = Duration::from_secs(300);
const QUERY_ID_FORCE_MIN_INTERVAL: Duration = Duration::from_secs(300);

const OP_USER_BY_SCREEN_NAME: &str = "UserByScreenName";
const OP_USER_TWEETS: &str = "UserTweets";

struct QueryIdCache {
    ids: HashMap<String, String>,
    fetched_at: Option<Instant>,
    last_failure: Option<Instant>,
    last_forced: Option<Instant>,
}

static QUERY_IDS: OnceLock<Mutex<QueryIdCache>> = OnceLock::new();

fn query_id_cache() -> &'static Mutex<QueryIdCache> {
    QUERY_IDS.get_or_init(|| {
        Mutex::new(QueryIdCache {
            ids: HashMap::new(),
            fetched_at: None,
            last_failure: None,
            last_forced: None,
        })
    })
}

fn fallback_query_id(op: &str) -> &'static str {
    match op {
        OP_USER_BY_SCREEN_NAME => QID_USER_BY_SCREEN_NAME,
        OP_USER_TWEETS => QID_USER_TWEETS,
        _ => "",
    }
}

async fn get_query_id(state: &AppState, op: &str, force: bool) -> String {
    {
        let mut cache = query_id_cache().lock().await;
        if !force {
            if cache
                .last_failure
                .is_some_and(|t| t.elapsed() < QUERY_ID_NEGATIVE_TTL)
            {
                return cache
                    .ids
                    .get(op)
                    .cloned()
                    .unwrap_or_else(|| fallback_query_id(op).to_string());
            }
            if let Some(at) = cache.fetched_at {
                if at.elapsed() < QUERY_ID_TTL {
                    if let Some(id) = cache.ids.get(op) {
                        return id.clone();
                    }
                }
            }
        } else {
            if cache
                .last_forced
                .is_some_and(|t| t.elapsed() < QUERY_ID_FORCE_MIN_INTERVAL)
            {
                return cache
                    .ids
                    .get(op)
                    .cloned()
                    .unwrap_or_else(|| fallback_query_id(op).to_string());
            }
            cache.last_forced = Some(Instant::now());
        }
    }

    match fetch_query_ids(state).await {
        Ok(ids) => {
            let mut cache = query_id_cache().lock().await;
            let resolved = ids.get(op).cloned().or_else(|| {
                let fallback = fallback_query_id(op);
                if !fallback.is_empty() {
                    tracing::warn!(
                        op = op,
                        fallback = fallback,
                        "X query-ID document is missing the operation; caching the hardcoded fallback"
                    );
                }
                (!fallback.is_empty()).then(|| fallback.to_string())
            });
            cache.ids = ids;
            if let Some(id) = &resolved {
                cache.ids.insert(op.to_string(), id.clone());
            }
            cache.fetched_at = Some(Instant::now());
            cache.last_failure = None;
            resolved.unwrap_or_default()
        }
        Err(e) => {
            tracing::warn!(
                error = format!("{:#}", e),
                op = op,
                "X query-ID refresh failed; using cached/fallback ID"
            );
            let mut cache = query_id_cache().lock().await;
            cache.last_failure = Some(Instant::now());
            cache
                .ids
                .get(op)
                .cloned()
                .unwrap_or_else(|| fallback_query_id(op).to_string())
        }
    }
}

async fn fetch_query_ids(state: &AppState) -> anyhow::Result<HashMap<String, String>> {
    let resp = crate::platforms::api_get(
        state,
        QUERY_ID_DOC_URL,
        &[("Accept", "application/json".to_string())],
        crate::httpfollow::MAX_REDIRECTS,
    )
    .await
    .context("query-ID document request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("query-ID document HTTP {}", resp.status());
    }
    let json: Value = crate::fetcher::read_json_capped(resp, "query-ID document").await?;

    let mut ids = HashMap::new();
    for op in [OP_USER_BY_SCREEN_NAME, OP_USER_TWEETS] {
        if let Some(qid) = json["graphql"][op]["queryId"].as_str() {
            // Validate the queryId charset before splicing it into a URL path (injection).
            let valid = !qid.is_empty()
                && qid
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if valid {
                ids.insert(op.to_string(), qid.to_string());
            }
        }
    }
    if ids.is_empty() {
        anyhow::bail!("query-ID document contained no usable queryIds");
    }
    Ok(ids)
}

#[derive(Debug)]
struct StaleQueryIdError;

impl std::fmt::Display for StaleQueryIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "X GraphQL queryId appears stale")
    }
}
impl std::error::Error for StaleQueryIdError {}

#[derive(Debug)]
struct StaleCt0Error;

impl std::fmt::Display for StaleCt0Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "X GraphQL request forbidden (ct0 likely stale)")
    }
}
impl std::error::Error for StaleCt0Error {}

fn is_stale_ct0_response(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("csrf")
        || lower.contains("ct0")
        || lower.contains("\"code\":353")
        || lower.contains("\"code\": 353")
}

async fn gql_request(
    state: &AppState,
    auth_token: &str,
    ct0: &str,
    op: &str,
    params: &[(&str, String)],
) -> anyhow::Result<Value> {
    match gql_request_once(state, auth_token, ct0, op, params, false).await {
        Ok(v) => Ok(v),
        Err(e) if e.downcast_ref::<StaleQueryIdError>().is_some() => {
            tracing::info!(
                op = op,
                "X queryId looks stale; force-refreshing and retrying once"
            );
            gql_request_once(state, auth_token, ct0, op, params, true).await
        }
        Err(e) => Err(e),
    }
}

async fn gql_request_once(
    state: &AppState,
    auth_token: &str,
    ct0: &str,
    op: &str,
    params: &[(&str, String)],
    force_refresh: bool,
) -> anyhow::Result<Value> {
    let qid = get_query_id(state, op, force_refresh).await;
    if qid.is_empty() {
        anyhow::bail!("no queryId available for X operation {}", op);
    }
    let path = format!("/graphql/{}/{}", qid, op);

    let mut url = format!("{}{}", BASE_API, path);
    if !params.is_empty() {
        url.push('?');
        let pairs: Vec<String> = params
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    k,
                    url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>()
                )
            })
            .collect();
        url.push_str(&pairs.join("&"));
    }

    let resp = crate::platforms::api_get(
        state,
        &url,
        &[
            ("Cookie", format!("auth_token={}; ct0={}", auth_token, ct0)),
            ("Authorization", BEARER.to_string()),
            ("x-csrf-token", ct0.to_string()),
            ("x-twitter-auth-type", "OAuth2Session".to_string()),
            ("x-twitter-active-user", "yes".to_string()),
            ("x-twitter-client-language", "en".to_string()),
            ("Content-Type", "application/json".to_string()),
            ("Referer", "https://x.com/".to_string()),
        ],
        crate::httpfollow::MAX_REDIRECTS,
    )
    .await
    .with_context(|| format!("X GraphQL request failed: {}", path))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(crate::platforms::auth_expired("X", "HTTP 401"));
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        let body = crate::fetcher::read_body_capped(resp, "X API 403 body")
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&body);
        if is_stale_ct0_response(&text) {
            return Err(anyhow::Error::new(StaleCt0Error));
        }
        return Err(crate::platforms::source_forbidden(
            "X",
            status.as_u16(),
            text.chars().take(200).collect::<String>(),
        ));
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(crate::platforms::rate_limited("X", status.as_u16()));
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(anyhow::Error::new(StaleQueryIdError));
    }
    if !status.is_success() {
        anyhow::bail!("X API HTTP {}", status);
    }

    let json: Value = crate::fetcher::read_json_capped(resp, "X API").await?;

    if let Some(errors) = json.get("errors").filter(|e| e.is_array()) {
        let has_data = json.get("data").is_some_and(|d| !d.is_null());
        if !has_data {
            if let Some(msg) = errors.as_array().and_then(|a| a.first()) {
                let message = msg["message"].as_str().unwrap_or("unknown");
                anyhow::bail!("X GraphQL error: {}", message);
            }
        } else if let Some(msg) = errors.as_array().and_then(|a| a.first()) {
            tracing::warn!(
                error = msg["message"].as_str().unwrap_or("unknown"),
                "X GraphQL returned partial data with errors; continuing"
            );
        }
    }
    Ok(json)
}

async fn resolve_user(
    state: &AppState,
    auth_token: &str,
    ct0: &str,
    screen_name: &str,
) -> anyhow::Result<(String, String)> {
    let variables = serde_json::json!({
        "screen_name": screen_name,
        "withSafetyModeUserFields": true
    });
    let field_toggles = serde_json::json!({"withAuxiliaryUserLabels": false});

    let json = gql_request(
        state,
        auth_token,
        ct0,
        OP_USER_BY_SCREEN_NAME,
        &[
            ("variables", variables.to_string()),
            ("features", FEATURES_USER.to_string()),
            ("fieldToggles", field_toggles.to_string()),
        ],
    )
    .await?;

    let result = &json["data"]["user"]["result"];
    if result.is_null() {
        anyhow::bail!("X user does not exist: @{}", screen_name);
    }
    if result["__typename"].as_str() == Some("UserUnavailable") {
        anyhow::bail!("X user unavailable: @{}", screen_name);
    }
    let rest_id = result["rest_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("X API did not return rest_id"))?
        .to_string();
    let display_name = result["legacy"]["name"]
        .as_str()
        .or_else(|| result["core"]["name"].as_str())
        .unwrap_or(screen_name)
        .to_string();

    Ok((rest_id, display_name))
}

async fn fetch_user_tweets(
    state: &AppState,
    auth_token: &str,
    ct0: &str,
    rest_id: &str,
    screen_name: &str,
) -> anyhow::Result<Vec<Value>> {
    let mut tweets: Vec<Value> = Vec::new();
    let mut total_non_cursor_entries = 0usize;
    let mut first_page_instr_types: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages_fetched = 0usize;
    let mut empty_pages = 0usize;
    let mut unhandled_instructions: BTreeMap<String, usize> = BTreeMap::new();

    for _page in 0..MAX_TIMELINE_PAGES {
        if pages_fetched > 0 {
            tokio::time::sleep(TIMELINE_PAGE_PAUSE).await;
        }
        let mut variables = serde_json::json!({
            "userId": rest_id,
            "count": 20,
            "includePromotedContent": false,
            "withQuickPromoteEligibilityTweetFields": true,
            "withVoice": true,
            "withV2Timeline": true
        });
        if let Some(c) = &cursor {
            variables["cursor"] = Value::String(c.clone());
        }

        let json = gql_request(
            state,
            auth_token,
            ct0,
            OP_USER_TWEETS,
            &[
                ("variables", variables.to_string()),
                ("features", FEATURES_FEED.to_string()),
            ],
        )
        .await?;
        pages_fetched += 1;

        let mut instructions =
            &json["data"]["user"]["result"]["timeline"]["timeline"]["instructions"];
        if !instructions.is_array() {
            let alt = &json["data"]["user"]["result"]["timeline_v2"]["timeline"]["instructions"];
            if !alt.is_array() {
                // Missing instructions mean the API shape changed; bail instead of returning a silent empty timeline.
                anyhow::bail!(
                    "X UserTweets response missing timeline instructions (API shape may have changed)"
                );
            }
            instructions = alt;
        }
        if pages_fetched == 1 {
            first_page_instr_types = instructions
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|i| i["type"].as_str().map(|t| t.to_string()))
                        .collect()
                })
                .unwrap_or_default();
        }

        let page = extract_timeline_page(instructions);
        for (kind, count) in &page.unhandled_instructions {
            *unhandled_instructions.entry(kind.clone()).or_default() += count;
        }
        total_non_cursor_entries += page.non_cursor_entries;
        let page_tweet_count = page.tweets.len();
        tweets.extend(page.tweets);

        let Some(next) = page.bottom_cursor else {
            break;
        };
        if Some(&next) == cursor.as_ref() {
            break;
        }
        if page_tweet_count == 0 {
            empty_pages += 1;
            if empty_pages >= MAX_EMPTY_TIMELINE_PAGES {
                break;
            }
        } else {
            empty_pages = 0;
        }
        cursor = Some(next);
    }

    if !unhandled_instructions.is_empty() {
        let item_count: usize = unhandled_instructions.values().copied().sum();
        tracing::warn!(
            screen_name = screen_name,
            instruction_types = ?unhandled_instructions,
            item_count = item_count,
            "X timeline contains unhandled instruction types; some tweets may be skipped"
        );
    }

    if tweets.is_empty() && total_non_cursor_entries > 0 {
        tracing::warn!(
            screen_name = screen_name,
            non_cursor_entries = total_non_cursor_entries,
            instruction_types = ?first_page_instr_types,
            "X timeline has entries but zero tweets extracted (API shape may have changed)"
        );
    }
    Ok(tweets)
}

struct TimelinePage {
    tweets: Vec<Value>,
    non_cursor_entries: usize,
    bottom_cursor: Option<String>,
    unhandled_instructions: Vec<(String, usize)>,
}

fn extract_timeline_page(instructions: &Value) -> TimelinePage {
    let mut tweets = Vec::new();
    let mut non_cursor_entries = 0usize;
    let mut bottom_cursor: Option<String> = None;
    let mut unhandled_instructions: Vec<(String, usize)> = Vec::new();
    let empty_instructions = Vec::new();
    for instr in instructions.as_array().unwrap_or(&empty_instructions) {
        let instr_type = instr["type"].as_str().unwrap_or("");
        if instr_type == "TimelineSuggestCursor" {
            if let Some(entry) = instr.get("entry") {
                let entry_id = entry["entryId"].as_str().unwrap_or("");
                if bottom_cursor.is_none() {
                    if let Some(v) = cursor_entry_value(entry_id, entry) {
                        bottom_cursor = Some(v);
                    }
                }
            }
            continue;
        }
        let entries: Option<Vec<&Value>> = if instr_type == "TimelineAddEntries" {
            instr["entries"].as_array().map(|a| a.iter().collect())
        } else if instr_type == "TimelineAddEntry" {
            instr.get("entry").map(|e| vec![e])
        } else {
            None
        };
        if let Some(entries) = entries {
            for entry in entries {
                let entry_id = entry["entryId"].as_str().unwrap_or("");
                if entry_id.starts_with("cursor-") {
                    if bottom_cursor.is_none() {
                        if let Some(v) = cursor_entry_value(entry_id, entry) {
                            bottom_cursor = Some(v);
                        }
                    }
                } else {
                    non_cursor_entries += 1;
                    collect_tweet_results(entry, &mut tweets);
                }
            }
            continue;
        }
        if !matches!(
            instr_type,
            "TimelineClearCache" | "TimelinePinEntry" | "TimelinePinnedEntry"
        ) {
            let count = instr["entries"].as_array().map_or(0, Vec::len)
                + usize::from(instr["entry"].is_object())
                + instr["moduleItems"].as_array().map_or(0, Vec::len);
            if count > 0 {
                unhandled_instructions.push((instr_type.to_string(), count));
            }
        }
    }
    TimelinePage {
        tweets,
        non_cursor_entries,
        bottom_cursor,
        unhandled_instructions,
    }
}

fn cursor_entry_value(entry_id: &str, entry: &Value) -> Option<String> {
    let is_bottom = entry["content"]["cursorType"].as_str() == Some("Bottom")
        || entry_id.starts_with("cursor-bottom-");
    if !is_bottom {
        return None;
    }
    if let Some(v) = entry["content"]["value"].as_str() {
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    let suffix = entry_id.strip_prefix("cursor-bottom-")?;
    if suffix.is_empty() {
        return None;
    }
    Some(suffix.to_string())
}

fn unwrap_tweet_result(result: &Value) -> &Value {
    if result["__typename"].as_str() == Some("TweetWithVisibilityResults") {
        &result["tweet"]
    } else {
        result
    }
}

fn collect_tweet_results(entry: &Value, out: &mut Vec<Value>) {
    let entry_id = entry["entryId"].as_str().unwrap_or("");
    if entry_id.starts_with("cursor-") {
        return;
    }
    let tweet_result =
        unwrap_tweet_result(&entry["content"]["itemContent"]["tweet_results"]["result"]);
    if !tweet_result.is_null() && tweet_result.get("rest_id").is_some() {
        out.push(tweet_result.clone());
        return;
    }
    if let Some(items) = entry["content"]["items"].as_array() {
        for item in items {
            let tr = unwrap_tweet_result(&item["item"]["itemContent"]["tweet_results"]["result"]);
            if !tr.is_null() && tr.get("rest_id").is_some() {
                out.push(tr.clone());
            }
        }
    }
}

fn upgrade_x_cover(url: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, q),
        None => (url, ""),
    };
    let mut parts: Vec<String> = Vec::new();
    let mut has_name = false;
    for p in query.split('&').filter(|p| !p.is_empty()) {
        match p.strip_prefix("name=") {
            Some(_v) => {
                has_name = true;
                parts.push("name=large".to_string());
            }
            None => parts.push(p.to_string()),
        }
    }
    if !has_name {
        parts.push("name=large".to_string());
    }
    format!("{base}?{}", parts.join("&"))
}

fn x_photo_orig(url: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, q),
        None => (url, ""),
    };
    let mut parts: Vec<String> = Vec::new();
    let mut has_name = false;
    for p in query.split('&').filter(|p| !p.is_empty()) {
        match p.strip_prefix("name=") {
            Some(_) => {
                has_name = true;
                parts.push("name=orig".to_string());
            }
            None => parts.push(p.to_string()),
        }
    }
    if !has_name {
        parts.push("name=orig".to_string());
    }
    format!("{base}?{}", parts.join("&"))
}

fn append_media_tags(body: &mut String, legacy: &Value) {
    let Some(media_arr) = legacy["extended_entities"]["media"]
        .as_array()
        .or_else(|| legacy["entities"]["media"].as_array())
    else {
        return;
    };
    for m in media_arr {
        match m["type"].as_str().unwrap_or("") {
            "photo" => {
                if let Some(url) = m["media_url_https"].as_str() {
                    let orig = x_photo_orig(url);
                    body.push_str(&format!(
                        "<img src=\"{}\">",
                        crate::htmltext::escape_attr(&orig)
                    ));
                }
            }
            "video" | "animated_gif" => {
                let variants = m["video_info"]["variants"].as_array();
                let best = variants.and_then(|vs| {
                    vs.iter()
                        .filter(|v| v["content_type"].as_str() == Some("video/mp4"))
                        .max_by_key(|v| v["bitrate"].as_i64().unwrap_or(0))
                });
                if let Some(v) = best {
                    if let Some(url) = v["url"].as_str() {
                        let poster = m["preview_image_url"]["url"]
                            .as_str()
                            .or_else(|| m["preview_image_url"].as_str())
                            .or_else(|| m["media_url_https"].as_str())
                            .filter(|s| !s.is_empty())
                            .map(upgrade_x_cover);
                        match poster {
                            Some(p) => body.push_str(&format!(
                                "<video src=\"{}\" poster=\"{}\"></video>",
                                crate::htmltext::escape_attr(url),
                                crate::htmltext::escape_attr(&p)
                            )),
                            None => body.push_str(&format!(
                                "<video src=\"{}\"></video>",
                                crate::htmltext::escape_attr(url)
                            )),
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn extract_mention_names(result: &Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Some(arr) = result["legacy"]["entities"]["user_mentions"].as_array() {
        for m in arr {
            if let Some(s) = m["screen_name"].as_str() {
                if !s.is_empty() {
                    names.push(s.to_string());
                }
            }
        }
    }
    let note_entities = &result["note_tweet"]["note_tweet_results"]["result"]["entity_set"];
    let note_mentions = note_entities["user_mentions"]
        .as_array()
        .or_else(|| note_entities["mentions"].as_array());
    if let Some(arr) = note_mentions {
        for m in arr {
            if let Some(s) = m["screen_name"].as_str() {
                if !s.is_empty() && !names.iter().any(|n| n.eq_ignore_ascii_case(s)) {
                    names.push(s.to_string());
                }
            }
        }
    }
    names
}

fn linkify_mentions(html: &str, names: &[String]) -> String {
    if names.is_empty() {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len() + names.len() * 60);
    let mut rest = html;
    loop {
        match rest.find("<a ") {
            Some(ap) => {
                out.push_str(&linkify_text_segment(&rest[..ap], names));
                match rest[ap..].find("</a>") {
                    Some(cp) => {
                        let anchor_end = ap + cp + 4;
                        out.push_str(&rest[ap..anchor_end]);
                        rest = &rest[anchor_end..];
                    }
                    None => {
                        out.push_str(&linkify_text_segment(&rest[ap..], names));
                        break;
                    }
                }
            }
            None => {
                out.push_str(&linkify_text_segment(rest, names));
                break;
            }
        }
    }
    out
}

fn linkify_text_segment(text: &str, names: &[String]) -> String {
    let lower = text.to_ascii_lowercase();
    let mut matches: Vec<(usize, usize, &str)> = Vec::new();
    for name in names {
        let needle = format!("@{}", name).to_ascii_lowercase();
        let mut search_from = 0usize;
        while let Some(idx) = lower[search_from..].find(&needle) {
            let abs = search_from + idx;
            let end = abs + needle.len();
            let ok_before = abs == 0 || {
                let prev = text.as_bytes()[abs - 1];
                !(prev.is_ascii_alphanumeric() || prev == b'_')
            };
            let ok_after = end >= text.len() || {
                let next = text.as_bytes()[end];
                !(next.is_ascii_alphanumeric() || next == b'_')
            };
            if ok_before && ok_after {
                matches.push((abs, end, name.as_str()));
            }
            search_from = abs + 1;
        }
    }
    if matches.is_empty() {
        return text.to_string();
    }
    matches.sort_by_key(|&(s, _, _)| s);
    let mut out = String::with_capacity(text.len() + matches.len() * 60);
    let mut last_end = 0usize;
    for (start, end, name) in &matches {
        if *start < last_end {
            continue;
        }
        out.push_str(&text[last_end..*start]);
        out.push_str(&format!(
            "<a href=\"https://x.com/{}\">{}</a>",
            crate::htmltext::escape_attr(name),
            crate::htmltext::escape_html(&text[*start..*end])
        ));
        last_end = *end;
    }
    out.push_str(&text[last_end..]);
    out
}

fn linkify_hashtags(html: &str) -> String {
    let mut out = String::with_capacity(html.len() + 128);
    let mut rest = html;
    loop {
        match rest.find("<a ") {
            Some(ap) => {
                out.push_str(&linkify_hashtags_in_text(&rest[..ap]));
                match rest[ap..].find("</a>") {
                    Some(cp) => {
                        let anchor_end = ap + cp + 4;
                        out.push_str(&rest[ap..anchor_end]);
                        rest = &rest[anchor_end..];
                    }
                    None => {
                        out.push_str(&linkify_hashtags_in_text(&rest[ap..]));
                        break;
                    }
                }
            }
            None => {
                out.push_str(&linkify_hashtags_in_text(rest));
                break;
            }
        }
    }
    out
}

fn linkify_hashtags_in_text(text: &str) -> String {
    let mut matches: Vec<(usize, usize, bool)> = Vec::new();
    let bytes = text.as_bytes();

    for (i, &b) in bytes.iter().enumerate() {
        if b != b'#' && b != b'$' {
            continue;
        }
        if i > 0 {
            let prev = bytes[i - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' || prev == b';' || prev == b'&' {
                continue;
            }
        }
        let is_cashtag = b == b'$';
        let mut end = i + 1;
        while end < bytes.len() {
            let c = bytes[end];
            if c.is_ascii_alphanumeric() || c == b'_' {
                end += 1;
            } else {
                break;
            }
        }
        if end <= i + 1 {
            continue;
        }
        if is_cashtag {
            let tag_body = &text[i + 1..end];
            if !tag_body.chars().all(|c| c.is_ascii_alphabetic()) {
                continue;
            }
        }
        matches.push((i, end, is_cashtag));
    }

    if matches.is_empty() {
        return text.to_string();
    }

    matches.sort_by_key(|&(s, _, _)| s);
    let mut out = String::with_capacity(text.len() + matches.len() * 80);
    let mut last_end = 0usize;
    for &(start, end, is_cashtag) in &matches {
        if start < last_end {
            continue;
        }
        out.push_str(&text[last_end..start]);
        let tag_text = &text[start..end];
        let tag_body = &text[start + 1..end];
        let href = if is_cashtag {
            format!(
                "https://x.com/search?q=%24{}",
                crate::htmltext::escape_attr(tag_body)
            )
        } else {
            format!(
                "https://x.com/hashtag/{}",
                crate::htmltext::escape_attr(tag_body)
            )
        };
        out.push_str(&format!(
            "<a href=\"{}\">{}</a>",
            href,
            crate::htmltext::escape_html(tag_text)
        ));
        last_end = end;
    }
    out.push_str(&text[last_end..]);
    out
}

fn tweet_full_text(result: &Value) -> String {
    let legacy = &result["legacy"];
    let raw = result["note_tweet"]["note_tweet_results"]["result"]["text"]
        .as_str()
        .or_else(|| legacy["full_text"].as_str())
        .unwrap_or_default();
    let note_urls =
        result["note_tweet"]["note_tweet_results"]["result"]["entity_set"]["urls"].as_array();
    clean_tweet_text(raw, legacy, note_urls)
}

fn append_quote_block(body: &mut String, result: &Value, prefix: &str) {
    let result = unwrap_tweet_result(result);
    if !quote_renderable(result) {
        return;
    }
    let legacy = &result["legacy"];
    let text = tweet_full_text(result);
    let screen = screen_name_opt(result);
    let mut mentions = extract_mention_names(result);
    if let Some(s) = screen {
        if !mentions.iter().any(|n| n.eq_ignore_ascii_case(s)) {
            mentions.push(s.to_string());
        }
    }
    let mut inner = linkify_hashtags(&linkify_mentions(&tweet_text_to_html(&text), &mentions));
    append_media_tags(&mut inner, legacy);
    let author = match screen {
        Some(s) => {
            let link = match legacy["id_str"].as_str() {
                Some(tid) => format!("https://x.com/{}/status/{}", s, tid),
                None => format!("https://x.com/{}", s),
            };
            format!(
                "<a href=\"{}\">@{}</a>: ",
                crate::htmltext::escape_attr(&link),
                crate::htmltext::escape_html(s)
            )
        }
        None => String::new(),
    };
    if !body.is_empty() {
        body.push('\n');
    }
    body.push_str(&format!(
        "<blockquote>{} {}{}</blockquote>",
        prefix, author, inner
    ));
}

enum ConvertOutcome {
    Ok(Box<Entry>),
    SkippedReply,
    Failed,
}

fn tweet_to_entry(
    tweet: &Value,
    screen_name: &str,
    display_name: &str,
    lang: Lang,
) -> ConvertOutcome {
    let tweet = unwrap_tweet_result(tweet);

    let legacy = &tweet["legacy"];
    if legacy.is_null() {
        return ConvertOutcome::Failed;
    }
    let Some(id_str) = legacy["id_str"]
        .as_str()
        .or_else(|| tweet["rest_id"].as_str())
    else {
        return ConvertOutcome::Failed;
    };
    let id = id_str.to_string();

    if legacy["in_reply_to_status_id_str"].is_string() {
        return ConvertOutcome::SkippedReply;
    }

    let text = tweet_full_text(tweet);

    let rt = &legacy["retweeted_status_result"]["result"];
    let is_pure_rt = !rt.is_null() && text.starts_with("RT @");

    let mut body = String::new();
    if !is_pure_rt {
        let mut mentions = extract_mention_names(tweet);
        if !mentions.iter().any(|n| n.eq_ignore_ascii_case(screen_name)) {
            mentions.push(screen_name.to_string());
        }
        body = linkify_hashtags(&linkify_mentions(&tweet_text_to_html(&text), &mentions));
        append_media_tags(&mut body, legacy);
    }

    if !rt.is_null() {
        append_quote_block(&mut body, rt, "RT");
    }

    let quote = &tweet["quoted_status_result"]["result"];
    if !quote.is_null() {
        append_quote_block(&mut body, quote, "QT");
    }

    let published = parse_twitter_time(legacy["created_at"].as_str().unwrap_or(""));

    let title = crate::platforms::make_title(&text, i18n::t(lang, "x_tweet_title"));

    let link = format!("https://x.com/{}/status/{}", screen_name, id);

    ConvertOutcome::Ok(Box::new(crate::platforms::make_entry(
        id,
        title,
        display_name.to_string(),
        body,
        "text/html",
        link,
        published,
    )))
}

fn quote_renderable(tr: &Value) -> bool {
    let legacy = &tr["legacy"];
    if !legacy.is_object() {
        return false;
    }
    let has_id = legacy["id_str"].as_str().is_some_and(|s| !s.is_empty())
        || tr["rest_id"].as_str().is_some_and(|s| !s.is_empty());
    let has_text = !tweet_full_text(tr).trim().is_empty();
    let has_media =
        legacy["extended_entities"]["media"].is_array() || legacy["entities"]["media"].is_array();
    has_id && (has_text || has_media)
}

fn screen_name_opt(tweet_result: &Value) -> Option<&str> {
    let tr = unwrap_tweet_result(tweet_result);
    if let Some(s) = tr["core"]["screen_name"].as_str() {
        return Some(s);
    }
    if let Some(s) = tr["core"]["user_results"]["result"]["legacy"]["screen_name"].as_str() {
        return Some(s);
    }
    if let Some(s) = tr["core"]["user_results"]["result"]["core"]["screen_name"].as_str() {
        return Some(s);
    }
    if let Some(s) = tr["user_results"]["result"]["legacy"]["screen_name"].as_str() {
        return Some(s);
    }
    tr["legacy"]["screen_name"].as_str()
}

fn clean_tweet_text(full_text: &str, legacy: &Value, note_urls: Option<&Vec<Value>>) -> String {
    let mut text = full_text.to_string();

    let media_entities = legacy["extended_entities"]["media"]
        .as_array()
        .or_else(|| legacy["entities"]["media"].as_array());
    if let Some(arr) = media_entities {
        for m in arr {
            if let Some(tco) = m["url"].as_str() {
                text = text.replace(tco, "");
            }
        }
    }

    let url_entities = note_urls.or_else(|| legacy["entities"]["urls"].as_array());
    if let Some(urls) = url_entities {
        for u in urls {
            let tco = u["url"].as_str().unwrap_or_default();
            let expanded = u["expanded_url"].as_str().unwrap_or_default();
            if !tco.is_empty() && !expanded.is_empty() {
                text = text.replace(tco, expanded);
            }
        }
    }

    text.trim_end().to_string()
}

fn tweet_text_to_html(text: &str) -> String {
    static RE_URL: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"https?://[^\s<>"]+"#).unwrap());
    let mut out = String::new();
    let mut last = 0usize;
    for caps in RE_URL.captures_iter(text) {
        let m = caps.get(0).unwrap();
        out.push_str(&crate::htmltext::escape_html(&text[last..m.start()]));
        let raw = m.as_str();
        let ascii_len = raw
            .char_indices()
            .find(|(_, c)| !c.is_ascii())
            .map(|(i, _)| i)
            .unwrap_or(raw.len());
        let mut url = &raw[..ascii_len];
        loop {
            let before = url;
            while url.ends_with(['.', ',', ';', '!', '?']) {
                url = &url[..url.len() - 1];
            }
            for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
                while url.ends_with(close) && url.matches(close).count() > url.matches(open).count()
                {
                    url = &url[..url.len() - close.len_utf8()];
                }
            }
            if url.len() == before.len() {
                break;
            }
        }
        out.push_str(&format!(
            "<a href=\"{}\">{}</a>",
            crate::htmltext::escape_attr(url),
            crate::htmltext::escape_html(url)
        ));
        let stripped = &raw[url.len()..];
        out.push_str(&crate::htmltext::escape_html(stripped));
        last = m.end();
    }
    out.push_str(&crate::htmltext::escape_html(&text[last..]));
    out.replace('\n', "<br>")
}

fn parse_twitter_time(s: &str) -> Option<Timestamp> {
    Zoned::strptime("%a %b %d %H:%M:%S %z %Y", s)
        .ok()
        .map(|z| z.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_body_classification() {
        assert!(is_stale_ct0_response(
            r#"{"errors":[{"code":353,"message":"This request requires a matching csrf cookie and header."}]}"#
        ));
        assert!(is_stale_ct0_response("ct0 is invalid"));
        assert!(!is_stale_ct0_response(
            "This account is protected and you are not authorized to view it"
        ));
        assert!(!is_stale_ct0_response(""));
    }

    #[test]
    fn parse_screen_name_valid() {
        assert_eq!(
            parse_screen_name("https://x.com/elonmusk"),
            Some("elonmusk".into())
        );
        assert_eq!(
            parse_screen_name("https://twitter.com/NASA"),
            Some("NASA".into())
        );
        assert_eq!(
            parse_screen_name("https://www.x.com/some_user"),
            Some("some_user".into())
        );
        assert_eq!(parse_screen_name("https://x.com/a"), Some("a".into()));
    }

    #[test]
    fn parse_screen_name_invalid() {
        assert_eq!(parse_screen_name("https://x.com/"), None);
        assert_eq!(parse_screen_name("https://x.com/home"), None);
        assert_eq!(parse_screen_name("https://x.com/search?q=test"), None);
        assert_eq!(parse_screen_name("https://x.com/i/flow/login"), None);
        assert_eq!(parse_screen_name("https://x.com/settings"), None);
        assert_eq!(parse_screen_name("https://example.com/elonmusk"), None);
        assert_eq!(parse_screen_name("https://x.com/abcdefghijklmnop"), None);
    }

    #[test]
    fn parse_twitter_time_works() {
        let t = parse_twitter_time("Wed Oct 10 20:19:24 +0000 2018");
        assert!(t.is_some());
        let dt = t.unwrap();
        assert_eq!(
            dt.to_zoned(jiff::tz::TimeZone::UTC)
                .strftime("%Y-%m-%d")
                .to_string(),
            "2018-10-10"
        );
    }

    #[test]
    fn extract_cookie_value_works() {
        let header = "ct0=abc123xyz; Max-Age=14400; Domain=.x.com; Path=/; Secure";
        assert_eq!(
            extract_cookie_value(header, "ct0"),
            Some("abc123xyz".into())
        );
        assert_eq!(extract_cookie_value(header, "other"), None);
    }

    #[test]
    fn extract_cookie_value_scans_all_segments() {
        let header = "guest_id=v1%3A123; ct0=abc123xyz; Path=/; Secure";
        assert_eq!(
            extract_cookie_value(header, "ct0"),
            Some("abc123xyz".into())
        );
        assert_eq!(
            extract_cookie_value(header, "guest_id"),
            Some("v1%3A123".into())
        );
    }

    #[test]
    fn extract_timeline_page_reads_bottom_cursor_from_content_value() {
        let instructions = serde_json::json!([
            {
                "type": "TimelineAddEntries",
                "entries": [
                    {"entryId": "cursor-top-1", "content": {"cursorType": "Top", "value": "TOPCURSOR"}},
                    {"entryId": "tweet-123", "content": {"itemContent": {"tweet_results": {"result":
                        {"rest_id": "123", "legacy": {"id_str": "123"}}}}}},
                    {"entryId": "cursor-bottom-1", "content": {"cursorType": "Bottom", "value": "BOTTOMCURSOR"}}
                ]
            }
        ]);
        let page = extract_timeline_page(&instructions);
        assert_eq!(page.tweets.len(), 1);
        assert_eq!(page.non_cursor_entries, 1);
        assert_eq!(page.bottom_cursor.as_deref(), Some("BOTTOMCURSOR"));
    }

    #[test]
    fn extract_timeline_page_reads_cursor_from_suggest_cursor_and_entry_id() {
        let instructions = serde_json::json!([
            {"type": "TimelineAddEntries", "entries": [
                {"entryId": "tweet-1", "content": {"itemContent": {"tweet_results": {"result":
                    {"rest_id": "1", "legacy": {"id_str": "1"}}}}}}
            ]},
            {"type": "TimelineSuggestCursor", "entry":
                {"entryId": "cursor-bottom-ABC123", "content": {"cursorType": "Bottom"}}}
        ]);
        let page = extract_timeline_page(&instructions);
        assert_eq!(page.tweets.len(), 1);
        assert_eq!(page.bottom_cursor.as_deref(), Some("ABC123"));
    }

    #[test]
    fn extract_timeline_page_no_cursor_when_bottom_missing() {
        let instructions = serde_json::json!([
            {"type": "TimelineAddEntries", "entries": [
                {"entryId": "cursor-top-1", "content": {"cursorType": "Top", "value": "TOP"}},
                {"entryId": "tweet-1", "content": {"itemContent": {"tweet_results": {"result":
                    {"rest_id": "1", "legacy": {"id_str": "1"}}}}}}
            ]}
        ]);
        let page = extract_timeline_page(&instructions);
        assert_eq!(page.bottom_cursor, None);
    }

    #[test]
    fn fixture_timeline_instructions_cover_all_supported_shapes() {
        let instructions: Value =
            serde_json::from_str(include_str!("fixtures/x_user_tweets_instructions.json"))
                .expect("fixture JSON");
        let page = extract_timeline_page(&instructions);
        assert_eq!(page.non_cursor_entries, 3);
        assert_eq!(page.tweets.len(), 4);
        let ids: Vec<&str> = page
            .tweets
            .iter()
            .filter_map(|tweet| tweet["rest_id"].as_str())
            .collect();
        assert_eq!(ids, vec!["9001", "9002", "9003", "9004"]);
        assert_eq!(page.bottom_cursor.as_deref(), Some("fixture-bottom-cursor"));
        assert!(page.unhandled_instructions.is_empty());
    }

    #[test]
    fn extract_timeline_page_reports_unhandled_instruction_types() {
        let instructions = serde_json::json!([
            {"type": "TimelineAddEntries", "entries": [
                {"entryId": "tweet-1", "content": {"itemContent": {"tweet_results": {"result":
                    {"rest_id": "1", "legacy": {"id_str": "1"}}}}}}
            ]},
            {"type": "TimelineAddToModule", "moduleItems": [{"item": {}}]},
            {"type": "TimelineReplaceEntry", "entry": {"entryId": "tweet-2"}},
            {"type": "TimelinePinEntry", "entry": {
                "entryId": "pin-1",
                "content": {"itemContent": {"tweet_results": {"result":
                    {"rest_id": "2", "legacy": {"id_str": "2"}}}}}
            }},
            {"type": "TimelineClearCache"}
        ]);
        let page = extract_timeline_page(&instructions);
        assert_eq!(page.tweets.len(), 1);
        assert_eq!(page.non_cursor_entries, 1);
        assert_eq!(
            page.unhandled_instructions,
            vec![
                ("TimelineAddToModule".to_string(), 1),
                ("TimelineReplaceEntry".to_string(), 1),
            ]
        );
    }

    #[test]
    fn tweet_to_entry_classifies_reply_as_expected_skip() {
        let reply = serde_json::json!({
            "rest_id": "5",
            "legacy": {
                "id_str": "5",
                "full_text": "hi",
                "in_reply_to_status_id_str": "4"
            }
        });
        assert!(matches!(
            tweet_to_entry(&reply, "user", "User", Lang::Zh),
            ConvertOutcome::SkippedReply
        ));
        let broken = serde_json::json!({"rest_id": "6"});
        assert!(matches!(
            tweet_to_entry(&broken, "user", "User", Lang::Zh),
            ConvertOutcome::Failed
        ));
        let ok = serde_json::json!({
            "rest_id": "7",
            "legacy": {"id_str": "7", "full_text": "hello"}
        });
        assert!(matches!(
            tweet_to_entry(&ok, "user", "User", Lang::Zh),
            ConvertOutcome::Ok(_)
        ));
        let empty = serde_json::json!({
            "rest_id": "8",
            "legacy": {"id_str": "8", "full_text": ""}
        });
        let ConvertOutcome::Ok(entry) = tweet_to_entry(&empty, "user", "User", Lang::En) else {
            panic!("empty tweet should still convert");
        };
        assert_eq!(entry.title.as_ref().unwrap().content, "(Tweet)");
    }

    #[test]
    fn linkify_mentions_basic() {
        let names = vec!["elonmusk".to_string()];
        let html = "Hello @elonmusk how are you";
        let out = linkify_mentions(html, &names);
        assert_eq!(
            out,
            "Hello <a href=\"https://x.com/elonmusk\">@elonmusk</a> how are you"
        );
    }

    #[test]
    fn linkify_mentions_case_insensitive_match_preserves_original_case() {
        let names = vec!["elonmusk".to_string()];
        let html = "cc @ElonMusk";
        let out = linkify_mentions(html, &names);
        assert_eq!(out, "cc <a href=\"https://x.com/elonmusk\">@ElonMusk</a>");
    }

    #[test]
    fn linkify_mentions_skips_inside_existing_anchor() {
        let names = vec!["elonmusk".to_string()];
        let html = "<a href=\"https://x.com/elonmusk\">@elonmusk</a> and @elonmusk";
        let out = linkify_mentions(html, &names);
        assert_eq!(
            out,
            "<a href=\"https://x.com/elonmusk\">@elonmusk</a> and <a href=\"https://x.com/elonmusk\">@elonmusk</a>"
        );
    }

    #[test]
    fn linkify_mentions_boundary_checks() {
        let names = vec!["elonmusk".to_string()];
        assert_eq!(linkify_mentions("foo@elonmusk", &names), "foo@elonmusk");
        assert_eq!(linkify_mentions("@elonmusk2x", &names), "@elonmusk2x");
        assert_eq!(linkify_mentions("@elonmusk_", &names), "@elonmusk_");
        assert!(linkify_mentions("@elonmusk", &names).contains("<a href"));
        assert!(linkify_mentions("(@elonmusk)", &names).contains("<a href"));
    }

    #[test]
    fn linkify_mentions_no_names_noop() {
        let html = "Hello @elonmusk";
        assert_eq!(linkify_mentions(html, &[]), html);
    }

    #[test]
    fn linkify_mentions_multiple_mentions() {
        let names = vec!["alice".to_string(), "bob".to_string()];
        let html = "@alice meets @bob";
        let out = linkify_mentions(html, &names);
        assert_eq!(
            out,
            "<a href=\"https://x.com/alice\">@alice</a> meets <a href=\"https://x.com/bob\">@bob</a>"
        );
    }

    #[test]
    fn linkify_mentions_self_mention_by_author() {
        let names = vec!["SpaceXSI".to_string(), "elonmusk".to_string()];
        let html = "Great work @SpaceXSI — @elonmusk";
        let out = linkify_mentions(html, &names);
        assert!(
            out.contains(r#"<a href="https://x.com/SpaceXSI">@SpaceXSI</a>"#),
            "out: {}",
            out
        );
        assert!(
            out.contains(r#"<a href="https://x.com/elonmusk">@elonmusk</a>"#),
            "out: {}",
            out
        );
    }

    #[test]
    fn extract_mention_names_from_legacy_entities() {
        let json: Value = serde_json::json!({
            "legacy": {
                "entities": {
                    "user_mentions": [
                        {"screen_name": "alice"},
                        {"screen_name": "bob"}
                    ]
                }
            }
        });
        let names = extract_mention_names(&json);
        assert_eq!(names, vec!["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn extract_mention_names_dedupes_note_tweet_mentions() {
        let json: Value = serde_json::json!({
            "legacy": {
                "entities": {
                    "user_mentions": [{"screen_name": "alice"}]
                }
            },
            "note_tweet": {
                "note_tweet_results": {
                    "result": {
                        "entity_set": {
                            "user_mentions": [
                                {"screen_name": "Alice"},
                                {"screen_name": "carol"}
                            ]
                        }
                    }
                }
            }
        });
        let names = extract_mention_names(&json);
        assert_eq!(names, vec!["alice".to_string(), "carol".to_string()]);
    }

    #[test]
    fn extract_mention_names_note_tweet_mentions_alias() {
        let json: Value = serde_json::json!({
            "note_tweet": {
                "note_tweet_results": {
                    "result": {
                        "entity_set": {
                            "mentions": [{"screen_name": "dave"}]
                        }
                    }
                }
            }
        });
        assert_eq!(extract_mention_names(&json), vec!["dave".to_string()]);
    }

    #[test]
    fn append_quote_block_links_note_tweet_mentions() {
        let legacy_text = "x".repeat(300);
        let note_text = format!("{legacy_text} cc @note_mention here.");
        let tweet: Value = serde_json::json!({
            "rest_id": "1001",
            "legacy": {
                "id_str": "1001",
                "full_text": legacy_text,
                "entities": {
                    "user_mentions": [],
                    "media": []
                }
            },
            "note_tweet": {
                "note_tweet_results": {
                    "result": {
                        "text": note_text,
                        "entity_set": {
                            "urls": [],
                            "user_mentions": [{"screen_name": "note_mention"}]
                        }
                    }
                }
            },
            "core": {
                "user_results": {
                    "result": {
                        "legacy": { "screen_name": "note_author" }
                    }
                }
            }
        });
        let mut body = String::new();
        append_quote_block(&mut body, &tweet, "RT");
        assert!(
            body.contains(r#"<a href="https://x.com/note_mention">@note_mention</a>"#),
            "note-tweet mention not linked: {body}"
        );
    }

    #[test]
    fn tweet_to_entry_links_note_tweet_mentions() {
        let legacy_text = "x".repeat(300);
        let note_text = format!("{legacy_text} cc @note_mention here.");
        let tweet: Value = serde_json::json!({
            "rest_id": "1002",
            "legacy": {
                "id_str": "1002",
                "full_text": legacy_text,
                "entities": {
                    "user_mentions": [],
                    "media": []
                }
            },
            "note_tweet": {
                "note_tweet_results": {
                    "result": {
                        "text": note_text,
                        "entity_set": {
                            "urls": [],
                            "user_mentions": [{"screen_name": "note_mention"}]
                        }
                    }
                }
            },
            "core": {
                "user_results": {
                    "result": {
                        "core": { "screen_name": "note_author" }
                    }
                }
            }
        });
        let ConvertOutcome::Ok(entry) =
            tweet_to_entry(&tweet, "note_author", "Note Author", Lang::En)
        else {
            panic!("note tweet should convert");
        };
        let body = entry
            .content
            .as_ref()
            .and_then(|c| c.body.as_deref())
            .unwrap_or_default();
        assert!(
            body.contains(r#"<a href="https://x.com/note_mention">@note_mention</a>"#),
            "note-tweet mention not linked: {body}"
        );
    }

    #[test]
    fn tweet_to_entry_renders_top_level_quoted_tweet() {
        let tweet: Value = serde_json::json!({
            "rest_id": "2001",
            "legacy": {
                "id_str": "2001",
                "full_text": "my comment",
                "entities": {
                    "user_mentions": [],
                    "media": []
                },
                "is_quote_status": true,
                "quoted_status_id_str": "2002"
            },
            "quoted_status_result": {
                "result": {
                    "__typename": "Tweet",
                    "rest_id": "2002",
                    "legacy": {
                        "id_str": "2002",
                        "full_text": "quoted text",
                        "entities": {
                            "user_mentions": [],
                            "media": []
                        }
                    },
                    "core": {
                        "user_results": {
                            "result": {
                                "__typename": "User",
                                "core": { "screen_name": "quoted_user" }
                            }
                        }
                    }
                }
            },
            "core": {
                "user_results": {
                    "result": {
                        "core": { "screen_name": "outer_user" }
                    }
                }
            }
        });
        let ConvertOutcome::Ok(entry) = tweet_to_entry(&tweet, "outer_user", "Outer", Lang::En)
        else {
            panic!("quote tweet should convert");
        };
        let body = entry
            .content
            .as_ref()
            .and_then(|c| c.body.as_deref())
            .unwrap_or_default();
        assert!(
            body.contains("<blockquote>QT "),
            "quote block missing: {body}"
        );
        assert!(
            body.contains(r#"<a href="https://x.com/quoted_user/status/2002">@quoted_user</a>"#),
            "quote author link missing: {body}"
        );
        assert!(body.contains("quoted text"), "quoted text missing: {body}");
    }

    #[test]
    fn screen_name_opt_reads_core_user_results_core_path() {
        let tweet: Value = serde_json::json!({
            "core": {
                "user_results": {
                    "result": {
                        "__typename": "User",
                        "core": { "screen_name": "alice" }
                    }
                }
            },
            "legacy": {}
        });
        assert_eq!(screen_name_opt(&tweet), Some("alice"));
    }

    #[test]
    fn append_quote_block_newline_separator() {
        let tweet: Value = serde_json::json!({
            "legacy": {
                "full_text": "quoted text",
                "id_str": "123",
                "entities": {
                    "user_mentions": [],
                    "media": []
                }
            },
            "core": {
                "user_results": {
                    "result": {
                        "legacy": { "screen_name": "someuser" }
                    }
                }
            }
        });
        let mut body = String::from("Grok <a href=\"https://x.com/Bot\">@Bot</a>");
        append_quote_block(&mut body, &tweet, "QT");
        assert!(
            body.contains("@Bot</a>\n<blockquote>"),
            "missing \\n separator: {}",
            body
        );
        assert!(
            !body.contains("@Bot</a><blockquote>"),
            "blockquote glued to anchor: {}",
            body
        );

        let mut empty_body = String::new();
        append_quote_block(&mut empty_body, &tweet, "RT");
        assert!(
            empty_body.starts_with("<blockquote>"),
            "empty body should not have leading \\n: {}",
            empty_body
        );
    }

    #[test]
    fn linkify_hashtags_basic() {
        let out = linkify_hashtags("Hello #Tesla world");
        assert!(
            out.contains("<a href=\"https://x.com/hashtag/Tesla\">#Tesla</a>"),
            "got: {}",
            out
        );
        assert!(out.starts_with("Hello "), "prefix lost: {}", out);
        assert!(out.ends_with(" world"), "suffix lost: {}", out);
    }

    #[test]
    fn linkify_hashtags_cashtag() {
        let out = linkify_hashtags("Buy $TSLA now");
        assert!(
            out.contains("<a href=\"https://x.com/search?q=%24TSLA\">$TSLA</a>"),
            "got: {}",
            out
        );
    }

    #[test]
    fn linkify_hashtags_skips_numeric_cashtag() {
        let out = linkify_hashtags("Price is $5 and $100");
        assert!(
            !out.contains("<a href"),
            "numeric should not linkify: {}",
            out
        );
    }

    #[test]
    fn linkify_hashtags_skips_inside_anchor() {
        let out = linkify_hashtags("<a href=\"https://x.com/foo\">#Tesla</a> plain #SpaceX");
        assert_eq!(out.matches("<a href").count(), 2, "got: {}", out);
        assert!(
            out.contains("#SpaceX</a>"),
            "plain hashtag should be linkified: {}",
            out
        );
    }

    #[test]
    fn linkify_hashtags_boundary_checks() {
        assert_eq!(linkify_hashtags("foo#Tesla"), "foo#Tesla");
        assert_eq!(linkify_hashtags("foo_#Tesla"), "foo_#Tesla");
        assert_eq!(linkify_hashtags("&amp;#123;"), "&amp;#123;");
        assert_eq!(linkify_hashtags("text;&#35;"), "text;&#35;");
        assert!(linkify_hashtags("#Tesla is great").contains("<a href"));
        assert!(linkify_hashtags("(#Tesla)").contains("<a href"));
    }

    #[test]
    fn linkify_hashtags_multiple() {
        let out = linkify_hashtags("#DavidLetterman #ElonMusk #Tesla");
        assert_eq!(out.matches("<a href").count(), 3, "got: {}", out);
        assert!(out.contains("hashtag/DavidLetterman"), "got: {}", out);
        assert!(out.contains("hashtag/ElonMusk"), "got: {}", out);
        assert!(out.contains("hashtag/Tesla"), "got: {}", out);
    }

    #[test]
    fn linkify_hashtags_no_tags_noop() {
        let html = "Hello world, no tags here.";
        assert_eq!(linkify_hashtags(html), html);
    }

    #[test]
    fn linkify_hashtags_with_br() {
        let out = linkify_hashtags("text<br>#Tesla<br>more");
        assert!(
            out.contains("<a href=\"https://x.com/hashtag/Tesla\">#Tesla</a>"),
            "got: {}",
            out
        );
    }

    #[test]
    fn tweet_text_to_html_strips_trailing_unbalanced_paren() {
        let out = tweet_text_to_html("see (https://x.com/a)");
        assert!(
            out.contains("<a href=\"https://x.com/a\">https://x.com/a</a>)"),
            "got: {}",
            out
        );
    }

    #[test]
    fn tweet_text_to_html_keeps_balanced_parens_in_url() {
        let out = tweet_text_to_html("https://en.wikipedia.org/wiki/Foo_(bar)");
        assert!(
            out.contains("<a href=\"https://en.wikipedia.org/wiki/Foo_(bar)\">"),
            "got: {}",
            out
        );
    }

    #[test]
    fn tweet_text_to_html_strips_trailing_punct_then_paren() {
        let out = tweet_text_to_html("(https://x.com/a).");
        assert!(
            out.contains("<a href=\"https://x.com/a\">https://x.com/a</a>)."),
            "got: {}",
            out
        );
    }

    #[test]
    fn tweet_text_to_html_stops_url_before_cjk_text() {
        let out = tweet_text_to_html("见 https://x.com/a，然后继续");
        assert!(
            out.contains("<a href=\"https://x.com/a\">https://x.com/a</a>，然后继续"),
            "got: {}",
            out
        );
        assert!(
            !out.contains("x.com/a，"),
            "CJK punctuation leaked into href: {out}"
        );
    }

    #[test]
    fn collect_tweet_results_unwraps_visibility_wrapper() {
        let entry = serde_json::json!({
            "entryId": "tweet-1",
            "content": {
                "itemContent": {
                    "tweet_results": {
                        "result": {
                            "__typename": "TweetWithVisibilityResults",
                            "tweet": { "rest_id": "123", "legacy": { "id_str": "123" } }
                        }
                    }
                }
            }
        });
        let mut out = Vec::new();
        collect_tweet_results(&entry, &mut out);
        assert_eq!(out.len(), 1, "wrapper tweet should be collected");
        assert_eq!(out[0]["rest_id"].as_str(), Some("123"));
    }

    #[test]
    fn unwrap_tweet_result_plain_passthrough() {
        let plain = serde_json::json!({ "rest_id": "999", "legacy": {} });
        assert_eq!(unwrap_tweet_result(&plain)["rest_id"].as_str(), Some("999"));
    }

    #[test]
    fn append_media_tags_video_poster_object_shape() {
        let legacy: Value = serde_json::json!({
            "extended_entities": { "media": [{
                "type": "video",
                "preview_image_url": { "url": "https://pbs.twimg.com/media/COVER.jpg" },
                "video_info": { "variants": [
                    { "content_type": "video/mp4", "bitrate": 832000, "url": "https://video.twimg.com/x/low.mp4" },
                    { "content_type": "video/mp4", "bitrate": 2560000, "url": "https://video.twimg.com/x/hd.mp4" }
                ]}
            }]}
        });
        let mut body = String::new();
        append_media_tags(&mut body, &legacy);
        assert_eq!(
            body,
            "<video src=\"https://video.twimg.com/x/hd.mp4\" poster=\"https://pbs.twimg.com/media/COVER.jpg?name=large\"></video>"
        );
    }

    #[test]
    fn append_media_tags_video_poster_bare_string_shape() {
        let legacy: Value = serde_json::json!({
            "extended_entities": { "media": [{
                "type": "video",
                "preview_image_url": "https://pbs.twimg.com/media/COVER2.jpg?name=small",
                "video_info": { "variants": [
                    { "content_type": "video/mp4", "bitrate": 100, "url": "https://video.twimg.com/y/a.mp4" }
                ]}
            }]}
        });
        let mut body = String::new();
        append_media_tags(&mut body, &legacy);
        assert!(body.contains("poster=\"https://pbs.twimg.com/media/COVER2.jpg?name=large\""));
        assert!(!body.contains("name=small"));
    }

    #[test]
    fn x_photo_orig_replaces_existing_name() {
        assert_eq!(
            x_photo_orig("https://pbs.twimg.com/media/A.jpg"),
            "https://pbs.twimg.com/media/A.jpg?name=orig"
        );
        assert_eq!(
            x_photo_orig("https://pbs.twimg.com/media/B.jpg?format=jpg&name=small"),
            "https://pbs.twimg.com/media/B.jpg?format=jpg&name=orig"
        );
        assert_eq!(
            x_photo_orig("https://pbs.twimg.com/media/C.jpg?name=orig"),
            "https://pbs.twimg.com/media/C.jpg?name=orig"
        );
        assert!(
            !x_photo_orig("https://pbs.twimg.com/media/D.jpg?name=small").contains("name=small")
        );
    }

    #[test]
    fn upgrade_x_cover_modes() {
        assert_eq!(
            upgrade_x_cover("https://pbs.twimg.com/media/A.jpg"),
            "https://pbs.twimg.com/media/A.jpg?name=large"
        );
        assert_eq!(
            upgrade_x_cover("https://pbs.twimg.com/media/B.jpg?format=jpg&name=small"),
            "https://pbs.twimg.com/media/B.jpg?format=jpg&name=large"
        );
        assert_eq!(
            upgrade_x_cover("https://pbs.twimg.com/media/C.jpg?name=orig"),
            "https://pbs.twimg.com/media/C.jpg?name=large"
        );
        assert_eq!(
            upgrade_x_cover("https://pbs.twimg.com/media/D.jpg?name=large"),
            "https://pbs.twimg.com/media/D.jpg?name=large"
        );
    }

    #[test]
    fn append_media_tags_video_poster_falls_back_to_media_url() {
        let legacy: Value = serde_json::json!({
            "extended_entities": { "media": [{
                "type": "video",
                "media_url_https": "https://pbs.twimg.com/amplify_video_thumb/123/img/cover.jpg",
                "video_info": { "variants": [
                    { "content_type": "video/mp4", "bitrate": 100, "url": "https://video.twimg.com/x/a.mp4?tag=29" }
                ]}
            }]}
        });
        let mut body = String::new();
        append_media_tags(&mut body, &legacy);
        assert_eq!(
            body,
            "<video src=\"https://video.twimg.com/x/a.mp4?tag=29\" poster=\"https://pbs.twimg.com/amplify_video_thumb/123/img/cover.jpg?name=large\"></video>"
        );
    }

    #[test]
    fn append_media_tags_video_without_preview_keeps_plain_tag() {
        let legacy: Value = serde_json::json!({
            "extended_entities": { "media": [{
                "type": "animated_gif",
                "video_info": { "variants": [
                    { "content_type": "video/mp4", "bitrate": 0, "url": "https://video.twimg.com/z/g.mp4" }
                ]}
            }]}
        });
        let mut body = String::new();
        append_media_tags(&mut body, &legacy);
        assert_eq!(
            body,
            "<video src=\"https://video.twimg.com/z/g.mp4\"></video>"
        );
    }

    #[test]
    fn pinned_timeline_instruction_is_intentionally_ignored() {
        let instructions = serde_json::json!([{
            "type": "TimelinePinEntry",
            "entry": {
                "entryId": "pin-1",
                "content": {
                    "itemContent": {
                        "tweet_results": {
                            "result": {
                                "rest_id": "1",
                                "legacy": { "id_str": "1", "full_text": "pinned" }
                            }
                        }
                    }
                }
            }
        }]);
        let page = extract_timeline_page(&instructions);
        assert!(page.tweets.is_empty());
        assert_eq!(page.non_cursor_entries, 0);
    }

    #[test]
    fn tombstoned_quote_is_not_renderable() {
        let tombstone: Value = serde_json::json!({
            "__typename": "TweetTombstone",
            "tombstone": { "text": { "text": "This Tweet is unavailable" } }
        });
        assert!(!quote_renderable(unwrap_tweet_result(&tombstone)));
        let normal: Value = serde_json::json!({
            "rest_id": "123",
            "legacy": { "id_str": "123", "full_text": "hello" }
        });
        assert!(quote_renderable(&normal));
    }
}
