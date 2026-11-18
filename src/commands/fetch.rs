use crate::fetcher::{self, Enrichment, FetchOptions, FetchResult};
use crate::i18n::{self, Lang};
use crate::platforms;
use crate::state::AppState;

pub(super) enum CommandFetchError {
    PlatformClosed(&'static str),
    SelfSource,
    CheckFailed(anyhow::Error),
    Busy,
    FetchFailed(anyhow::Error),
}

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

pub(super) async fn validate_command_source(
    state: &AppState,
    chat_id: &str,
    url: &str,
) -> Result<(), CommandFetchError> {
    if let Some(p) = platforms::platform_for_url(url) {
        if p.needs_cookie && state.get_cookie(p.id).await.is_none() {
            return Err(CommandFetchError::PlatformClosed(p.label));
        }
        if let Some(check) = p.self_source {
            if check(state, chat_id, url)
                .await
                .map_err(CommandFetchError::CheckFailed)?
            {
                return Err(CommandFetchError::SelfSource);
            }
        }
    }
    Ok(())
}

pub(super) async fn fetch_command_feed_unchecked(
    state: &AppState,
    url: &str,
    lang: Lang,
    allow_trusted_private: bool,
    enrichment: Enrichment,
) -> Result<FetchResult, CommandFetchError> {
    let is_platform = platforms::platform_for_url(url).is_some();
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
        CommandFetchError::Busy => {
            i18n::t(lang, "sub_fetch_failed").replacen("{}", i18n::t(lang, "platform_busy"), 1)
        }
        CommandFetchError::FetchFailed(e) => i18n::t(lang, "sub_fetch_failed").replacen(
            "{}",
            &crate::htmltext::escape_html(&format!("{:#}", e)),
            1,
        ),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_error_text_platform_closed_and_self_source() {
        let closed = sub_error_text(Lang::En, CommandFetchError::PlatformClosed("Weibo"));
        assert!(closed.contains("Weibo"));
        assert!(closed.contains("unavailable"));

        assert_eq!(
            sub_error_text(Lang::Zh, CommandFetchError::SelfSource),
            i18n::t(Lang::Zh, "sub_self_channel")
        );
    }

    #[test]
    fn sub_error_text_escapes_fetch_and_check_errors() {
        let fetch = sub_error_text(
            Lang::En,
            CommandFetchError::FetchFailed(anyhow::anyhow!("bad <tag> & value")),
        );
        assert!(fetch.contains("<blockquote>"));
        assert!(fetch.contains("&lt;tag&gt; &amp; value"));

        let check = sub_error_text(
            Lang::En,
            CommandFetchError::CheckFailed(anyhow::anyhow!("db <error>")),
        );
        assert!(check.contains("&lt;error&gt;"));
    }

    #[test]
    fn sub_error_text_busy_wraps_platform_busy() {
        let text = sub_error_text(Lang::En, CommandFetchError::Busy);
        assert!(text.contains(i18n::t(Lang::En, "platform_busy")));
        assert!(text.contains("Fetch failed"));
    }

    #[test]
    fn test_error_text_is_plain_text() {
        let fetch = test_error_text(
            Lang::En,
            CommandFetchError::FetchFailed(anyhow::anyhow!("bad <tag> & value")),
        );
        assert!(fetch.contains("bad <tag> & value"));
        assert!(!fetch.contains("&lt;"));

        assert_eq!(
            test_error_text(Lang::En, CommandFetchError::Busy),
            i18n::t(Lang::En, "platform_busy")
        );
    }
}
