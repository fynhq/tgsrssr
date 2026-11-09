//! Owner/admin notification subsystem.
//!
//! Extracted from scheduler.rs: everything that builds and delivers
//! out-of-band notices (push failures, auto-disable, platform health,
//! oversized video, download failures). Pure notification concern — no fetch
//! scheduling, no outbox logic.

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

/// Send one message with the shared per-chat pacer and flood-control
/// backoff. Returns None on success, or the first non-flood error string.
/// Both notification passes (HTML and plain-text fallback) share this helper
/// so pacing and flood handling stay identical across them.
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
                        warn!(chat_id = chat_id, wait_secs = delay.as_secs(), "notification flood control, backing off");
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    _ => return Some(err_str),
                }
            }
        }
    }
}

/// Send an HTML notification to a single user (falls back to plain text
/// when HTML parsing fails, so the notice is never lost).
pub(crate) async fn notify_user_html(bot: &Bot, user_id: i64, html_msg: &str) {
    // Unified blockquote folding rule (fold when >200 characters), consistent
    // with message pushing.
    let html_msg = push::normalize_blockquote_fold(html_msg);
    // Notifications are assembled from externally controlled values (feed
    // titles, error chains, download warnings). Keep them within Telegram's
    // limit; an over-long alert would fail both the HTML send and the plain
    // fallback, losing exactly the failure notice the owner needs.
    const NOTIFY_MAX_VISIBLE: usize = 4000;
    let html_msg = if push::visible_len(&html_msg) > NOTIFY_MAX_VISIBLE {
        let (cut, _) = crate::htmltext::truncate_html_keep_links(
            &html_msg,
            NOTIFY_MAX_VISIBLE.saturating_sub(1),
        );
        format!("{cut}…")
    } else {
        html_msg
    };
    // Notifications uniformly disable link previews: they often contain
    // source/message links, and preview cards are noise.
    let no_preview = push::no_preview();
    // Notifications share the per-chat pacer with pushes: a burst of owner
    // alerts (media failures / disabled sources) can otherwise hit Telegram's
    // flood control. Flood errors back off and retry the notification once.
    let first_err = send_message_with_pace_flood(bot, user_id, &html_msg, true, &no_preview).await;
    let Some(first_err) = first_err else { return };
    // 403 = the owner never /start'ed the bot (or blocked it): the plain-
    // text fallback would also inevitably 403, so log a prominent warn and
    // skip the fallback, saving one wasted API call.
    if first_err.contains("Forbidden") || first_err.contains("403") {
        warn!(
            user = user_id,
            "NOTIFICATION UNDELIVERABLE: user has not started a private chat with the bot (or blocked it). \
             All alerts for this user are silently lost. Ask the user to send /start to the bot."
        );
        return;
    }
    // Uncertain outcome (timeout / connection reset / lost response): the
    // HTML notice may already be delivered. Resending as plain text would
    // duplicate it, so apply the same conservative rule as the rich-message
    // A1 path: log and stop instead of falling back.
    if crate::sendpolicy::is_ambiguous_send_error(&first_err) {
        warn!(
            user = user_id,
            error = %first_err,
            "notification outcome uncertain; skipping plain-text fallback to avoid duplication"
        );
        return;
    }
    if first_err.contains("message is too long") {
        // The cap above should make this unreachable; do not send a second
        // equally over-long plain-text copy.
        error!(user = user_id, "notification still too long after truncation; dropping fallback");
        return;
    }
    // Downgrade to plain text: first restore <a href="URL">text</a> to
    // "URL text" (link information in notifications must not be lost),
    // then strip tags uniformly (reusing fetcher::strip_tags).
    let plain = html_msg
        .replace("<a href=\"", "")
        .replace("\">", " ")
        .replace("</a>", "");
    let plain = crate::fetcher::strip_tags(&plain);
    let fallback_err = send_message_with_pace_flood(bot, user_id, &plain, false, &no_preview).await;
    if let Some(err_str) = fallback_err {
        error!(user = user_id, error = %err_str, "failed to notify user (html fallback also failed, first error: {})", first_err);
    }
}

/// Resolve the owner of a chat and send an HTML notification to them.
/// If the chat has no owner (shouldn't happen post-migration), log and skip.
pub(crate) async fn notify_owner_html(bot: &Bot, state: &AppState, chat_id: &str, html_msg: &str) {
    match db::get_chat_owner(&state.db, chat_id).await {
        Ok(Some(owner)) => notify_user_html(bot, owner, html_msg).await,
        Ok(None) => warn!(chat_id = chat_id, "cannot notify: chat has no owner"),
        Err(e) => warn!(chat_id = chat_id, error = %e, "cannot notify: get_chat_owner failed"),
    }
}

/// Send an HTML notification to the admin (v1.11.0: after cookies were
/// globalized, platform-level alerts go to admins instead of channel owners;
/// v1.11.3: exactly one admin is configured, so this is a single recipient).
/// The copy is rendered in the admin's own language preference.
pub(crate) async fn notify_admins_html(
    bot: &Bot,
    state: &AppState,
    build_msg: impl Fn(Lang) -> String,
) {
    let admin = state.config.admin_user_id;
    let lang = owner_lang(state, admin).await;
    notify_user_html(bot, admin, &build_msg(lang)).await;
}

/// Resolve the owner's language preference from DB.
pub(crate) async fn owner_lang(state: &AppState, owner_id: i64) -> Lang {
    // Delegate to the single source of truth for language resolution
    // (callbacks::resolve_lang) — notifications have no Telegram client
    // language hint, so pass None (falls back to En like before).
    crate::callbacks::resolve_lang(state, owner_id, None).await
}

/// Append a line "\nlabel：value" (skipped when the value is empty). The
/// unified way of assembling notification copy.
pub(crate) fn push_line(msg: &mut String, lang: Lang, label_key: &str, value: &str) {
    if !value.is_empty() {
        // Values are mostly URLs (containing & query parameters); escape them
        // uniformly so they are not parsed as HTML entities.
        msg.push_str(&format!("\n{}：{}", i18n::t(lang, label_key), push::escape_html(value)));
    }
}

/// Unified construction of push-failure notifications (M5 fix: the original
/// 4 format! templates were verbatim duplicates).
/// header = the text inside the parentheses (e.g. "queued for retry" /
/// "abandoned" / "enqueue failed, entry lost"), reason = the details
/// inside the blockquote, link_label/link_url = optional source/entry link.
/// i18n::fill (single pass, values never rescanned) keeps a feed title that
/// contains literal `{}` from swallowing the reason placeholder.
pub(crate) fn push_fail_msg(lang: Lang, header: &str, feed_title: &str, reason: &str, link_label_key: &str, link_url: &str) -> String {
    let template = i18n::t(lang, "notif_push_failed");
    let title = push::escape_html(feed_title);
    let mut msg = i18n::fill(template, &[header, &title, reason]);
    if !link_url.is_empty() {
        push_line(&mut msg, lang, link_label_key, link_url);
    }
    msg
}

/// Oversized-video notification (build + send).
/// `delivered_as_text` is true only when the entry's main message is the
/// plain-text fallback. `delivered_tg_link` can be Some for a media message
/// too (the entry was sent with other media), so it must not be used to
/// decide the copy. When the link is None the entry was not pushed
/// (media-only mode / still queued).
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
    // The video's actual size is folded into the first line (e.g. "video
    // ~155 MB not pushed"); omitted when the size is unknown, leaving no
    // extra spaces. The video direct link is no longer listed separately.
    let size_part = ov
        .size
        .map(|s| i18n::t(lang, "notif_video_size").replacen("{}", &(s / (1024 * 1024)).to_string(), 1))
        .unwrap_or_default();
    let mut msg = if delivered_as_text {
        i18n::t(lang, "notif_video_oversized")
            .replacen("{}", &size_part, 1)
            .replacen("{}", &push::escape_html(chat_title), 1)
    } else {
        // Either not delivered yet, or delivered with other media; in both
        // cases "text message sent" would be wrong.
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

/// Media download failure notification (build + send).
/// `header_note` is an additional note (e.g. the media-only mode's "entry not
/// pushed, retrying next round"); an empty string means none.
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
