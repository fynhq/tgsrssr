use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, ChatId, ChatMemberStatus, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode,
};
use tracing::{debug, error, info, warn};

use crate::db;
use crate::i18n::{self, Lang};
use crate::langpref::resolve_lang;
use crate::state::AppState;
use crate::ui::{
    render_channel_list, render_channel_page, render_feed_filter, render_feed_page,
    render_language_picker, PageTarget,
};

use settings::{
    load_chat_config_checked, render_feed_interval, render_feed_mode, render_feed_source,
    render_settings_interval, render_settings_mode, render_settings_page, render_settings_source,
};

pub mod settings;
pub mod unsub;

mod data;
mod dispatch;

/// Telegram callback-query toasts are limited to 200 characters; a long
/// database error chain would otherwise make the toast itself fail and leave
/// the user with no feedback at all.
fn toast_text(text: String) -> String {
    const MAX: usize = 200;
    if crate::htmltext::utf16_len(&text) <= MAX {
        return text;
    }
    format!(
        "{}…",
        crate::htmltext::take_utf16_prefix(&text, MAX.saturating_sub(1))
    )
}

/// Ownership guard level for callback ops (single-source definition, shared by the guard and
/// the dispatch, eliminating drift from maintaining two copies).
/// - Chat: args[0] is the chat_id; verify ownership directly;
/// - Sub: args[0] is the sub_id; reverse-look-up the subscription to get chat_id, then verify;
/// - None: no target channel involved (list page/cancel/cookies panel/silent pagination); no guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardTarget {
    Chat,
    Sub,
    None,
}

/// Shared context for dispatch functions. Borrows the strings so the original
/// branch bodies keep their `parts` indexing unchanged.
pub(super) struct CallbackCtx<'a> {
    pub bot: &'a Bot,
    pub q: &'a CallbackQuery,
    pub state: &'a AppState,
    pub lang: Lang,
    pub qid: teloxide::types::CallbackQueryId,
    pub parts: &'a [&'a str],
    pub op: data::CallbackOp,
    /// Resolved ownership target (None when the subscription vanished).
    pub guard_chat_id: Option<&'a str>,
}
/// Handle incoming callback queries.
pub async fn handle_callback(bot: Bot, q: CallbackQuery, state: AppState) -> ResponseResult<()> {
    // Hoist the callback ID to the top of the function: subsequent branches use qid uniformly
    // (cloning when ownership is needed), avoiding q.id.clone() scattered everywhere.
    // CallbackQueryId is not Copy; clone it to keep q intact.
    let qid = q.id.clone();
    let data = match &q.data {
        Some(d) => d.clone(),
        None => {
            // #34: even empty data needs an answer, to stop the client's loading spinner.
            bot.answer_callback_query(qid).await.ok();
            return Ok(());
        }
    };

    let user_id = q.from.id.0 as i64;
    // Silently ignore non-whitelisted users.
    if !state.config.is_allowed(user_id) {
        bot.answer_callback_query(qid).await.ok();
        return Ok(());
    }
    // Commands/panels are private-chat only. Reject callbacks attached to a
    // group/channel message (forged or left over from before the restriction)
    // so an old panel cannot be used to manage a channel from a group.
    if !q
        .message
        .as_ref()
        .map(|m| m.chat().is_private())
        .unwrap_or(false)
    {
        bot.answer_callback_query(qid).await.ok();
        return Ok(());
    }

    let lang = resolve_lang(&state, user_id, q.from.language_code.as_deref()).await;

    let parts: Vec<&str> = data.split('|').collect();
    if parts.is_empty() || parts[0] != "c" {
        // #34: callbacks with a prefix other than this bot's also need an answer, to stop the client's loading spinner.
        bot.answer_callback_query(qid).await.ok();
        return Ok(());
    }

    // Ownership guard (pure personal isolation, no admin bypass): callback data comes from the
    // client, and chat_id/sub_id can be enumerated and forged, so ownership must be verified
    // before any management operation runs.
    // For chat-level routes args[0] is the chat_id; for sub-level routes args[0] is the sub_id,
    // requiring a subscription reverse-lookup to obtain the chat_id. chlist/cancelop/ck/page etc.
    // don't involve a target channel and are skipped.
    //
    // The op → guard mapping lives in `CallbackOp::guard_target`; unknown or
    // malformed callback data is rejected before this point.
    let cb = match data::CallbackData::parse(&data) {
        Some(cb) => cb,
        None => {
            tracing::warn!("malformed or unknown callback data rejected (fail-closed)");
            bot.answer_callback_query(qid).await.ok();
            return Ok(());
        }
    };
    let guard_chat_id: Option<String> = match cb.op.guard_target() {
        GuardTarget::Chat => cb.args.first().map(|s| s.to_string()),
        GuardTarget::Sub => {
            match cb.args.first().and_then(|s| s.parse::<i64>().ok()) {
                Some(sub_id) => {
                    match db::get_subscription_by_id(&state.db, sub_id).await {
                        Ok(Some(s)) => Some(s.chat_id),
                        // Subscription doesn't exist: let it through to each branch's "subscription
                        // not found" notice (no operation target, safe).
                        Ok(None) => None,
                        // DB query error: reject directly (fail-closed); the guard must not be skipped
                        // (the former .ok().flatten() treated transient DB errors as None, letting the guard be bypassed).
                        Err(e) => {
                            tracing::warn!(
                                user_id = user_id,
                                op = %parts.get(1).copied().unwrap_or("?"),
                                error = %e,
                                "ownership guard: subscription lookup failed, rejecting (fail-closed)"
                            );
                            bot.answer_callback_query(qid.clone())
                                .text(i18n::t(lang, "op_failed_retry"))
                                .await
                                .ok();
                            return Ok(());
                        }
                    }
                }
                None => None,
            }
        }
        GuardTarget::None => None,
    };
    if let Some(gcid) = &guard_chat_id {
        let allowed = match db::can_manage_chat(&state.db, user_id, gcid).await {
            Ok(v) => v,
            Err(e) => {
                // A DB failure is not a permission denial: keep the failure
                // closed, but leave the error chain in the log and tell the
                // user to retry instead of claiming they lack permission.
                tracing::warn!(
                    user_id = user_id,
                    op = %parts.get(1).copied().unwrap_or("?"),
                    target_chat = %gcid,
                    error = %e,
                    "ownership guard: permission lookup failed, rejecting (fail-closed)"
                );
                bot.answer_callback_query(qid.clone())
                    .text(i18n::t(lang, "op_failed_retry"))
                    .await
                    .ok();
                return Ok(());
            }
        };
        if !allowed {
            // callback data comes from the client and chat_id/sub_id can be enumerated and forged;
            // rejection events are a key signal for security auditing and must be logged.
            tracing::warn!(
                user_id = user_id,
                op = %parts.get(1).copied().unwrap_or("?"),
                target_chat = %gcid,
                "ownership guard rejected callback"
            );
            bot.answer_callback_query(qid.clone())
                .text(i18n::t(lang, "no_permission"))
                .await
                .ok();
            return Ok(());
        }
    }

    // Dispatch the op inside an async block so that a `?` in any branch
    // returns to this wrapper instead of exiting handle_callback directly.
    // Without the wrapper, a failing `render_*` (Flood / transient network
    // error) short-circuited past the fallback answer below, leaving the
    // client's loading spinner on forever with no feedback — contradicting
    // the "every path clears the spinner" guarantee.
    let ctx = CallbackCtx {
        bot: &bot,
        q: &q,
        state: &state,
        lang,
        qid: qid.clone(),
        parts: &parts,
        op: cb.op,
        guard_chat_id: guard_chat_id.as_deref(),
    };
    let res: ResponseResult<()> = async {
        match parts.get(1).copied() {
        Some(_) if dispatch::is_config_op(cb.op) => dispatch::config(&ctx).await?,
        Some(_) if dispatch::is_source_op(cb.op) => dispatch::source(&ctx).await?,
        Some("ch") => {
            // c|ch|<chat_id>[|<page>[|<chlist_page>]]
            if let Some(target) = parts.get(2) {
                let page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_channel_page(&bot, PageTarget::Edit(&q), target, &state, page, chlist_page, lang).await?;
            }
        }
        Some("chlist") => {
            // Back to channel list — edit in place. c|chlist[|<page>]
            let page = parts.get(2).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
            render_channel_list(&bot, PageTarget::Edit(&q), &state, user_id, page, lang).await?;
        }
        Some("lang") => {
            // c|lang opens the picker; c|lang|<code> selects a language.
            let Some(code) = parts.get(2).copied() else {
                render_language_picker(&bot, PageTarget::Edit(&q), lang).await?;
                return Ok(());
            };
            let Some(next) = Lang::from_code(code) else {
                bot.answer_callback_query(qid.clone())
                    .text(i18n::t(lang, "op_failed_retry"))
                    .await
                    .ok();
                return Ok(());
            };
            // Write strictly: only announce + re-render from `next` once it is
            // actually persisted, so the toast and the panel never disagree.
            let key = Lang::setting_key(user_id);
            if let Err(e) = db::set_setting(&state.db, &key, next.db_value()).await {
                tracing::warn!(user_id = user_id, error = %e, "language picker: preference write failed");
                bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                return Ok(());
            }
            let toast = i18n::t(next, "lang_switched");
            bot.answer_callback_query(q.id.clone()).text(toast).await.ok();
            render_channel_list(&bot, PageTarget::Edit(&q), &state, user_id, 0, next).await?;
        }
        Some("unsub") => {
            // c|unsub|<sub_id>[|<page>[|<ch_page>[|<chlist_page>]]]
            if let Some(sub_id_str) = parts.get(2) {
                if let Ok(sub_id) = sub_id_str.parse::<i64>() {
                    let page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                    let ch_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                    let chlist_page = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                    unsub::handle_unsub_callback(&bot, &q, &state, sub_id, page, ch_page, chlist_page, lang).await?;
                }
            }
        }
        Some("unsubpg") => {
            // c|unsubpg|<chat_id>|<page>|<ch_page>[|<chlist_page>] — paginate the unsub panel
            if let Some(cid) = parts.get(2) {
                let page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let ch_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                unsub::render_unsub_panel(&bot, PageTarget::Edit(&q), cid, &state, page, ch_page, chlist_page, lang).await?;
            }
        }
        Some("unsuballask") => {
            // c|unsuballask|<chat_id>[|<page>[|<ch_page>[|<chlist_page>]]]
            // #36: the page parameter is passed through so the "Back" button on the
            // confirmation page can restore the unsub panel's original page.
            if let Some(cid) = parts.get(2) {
                let page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let ch_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                unsub::render_unsub_all_confirm(&bot, PageTarget::Edit(&q), cid, &state, page, ch_page, chlist_page, lang).await?;
            }
        }
        Some("unsuball") => {
            // c|unsuball|<chat_id>|<ch_page>|<page>[|<chlist_page>]
            // page is used to restore the confirmation page on failure (the confirmation page needs the unsub panel's page number).
            if let Some(cid) = parts.get(2) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                unsub::handle_unsub_all_callback(&bot, &q, &state, cid, ch_page, page, chlist_page, lang).await?;
            }
        }
        Some("cancelop") => {
            // Abort any in-flight multi-step flow for this user (e.g. rename
            // awaiting reply) and close the page silently. Already-applied
            // changes stay applied.
            if let Some(m) = q.message.as_ref() {
                let key = (m.chat().id.0, q.from.id.0 as i64);
                // If a pending op was active, retire its orphaned prompt
                // so it doesn't linger as a stale instruction.
                let removed_op = state.pending_ops.lock().await.remove(&key);
                bot.answer_callback_query(qid.clone()).await.ok();
                if let Some(op) = removed_op {
                    crate::push::retire_prompt(&bot, m.chat().id, op.prompt_msg_id(), i18n::t(lang, "cancelled")).await;
                }
                // The panel is not deleted; it is edited in place to "Closed." and its buttons are
                // cleared (avoid deleting messages whenever possible).
                // Fall back to deletion if the edit fails (message too old, etc.).
                if bot
                    .edit_message_text(m.chat().id, m.id(), i18n::t(lang, "closed"))
                    .reply_markup(InlineKeyboardMarkup::new(
                        Vec::<Vec<InlineKeyboardButton>>::new(),
                    ))
                    .await
                    .is_err()
                {
                    bot.delete_message(m.chat().id, m.id()).await.ok();
                }
            } else {
                bot.answer_callback_query(qid.clone()).await.ok();
            }
        }
        Some("ck") => {
            // c|ck|<platform> — pick a platform, then reply with the new cookie.
            // Admin-only: cookies are global (admin configures once, all users share).
            // Non-admins should never see this button (the panel is only visible to admins);
            // defensive re-check: callback data comes from the client and can be forged, so silently ignore.
            if !state.config.is_admin(user_id) {
                bot.answer_callback_query(qid.clone()).await.ok();
            } else if let Some(pid) = parts.get(2) {
                if let Some(p) = crate::platforms::find_platform(pid) {
                    if !p.needs_cookie {
                        // Cookie-less platform: just acknowledge.
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "ck_no_need").replacen("{}", p.label, 1))
                            .await.ok();
                    } else {
                    let chat_obj = q.message.as_ref().map(|m| m.chat().clone());
                    // The cookie input flow is private-chat only: in a group chat the cookie
                    // plaintext in the user's reply would be visible, creating a leak window.
                    if !chat_obj.as_ref().map(|c| c.is_private()).unwrap_or(false) {
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "ck_private_only"))
                            .await.ok();
                    } else {
                    let chat = chat_obj.map(|c| c.id);
                    // The /cookies panel this button lives on; refreshed in
                    // place after a successful update.
                    let panel_msg_id = q.message.as_ref().map(|m| m.id().0).unwrap_or(0);
                    if let Some(chat) = chat {
                        bot.answer_callback_query(qid.clone()).await.ok();
                        let prompt_text = i18n::t(lang, "ck_prompt").replacen("{}", p.label, 1);
                        let platform_id = p.id.to_string();
                        start_pending_op(&bot, &state, chat, user_id, prompt_text, false, lang, |prompt_msg_id| {
                            crate::state::PendingOp::SetCookie {
                                platform: platform_id,
                                prompt_msg_id,
                                panel_msg_id,
                                created_at: std::time::Instant::now(),
                            }
                        }).await?;
                    }
                    }
                    }
                } else {
                    tracing::warn!(platform = %pid, "ck: unknown platform id in callback");
                    bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "ck_unknown")).await.ok();
                }
            }
        }
        _ => {
            bot.answer_callback_query(qid.clone()).await.ok();
        }
        }
        Ok(())
    }
    .await;

    // Fallback answer: every exit path now reaches here — both silent Ok
    // branches (unknown platform, subscription not found, etc.) and Err
    // results from `render_*`/`start_pending_op` short-circuits — so the
    // client's loading indicator is always cleared. TG only honors the first
    // answer; repeated calls here are harmless (errors are swallowed by ok()).
    bot.answer_callback_query(qid).await.ok();

    if let Err(e) = &res {
        tracing::warn!(user_id = user_id, op = %parts.get(1).copied().unwrap_or("?"), error = %e, "callback dispatch failed (spinner cleared)");
    }
    res
}

/// Sub-level callback guard: look up the subscription by id; notify via toast on failure.
/// Returns Some(sub) on success; on Ok(None)/Err a toast answer has already been sent and the caller just skips.
pub(super) async fn fetch_sub_or_toast(
    state: &AppState,
    bot: &Bot,
    qid: teloxide::types::CallbackQueryId,
    id: i64,
    lang: Lang,
) -> Option<db::SubRow> {
    match db::get_subscription_by_id(&state.db, id).await {
        Ok(Some(sub)) => Some(sub),
        Ok(None) => {
            // #37: the subscription has been deleted (stale button left by an old panel) → show a toast instead of staying silent.
            bot.answer_callback_query(qid)
                .text(i18n::t(lang, "sub_not_found"))
                .await
                .ok();
            None
        }
        Err(e) => {
            bot.answer_callback_query(qid)
                .text(toast_text(
                    i18n::t(lang, "op_failed").replace("{:#}", &format!("{:#}", e)),
                ))
                .await
                .ok();
            None
        }
    }
}

/// Send a prompt message and register a pending op; if a previous flow was
/// still pending for this (chat, user), retire its orphaned prompt.
/// `html`: whether the prompt text uses HTML parse mode.
/// `make_op`: builds the PendingOp variant from the prompt message id.
#[allow(clippy::too_many_arguments)]
async fn start_pending_op(
    bot: &Bot,
    state: &AppState,
    chat: ChatId,
    user_id: i64,
    prompt_text: String,
    html: bool,
    lang: Lang,
    make_op: impl FnOnce(i32) -> crate::state::PendingOp,
) -> ResponseResult<()> {
    let mut req = bot.send_message(chat, prompt_text);
    if html {
        req = req.parse_mode(ParseMode::Html);
    }
    // Send the prompt BEFORE taking the global pending_ops lock. The lock must
    // never be held across a network await: the Bot API client timeout is
    // 600s, so one stalled send would freeze message handling for every chat.
    // The "send + register" race is already covered by teloxide's same-chat
    // serialization: a reply in this private chat cannot be dispatched until
    // this callback handler returns. A reply therefore either arrives before
    // the prompt exists (treated as a normal message) or after the insert
    // (consumed correctly).
    let prompt = match req.await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(chat_id = chat.0, user_id, error = %e, "pending-op prompt send failed");
            bot.send_message(chat, i18n::t(lang, "op_failed_retry"))
                .await
                .ok();
            return Err(e);
        }
    };
    let mut ops = state.pending_ops.lock().await;
    let old = ops.insert((chat.0, user_id), make_op(prompt.id.0));
    drop(ops);
    if let Some(old) = old {
        crate::push::retire_prompt(
            bot,
            chat,
            old.prompt_msg_id(),
            i18n::t(lang, "new_op_started"),
        )
        .await;
    }
    Ok(())
}

/// Handle my_chat_member updates: auto-register or purge on removal.
pub async fn handle_my_chat_member(
    update: ChatMemberUpdated,
    state: AppState,
) -> ResponseResult<()> {
    let chat = update.chat.clone();
    let chat_id = chat.id.0.to_string();
    // Telegram also emits my_chat_member for private chats (user blocks /
    // unblocks the bot). Those must not be registered as manageable "channels"
    // (they would pollute the /set list and allow a nonsensical self-subscribe),
    // nor purged as if a channel. Only groups/supergroups/channels are in scope.
    if !(chat.is_channel() || chat.is_supergroup() || chat.is_group()) {
        return Ok(());
    }
    let chat_type =
        crate::db::chat_type_str(chat.is_channel(), chat.is_group(), chat.is_supergroup());
    // Record the inviter (the user who performed the add/remove action) as the channel owner.
    let inviter = update.from.id.0 as i64;
    let inviter_id = inviter.to_string();

    match update.new_chat_member.status() {
        ChatMemberStatus::Member | ChatMemberStatus::Administrator | ChatMemberStatus::Owner => {
            // Anyone can add a bot to a chat they administer. Registering the
            // chat for an unapproved inviter would both pollute chats and let
            // that stranger become the permanent owner (blocking the intended
            // owner from ever managing the channel). Ignore such adds.
            if !state.config.is_allowed(inviter) {
                warn!(
                    chat_id = %chat_id,
                    inviter = %inviter,
                    "ignoring bot add by a non-whitelisted user"
                );
                return Ok(());
            }
            let title = chat.title().map(|t| t.to_string());
            let username = chat.username().map(|u| u.to_string());
            if let Err(e) = db::upsert_chat(
                &state.db,
                &chat_id,
                chat_type,
                title.as_deref(),
                username.as_deref(),
                Some(&inviter_id),
            )
            .await
            {
                error!(chat_id = %chat_id, error = %e, "failed to upsert chat");
            } else {
                info!(chat_id = %chat_id, added_by = %inviter_id, "chat registered");
            }
        }
        ChatMemberStatus::Left | ChatMemberStatus::Banned => {
            // The bot was removed (or banned) by a chat administrator. That
            // actor is not necessarily the owner or a whitelisted user, so a
            // destructive purge here is an unauthenticated data-loss path
            // (and, after the row is gone, a re-add can steal ownership).
            // Soft-disable instead: keep subscriptions/history and the owner,
            // stop scheduling/outbox retries, and let a later re-add revive
            // the chat with its data intact.
            match db::set_chat_enabled(&state.db, &chat_id, false).await {
                Ok(true) => {
                    if let Err(e) = db::delete_outbox_for_chat(&state.db, &chat_id).await {
                        warn!(chat_id = %chat_id, error = %e, "bot removed: failed to clear outbox");
                    }
                    info!(
                        chat_id = %chat_id,
                        actor = %inviter_id,
                        "bot removed — chat soft-disabled, data retained"
                    );
                }
                Ok(false) => {
                    debug!(chat_id = %chat_id, actor = %inviter_id, "bot removed from an unregistered chat");
                }
                Err(e) => {
                    error!(chat_id = %chat_id, error = %e, "failed to soft-disable chat");
                }
            }
        }
        _ => {}
    }

    Ok(())
}
