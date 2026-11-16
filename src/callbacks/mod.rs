//! Callback routing, ownership guard and pending-op prompts.
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
pub(crate) mod wire;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardTarget {
    Chat,
    Sub,
    None,
}

pub(super) struct CallbackCtx<'a> {
    pub bot: &'a Bot,
    pub q: &'a CallbackQuery,
    pub state: &'a AppState,
    pub lang: Lang,
    pub qid: teloxide::types::CallbackQueryId,
    pub parts: &'a [&'a str],
    pub op: data::CallbackOp,
    pub guard_chat_id: Option<&'a str>,
}
pub async fn handle_callback(bot: Bot, q: CallbackQuery, state: AppState) -> ResponseResult<()> {
    let qid = q.id.clone();
    let data = match &q.data {
        Some(d) => d.clone(),
        None => {
            bot.answer_callback_query(qid).await.ok();
            return Ok(());
        }
    };

    let user_id = q.from.id.0 as i64;
    if !state.config.is_allowed(user_id) {
        bot.answer_callback_query(qid).await.ok();
        return Ok(());
    }
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
        bot.answer_callback_query(qid).await.ok();
        return Ok(());
    }

    // Callback data is client-controlled and forgeable; ownership must be verified (no admin bypass) before any management op.
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
        GuardTarget::Sub => match cb.args.first().and_then(|s| s.parse::<i64>().ok()) {
            Some(sub_id) => match db::get_subscription_by_id(&state.db, sub_id).await {
                Ok(Some(s)) => Some(s.chat_id),
                Ok(None) => None,
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
            },
            None => None,
        },
        GuardTarget::None => None,
    };
    if let Some(gcid) = &guard_chat_id {
        let allowed = match db::can_manage_chat(&state.db, user_id, gcid).await {
            Ok(v) => v,
            Err(e) => {
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
            if let Some(target) = parts.get(2) {
                let page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                render_channel_page(&bot, PageTarget::Edit(&q), target, &state, page, chlist_page, lang).await?;
            }
        }
        Some("chlist") => {
            let page = parts.get(2).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
            render_channel_list(&bot, PageTarget::Edit(&q), &state, user_id, page, lang).await?;
        }
        Some("lang") => {
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
            if let Some(cid) = parts.get(2) {
                let page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let ch_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                unsub::render_unsub_panel(&bot, PageTarget::Edit(&q), cid, &state, page, ch_page, chlist_page, lang).await?;
            }
        }
        Some("unsuballask") => {
            if let Some(cid) = parts.get(2) {
                let page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let ch_page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                unsub::render_unsub_all_confirm(&bot, PageTarget::Edit(&q), cid, &state, page, ch_page, chlist_page, lang).await?;
            }
        }
        Some("unsuball") => {
            if let Some(cid) = parts.get(2) {
                let ch_page = parts.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let page = parts.get(4).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let chlist_page = parts.get(5).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                unsub::handle_unsub_all_callback(&bot, &q, &state, cid, ch_page, page, chlist_page, lang).await?;
            }
        }
        Some("cancelop") => {
            if let Some(m) = q.message.as_ref() {
                let key = (m.chat().id.0, q.from.id.0 as i64);
                let removed_op = state.pending_ops.lock().await.remove(&key);
                bot.answer_callback_query(qid.clone()).await.ok();
                if let Some(op) = removed_op {
                    crate::push::retire_prompt(&bot, m.chat().id, op.prompt_msg_id(), i18n::t(lang, "cancelled")).await;
                }
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
            if !state.config.is_admin(user_id) {
                bot.answer_callback_query(qid.clone()).await.ok();
            } else if let Some(pid) = parts.get(2) {
                if let Some(p) = crate::platforms::find_platform(pid) {
                    if !p.needs_cookie {
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "ck_no_need").replacen("{}", p.label, 1))
                            .await.ok();
                    } else {
                    let chat_obj = q.message.as_ref().map(|m| m.chat().clone());
                    if !chat_obj.as_ref().map(|c| c.is_private()).unwrap_or(false) {
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "ck_private_only"))
                            .await.ok();
                    } else {
                    let chat = chat_obj.map(|c| c.id);
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

    bot.answer_callback_query(qid).await.ok();

    if let Err(e) = &res {
        tracing::warn!(user_id = user_id, op = %parts.get(1).copied().unwrap_or("?"), error = %e, "callback dispatch failed (spinner cleared)");
    }
    res
}

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
    // Never hold the pending-ops lock across a network await; one stalled send would freeze every chat.
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

pub async fn handle_my_chat_member(
    update: ChatMemberUpdated,
    state: AppState,
) -> ResponseResult<()> {
    let chat = update.chat.clone();
    let chat_id = chat.id.0.to_string();
    if !(chat.is_channel() || chat.is_supergroup() || chat.is_group()) {
        return Ok(());
    }
    let chat_type =
        crate::db::chat_type_str(chat.is_channel(), chat.is_group(), chat.is_supergroup());
    let inviter = update.from.id.0 as i64;
    let inviter_id = inviter.to_string();

    match update.new_chat_member.status() {
        ChatMemberStatus::Member | ChatMemberStatus::Administrator | ChatMemberStatus::Owner => {
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
            // A bot-removal event must soft-disable, not purge; purge is an unauthenticated data-loss path.
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
