//! Send-failure classification, retry policy and rate pacing.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

static RE_URL: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"https?://[^\s<>()]+").unwrap());

fn mask_urls(err_str: &str) -> Cow<'_, str> {
    if RE_URL.is_match(err_str) {
        Cow::Owned(RE_URL.replace_all(err_str, "<url>").into_owned())
    } else {
        Cow::Borrowed(err_str)
    }
}

static RE_RETRY_AFTER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)Retry after (\d+)").unwrap());

pub fn flood_retry_secs(err_str: &str) -> Option<u64> {
    let err_str = mask_urls(err_str);
    RE_RETRY_AFTER
        .captures(&err_str)
        .map(|caps| caps[1].parse().unwrap_or(5).min(300))
}

// Timeouts/resets/5xx are ambiguous: never retry inline, the message may already have been delivered.
pub fn is_net_error(err_str: &str) -> bool {
    let err_str = mask_urls(err_str);
    (err_str.contains("error sending request")
        || err_str.contains("connection")
        || err_str.contains("dns resolve failed"))
        && !err_str.contains("timeout")
        && !err_str.contains("timed out")
}

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

pub fn is_permanent_chat_error(err_str: &str) -> bool {
    let lower = mask_urls(err_str).to_lowercase();
    lower.contains("chat not found")
        || lower.contains("bot was kicked")
        || lower.contains("bot is not a member")
        || lower.contains("not enough rights")
        || lower.contains("chat_write_forbidden")
        || lower.contains("have no rights to send")
}

pub fn is_send_retryable_error(err_str: &str) -> bool {
    let err_str = mask_urls(err_str);
    err_str.contains("dns resolve failed")
        || err_str.contains("tcp connect error")
        || err_str.contains("client error (Connect)")
        || err_str.contains("connection refused")
}

fn is_explicitly_undelivered(err_str: &str) -> bool {
    let err_str = mask_urls(err_str);
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
        || err_str.contains("rich media upload body read failed")
}

pub fn is_ambiguous_send_error(err_str: &str) -> bool {
    if is_explicitly_undelivered(err_str) {
        return false;
    }
    let err_str = mask_urls(err_str);
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

pub fn classify_send_error(err_str: &str) -> SendFailureClass {
    if flood_retry_secs(err_str).is_some() {
        return SendFailureClass::Flood;
    }
    let lower = mask_urls(err_str).to_lowercase();
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendRetryAction {
    RetryFlood { delay: Duration },
    RetryTransient { delay: Duration },
    DropFileIdCache,
    DropThumbnail,
    FallbackPlainText,
    GiveUp,
}

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

    pub fn use_file_id(&self) -> bool {
        self.file_id_available
    }

    pub fn use_thumbnail(&self) -> bool {
        self.thumbnail_available
    }

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

pub const SEND_BACKOFF_SECS: [u64; 3] = [2, 5, 10];

struct ChatPacer {
    last: tokio::sync::Mutex<HashMap<i64, Instant>>,
}

impl ChatPacer {
    const MIN_INTERVAL: Duration = Duration::from_millis(3000);
    const MAX_TRACKED_CHATS: usize = 4096;
    const STALE_AFTER: Duration = Duration::from_secs(600);

    async fn wait(&self, chat_id: i64) {
        loop {
            let delay = {
                let mut m = self.last.lock().await;
                let now = Instant::now();
                if m.len() >= Self::MAX_TRACKED_CHATS && !m.contains_key(&chat_id) {
                    m.retain(|_, t| now.duration_since(*t) < Self::STALE_AFTER);
                    if m.len() >= Self::MAX_TRACKED_CHATS {
                        if let Some(oldest) = m.iter().min_by_key(|(_, t)| **t).map(|(k, _)| *k) {
                            m.remove(&oldest);
                        }
                    }
                }
                match m.get(&chat_id).copied() {
                    Some(t) if t + Self::MIN_INTERVAL > now => Some(t + Self::MIN_INTERVAL - now),
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
        classify_send_error, flood_retry_secs, is_ambiguous_send_error, is_explicitly_undelivered,
        is_net_error, is_send_retryable_error, SendFailureClass, SendRetryAction, SendRetryPolicy,
        SendRetryState,
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
            "rich media upload body read failed: request or response body error",
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
        for err in [
            "rich message request failed: error sending request: tcp connect error: Connection refused (os error 111)",
            "rich message request failed: error sending request: dns resolve failed",
        ] {
            assert!(!is_ambiguous_send_error(err), "{err}");
        }
    }

    #[test]
    fn url_contents_do_not_change_classification() {
        assert!(is_net_error(
            "error sending request for url (https://example.com/timeout)"
        ));
        assert!(!is_net_error(
            "request failed: https://example.com/connection"
        ));
        assert!(!is_explicitly_undelivered(
            "request failed: https://example.com/HTTP 400"
        ));
        assert!(!is_send_retryable_error(
            "request failed: https://example.com/dns resolve failed"
        ));
        assert_eq!(
            flood_retry_secs("request failed: https://example.com/Retry after 30"),
            None
        );
    }
}
