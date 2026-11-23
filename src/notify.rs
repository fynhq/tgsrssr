use teloxide::prelude::*;
use teloxide::types::ChatId;
use tracing::{error, warn};

use crate::db;
use crate::i18n::{self, Lang};
use crate::push;
use crate::sendpolicy::{
    error_chain_string, pace, SendRetryAction, SendRetryPolicy, SendRetryState,
};
use crate::state::AppState;

async fn send_message_with_pace_flood(
    bot: &Bot,
    chat_id: i64,
    text: &str,
    html: bool,
    no_preview: &teloxide::types::LinkPreviewOptions,
) -> Option<String> {
    let mut retry = SendRetryState::new(SendRetryPolicy::FLOOD_ONLY, false, false);
    loop {
        pace(chat_id).await;
        let mut req = bot
            .send_message(ChatId(chat_id), text)
            .link_preview_options(no_preview.clone());
        if html {
            req = req.parse_mode(teloxide::types::ParseMode::Html);
        }
        match req.await {
            Ok(_) => return None,
            Err(e) => {
                let err_str = error_chain_string(&e);
                match retry.next_action(&err_str) {
                    SendRetryAction::RetryFlood { delay } => {
                        warn!(
                            chat_id = chat_id,
                            wait_secs = delay.as_secs(),
                            "notification flood control, backing off"
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    _ => return Some(err_str),
                }
            }
        }
    }
}

pub(crate) async fn notify_user_html(bot: &Bot, user_id: i64, html_msg: &str) -> bool {
    let html_msg = push::normalize_blockquote_fold(html_msg);
    const NOTIFY_MAX_VISIBLE: usize = push::CHUNK_LIMIT;
    let html_msg = if push::visible_len(&html_msg) > NOTIFY_MAX_VISIBLE {
        let (cut, _) = crate::htmltext::truncate_html_keep_links(
            &html_msg,
            NOTIFY_MAX_VISIBLE.saturating_sub(1),
        );
        format!("{cut}…")
    } else {
        html_msg
    };
    let no_preview = push::no_preview();
    let first_err = send_message_with_pace_flood(bot, user_id, &html_msg, true, &no_preview).await;
    let Some(first_err) = first_err else {
        return true;
    };
    if first_err.contains("Forbidden") || first_err.contains("403") {
        warn!(
            user = user_id,
            "NOTIFICATION UNDELIVERABLE: user has not started a private chat with the bot (or blocked it). \
             All alerts for this user are silently lost. Ask the user to send /start to the bot."
        );
        return false;
    }
    if crate::sendpolicy::is_ambiguous_send_error(&first_err) {
        warn!(
            user = user_id,
            error = %first_err,
            "notification outcome uncertain; skipping plain-text fallback to avoid duplication"
        );
        return true;
    }
    if first_err.contains("message is too long") {
        error!(
            user = user_id,
            "notification still too long after truncation; dropping fallback"
        );
        return false;
    }
    let plain = crate::htmltext::strip_html_keep_links(&html_msg, NOTIFY_MAX_VISIBLE);
    let fallback_err = send_message_with_pace_flood(bot, user_id, &plain, false, &no_preview).await;
    match fallback_err {
        None => true,
        Some(err_str) => {
            error!(user = user_id, error = %err_str, "failed to notify user (html fallback also failed, first error: {})", first_err);
            false
        }
    }
}

pub(crate) async fn notify_owner_html(bot: &Bot, state: &AppState, chat_id: &str, html_msg: &str) {
    match db::get_chat_owner(&state.db, chat_id).await {
        Ok(Some(owner)) => {
            if !state.config.is_allowed(owner) {
                warn!(
                    chat_id = chat_id,
                    owner, "cannot notify: owner is no longer whitelisted"
                );
                return;
            }
            notify_user_html(bot, owner, html_msg).await;
        }
        Ok(None) => warn!(chat_id = chat_id, "cannot notify: chat has no owner"),
        Err(e) => warn!(chat_id = chat_id, error = %e, "cannot notify: get_chat_owner failed"),
    }
}

pub(crate) async fn notify_admins_html(
    bot: &Bot,
    state: &AppState,
    build_msg: impl Fn(Lang) -> String,
) -> bool {
    let admin = state.config.admin_user_id;
    let lang = owner_lang(state, admin).await;
    notify_user_html(bot, admin, &build_msg(lang)).await
}

pub(crate) async fn owner_lang(state: &AppState, owner_id: i64) -> Lang {
    crate::langpref::resolve_lang(state, owner_id, None).await
}

pub(crate) fn push_line(msg: &mut String, lang: Lang, label_key: &str, value: &str) {
    if !value.is_empty() {
        let escaped = push::escape_html(value);
        msg.push_str(&i18n::fill(
            i18n::t(lang, "label_line"),
            &[i18n::t(lang, label_key), &escaped],
        ));
    }
}

pub(crate) fn push_fail_msg(
    lang: Lang,
    header: &str,
    feed_title: &str,
    reason: &str,
    link_label_key: &str,
    link_url: &str,
) -> String {
    let template = i18n::t(lang, "notif_push_failed");
    let title = push::escape_html(feed_title);
    let reason = push::escape_html(reason);
    let mut msg = i18n::fill(template, &[header, &title, &reason]);
    if !link_url.is_empty() {
        push_line(&mut msg, lang, link_label_key, link_url);
    }
    msg
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn notify_oversized_video(
    bot: &Bot,
    lang: Lang,
    owner_id: Option<i64>,
    chat_title: &str,
    entry_link: &str,
    delivered_tg_link: Option<&str>,
    delivered_as_text: bool,
    ov: &crate::media::OversizedVideo,
) {
    let Some(owner) = owner_id else { return };
    let size_part = ov
        .size
        .map(|s| {
            i18n::t(lang, "notif_video_size").replacen("{}", &(s / (1024 * 1024)).to_string(), 1)
        })
        .unwrap_or_default();
    let mut msg = if delivered_as_text {
        i18n::t(lang, "notif_video_oversized")
            .replacen("{}", &size_part, 1)
            .replacen("{}", &push::escape_html(chat_title), 1)
    } else {
        i18n::t(lang, "notif_video_oversized_no_text")
            .replacen("{}", &size_part, 1)
            .replacen("{}", &push::escape_html(chat_title), 1)
    };
    if let Some(tg) = delivered_tg_link {
        push_line(&mut msg, lang, "label_message", tg);
    }
    push_line(&mut msg, lang, "label_entry", entry_link);
    notify_user_html(bot, owner, &msg).await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn notify_oversized_videos(
    bot: &Bot,
    lang: Lang,
    owner_id: Option<i64>,
    chat_title: &str,
    entry_link: &str,
    delivered_tg_link: Option<&str>,
    delivered_as_text: bool,
    oversized: &[crate::media::OversizedVideo],
) {
    for ov in oversized {
        notify_oversized_video(
            bot,
            lang,
            owner_id,
            chat_title,
            entry_link,
            delivered_tg_link,
            delivered_as_text,
            ov,
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn notify_download_failed(
    bot: &Bot,
    lang: Lang,
    owner_id: Option<i64>,
    feed_title: &str,
    warnings: &[String],
    header_note: &str,
    delivered_tg_link: Option<&str>,
    entry_link: &str,
) {
    let Some(owner) = owner_id else { return };
    let mut msg = i18n::t(lang, "notif_dl_failed")
        .replacen("{}", header_note, 1)
        .replacen("{}", &push::escape_html(feed_title), 1);
    for w in warnings {
        msg.push('\n');
        msg.push_str(&push::escape_html(w));
    }
    if let Some(tg) = delivered_tg_link {
        push_line(&mut msg, lang, "label_message", tg);
    }
    push_line(&mut msg, lang, "label_source", entry_link);
    notify_user_html(bot, owner, &msg).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_line_uses_the_locale_separator() {
        let mut en = String::new();
        push_line(&mut en, Lang::En, "label_message", "https://t.me/example");
        assert_eq!(en, "\nMessage: https://t.me/example");

        let mut zh = String::new();
        push_line(&mut zh, Lang::Zh, "label_message", "https://t.me/example");
        assert_eq!(zh, "\n\u{6d88}\u{606f}\u{ff1a}https://t.me/example");
    }

    #[test]
    fn push_line_skips_empty_values_and_escapes_html() {
        let mut msg = String::new();
        push_line(&mut msg, Lang::En, "label_source", "");
        assert!(msg.is_empty());

        push_line(
            &mut msg,
            Lang::En,
            "label_source",
            "https://example.com/?a=1&b=2",
        );
        assert_eq!(msg, "\nSource: https://example.com/?a=1&amp;b=2");
    }
}
