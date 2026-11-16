use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};
use tracing::{info, warn};

use super::wire;
use super::{fetch_sub_or_toast, toast_text};
use crate::db;
use crate::i18n::{self, Lang};
use crate::push::no_preview;
use crate::state::AppState;
use crate::ui::{chat_display_title, deliver, deliver_error_page, nav_row, paginate, PageTarget};

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
        return Ok(());
    };

    let name = sub
        .feed_title
        .as_deref()
        .unwrap_or(&sub.feed_url)
        .to_string();
    let feed_url = sub.feed_url.clone();
    let chat_id = sub.chat_id.clone();

    if let Err(e) = db::delete_subscription(&state.db, sub_id).await {
        bot.answer_callback_query(qid.clone())
            .text(toast_text(i18n::t(lang, "op_failed").replacen(
                "{:#}",
                &format!("{:#}", e),
                1,
            )))
            .await
            .ok();
        return Ok(());
    }

    info!(sub_id = sub_id, feed = %feed_url, "subscription removed");

    let chat = q.message.as_ref().map(|m| m.chat().id);
    bot.answer_callback_query(qid.clone()).await.ok();
    if let Some(chat) = chat {
        let href = crate::push::escape_attr(&feed_url);
        let name_html = crate::push::escape_html(&name);
        let msg = i18n::fill(i18n::t(lang, "unsub_ok"), &[&href, &name_html]);
        bot.send_message(chat, msg)
            .parse_mode(ParseMode::Html)
            .link_preview_options(no_preview())
            .await
            .ok();
    }

    render_unsub_panel(
        bot,
        PageTarget::Edit(q),
        &chat_id,
        state,
        page,
        ch_page,
        chlist_page,
        lang,
    )
    .await?;

    Ok(())
}

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
        let kb = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            i18n::t(lang, "unsub_back"),
            wire::channel(chat_id, ch_page, chlist_page),
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
        i18n::t(lang, "unsub_panel_single").replacen("{}", &subs.len().to_string(), 1)
    };

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for (offset, sub) in subs[start..end].iter().enumerate() {
        let name = sub.feed_title.as_deref().unwrap_or(&sub.feed_url);
        let flag = if sub.active == 0 { " ⏸" } else { "" };
        buttons.push(vec![InlineKeyboardButton::callback(
            format!(
                "{}. {}{}",
                start + offset + 1,
                crate::htmltext::truncate_text(name, 30),
                flag
            ),
            wire::unsub(sub.id, page, ch_page, chlist_page),
        )]);
    }
    if let Some(nav) = nav_row(page, total_pages, lang, |p| {
        wire::unsub_page(chat_id, p, ch_page, chlist_page)
    }) {
        buttons.push(nav);
    }
    buttons.push(vec![InlineKeyboardButton::callback(
        i18n::t(lang, "unsub_all_btn"),
        wire::unsub_all_ask(chat_id, page, ch_page, chlist_page),
    )]);
    buttons.push(vec![InlineKeyboardButton::callback(
        i18n::t(lang, "unsub_back"),
        wire::channel(chat_id, ch_page, chlist_page),
    )]);

    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

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
            wire::channel(chat_id, ch_page, chlist_page),
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
        InlineKeyboardButton::callback(
            i18n::t(lang, "unsuball_confirm_btn"),
            wire::unsub_all(chat_id, ch_page, page, chlist_page),
        ),
        InlineKeyboardButton::callback(
            i18n::t(lang, "back"),
            wire::unsub_page(chat_id, page, ch_page, chlist_page),
        ),
    ]]);

    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

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
            bot.answer_callback_query(qid.clone())
                .text(toast_text(i18n::t(lang, "op_failed").replacen(
                    "{:#}",
                    &format!("{:#}", e),
                    1,
                )))
                .await
                .ok();
            return Ok(());
        }
    };

    if subs.is_empty() {
        bot.answer_callback_query(qid.clone())
            .text(i18n::t(lang, "unsub_no_subs"))
            .await
            .ok();
        return Ok(());
    }

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

    let target_chat = match q.message.as_ref().map(|m| m.chat().id) {
        Some(t) => t,
        None => {
            bot.answer_callback_query(qid.clone())
                .text(i18n::t(lang, "unsuball_no_target"))
                .await
                .ok();
            return Ok(());
        }
    };
    let chat = db::get_chat(&state.db, chat_id).await.ok().flatten();
    let title = chat_display_title(chat.as_ref().and_then(|c| c.title.as_deref()), chat_id);

    let count = match db::delete_all_subscriptions(&state.db, chat_id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                chat_id = chat_id,
                error = format!("{:#}", e),
                "failed to delete all subscriptions (transaction rolled back)"
            );
            bot.answer_callback_query(qid.clone())
                .text(toast_text(i18n::t(lang, "op_failed").replacen(
                    "{:#}",
                    &format!("{:#}", e),
                    1,
                )))
                .await
                .ok();
            if let Err(render_err) = render_unsub_all_confirm(
                bot,
                PageTarget::Edit(q),
                chat_id,
                state,
                page,
                ch_page,
                chlist_page,
                lang,
            )
            .await
            {
                warn!(chat_id = chat_id, error = %render_err, "unsuball: failed to restore confirmation page");
                bot.send_message(target_chat, i18n::t(lang, "op_failed_retry"))
                    .await
                    .ok();
            }
            return Ok(());
        }
    };

    info!(
        chat_id = chat_id,
        count = count,
        "all subscriptions removed"
    );

    bot.answer_callback_query(qid.clone()).await.ok();
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

    if let Some(m) = q.message.as_ref() {
        let kb = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            i18n::t(lang, "unsuball_view"),
            wire::channel(chat_id, ch_page, chlist_page),
        )]]);
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
