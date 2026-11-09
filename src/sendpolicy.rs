//! Send policy: per-chat pacing, flood-control backoff, and retryable-error
//! classification for Telegram sends.
//!
//! Extracted from push.rs (batch-B2 architecture refactor) so the
//! rate-limiting / retry-strategy concern lives apart from message rendering
//! and media orchestration. Pure policy — no rendering, no DB.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Retry seconds extracted from a Telegram flood control error.
/// Case-insensitive: teloxide/Telegram Display text has been observed with
/// both "Retry after N" and lowercase "retry after N" spellings across
/// versions; anchoring to one case silently disables flood backoff when the
/// wording drifts (v1.11.2 #25 typed the "message is not modified" check;
/// this hardens the flood path the same way is not possible without a typed
/// variant, so at least match case-insensitively).
static RE_RETRY_AFTER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)Retry after (\d+)").unwrap());

/// Extract the flood-control backoff seconds demanded by Telegram from an
/// error string (capped at 60s). Returns None for non-flood errors. Shared
/// by both the text and media send paths.
pub fn flood_retry_secs(err_str: &str) -> Option<u64> {
    RE_RETRY_AFTER
        .captures(err_str)
        .map(|caps| caps[1].parse().unwrap_or(5).min(60))
}

/// Only retry on connection-class errors that are definitely undelivered;
/// timeouts are not retried — on timeout the server may have already received
/// and started processing the request, and resending that op would duplicate
/// the media group.
/// (#14 fix: extracted as a single module-level function, deduplicating the
/// two original closures.)
/// pub: the scheduler's transient RSS-fetch retry (scheduler.rs `process_feed`,
/// the v1.10.3 debounce path) reuses this same criterion via
/// `sendpolicy::is_net_error`, so fetch and send classify connection errors
/// identically.
pub fn is_net_error(err_str: &str) -> bool {
    // Two reqwest-flavoured prefixes plus the pre-send DNS check from
    // fetcher.rs (entry + redirect hops): "dns resolve failed: ..." is
    // emitted when tokio's lookup_host cannot confirm the target. Matching it
    // here lets the scheduler's one-shot retry absorb a transient DNS blip
    // instead of counting it on the first round; a *sustained* DNS outage
    // still escalates (the retry fails too and the error is counted), which is
    // correct — repeated unreachability should eventually disable the source.
    (err_str.contains("error sending request")
        || err_str.contains("connection")
        || err_str.contains("dns resolve failed"))
        && !err_str.contains("timeout")
        && !err_str.contains("timed out")
}

/// Render an error and its full source chain. Wrapper Display impls (notably
/// teloxide's `RequestError::Network`) hide the reqwest source detail, so
/// substring classification must walk the chain instead of calling
/// `to_string()` alone.
pub fn error_chain_string(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        out.push_str(": ");
        out.push_str(&e.to_string());
        source = e.source();
    }
    out
}

/// True when a send failure is explicitly known to be undelivered, so an
/// inline retry cannot duplicate a message. Only the connect/DNS subset of
/// the explicit list qualifies: Telegram may have processed a request that
/// failed after the request bytes were written (timeout, reset, closed,
/// interrupted response, 5xx), so those must not be retried inline.
pub fn is_send_retryable_error(err_str: &str) -> bool {
    err_str.contains("dns resolve failed")
        || err_str.contains("tcp connect error")
        || err_str.contains("client error (Connect)")
        || err_str.contains("connection refused")
}

/// Failures that prove the request was rejected before delivery: a Telegram
/// parameter-class 400/401/403/429 response, a DNS/connect failure, or a
/// refused connection. A 400 whose description is "internal Server Error"
/// intentionally does not match this list, so it falls through to the
/// ambiguous checks below.
fn is_explicitly_undelivered(err_str: &str) -> bool {
    err_str.contains("Too Many Requests")
        || err_str.contains("Retry after")
        || err_str.contains("HTTP 400")
        || err_str.contains("Bad Request")
        || err_str.contains("HTTP 401")
        || err_str.contains("HTTP 403")
        || err_str.contains("Unauthorized")
        || err_str.contains("Forbidden")
        || err_str.contains("dns resolve failed")
        || err_str.contains("tcp connect error")
        || err_str.contains("client error (Connect)")
        || err_str.contains("connection refused")
}

/// Classify a rich-message send failure by whether the message may already
/// have been delivered.
///
/// Telegram has no idempotency key, so "ambiguous" errors must not trigger a
/// full standard-path resend: the request may have reached Telegram and
/// completed delivery while only the response was lost. The caller marks the
/// entry sent and asks the owner to confirm instead.
///
/// - Explicitly undelivered (400/401/403/429, DNS, connect/refused):
///   returns false — the caller may safely fall back. These are checked
///   first, so a detailed error chain (richmsg attaches it via
///   `.context(...)`) wins over the generic transport prefix below.
/// - Ambiguous (timeout, connection reset/closed mid-request, interrupted
///   response body, 5xx / internal Server Error): returns true.
/// - Bare "error sending request" with no further detail: also ambiguous.
///   reqwest uses this prefix for every transport-phase failure; when no
///   source detail is available the delivery state is genuinely unknown, and
///   A1 prefers an uncertain-send notification over risking a duplicate.
pub fn is_ambiguous_send_error(err_str: &str) -> bool {
    if is_explicitly_undelivered(err_str) {
        return false;
    }
    err_str.contains("timed out")
        || err_str.contains("timeout")
        || err_str.contains("connection reset")
        || err_str.contains("connection closed")
        || err_str.contains("incomplete")
        || err_str.contains("error decoding response body")
        || err_str.contains("request or response body error")
        || err_str.contains("error sending request")
        || err_str.contains("HTTP 5")
        || err_str.contains("internal Server Error")
        || err_str.contains("Internal Server Error")
}

/// Single classification of a send failure, used by every send path.
///
/// `Flood` is checked first because a 429 can safely be retried after the
/// server-provided delay. The Telegram parameter-class rejections
/// (`FileId` / `Thumbnail` / `HtmlParse`) are checked before the transport
/// predicates so callers can apply their specific recovery (invalidate the
/// cached file_id, drop the thumbnail, fall back to plain text) instead of
/// treating them as generic failures. `Retryable` is the connect/DNS subset
/// that is definitely undelivered; `Ambiguous` may already have been
/// delivered and must not be retried inline; everything else is `Fatal`.
///
/// Callers must pass `error_chain_string(&err)` (or anyhow's `{:#}`) rather
/// than `Display::to_string()`: teloxide wrapper errors hide the reqwest
/// source detail, and the connect/ambiguous predicates only match that detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendFailureClass {
    Flood,
    FileId,
    Thumbnail,
    HtmlParse,
    Retryable,
    Ambiguous,
    Fatal,
}

/// Classify a send failure once, so the retry wrappers in push.rs only decide
/// which action to take for each class.
pub fn classify_send_error(err_str: &str) -> SendFailureClass {
    if flood_retry_secs(err_str).is_some() {
        return SendFailureClass::Flood;
    }
    let lower = err_str.to_lowercase();
    if lower.contains("file_id") || lower.contains("file identifier") {
        return SendFailureClass::FileId;
    }
    if lower.contains("thumbnail")
        || lower.contains("photo_invalid_dimensions")
        || lower.contains("wrong file dimensions")
    {
        return SendFailureClass::Thumbnail;
    }
    if lower.contains("parse entities") || lower.contains("can't parse") {
        return SendFailureClass::HtmlParse;
    }
    if is_send_retryable_error(err_str) {
        return SendFailureClass::Retryable;
    }
    if is_ambiguous_send_error(err_str) {
        return SendFailureClass::Ambiguous;
    }
    SendFailureClass::Fatal
}

/// Capability set for one send-retry path. `SendRetryState` only offers the
/// actions a path can actually perform: document sends may invalidate the
/// file_id cache, drop a thumbnail, and retry transport errors; rich ops
/// handle cache/thumbnail/flood but leave transport retries to the outer
/// rich-ops loop; text sends add the HTML-to-plain fallback.
#[derive(Debug, Clone, Copy)]
pub struct SendRetryPolicy {
    pub flood: bool,
    pub transient: bool,
    pub file_id: bool,
    pub thumbnail: bool,
    pub html_fallback: bool,
}

impl SendRetryPolicy {
    pub const DOCUMENT: Self = Self {
        flood: true,
        transient: true,
        file_id: true,
        thumbnail: true,
        html_fallback: false,
    };
    pub const RICH_OP: Self = Self {
        flood: true,
        transient: false,
        file_id: true,
        thumbnail: true,
        html_fallback: false,
    };
    pub const TRANSIENT_ONLY: Self = Self {
        flood: false,
        transient: true,
        file_id: false,
        thumbnail: false,
        html_fallback: false,
    };
    pub const FLOOD_ONLY: Self = Self {
        flood: true,
        transient: false,
        file_id: false,
        thumbnail: false,
        html_fallback: false,
    };
    pub const TEXT: Self = Self {
        flood: true,
        transient: false,
        file_id: false,
        thumbnail: false,
        html_fallback: true,
    };
    pub const POLL: Self = Self {
        flood: true,
        transient: true,
        file_id: false,
        thumbnail: false,
        html_fallback: false,
    };
}

/// The action a retry state wants the caller to take. The state owns the
/// counters and capability checks; callers only perform the side effect
/// (sleep, pace, invalidate a cache, or switch to plain text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendRetryAction {
    RetryFlood { delay: Duration },
    RetryTransient { delay: Duration },
    DropFileIdCache,
    DropThumbnail,
    FallbackPlainText,
    GiveUp,
}

/// Per-send retry state shared by every send path.
///
/// This centralizes the branch policy that used to be copied into each
/// wrapper: flood is capped at two retries, transient errors follow
/// `SEND_BACKOFF_SECS`, file_id/thumbnail recovery is available once, and
/// the HTML-to-plain fallback gets a fresh flood budget (the old nested
/// fallback loop kept its own counter).
pub struct SendRetryState {
    policy: SendRetryPolicy,
    flood_tries: u32,
    transient_tries: usize,
    file_id_available: bool,
    thumbnail_available: bool,
    html_fallback_available: bool,
}

impl SendRetryState {
    pub fn new(policy: SendRetryPolicy, file_id_cache: bool, thumbnail: bool) -> Self {
        Self {
            policy,
            flood_tries: 0,
            transient_tries: 0,
            file_id_available: policy.file_id && file_id_cache,
            thumbnail_available: policy.thumbnail && thumbnail,
            html_fallback_available: policy.html_fallback,
        }
    }

    /// Whether the next attempt should use the cached file_id.
    pub fn use_file_id(&self) -> bool {
        self.file_id_available
    }

    /// Whether the next attempt should attach the thumbnail.
    pub fn use_thumbnail(&self) -> bool {
        self.thumbnail_available
    }

    /// Classify one failure and return the next action. Counters and
    /// one-shot capabilities are updated here so callers cannot drift.
    pub fn next_action(&mut self, err_str: &str) -> SendRetryAction {
        match classify_send_error(err_str) {
            SendFailureClass::Flood if self.policy.flood && self.flood_tries < 2 => {
                let secs = flood_retry_secs(err_str).unwrap_or(5);
                self.flood_tries += 1;
                SendRetryAction::RetryFlood {
                    delay: Duration::from_secs(secs + 1),
                }
            }
            SendFailureClass::FileId if self.file_id_available => {
                self.file_id_available = false;
                SendRetryAction::DropFileIdCache
            }
            SendFailureClass::Thumbnail if self.thumbnail_available => {
                self.thumbnail_available = false;
                SendRetryAction::DropThumbnail
            }
            SendFailureClass::HtmlParse if self.html_fallback_available => {
                self.html_fallback_available = false;
                // The plain-text fallback previously had its own flood
                // counter; reset the budget so this path keeps the same
                // number of retries after the switch.
                self.flood_tries = 0;
                SendRetryAction::FallbackPlainText
            }
            SendFailureClass::Retryable
                if self.policy.transient && self.transient_tries < SEND_BACKOFF_SECS.len() =>
            {
                let delay = Duration::from_secs(SEND_BACKOFF_SECS[self.transient_tries]);
                self.transient_tries += 1;
                SendRetryAction::RetryTransient { delay }
            }
            _ => SendRetryAction::GiveUp,
        }
    }
}

/// Bounded exponential backoff sequence (seconds) for network errors
/// (proxy flaps / upstream outages) and transient TG errors. Shared by
/// media-op-level and text-chunk-level retries (#14 fix: merged the two
/// identical constants NET_BACKOFF_SECS and CHUNK_BACKOFF_SECS).
pub const SEND_BACKOFF_SECS: [u64; 3] = [2, 5, 10];

/// Per-chat send rate limiting: when multiple sources push to the same chat
/// in parallel, without pacing they all hit Telegram's per-chat rate limit
/// (~20 messages/minute) at once, triggering passive flood-control backoff.
/// Sends to the same chat are queued centrally here: two adjacent messages
/// are spaced at least MIN_INTERVAL apart.
struct ChatPacer {
    last: tokio::sync::Mutex<HashMap<i64, Instant>>,
}

impl ChatPacer {
    // 3s keeps the sustained per-chat rate at the ~20 messages/minute the
    // per-chat Telegram limit cited above allows; a tighter interval would
    // only converge through passive 429 backoff under sustained load.
    const MIN_INTERVAL: Duration = Duration::from_millis(3000);

    async fn wait(&self, chat_id: i64) {
        loop {
            let delay = {
                let mut m = self.last.lock().await;
                let now = Instant::now();
                match m.get(&chat_id).copied() {
                    Some(t) if t + Self::MIN_INTERVAL > now => {
                        Some(t + Self::MIN_INTERVAL - now)
                    }
                    _ => {
                        m.insert(chat_id, now);
                        None
                    }
                }
            };
            match delay {
                Some(d) => tokio::time::sleep(d).await,
                None => return,
            }
        }
    }
}

static PACER: std::sync::OnceLock<ChatPacer> = std::sync::OnceLock::new();

/// Queue before sending: guarantees adjacent messages to the same chat are
/// spaced at least 3 seconds apart.
pub async fn pace(chat_id: i64) {
    PACER
        .get_or_init(|| ChatPacer {
            last: tokio::sync::Mutex::new(HashMap::new()),
        })
        .wait(chat_id)
        .await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        classify_send_error, is_ambiguous_send_error, is_send_retryable_error, SendFailureClass,
        SendRetryAction, SendRetryPolicy, SendRetryState,
    };

    #[test]
    fn failure_classes_cover_each_recovery_branch() {
        use SendFailureClass::*;
        for (err, expected) in [
            ("Too Many Requests: retry after 30", Flood),
            ("Bad Request: wrong file identifier/HTTP URL specified", FileId),
            ("Bad Request: file_id is invalid", FileId),
            ("Bad Request: PHOTO_INVALID_DIMENSIONS", Thumbnail),
            ("Bad Request: THUMBNAIL_DIMENSIONS_INVALID", Thumbnail),
            ("Bad Request: wrong file dimensions", Thumbnail),
            (
                "Bad Request: can't parse entities: Unexpected end tag",
                HtmlParse,
            ),
            ("tcp connect error: Connection refused (os error 111)", Retryable),
            ("dns resolve failed: https://example.com", Retryable),
            (
                "rich message request failed: error sending request: tcp connect error: Connection refused (os error 111)",
                Retryable,
            ),
            ("operation timed out", Ambiguous),
            ("connection reset by peer", Ambiguous),
            ("HTTP 500 Internal Server Error", Ambiguous),
            ("Bad Request: message is too long (HTTP 400)", Fatal),
            ("Unauthorized (HTTP 401)", Fatal),
        ] {
            assert_eq!(classify_send_error(err), expected, "{err}");
        }
    }

    #[test]
    fn retry_state_caps_flood_and_transient_budgets() {
        let mut flood = SendRetryState::new(SendRetryPolicy::FLOOD_ONLY, false, false);
        for _ in 0..2 {
            assert_eq!(
                flood.next_action("Too Many Requests: retry after 5"),
                SendRetryAction::RetryFlood {
                    delay: Duration::from_secs(6)
                }
            );
        }
        assert_eq!(
            flood.next_action("Too Many Requests: retry after 5"),
            SendRetryAction::GiveUp
        );

        let mut transient = SendRetryState::new(SendRetryPolicy::TRANSIENT_ONLY, false, false);
        for expected in [2, 5, 10] {
            assert_eq!(
                transient.next_action("tcp connect error: Connection refused"),
                SendRetryAction::RetryTransient {
                    delay: Duration::from_secs(expected)
                }
            );
        }
        assert_eq!(
            transient.next_action("tcp connect error: Connection refused"),
            SendRetryAction::GiveUp
        );
    }

    #[test]
    fn retry_state_uses_cache_and_thumbnail_once() {
        let mut state = SendRetryState::new(SendRetryPolicy::DOCUMENT, true, true);
        assert!(state.use_file_id());
        assert_eq!(
            state.next_action("Bad Request: wrong file identifier"),
            SendRetryAction::DropFileIdCache
        );
        assert!(!state.use_file_id());
        assert_eq!(
            state.next_action("Bad Request: wrong file identifier"),
            SendRetryAction::GiveUp
        );

        assert!(state.use_thumbnail());
        assert_eq!(
            state.next_action("Bad Request: PHOTO_INVALID_DIMENSIONS"),
            SendRetryAction::DropThumbnail
        );
        assert!(!state.use_thumbnail());
        assert_eq!(
            state.next_action("Bad Request: PHOTO_INVALID_DIMENSIONS"),
            SendRetryAction::GiveUp
        );
    }

    #[test]
    fn retry_state_html_fallback_resets_the_flood_budget() {
        let mut state = SendRetryState::new(SendRetryPolicy::TEXT, false, false);
        for _ in 0..2 {
            assert!(matches!(
                state.next_action("Too Many Requests: retry after 1"),
                SendRetryAction::RetryFlood { .. }
            ));
        }
        assert_eq!(
            state.next_action("Bad Request: can't parse entities"),
            SendRetryAction::FallbackPlainText
        );
        // The plain-text fallback has its own two-retry flood budget.
        for _ in 0..2 {
            assert!(matches!(
                state.next_action("Too Many Requests: retry after 1"),
                SendRetryAction::RetryFlood { .. }
            ));
        }
        assert_eq!(
            state.next_action("Too Many Requests: retry after 1"),
            SendRetryAction::GiveUp
        );
        assert_eq!(
            state.next_action("Bad Request: can't parse entities"),
            SendRetryAction::GiveUp
        );
    }

    #[test]
    fn retry_state_only_offers_actions_enabled_by_its_policy() {
        // Rich ops handle cache/thumbnail/flood; transport retries belong to
        // the outer rich-ops loop.
        let mut rich = SendRetryState::new(SendRetryPolicy::RICH_OP, true, true);
        assert_eq!(
            rich.next_action("tcp connect error: Connection refused"),
            SendRetryAction::GiveUp
        );
        assert_eq!(
            rich.next_action("Bad Request: PHOTO_INVALID_DIMENSIONS"),
            SendRetryAction::DropThumbnail
        );

        let mut flood_only = SendRetryState::new(SendRetryPolicy::FLOOD_ONLY, true, true);
        assert_eq!(
            flood_only.next_action("Bad Request: wrong file identifier"),
            SendRetryAction::GiveUp
        );
        assert_eq!(
            flood_only.next_action("Bad Request: can't parse entities"),
            SendRetryAction::GiveUp
        );
    }

    #[test]
    fn only_connect_phase_failures_are_retried_inline() {
        for err in [
            "dns resolve failed: https://example.com",
            "tcp connect error: Connection refused (os error 111)",
            "client error (Connect)",
            "connection refused",
        ] {
            assert!(is_send_retryable_error(err), "{err}");
        }
        for err in [
            "operation timed out",
            "request timed out",
            "connection reset by peer",
            "connection closed before message completed",
            "error sending request",
            "HTTP 500 Internal Server Error",
            "400: internal Server Error during file upload",
            "request or response body error",
        ] {
            assert!(!is_send_retryable_error(err), "{err}");
        }
    }

    #[test]
    fn explicit_failures_are_not_ambiguous() {
        for err in [
            "Too Many Requests: retry after 30",
            "Retry after 12",
            "Bad Request: message is too long (HTTP 400)",
            "Unauthorized (HTTP 401)",
            "Forbidden: bot was blocked (HTTP 403)",
            "dns resolve failed: https://example.com",
            "tcp connect error: Connection refused (os error 111)",
            "client error (Connect)",
            "Bad Request: wrong file identifier/HTTP URL specified",
            "Bad Request: PHOTO_INVALID_DIMENSIONS",
            "Bad Request: can't parse entities: Unexpected end tag",
        ] {
            assert!(!is_ambiguous_send_error(err), "{err}");
        }
    }

    #[test]
    fn uncertain_failures_are_ambiguous() {
        for err in [
            "operation timed out",
            "request timed out",
            "connection reset by peer",
            "connection closed before message completed",
            "error decoding response body: incomplete message",
            "request or response body error",
            "HTTP 500 Internal Server Error",
            "400: internal Server Error during file upload",
        ] {
            assert!(is_ambiguous_send_error(err), "{err}");
        }
    }

    #[test]
    fn rich_send_transport_failures_are_ambiguous() {
        // richmsg wraps reqwest transport errors with .context(...); the
        // classification sees the whole {:#} chain. A bare transport prefix
        // with no source detail is genuinely uncertain, and a detailed chain
        // must reach the matching ambiguous entry.
        for err in [
            "rich message request failed: error sending request",
            "rich message upload failed: error sending request",
            "rich message upload failed: error sending request: connection closed before message completed",
            "rich message request failed: error sending request: operation timed out",
            "rich message response read failed: request or response body error",
        ] {
            assert!(is_ambiguous_send_error(err), "{err}");
        }
    }

    #[test]
    fn explicit_detail_wins_over_transport_prefix() {
        // The explicit-undelivered list is checked first, so a preserved
        // connect-phase source chain still classifies as safely undelivered
        // even though it carries the generic transport prefix.
        for err in [
            "rich message request failed: error sending request: tcp connect error: Connection refused (os error 111)",
            "rich message request failed: error sending request: dns resolve failed",
        ] {
            assert!(!is_ambiguous_send_error(err), "{err}");
        }
    }
}
