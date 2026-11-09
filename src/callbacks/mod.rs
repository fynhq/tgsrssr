use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, ChatId, ChatMemberStatus, InlineKeyboardButton,
    InlineKeyboardMarkup, ParseMode,
};
use tracing::{error, info};

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
/// - Chat: parts[2] is the chat_id; verify ownership directly;
/// - Sub: parts[2] is the sub_id; reverse-look-up the subscription to get chat_id, then verify;
/// - None: no target channel involved (list page/cancel/cookies panel/silent pagination); no guard.
enum GuardTarget {
    Chat,
    Sub,
    None,
}

/// The single mapping from op → guard level. Every op handled by the dispatch
/// below MUST be classified explicitly; an unknown op returns None and is
/// rejected (fail-closed). Previously unclassified ops defaulted to "no
/// target", so a new channel/source-management op could silently skip the
/// ownership guard.
fn guard_target(op: &str) -> Option<GuardTarget> {
    Some(match op {
        // chat level: parts[2] = chat_id
        "ch" | "cfg" | "cfgi" | "cfgm" | "cfgs" | "cfglp" | "cfgcp" | "resumeall"
        | "cfgiv" | "cfgmv" | "cfgsv" | "unsubpg" | "unsuballask" | "unsuball" => GuardTarget::Chat,
        // sub level: parts[2] = sub_id
        "src" | "srcpause" | "srcresume" | "srcrename" | "srcfip" | "srcfmp"
        | "srcfsp" | "srcfcp" | "srcflp" | "srcfilter" | "srcftoggle" | "srcfmode" | "srcfkw"
        | "srcfi" | "srcfm" | "srcfs" | "unsub" => GuardTarget::Sub,
        // explicitly no target channel involved
        "chlist" | "lang" | "cancelop" | "ck" | "page" => GuardTarget::None,
        // Unknown op: fail closed.
        _ => return None,
    })
}

/// Handle incoming callback queries.
pub async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    state: AppState,
) -> ResponseResult<()> {
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
    // For chat-level routes parts[2] is the chat_id; for sub-level routes parts[2] is the sub_id,
    // requiring a subscription reverse-lookup to obtain the chat_id. chlist/cancelop/ck/page etc.
    // don't involve a target channel and are skipped.
    //
    // Single-sourcing the guard tiers: the op → guard level is defined solely by guard_target()
    // (previously the two chat/sub arrays had to be manually kept in sync with the match branches
    // below; forgetting to add a guard for a new op would silently pass it through — fail-open).
    // When adding an op, just classify it into the right tier inside guard_target(); unclassified
    // ops are uniformly treated as having no target (no guard), so ops involving channel/source
    // management must be classified explicitly.
    let op = parts.get(1).copied().unwrap_or("");
    let guard_chat_id: Option<String> = match guard_target(op) {
        Some(GuardTarget::Chat) => parts.get(2).map(|s| s.to_string()),
        Some(GuardTarget::Sub) => {
            match parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
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
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                            return Ok(());
                        }
                    }
                }
                None => None,
            }
        }
        Some(GuardTarget::None) => None,
        None => {
            // Unknown op: fail closed instead of dispatching with no ownership
            // guard. The dispatch match would ignore it anyway; rejecting here
            // guarantees a future op cannot bypass the guard by being forgotten
            // in guard_target().
            tracing::warn!(user_id = user_id, op = %op, "unknown callback op rejected (fail-closed)");
            bot.answer_callback_query(qid).await.ok();
            return Ok(());
        }
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
                bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
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
            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "no_permission")).await.ok();
            return Ok(());
        }
    }

    // Dispatch the op inside an async block so that a `?` in any branch
    // returns to this wrapper instead of exiting handle_callback directly.
    // Without the wrapper, a failing `render_*` (Flood / transient network
    // error) short-circuited past the fallback answer below, leaving the
    // client's loading spinner on forever with no feedback — contradicting
    // the "every path clears the spinner" guarantee.
    let res: ResponseResult<()> = async {
        match parts.get(1).copied() {
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
        Some("cfg") => {
            // c|cfg|<chat_id>|<ch_page>|<chlist_page> — open settings page
            if let Some(cid) = parts.get(2) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_settings_page(&bot, PageTarget::Edit(&q), cid, &state, ch_page, chlist_page, lang).await?;
            }
        }
        Some("cfgi") => {
            if let Some(cid) = parts.get(2) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_settings_interval(&bot, PageTarget::Edit(&q), cid, &state, ch_page, chlist_page, lang).await?;
            }
        }
        Some("cfgm") => {
            if let Some(cid) = parts.get(2) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_settings_mode(&bot, PageTarget::Edit(&q), cid, &state, ch_page, chlist_page, lang).await?;
            }
        }
        Some("cfgs") => {
            if let Some(cid) = parts.get(2) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_settings_source(&bot, PageTarget::Edit(&q), cid, &state, ch_page, chlist_page, lang).await?;
            }
        }
        Some("cfglp") => {
            // Toggle link preview (chat-level only)
            if let Some(cid) = parts.get(2) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                // Strict baseline read: a transient DB error aborts the toggle
                // with a toast instead of flipping from a wrong default and
                // persisting a contradictory override.
                let cfg = match load_chat_config_checked(&state.db, cid).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(chat = %cid, error = %e, "cfglp: config read failed, toggle aborted");
                        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                        return Ok(());
                    }
                };
                let next = if cfg.link_preview != 0 { "0" } else { "1" };
                match db::set_chat_config_field(&state.db, cid, "link_preview", next).await {
                    Ok(_) => {
                        render_settings_page(&bot, PageTarget::Edit(&q), cid, &state, ch_page, chlist_page, lang).await?;
                    }
                    Err(e) => {
                        bot.answer_callback_query(qid.clone()).text(toast_text(i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e)))).await.ok();
                    }
                }
            }
        }
        Some("cfgcp") => {
            // Toggle caption position (chat-level): Telegram's
            // show_caption_above_media only applies to photo/video/animation.
            if let Some(cid) = parts.get(2) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let cfg = match load_chat_config_checked(&state.db, cid).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(chat = %cid, error = %e, "cfgcp: config read failed, toggle aborted");
                        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                        return Ok(());
                    }
                };
                let next = if cfg.caption_above_media != 0 { "0" } else { "1" };
                match db::set_chat_config_field(&state.db, cid, "caption_above_media", next).await {
                    Ok(_) => {
                        render_settings_page(&bot, PageTarget::Edit(&q), cid, &state, ch_page, chlist_page, lang).await?;
                    }
                    Err(e) => {
                        bot.answer_callback_query(qid.clone()).text(toast_text(i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e)))).await.ok();
                    }
                }
            }
        }
        Some("resumeall") => {
            // c|resumeall|<chat_id>|<chlist_page>[|<ch_page>] — re-enable all paused sources under this channel in one click
            if let Some(cid) = parts.get(2) {
                let chlist_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let ch_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                match db::resume_all_subscriptions(&state.db, cid).await {
                    Ok(n) => {
                        info!(chat_id = %cid, count = n, "resume-all: subscriptions re-enabled");
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "resumeall_ok").replacen("{}", &n.to_string(), 1))
                            .await
                            .ok();
                        // Keep the source-list page (ch_page) instead of resetting to 0, so the
                        // "enable all" action does not jump the user back to the first page.
                        render_channel_page(&bot, PageTarget::Edit(&q), cid, &state, ch_page, chlist_page, lang).await?;
                    }
                    Err(e) => {
                        bot.answer_callback_query(qid.clone()).text(toast_text(i18n::t(lang, "op_failed").replace("{:#}", &format!("{:#}", e)))).await.ok();
                    }
                }
            }
        }
        Some("cfgiv") | Some("cfgmv") | Some("cfgsv") => {
            // c|cfgiv|<chat_id>|<value>|<ch_page>|<chlist_page>
            if let (Some(cid), Some(val)) = (parts.get(2), parts.get(3)) {
                let ch_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let field = match parts[1] {
                    "cfgiv" => "interval_minutes",
                    "cfgmv" => "content_mode",
                    _ => "source_display",
                };
                // Range guard for the fetch interval: a forged/edge callback
                // with 0 or a negative value would make the due check
                // (now-last_fetch)*1440 >= interval always true, so the channel
                // is fetched on every 5-minute tick — easily tripping source
                // rate limits / IP bans. Clamp to a sane minimum (1 minute).
                if field == "interval_minutes" {
                    let ok = val.parse::<i64>().map(|n| n >= 1).unwrap_or(false);
                    if !ok {
                        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "invalid_interval")).await.ok();
                        return Ok(());
                    }
                }
                match db::set_chat_config_field(&state.db, cid, field, val).await {
                    Ok(_) => {
                        info!(chat_id = %cid, field = field, value = %val, "chat config updated");
                        render_settings_page(&bot, PageTarget::Edit(&q), cid, &state, ch_page, chlist_page, lang).await?;
                    }
                    Err(e) => {
                        bot.answer_callback_query(qid.clone()).text(toast_text(i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e)))).await.ok();
                    }
                }
            }
        }
        Some("src") => {
            // c|src|<sub_id>[|<ch_page>[|<chlist_page>]] — feed (source) page.
            // ch_page is the source-list page on the channel page the user came
            // from; chlist_page is the channel-list page. Both are passed back
            // through the "back to channel" button so a full round trip keeps
            // the user's position.
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_feed_page(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
            }
        }
        Some("srcpause") | Some("srcresume") => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let active = parts[1] == "srcresume";
                // Fail closed when the guard could not resolve an owner (the
                // subscription did not exist at guard time). The update also
                // carries chat_id, so an id recreated between the guard SELECT
                // and this UPDATE cannot be toggled.
                let Some(expected_chat) = guard_chat_id.as_deref() else {
                    bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "sub_not_found")).await.ok();
                    return Ok(());
                };
                match db::set_subscription_active(&state.db, id, expected_chat, active).await {
                    Ok(true) => {
                        info!(sub_id = id, active = active, "subscription active toggled");
                        render_feed_page(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
                    }
                    Ok(false) => {
                        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "sub_not_found")).await.ok();
                    }
                    Err(e) => {
                        bot.answer_callback_query(qid.clone()).text(toast_text(i18n::t(lang, "op_failed").replace("{:#}", &format!("{:#}", e)))).await.ok();
                    }
                }
            }
        }
        Some("srcrename") => {
            // Ask for a new name via plain text prompt; the reply is handled in
            // commands. Both page positions are stored so the post-rename
            // refresh keeps the channel-list page too.
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(&state, &bot, qid.clone(), id, lang).await {
                    let chat = q.message.as_ref().map(|m| m.chat().id);
                    // The feed page this button lives on; refreshed in place
                    // after a successful rename.
                    let panel_msg_id = q.message.as_ref().map(|m| m.id().0).unwrap_or(0);
                    if let Some(chat) = chat {
                        let user_id = q.from.id.0 as i64;
                        bot.answer_callback_query(qid.clone()).await.ok();
                        let prompt_text = i18n::t(lang, "rename_prompt")
                            .replacen("{}", sub.feed_title.as_deref().unwrap_or(&sub.feed_url), 1);
                        start_pending_op(&bot, &state, chat, user_id, prompt_text, false, lang, |prompt_msg_id| {
                            crate::state::PendingOp::RenameSub {
                                sub_id: id,
                                prompt_msg_id,
                                panel_msg_id,
                                ch_page,
                                chlist_page,
                                created_at: std::time::Instant::now(),
                            }
                        }).await?;
                    }
                }
            }
        }
        // Second-level feed pages.
        Some("srcfip") => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_feed_interval(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
            }
        }
        Some("srcfmp") => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_feed_mode(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
            }
        }
        Some("srcfsp") => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_feed_source(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
            }
        }
        Some("srcfcp") => {
            // Toggle feed-level caption position directly (mirrors channel cfgcp).
            // Approach A: when the toggled value equals the channel default, clear the override (marker goes back to 🔗).
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(&state, &bot, qid.clone(), id, lang).await {
                    let (cfg, chat_cfg) = match (
                        db::get_effective_config(&state.db, &sub.chat_id, &sub.feed_url).await,
                        load_chat_config_checked(&state.db, &sub.chat_id).await,
                    ) {
                        (Ok(cfg), Ok(chat_cfg)) => (cfg, chat_cfg),
                        (Err(e), _) | (_, Err(e)) => {
                            tracing::warn!(sub_id = id, error = %e, "srcfcp: config read failed, toggle aborted");
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                            return Ok(());
                        }
                    };
                    let next = if cfg.caption_above_media { "0" } else { "1" };
                    let channel_default = if chat_cfg.caption_above_media != 0 { "1" } else { "0" };
                    let value: Option<&str> = if next == channel_default { None } else { Some(next) };
                    match db::set_feed_config_field(&state.db, &sub.chat_id, &sub.feed_url, "caption_above_media", value).await {
                        Ok(_) => {
                            info!(sub_id = id, caption_above_media = next, "feed caption position toggled");
                            render_feed_page(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
                        }
                        Err(e) => {
                            bot.answer_callback_query(qid.clone()).text(toast_text(i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e)))).await.ok();
                        }
                    }
                }
            }
        }
        Some("srcflp") => {
            // Toggle feed-level link preview directly (mirrors channel cfglp).
            // Approach A: when the toggled value equals the channel default, clear the override (marker goes back to 🔗).
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(&state, &bot, qid.clone(), id, lang).await {
                    // Read the current baseline strictly: a transient DB error
                    // must abort (with a toast, spinner cleared) rather than
                    // toggle from a wrong default and persist an override that
                    // contradicts the user's actual setting. A missing override
                    // row still yields the effective-config default normally.
                    let (cfg, chat_cfg) = match (
                        db::get_effective_config(&state.db, &sub.chat_id, &sub.feed_url).await,
                        load_chat_config_checked(&state.db, &sub.chat_id).await,
                    ) {
                        (Ok(cfg), Ok(chat_cfg)) => (cfg, chat_cfg),
                        (Err(e), _) | (_, Err(e)) => {
                            tracing::warn!(sub_id = id, error = %e, "srcflp: config read failed, toggle aborted");
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                            return Ok(());
                        }
                    };
                    let next = if cfg.link_preview { "0" } else { "1" };
                    let channel_default = if chat_cfg.link_preview != 0 { "1" } else { "0" };
                    let value: Option<&str> = if next == channel_default { None } else { Some(next) };
                    match db::set_feed_config_field(&state.db, &sub.chat_id, &sub.feed_url, "link_preview", value).await {
                        Ok(_) => {
                            info!(sub_id = id, link_preview = next, "feed link preview toggled");
                            render_feed_page(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
                        }
                        Err(e) => {
                            bot.answer_callback_query(qid.clone()).text(toast_text(i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e)))).await.ok();
                        }
                    }
                }
            }
        }
        Some("srcfilter") => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_feed_filter(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
            }
        }
        Some("srcftoggle") => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(&state, &bot, qid.clone(), id, lang).await {
                    // Strict baseline read (same contract as cfglp/srcflp): a
                    // transient DB error must abort the toggle instead of
                    // flipping from the FilterConfig::default() and persisting
                    // a state that contradicts what the user actually set.
                    let fc = match db::get_feed_filter(&state.db, &sub.chat_id, &sub.feed_url).await {
                        Ok(fc) => fc,
                        Err(e) => {
                            tracing::warn!(sub_id = id, error = %e, "srcftoggle: filter read failed, toggle aborted");
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                            return Ok(());
                        }
                    };
                    match db::set_feed_filter_enabled(&state.db, &sub.chat_id, &sub.feed_url, !fc.enabled).await {
                        Ok(_) => {
                            render_feed_filter(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
                        }
                        Err(e) => {
                            bot.answer_callback_query(qid.clone()).text(toast_text(i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e)))).await.ok();
                        }
                    }
                }
            }
        }
        Some("srcfmode") => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(&state, &bot, qid.clone(), id, lang).await {
                    // Strict baseline read (see srcftoggle): abort on a DB
                    // error rather than persisting a mode flipped from a
                    // wrong default.
                    let fc = match db::get_feed_filter(&state.db, &sub.chat_id, &sub.feed_url).await {
                        Ok(fc) => fc,
                        Err(e) => {
                            tracing::warn!(sub_id = id, error = %e, "srcfmode: filter read failed, toggle aborted");
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                            return Ok(());
                        }
                    };
                    let next = if fc.mode == "whitelist" { "blacklist" } else { "whitelist" };
                    match db::set_feed_filter_mode(&state.db, &sub.chat_id, &sub.feed_url, next).await {
                        Ok(_) => {
                            render_feed_filter(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
                        }
                        Err(e) => {
                            bot.answer_callback_query(qid.clone()).text(toast_text(i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e)))).await.ok();
                        }
                    }
                }
            }
        }
        Some("srcfkw") => {
            // Open keyword-input flow (plain text prompt; reply handled in commands)
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(&state, &bot, qid.clone(), id, lang).await {
                    let chat = q.message.as_ref().map(|m| m.chat().id);
                    let panel_msg_id = q.message.as_ref().map(|m| m.id().0).unwrap_or(0);
                    if let Some(chat) = chat {
                        let user_id = q.from.id.0 as i64;
                        // A failed read must abort the flow: showing the
                        // default "(none)" baseline would make the user's
                        // reply overwrite the real keywords.
                        let fc = match db::get_feed_filter(&state.db, &sub.chat_id, &sub.feed_url).await {
                            Ok(fc) => fc,
                            Err(e) => {
                                tracing::warn!(sub_id = id, error = %e, "feed filter read failed, aborting keyword flow");
                                bot.answer_callback_query(qid.clone())
                                    .text(i18n::t(lang, "op_failed_retry"))
                                    .await
                                    .ok();
                                return Ok(());
                            }
                        };
                        // Acknowledge only after the read succeeded, so the
                        // failure branch's toast is the first answer Telegram
                        // sees (subsequent answers to the same query are
                        // ignored).
                        bot.answer_callback_query(qid.clone()).await.ok();
                        let current = if fc.keywords.is_empty() {
                            i18n::t(lang, "none").to_string()
                        } else {
                            format!("<code>{}</code>", crate::push::escape_html(&fc.keywords))
                        };
                        let prompt_text = i18n::t(lang, "fkw_prompt").replacen("{}", &current, 1);
                        start_pending_op(&bot, &state, chat, user_id, prompt_text, true, lang, |prompt_msg_id| {
                            crate::state::PendingOp::SetFilterKeywords {
                                sub_id: id,
                                prompt_msg_id,
                                panel_msg_id,
                                ch_page,
                                chlist_page,
                                created_at: std::time::Instant::now(),
                            }
                        }).await?;
                    }
                }
            }
        }
        Some("srcfi") | Some("srcfm") | Some("srcfs") => {
            // Feed-level overrides: c|srcfi|<sub_id>|<value>[|<ch_page>] etc.
            // A value of "inherit" clears the override (NULL → fall back to channel).
            // Approach A: when the selected value equals the channel default, the override is
            // cleared automatically (marker goes back to 🔗); an override record (marker ✏️) is
            // only written when the value truly differs from the channel.
            if let (Some(id), Some(val)) = (
                parts.get(2).and_then(|s| s.parse::<i64>().ok()),
                parts.get(3),
            ) {
                let ch_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                // Range guard mirroring the chat-level cfgiv path: a forged or
                // edge callback with a 0/negative interval makes the due check
                // always true, hammering the source every tick (rate-limit /
                // IP-ban risk). "inherit" clears the override and is allowed.
                if parts[1] == "srcfi" && *val != "inherit" {
                    let ok = val.parse::<i64>().map(|n| n >= 1).unwrap_or(false);
                    if !ok {
                        bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "invalid_interval")).await.ok();
                        return Ok(());
                    }
                }
                if let Some(sub) = fetch_sub_or_toast(&state, &bot, qid.clone(), id, lang).await {
                    let field = match parts[1] {
                        "srcfi" => "interval_minutes",
                        "srcfm" => "content_mode",
                        _ => "source_display",
                    };
                    // Compare with the channel default: clear the override when equal, only write an override when different.
                    // Strict baseline read (same contract as cfglp/srcflp): a
                    // transient DB error aborts with a toast instead of
                    // persisting an override computed from a wrong default.
                    let chat_cfg = match load_chat_config_checked(&state.db, &sub.chat_id).await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(sub_id = id, error = %e, "srcf*: config read failed, update aborted");
                            bot.answer_callback_query(qid.clone()).text(i18n::t(lang, "op_failed_retry")).await.ok();
                            return Ok(());
                        }
                    };
                    let channel_default = match field {
                        "interval_minutes" => chat_cfg.interval_minutes.to_string(),
                        "content_mode" => chat_cfg.content_mode.clone(),
                        _ => chat_cfg.source_display.clone(),
                    };
                    let value: Option<&str> = if *val == "inherit" || *val == channel_default {
                        None
                    } else {
                        Some(*val)
                    };
                    match db::set_feed_config_field(&state.db, &sub.chat_id, &sub.feed_url, field, value).await {
                        Ok(_) => {
                            info!(sub_id = id, field = field, value = %val, "feed config updated");
                            // Return to the feed (source) page after a selection.
                            render_feed_page(&bot, PageTarget::Edit(&q), id, &state, ch_page, chlist_page, lang).await?;
                        }
                        Err(e) => {
                            bot.answer_callback_query(qid.clone()).text(toast_text(i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e)))).await.ok();
                        }
                    }
                }
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
        tracing::warn!(user_id = user_id, op = %op, error = %e, "callback dispatch failed (spinner cleared)");
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
            bot.answer_callback_query(qid).text(i18n::t(lang, "sub_not_found")).await.ok();
            None
        }
        Err(e) => {
            bot.answer_callback_query(qid).text(toast_text(i18n::t(lang, "op_failed").replace("{:#}", &format!("{:#}", e)))).await.ok();
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
            bot.send_message(chat, i18n::t(lang, "op_failed_retry")).await.ok();
            return Err(e);
        }
    };
    let mut ops = state.pending_ops.lock().await;
    let old = ops.insert((chat.0, user_id), make_op(prompt.id.0));
    drop(ops);
    if let Some(old) = old {
        crate::push::retire_prompt(bot, chat, old.prompt_msg_id(), i18n::t(lang, "new_op_started")).await;
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
    let chat_type = crate::db::chat_type_str(chat.is_channel(), chat.is_group(), chat.is_supergroup());
    // Record the inviter (the user who performed the add/remove action) as the channel owner.
    let inviter_id = update.from.id.0.to_string();

    match update.new_chat_member.status() {
        ChatMemberStatus::Member
        | ChatMemberStatus::Administrator
        | ChatMemberStatus::Owner => {
            let title = chat.title().map(|t| t.to_string());
            let username = chat.username().map(|u| u.to_string());
            if let Err(e) = db::upsert_chat(&state.db, &chat_id, chat_type, title.as_deref(), username.as_deref(), Some(&inviter_id)).await {
                error!(chat_id = %chat_id, error = %e, "failed to upsert chat");
            } else {
                info!(chat_id = %chat_id, added_by = %inviter_id, "chat registered");
            }
        }
        ChatMemberStatus::Left | ChatMemberStatus::Banned => {
            // Bot removed: purge all data for this chat
            if let Err(e) = db::purge_chat(&state.db, &chat_id).await {
                error!(chat_id = %chat_id, error = %e, "failed to purge chat");
            } else {
                info!(chat_id = %chat_id, "bot removed — chat data purged");
            }
        }
        _ => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{guard_target, GuardTarget};

    /// Every op handled by the dispatch must be explicitly classified. This
    /// list is the regression guard against a new op silently bypassing the
    /// ownership check; unknown ops (the last assert) now fail closed.
    #[test]
    fn all_dispatched_ops_are_classified() {
        for op in [
            "ch", "cfg", "cfgi", "cfgm", "cfgs", "cfglp", "cfgcp", "resumeall", "cfgiv",
            "cfgmv", "cfgsv", "unsubpg", "unsuballask", "unsuball",
        ] {
            assert!(matches!(guard_target(op), Some(GuardTarget::Chat)), "{op}");
        }
        for op in [
            "src", "srcpause", "srcresume", "srcrename", "srcfip", "srcfmp", "srcfsp",
            "srcfcp", "srcflp", "srcfilter", "srcftoggle", "srcfmode", "srcfkw", "srcfi",
            "srcfm", "srcfs", "unsub",
        ] {
            assert!(matches!(guard_target(op), Some(GuardTarget::Sub)), "{op}");
        }
        for op in ["chlist", "lang", "cancelop", "ck"] {
            assert!(matches!(guard_target(op), Some(GuardTarget::None)), "{op}");
        }
        assert!(guard_target("unknown-op").is_none());
    }
}
