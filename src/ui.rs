use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode,
};

use crate::callbacks::wire;
use crate::db;
use crate::i18n::{self, Lang};
use crate::push::{no_preview, ContentMode, SourceDisplay};
use crate::state::AppState;

pub(crate) const PAGE_SIZE: usize = 10;

pub(crate) fn chat_display_title(title: Option<&str>, chat_id: &str) -> String {
    title.unwrap_or(chat_id).to_string()
}

pub(crate) fn paginate(len: usize, page: usize) -> (usize, usize, usize, usize) {
    let total_pages = len.div_ceil(PAGE_SIZE).max(1);
    let page = page.min(total_pages - 1);
    let start = page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(len);
    (total_pages, page, start, end)
}

pub(crate) fn close_row(lang: Lang) -> Vec<InlineKeyboardButton> {
    vec![InlineKeyboardButton::callback(
        i18n::t(lang, "close"),
        wire::cancelop(),
    )]
}

pub(crate) fn back_close_row(
    lang: Lang,
    back_key: &'static str,
    back_cb: String,
) -> Vec<InlineKeyboardButton> {
    vec![
        InlineKeyboardButton::callback(i18n::t(lang, back_key), back_cb),
        InlineKeyboardButton::callback(i18n::t(lang, "close"), wire::cancelop()),
    ]
}

pub(crate) fn nav_row(
    page: usize,
    total_pages: usize,
    lang: Lang,
    mk_url: impl Fn(usize) -> String,
) -> Option<Vec<InlineKeyboardButton>> {
    if total_pages <= 1 {
        return None;
    }
    let mut nav: Vec<InlineKeyboardButton> = Vec::new();
    if page > 0 {
        nav.push(InlineKeyboardButton::callback(
            i18n::t(lang, "prev_page"),
            mk_url(page - 1),
        ));
    }
    nav.push(InlineKeyboardButton::callback(
        format!("{} / {}", page + 1, total_pages),
        wire::page(),
    ));
    if page + 1 < total_pages {
        nav.push(InlineKeyboardButton::callback(
            i18n::t(lang, "next_page"),
            mk_url(page + 1),
        ));
    }
    Some(nav)
}

pub(crate) enum PageTarget<'a> {
    Send(ChatId),
    Edit(&'a CallbackQuery),
    EditMessage(ChatId, teloxide::types::MessageId),
}

pub(crate) fn close_kb(lang: Lang) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![close_row(lang)])
}

pub(crate) async fn deliver(
    bot: &Bot,
    target: &PageTarget<'_>,
    text: &str,
    kb: Option<InlineKeyboardMarkup>,
) -> ResponseResult<()> {
    match target {
        PageTarget::Send(chat) => {
            let mut req = bot
                .send_message(*chat, text)
                .parse_mode(ParseMode::Html)
                .link_preview_options(no_preview());
            if let Some(k) = kb {
                req = req.reply_markup(k);
            }
            req.await?;
        }
        PageTarget::Edit(q) => {
            if let Some(m) = &q.message {
                let chat = m.chat().id;
                let mid = m.id();
                bot.answer_callback_query(q.id.clone()).await.ok();
                edit_or_send(bot, chat, mid, text, kb).await?;
            } else {
                tracing::warn!(callback_id = %q.id, "deliver: q.message is None, cannot render page");
            }
        }
        PageTarget::EditMessage(chat, mid) => {
            edit_or_send(bot, *chat, *mid, text, kb).await?;
        }
    }
    Ok(())
}

pub(crate) async fn deliver_error_page(
    bot: &Bot,
    target: &PageTarget<'_>,
    lang: Lang,
    err_display: &str,
) -> ResponseResult<()> {
    deliver(
        bot,
        target,
        &i18n::t(lang, "error_page").replacen("{}", &crate::push::escape_html(err_display), 1),
        Some(close_kb(lang)),
    )
    .await
}

pub(crate) async fn edit_or_send(
    bot: &Bot,
    chat: ChatId,
    mid: teloxide::types::MessageId,
    text: &str,
    kb: Option<InlineKeyboardMarkup>,
) -> ResponseResult<()> {
    let mut req = bot
        .edit_message_text(chat, mid, text)
        .parse_mode(ParseMode::Html)
        .link_preview_options(no_preview());
    if let Some(k) = kb.clone() {
        req = req.reply_markup(k);
    }
    match req.await {
        Ok(_) => {}
        Err(e) => {
            if matches!(
                e,
                teloxide::RequestError::Api(teloxide::ApiError::MessageNotModified)
            ) || e.to_string().contains("not modified")
            {
                return Ok(());
            }
            let mut s = bot
                .send_message(chat, text)
                .parse_mode(ParseMode::Html)
                .link_preview_options(no_preview());
            if let Some(k) = kb {
                s = s.reply_markup(k);
            }
            s.await?;
        }
    }
    Ok(())
}

pub(crate) async fn render_cookies_panel(
    bot: &Bot,
    target: PageTarget<'_>,
    state: &AppState,
    lang: Lang,
) -> ResponseResult<()> {
    let text = String::from(i18n::t(lang, "ck_panel_title"));

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    for p in crate::platforms::PLATFORMS {
        if !p.needs_cookie {
            buttons.push(vec![InlineKeyboardButton::callback(
                i18n::t(lang, "ck_no_config").replacen("{}", p.label, 1),
                wire::cookies(p.id),
            )]);
            continue;
        }
        let current = state.get_cookie(p.id).await;
        let status = if current.is_none() {
            i18n::t(lang, "ck_not_set")
        } else {
            let flag_key = crate::db::setting_key::cookie_expired_notified(p.id);
            let expired = crate::db::get_setting(&state.db, &flag_key)
                .await
                .ok()
                .flatten()
                .map(|v| v == "1")
                .unwrap_or(false);
            if expired {
                i18n::t(lang, "ck_expired")
            } else {
                i18n::t(lang, "ck_configured")
            }
        };
        buttons.push(vec![InlineKeyboardButton::callback(
            format!("{}（{}）", p.label, status),
            wire::cookies(p.id),
        )]);
    }

    buttons.push(close_row(lang));

    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

pub(crate) async fn render_channel_list(
    bot: &Bot,
    target: PageTarget<'_>,
    state: &AppState,
    user_id: i64,
    page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let chats = match db::list_chats(&state.db, user_id).await {
        Ok(c) => c,
        Err(e) => {
            deliver_error_page(bot, &target, lang, &format!("{:#}", e)).await?;
            return Ok(());
        }
    };

    if chats.is_empty() {
        deliver(
            bot,
            &target,
            i18n::t(lang, "chlist_empty"),
            Some(close_kb(lang)),
        )
        .await?;
        return Ok(());
    }

    let (total_pages, page, start, end) = paginate(chats.len(), page);

    let text = if total_pages > 1 {
        let page_s = (page + 1).to_string();
        let pages_s = total_pages.to_string();
        let count_s = chats.len().to_string();
        i18n::fill(
            i18n::t(lang, "chlist_multi"),
            &[&page_s, &pages_s, &count_s],
        )
    } else {
        i18n::t(lang, "chlist_single").replacen("{}", &chats.len().to_string(), 1)
    };
    let sub_counts: Option<std::collections::HashMap<String, i64>> =
        match db::count_subscriptions_per_chat(&state.db).await {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::warn!(error = %e, "count_subscriptions_per_chat failed; channel list will show ? for feed counts");
                None
            }
        };

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let mut row: Vec<InlineKeyboardButton> = Vec::new();

    for chat in &chats[start..end] {
        let title = chat_display_title(chat.title.as_deref(), &chat.chat_id);
        let cnt_label = match &sub_counts {
            Some(m) => m.get(&chat.chat_id).copied().unwrap_or(0).to_string(),
            None => "?".to_string(),
        };
        row.push(InlineKeyboardButton::callback(
            format!(
                "{}（{}）",
                crate::htmltext::truncate_text(&title, 32),
                cnt_label
            ),
            wire::channel(&chat.chat_id, 0, page),
        ));
        if row.len() >= 2 {
            buttons.push(std::mem::take(&mut row));
        }
    }
    if !row.is_empty() {
        buttons.push(row);
    }
    if let Some(nav) = nav_row(page, total_pages, lang, wire::channel_list) {
        buttons.push(nav);
    }
    buttons.push(vec![InlineKeyboardButton::callback(
        i18n::t(lang, "lang_btn"),
        wire::op_prefix(wire::CallbackOp::Language),
    )]);
    buttons.push(close_row(lang));

    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

pub(crate) async fn render_language_picker(
    bot: &Bot,
    target: PageTarget<'_>,
    lang: Lang,
) -> ResponseResult<()> {
    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for option in Lang::ALL {
        let label = option.label();
        let mark = if option == lang { "✓ " } else { "" };
        buttons.push(vec![InlineKeyboardButton::callback(
            format!("{mark}{label}"),
            wire::language(option.db_value()),
        )]);
    }
    buttons.push(back_close_row(lang, "back", wire::channel_list(0)));
    deliver(
        bot,
        &target,
        i18n::t(lang, "lang_title"),
        Some(InlineKeyboardMarkup::new(buttons)),
    )
    .await
}

pub(crate) async fn render_channel_page(
    bot: &Bot,
    target: PageTarget<'_>,
    chat_id: &str,
    state: &AppState,
    page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let chat = match db::get_chat(&state.db, chat_id).await {
        Ok(c) => c,
        Err(e) => {
            deliver_error_page(bot, &target, lang, &format!("{:#}", e)).await?;
            return Ok(());
        }
    };
    let title = chat_display_title(chat.as_ref().and_then(|c| c.title.as_deref()), chat_id);
    let subs = match db::get_subscriptions(&state.db, chat_id).await {
        Ok(s) => s,
        Err(e) => {
            deliver_error_page(bot, &target, lang, &format!("{:#}", e)).await?;
            return Ok(());
        }
    };

    let (total_pages, page, start, end) = paginate(subs.len(), page);

    let mut text = crate::push::channel_header_text(lang, "chpage_header", &title, chat_id, &subs);
    let (_, paused_count) = crate::push::channel_sub_counts(&subs);
    if subs.is_empty() {
        text.push_str(i18n::t(lang, "chpage_no_subs"));
    } else if total_pages > 1 {
        let page_s = (page + 1).to_string();
        let pages_s = total_pages.to_string();
        text.push_str(&i18n::fill(
            i18n::t(lang, "chpage_multi"),
            &[&page_s, &pages_s],
        ));
    } else {
        text.push_str(i18n::t(lang, "chpage_single"));
    }

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    for (offset, sub) in subs[start..end].iter().enumerate() {
        let name = sub.feed_title.as_deref().unwrap_or(&sub.feed_url);
        let flag = if sub.active == 0 { " ⏸" } else { "" };
        buttons.push(vec![InlineKeyboardButton::callback(
            format!(
                "{}. {}{}",
                start + offset + 1,
                crate::htmltext::truncate_text(name, 36),
                flag
            ),
            wire::source(sub.id, page, chlist_page),
        )]);
    }

    if let Some(nav) = nav_row(page, total_pages, lang, |p| {
        wire::channel(chat_id, p, chlist_page)
    }) {
        buttons.push(nav);
    }

    buttons.push(vec![
        InlineKeyboardButton::callback(
            i18n::t(lang, "chpage_settings"),
            wire::config(chat_id, page, chlist_page),
        ),
        InlineKeyboardButton::callback(
            i18n::t(lang, "chpage_unsub"),
            wire::unsub_page(chat_id, 0, page, chlist_page),
        ),
    ]);
    if paused_count > 0 {
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "chpage_resume_all").replacen("{}", &paused_count.to_string(), 1),
            wire::resume_all(chat_id, chlist_page, page),
        )]);
    }
    buttons.push(back_close_row(
        lang,
        "chpage_back",
        wire::channel_list(chlist_page),
    ));

    let kb = InlineKeyboardMarkup::new(buttons);

    deliver(bot, &target, &text, Some(kb)).await?;

    Ok(())
}

pub(crate) async fn load_sub_or_deliver(
    bot: &Bot,
    target: &PageTarget<'_>,
    db: &crate::db::Db,
    sub_id: i64,
    lang: Lang,
) -> ResponseResult<Option<db::SubRow>> {
    match db::get_subscription_by_id(db, sub_id).await {
        Ok(Some(s)) => Ok(Some(s)),
        Ok(None) => {
            deliver(
                bot,
                target,
                i18n::t(lang, "sub_not_found"),
                Some(close_kb(lang)),
            )
            .await?;
            Ok(None)
        }
        Err(e) => {
            deliver_error_page(bot, target, lang, &format!("{}", e)).await?;
            Ok(None)
        }
    }
}

pub(crate) async fn render_feed_page(
    bot: &Bot,
    target: PageTarget<'_>,
    sub_id: i64,
    state: &AppState,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let Some(sub) = load_sub_or_deliver(bot, &target, &state.db, sub_id, lang).await? else {
        return Ok(());
    };

    let cfg = match db::get_effective_config(&state.db, &sub.chat_id, &sub.feed_url).await {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(sub_id, error = %e, "feed page config read failed");
            return deliver_error_page(bot, &target, lang, &format!("{:#}", e)).await;
        }
    };

    let name = sub.feed_title.as_deref().unwrap_or(&sub.feed_url);
    let status = if sub.active == 1 {
        i18n::t(lang, "feed_active")
    } else {
        i18n::t(lang, "feed_paused")
    };

    let url_attr = crate::push::escape_attr(&sub.feed_url);
    let name_html = crate::push::escape_html(&crate::htmltext::truncate_text(name, 200));
    let mut text = i18n::fill(
        i18n::t(lang, "feed_header"),
        &[&url_attr, &name_html, status],
    );
    if sub.title_overridden == 1 {
        if let Some(orig) = &sub.original_title {
            let orig = crate::htmltext::truncate_text(orig, 200);
            text.push_str(&i18n::t(lang, "feed_orig_name").replacen(
                "{}",
                &crate::push::escape_html(&orig),
                1,
            ));
        }
    }

    if let Some(c) = &cfg {
        let iv_mark = if c.iv_overridden { "✏️" } else { "🔗" };
        let cm_mark = if c.cm_overridden { "✏️" } else { "🔗" };
        let sd_mark = if c.sd_overridden { "✏️" } else { "🔗" };
        let cp_mark = if c.cp_overridden { "✏️" } else { "🔗" };
        let lp_mark = if c.lp_overridden { "✏️" } else { "🔗" };
        let iv_label = fmt_interval(lang, c.interval_minutes);
        let cm_label = fmt_content_mode(lang, &c.content_mode);
        let sd_label = fmt_source_display(lang, &c.source_display);
        let cp_label = if c.caption_above_media {
            i18n::t(lang, "on")
        } else {
            i18n::t(lang, "off")
        };
        let lp_label = if c.link_preview {
            i18n::t(lang, "on")
        } else {
            i18n::t(lang, "off")
        };
        text.push_str(&i18n::fill(
            i18n::t(lang, "feed_config"),
            &[
                &iv_label, iv_mark, cm_label, cm_mark, sd_label, sd_mark, cp_label, cp_mark,
                lp_label, lp_mark,
            ],
        ));
    }

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    if sub.active == 1 {
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_pause"),
            wire::source_action(wire::CallbackOp::SourcePause, sub_id, ch_page, chlist_page),
        )]);
    } else {
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_resume"),
            wire::source_action(wire::CallbackOp::SourceResume, sub_id, ch_page, chlist_page),
        )]);
    }

    buttons.push(vec![InlineKeyboardButton::callback(
        i18n::t(lang, "feed_rename"),
        wire::source_action(wire::CallbackOp::SourceRename, sub_id, ch_page, chlist_page),
    )]);

    if let Some(c) = &cfg {
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_interval").replacen(
                "{}",
                &fmt_interval(lang, c.interval_minutes),
                1,
            ),
            wire::source_action(
                wire::CallbackOp::SourceIntervalPage,
                sub_id,
                ch_page,
                chlist_page,
            ),
        )]);
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_mode").replacen("{}", fmt_content_mode(lang, &c.content_mode), 1),
            wire::source_action(
                wire::CallbackOp::SourceModePage,
                sub_id,
                ch_page,
                chlist_page,
            ),
        )]);
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_source").replacen(
                "{}",
                fmt_source_display(lang, &c.source_display),
                1,
            ),
            wire::source_action(
                wire::CallbackOp::SourceSourcePage,
                sub_id,
                ch_page,
                chlist_page,
            ),
        )]);
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "settings_caption").replacen(
                "{}",
                if c.caption_above_media {
                    i18n::t(lang, "on")
                } else {
                    i18n::t(lang, "off")
                },
                1,
            ),
            wire::source_action(
                wire::CallbackOp::SourceCaption,
                sub_id,
                ch_page,
                chlist_page,
            ),
        )]);
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "settings_lp").replacen(
                "{}",
                if c.link_preview {
                    i18n::t(lang, "on")
                } else {
                    i18n::t(lang, "off")
                },
                1,
            ),
            wire::source_action(
                wire::CallbackOp::SourceLinkPreview,
                sub_id,
                ch_page,
                chlist_page,
            ),
        )]);
    }

    {
        let fc = match db::get_feed_filter(&state.db, &sub.chat_id, &sub.feed_url).await {
            Ok(fc) => fc,
            Err(e) => {
                tracing::warn!(sub_id, error = %e, "feed filter read failed");
                return deliver_error_page(bot, &target, lang, &format!("{:#}", e)).await;
            }
        };
        let label = if fc.is_active() {
            let mode_label = if fc.mode == "whitelist" {
                i18n::t(lang, "whitelist")
            } else {
                i18n::t(lang, "blacklist")
            };
            let kw_count = fc.keyword_list().len().to_string();
            i18n::fill(
                i18n::t(lang, "feed_filter_active"),
                &[mode_label, &kw_count],
            )
        } else {
            i18n::t(lang, "feed_filter").to_string()
        };
        buttons.push(vec![InlineKeyboardButton::callback(
            label,
            wire::source_action(wire::CallbackOp::SourceFilter, sub_id, ch_page, chlist_page),
        )]);
    }

    buttons.push(back_close_row(
        lang,
        "unsub_back",
        wire::channel(&sub.chat_id, ch_page, chlist_page),
    ));

    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

pub(crate) fn fmt_interval(lang: Lang, min: i64) -> String {
    match min {
        180 => i18n::t(lang, "3h").to_string(),
        360 => i18n::t(lang, "6h").to_string(),
        720 => i18n::t(lang, "12h").to_string(),
        m => i18n::t(lang, "n_min").replacen("{}", &m.to_string(), 1),
    }
}

pub(crate) fn fmt_content_mode(lang: Lang, m: &str) -> &'static str {
    match ContentMode::from_str(m) {
        ContentMode::TitleSummary => i18n::t(lang, "mode_title_summary"),
        ContentMode::TitleContent => i18n::t(lang, "mode_title_content"),
        ContentMode::TitleOnly => i18n::t(lang, "mode_title_only"),
        ContentMode::ContentOnly => i18n::t(lang, "mode_content_only"),
        ContentMode::MediaOnly => i18n::t(lang, "mode_media_only"),
        ContentMode::TitleLink => i18n::t(lang, "mode_title_link"),
    }
}

pub(crate) fn fmt_source_display(lang: Lang, s: &str) -> &'static str {
    match SourceDisplay::from_str(s) {
        SourceDisplay::Author => i18n::t(lang, "src_author"),
        SourceDisplay::None => i18n::t(lang, "src_none"),
        SourceDisplay::Source => i18n::t(lang, "src_title"),
    }
}

pub(crate) async fn render_feed_filter(
    bot: &Bot,
    target: PageTarget<'_>,
    sub_id: i64,
    state: &AppState,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    let Some(sub) = load_sub_or_deliver(bot, &target, &state.db, sub_id, lang).await? else {
        return Ok(());
    };
    let fc = match db::get_feed_filter(&state.db, &sub.chat_id, &sub.feed_url).await {
        Ok(fc) => fc,
        Err(e) => {
            tracing::warn!(sub_id, error = %e, "filter page read failed");
            return deliver_error_page(bot, &target, lang, &format!("{:#}", e)).await;
        }
    };

    let status = if fc.is_active() {
        i18n::t(lang, "filter_on")
    } else if fc.enabled {
        i18n::t(lang, "filter_on_no_keywords")
    } else {
        i18n::t(lang, "filter_off")
    };
    let mode_label = if fc.mode == "whitelist" {
        i18n::t(lang, "filter_wl_desc")
    } else {
        i18n::t(lang, "filter_bl_desc")
    };
    let kw_display = if fc.keywords.is_empty() {
        i18n::t(lang, "none").to_string()
    } else {
        format!("<code>{}</code>", crate::push::escape_html(&fc.keywords))
    };

    let text = i18n::fill(
        i18n::t(lang, "filter_panel"),
        &[status, mode_label, &kw_display],
    );

    let toggle_label = if fc.enabled {
        i18n::t(lang, "filter_disable")
    } else {
        i18n::t(lang, "filter_enable")
    };
    let mode_btn_label = if fc.mode == "whitelist" {
        i18n::t(lang, "filter_to_bl")
    } else {
        i18n::t(lang, "filter_to_wl")
    };

    let buttons = vec![
        vec![InlineKeyboardButton::callback(
            toggle_label,
            wire::source_action(
                wire::CallbackOp::SourceFilterToggle,
                sub_id,
                ch_page,
                chlist_page,
            ),
        )],
        vec![InlineKeyboardButton::callback(
            mode_btn_label,
            wire::source_action(
                wire::CallbackOp::SourceFilterMode,
                sub_id,
                ch_page,
                chlist_page,
            ),
        )],
        vec![InlineKeyboardButton::callback(
            i18n::t(lang, "filter_set_kw"),
            wire::source_action(
                wire::CallbackOp::SourceFilterKeywords,
                sub_id,
                ch_page,
                chlist_page,
            ),
        )],
        back_close_row(lang, "back", wire::source(sub_id, ch_page, chlist_page)),
    ];
    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}
