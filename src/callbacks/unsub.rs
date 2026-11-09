//! Unsubscribe flow: single unsub, unsub panel, unsub-all confirmation.
//! Split out of callbacks/mod.rs (batch-3 refactor). Uses shared panel
//! helpers from `crate::ui`; callback-only toast/guard helpers stay in
//! `super`.

use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};
use tracing::{info, warn};

use super::{fetch_sub_or_toast, toast_text};
use crate::ui::{
    chat_display_title, deliver, deliver_error_page, nav_row, paginate, PageTarget,
};
use crate::push::no_preview;
use crate::db;
use crate::i18n::{self, Lang};
use crate::state::AppState;

/// Handle single unsub callback: delete subscription, notify, then refresh the
/// panel in place (same page, clamped) so the removed button disappears.
// Page-threading params (page/ch_page/chlist_page) pushed the arity past the
// clippy default; bundling them into a struct was considered and rejected as
// more disruptive than the established allow pattern (start_pending_op).
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_unsub_callback(
    bot: &Bot,
    q: &CallbackQuery,
    state: &AppState,
    sub_id: i64,
    page: usize,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let qid = q.id.clone();
    let Some(sub) = fetch_sub_or_toast(state, bot, qid.clone(), sub_id, lang).await else {
        // fetch_sub_or_toast already answered the query with the failure or
        // "not found" toast; nothing else to do.
        return Ok(());
    };

    let name = sub.feed_title.as_deref().unwrap_or(&sub.feed_url).to_string();
    let feed_url = sub.feed_url.clone();
    let chat_id = sub.chat_id.clone();

    if let Err(e) = db::delete_subscription(&state.db, sub_id).await {
        bot.answer_callback_query(qid.clone()).text(toast_text(i18n::t(lang, "op_failed").replacen("{:#}", &format!("{:#}", e), 1))).await.ok();
        return Ok(());
    }

    // feed_configs overrides are dropped inside delete_subscription's
    // transaction, so a re-subscribe starts from channel defaults.

    info!(sub_id = sub_id, feed = %feed_url, "subscription removed");

    // Immediately send a notice (source name hyperlinked to the feed URL);
    // the /unsub list page stays open so the user can continue unsubscribing
    // (closed later via the Cancel button, silently).
    let chat = q.message.as_ref().map(|m| m.chat().id);
    bot.answer_callback_query(qid.clone()).await.ok();
    if let Some(chat) = chat {
        let href = crate::push::escape_attr(&feed_url);
        let name_html = crate::push::escape_html(&name);
        let msg = i18n::fill(i18n::t(lang, "unsub_ok"), &[&href, &name_html]);
        bot.send_message(chat, msg)
            .parse_mode(ParseMode::Html)
            .link_preview_options(no_preview())
            .await.ok();
    }

    // Refresh the panel in place so the deleted source's button disappears.
    render_unsub_panel(bot, PageTarget::Edit(q), &chat_id, state, page, ch_page, chlist_page, lang).await?;

    Ok(())
}

/// Render the /unsub panel (paginated, PAGE_SIZE items per page). Each source
/// is directly an "unsubscribe" button; paused sources get a ⏸ marker.
/// `chlist_page` is the channel-list page threaded through the round trip so
/// the "back to channel page" button restores the user's original list page.
#[allow(clippy::too_many_arguments)]
pub(super) async fn render_unsub_panel(
    bot: &Bot,
    target: PageTarget<'_>,
    chat_id: &str,
    state: &AppState,
    page: usize,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let subs = match db::get_subscriptions(&state.db, chat_id).await {
        Ok(s) => s,
        Err(e) => {
            deliver_error_page(bot, &target, lang, &format!("{:#}", e)).await?;
            return Ok(());
        }
    };

    if subs.is_empty() {
        // #15: the empty state provides a back-to-channel-page button; no dead page is left behind.
        let kb = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            i18n::t(lang, "unsub_back"),
            format!("c|ch|{}|{}|{}", chat_id, ch_page, chlist_page),
        )]]);
        deliver(bot, &target, i18n::t(lang, "unsub_panel_empty"), Some(kb)).await?;
        return Ok(());
    }

    let (total_pages, page, start, end) = paginate(subs.len(), page);

    let text = if total_pages > 1 {
        let page_s = (page + 1).to_string();
        let pages_s = total_pages.to_string();
        let count_s = subs.len().to_string();
        i18n::fill(
            i18n::t(lang, "unsub_panel_multi"),
            &[&page_s, &pages_s, &count_s],
        )
    } else {
        i18n::t(lang, "unsub_panel_single")
            .replacen("{}", &subs.len().to_string(), 1)
    };

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for (offset, sub) in subs[start..end].iter().enumerate() {
        let name = sub.feed_title.as_deref().unwrap_or(&sub.feed_url);
        let flag = if sub.active == 0 { " ⏸" } else { "" };
        buttons.push(vec![InlineKeyboardButton::callback(
            format!("{}. {}{}", start + offset + 1, crate::fetcher::truncate_text(name, 30), flag),
            format!("c|unsub|{}|{}|{}|{}", sub.id, page, ch_page, chlist_page),
        )]);
    }
    if let Some(nav) = nav_row(page, total_pages, lang, |p| format!("c|unsubpg|{}|{}|{}|{}", chat_id, p, ch_page, chlist_page)) {
        buttons.push(nav);
    }
    // The "unsubscribe all" entry point appears only in this panel (there is no /unsub_all command);
    // tapping it enters the confirmation panel.
    // #36: pass through the current page number so the confirmation page's "Back" button can restore the original page.
    buttons.push(vec![InlineKeyboardButton::callback(
        i18n::t(lang, "unsub_all_btn"),
        format!("c|unsuballask|{}|{}|{}|{}", chat_id, page, ch_page, chlist_page),
    )]);
    buttons.push(vec![InlineKeyboardButton::callback(
        i18n::t(lang, "unsub_back"),
        format!("c|ch|{}|{}|{}", chat_id, ch_page, chlist_page),
    )]);

    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

/// Render the "unsubscribe all" confirmation panel. Entry point is the button
/// on the /unsub panel (c|unsuballask); there is no /unsub_all command.
/// #36: the page parameter lets the "Back" button restore the unsub panel's original page number.
#[allow(clippy::too_many_arguments)]
pub(super) async fn render_unsub_all_confirm(
    bot: &Bot,
    target: PageTarget<'_>,
    chat_id: &str,
    state: &AppState,
    page: usize,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let subs = match db::get_subscriptions(&state.db, chat_id).await {
        Ok(s) => s,
        Err(e) => {
            deliver_error_page(bot, &target, lang, &format!("{}", e)).await?;
            return Ok(());
        }
    };
    if subs.is_empty() {
        let kb = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            i18n::t(lang, "unsub_back"),
            format!("c|ch|{}|{}|{}", chat_id, ch_page, chlist_page),
        )]]);
        deliver(bot, &target, i18n::t(lang, "unsub_panel_empty"), Some(kb)).await?;
        return Ok(());
    }

    let chat = db::get_chat(&state.db, chat_id).await.ok().flatten();
    let title = chat_display_title(chat.as_ref().and_then(|c| c.title.as_deref()), chat_id);

    let title_html = crate::push::format_chat_title_html(&title, chat_id);
    let chat_html = crate::push::format_chat_id_html(chat_id);
    let count_s = subs.len().to_string();
    let text = i18n::fill(
        i18n::t(lang, "unsuball_confirm"),
        &[&title_html, &chat_html, &count_s],
    );

    let kb = InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback(i18n::t(lang, "unsuball_confirm_btn"), format!("c|unsuball|{}|{}|{}|{}", chat_id, ch_page, page, chlist_page)),
        InlineKeyboardButton::callback(i18n::t(lang, "back"), format!("c|unsubpg|{}|{}|{}|{}", chat_id, page, ch_page, chlist_page)),
    ]]);

    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

/// In-memory per-message lock for the unsub-all confirmation. Unlike the old
/// message-text lock, it does not survive a process restart, so a click after
/// a crash can still complete the operation.
struct UnsubAllGuard<'a> {
    state: &'a AppState,
    key: (i64, i32),
}

impl Drop for UnsubAllGuard<'_> {
    fn drop(&mut self) {
        self.state
            .unsuball_inflight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.key);
    }
}

/// Handle unsub_all callback: delete every subscription, then send a text
/// notice (the former OPML file backup was removed by product decision).
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_unsub_all_callback(
    bot: &Bot,
    q: &CallbackQuery,
    state: &AppState,
    chat_id: &str,
    ch_page: usize,
    page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let qid = q.id.clone();

    let subs = match db::get_subscriptions(&state.db, chat_id).await {
        Ok(s) => s,
        Err(e) => {
            bot.answer_callback_query(qid.clone()).text(toast_text(i18n::t(lang, "op_failed").replacen("{:#}", &format!("{:#}", e), 1))).await.ok();
            return Ok(());
        }
    };

    if subs.is_empty() {
        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "unsub_no_subs")).await.ok();
        return Ok(());
    }

    // Idempotency protection: the in-memory (chat, message) guard rejects a
    // concurrent double-tap. The message edit below is only cosmetic feedback;
    // a failure (message too old, network error, or already "Processing…" from
    // a previous crashed run) must not abort the deletion.
    let key = q
        .message
        .as_ref()
        .map(|m| (m.chat().id.0, m.id().0))
        .unwrap_or((0, 0));
    let duplicate = {
        let mut inflight = state
            .unsuball_inflight
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        !inflight.insert(key)
    };
    if duplicate {
        bot.answer_callback_query(qid.clone())
            .text(i18n::t(lang, "unsuball_dup"))
            .await
            .ok();
        return Ok(());
    }
    let _inflight_guard = UnsubAllGuard { state, key };

    if let Some(m) = q.message.as_ref() {
        if let Err(e) = bot
            .edit_message_text(m.chat().id, m.id(), i18n::t(lang, "unsuball_processing"))
            .await
        {
            warn!(chat_id = %m.chat().id, error = %e, "unsuball: processing edit failed; continuing with deletion");
        }
    }

    // The notice and the final confirmation edit need the panel's chat. Check
    // it before deleting anything so a message-less callback can never remove
    // subscriptions with no way to report the result.
    let target_chat = match q.message.as_ref().map(|m| m.chat().id) {
        Some(t) => t,
        None => {
            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "unsuball_no_target")).await.ok();
            return Ok(());
        }
    };
    let chat = db::get_chat(&state.db, chat_id).await.ok().flatten();
    let title = chat_display_title(chat.as_ref().and_then(|c| c.title.as_deref()), chat_id);

    // Delete first; there is no file backup to gate the operation anymore.
    let count = match db::delete_all_subscriptions(&state.db, chat_id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                chat_id = chat_id,
                error = format!("{:#}", e),
                "failed to delete all subscriptions (transaction rolled back)"
            );
            bot.answer_callback_query(qid.clone()).text(toast_text(i18n::t(lang, "op_failed").replacen("{:#}", &format!("{:#}", e), 1))).await.ok();
            // Restore the confirmation page when possible. A failed restore
            // must still leave the user with actionable feedback instead of a
            // silent "Processing…" panel.
            if let Err(render_err) =
                render_unsub_all_confirm(bot, PageTarget::Edit(q), chat_id, state, page, ch_page, chlist_page, lang).await
            {
                warn!(chat_id = chat_id, error = %render_err, "unsuball: failed to restore confirmation page");
                bot.send_message(target_chat, i18n::t(lang, "op_failed_retry")).await.ok();
            }
            return Ok(());
        }
    };

    // feed_configs overrides are dropped inside delete_all_subscriptions'
    // transaction, so re-subscribes start from channel defaults.

    info!(chat_id = chat_id, count = count, "all subscriptions removed");

    bot.answer_callback_query(qid.clone()).await.ok();
    // Replace the old OPML file with a plain notice, matching the single
    // unsubscribe flow. A notice failure must not undo a completed deletion.
    let title_html = crate::push::format_chat_title_html(&title, chat_id);
    let chat_html = crate::push::format_chat_id_html(chat_id);
    let count_s = count.to_string();
    let notice = i18n::fill(
        i18n::t(lang, "unsuball_ok"),
        &[&title_html, &chat_html, &count_s],
    );
    if let Err(e) = bot
        .send_message(target_chat, &notice)
        .parse_mode(ParseMode::Html)
        .link_preview_options(no_preview())
        .await
    {
        warn!(chat_id = chat_id, error = %e, "failed to send unsubscribe-all notice (deletion already committed)");
    }

    // Retire the confirmation panel in place; keep a button back to the
    // channel page. Fall back to a fresh message when editing is impossible.
    if let Some(m) = q.message.as_ref() {
        let kb = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback(
                i18n::t(lang, "unsuball_view"),
                format!("c|ch|{}|{}|{}", chat_id, ch_page, chlist_page),
            ),
        ]]);
        if let Err(e) = bot
            .edit_message_text(m.chat().id, m.id(), i18n::t(lang, "closed"))
            .reply_markup(kb.clone())
            .await
        {
            if let Err(send_err) = bot
                .send_message(m.chat().id, i18n::t(lang, "closed"))
                .reply_markup(kb)
                .await
            {
                warn!(
                    chat_id = chat_id,
                    edit_error = %e,
                    send_error = %send_err,
                    "failed to retire the unsubscribe-all panel (both edit and fallback send failed)"
                );
            }
        }
    } else {
        bot.answer_callback_query(qid.clone())
            .text(i18n::t(lang, "unsuball_toast").replacen("{}", &count.to_string(), 1))
            .await?;
    }

    Ok(())
}
