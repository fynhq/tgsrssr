//! Shared command-path fetch flow.
//!
//! `/sub` and `/test` both resolve a target chat, reject self-feeds, check the
//! platform cookie, take the command-path platform semaphore and fetch with a
//! no-conditional-request option set. Keeping the flow here removes the
//! duplicated validation blocks and gives `/sub` a single place to opt out of
//! Weibo detail/vote enrichment.

use crate::db;
use crate::fetcher::{self, Enrichment, FetchOptions, FetchResult};
use crate::i18n::{self, Lang};
use crate::platforms;
use crate::state::AppState;

/// User-facing failure classes shared by `/sub` and `/test`.
pub(super) enum CommandFetchError {
    /// Platform source requires an admin-configured cookie.
    PlatformClosed(&'static str),
    /// The target channel's own Telegram preview is the source.
    SelfSource,
    /// `is_self_telegram_source` DB lookup failed.
    CheckFailed(anyhow::Error),
    /// The command-path platform semaphore is closed.
    Busy,
    /// The feed/platform fetch failed.
    FetchFailed(anyhow::Error),
}

/// Resolve validation + fetch for one URL. The caller maps the error to its
/// own command's copy (`/sub` uses HTML blockquotes; `/test` uses plain text).
pub(super) async fn fetch_command_feed(
    state: &AppState,
    chat_id: &str,
    url: &str,
    lang: Lang,
    allow_trusted_private: bool,
    enrichment: Enrichment,
) -> Result<FetchResult, CommandFetchError> {
    validate_command_source(state, chat_id, url).await?;
    fetch_command_feed_unchecked(state, url, lang, allow_trusted_private, enrichment).await
}

/// Pre-fetch validation shared by `/sub` and `/test`: platform cookie and
/// self-feed checks. `/test` runs this before sending its progress message so
/// validation failures still arrive as a direct reply.
pub(super) async fn validate_command_source(
    state: &AppState,
    chat_id: &str,
    url: &str,
) -> Result<(), CommandFetchError> {
    if let Some(p) = platforms::platform_for_url(url) {
        if p.needs_cookie && state.get_cookie(p.id).await.is_none() {
            return Err(CommandFetchError::PlatformClosed(p.label));
        }
    }

    match is_self_telegram_source(state, chat_id, url).await {
        Ok(true) => Err(CommandFetchError::SelfSource),
        Ok(false) => Ok(()),
        Err(e) => Err(CommandFetchError::CheckFailed(e)),
    }
}

/// Semaphore-guarded fetch without the pre-checks above.
pub(super) async fn fetch_command_feed_unchecked(
    state: &AppState,
    url: &str,
    lang: Lang,
    allow_trusted_private: bool,
    enrichment: Enrichment,
) -> Result<FetchResult, CommandFetchError> {
    let is_platform = platforms::platform_for_url(url).is_some();
    // Platform fetches share a capacity-2 command semaphore; plain RSS fetches
    // are already bounded by the fetch semaphore inside fetch_feed.
    let _permit = if is_platform {
        Some(
            state
                .platform_command_semaphore
                .acquire()
                .await
                .map_err(|_| CommandFetchError::Busy)?,
        )
    } else {
        None
    };

    let opts = FetchOptions {
        etag: None,
        last_modified: None,
        known_sent: None,
        lang,
        allow_trusted_private,
        enrichment,
    };
    fetcher::fetch_feed(state, url, &opts)
        .await
        .map_err(CommandFetchError::FetchFailed)
}

/// `/sub` copy for a command fetch failure.
pub(super) fn sub_error_text(lang: Lang, err: CommandFetchError) -> String {
    match err {
        CommandFetchError::PlatformClosed(label) => {
            i18n::t(lang, "sub_platform_closed").replacen("{}", label, 1)
        }
        CommandFetchError::SelfSource => i18n::t(lang, "sub_self_channel").to_string(),
        CommandFetchError::CheckFailed(e) => i18n::t(lang, "sub_check_failed").replacen(
            "{}",
            &crate::htmltext::escape_html(&format!("{:#}", e)),
            1,
        ),
        CommandFetchError::Busy => i18n::t(lang, "sub_fetch_failed")
            .replacen("{}", i18n::t(lang, "platform_busy"), 1),
        CommandFetchError::FetchFailed(e) => i18n::t(lang, "sub_fetch_failed").replacen(
            "{}",
            &crate::htmltext::escape_html(&format!("{:#}", e)),
            1,
        ),
    }
}

/// `/test` copy for a command fetch failure.
pub(super) fn test_error_text(lang: Lang, err: CommandFetchError) -> String {
    match err {
        CommandFetchError::PlatformClosed(label) => {
            i18n::t(lang, "sub_platform_closed").replacen("{}", label, 1)
        }
        CommandFetchError::SelfSource => i18n::t(lang, "sub_self_channel").to_string(),
        CommandFetchError::CheckFailed(e) => i18n::t(lang, "sub_check_failed").replacen(
            "{}",
            &crate::htmltext::escape_html(&format!("{:#}", e)),
            1,
        ),
        CommandFetchError::Busy => i18n::t(lang, "platform_busy").to_string(),
        CommandFetchError::FetchFailed(e) => {
            i18n::t(lang, "test_fetch_failed").replace("{:#}", &format!("{:#}", e))
        }
    }
}

/// Reject subscribing a public Telegram channel to its own web preview. The
/// preview contains the bot's own posts, and every push creates a new msgid
/// that the next fetch sees as a new entry, forming an endless self-feed.
async fn is_self_telegram_source(
    state: &AppState,
    chat_id: &str,
    feed_url: &str,
) -> anyhow::Result<bool> {
    let Some(p) = platforms::platform_for_url(feed_url) else {
        return Ok(false);
    };
    if p.id != "telegram" {
        return Ok(false);
    }
    let Some(channel) = platforms::telegram::parse_channel_name(feed_url) else {
        return Ok(false);
    };
    let Some(chat) = db::get_chat(&state.db, chat_id).await? else {
        return Ok(false);
    };
    Ok(chat
        .username
        .as_deref()
        .is_some_and(|u| u.eq_ignore_ascii_case(&channel)))
}
