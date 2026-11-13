//! Shared panel rendering and delivery: page targets, keyboard helpers,
//! pagination, error pages, and the panels reused by both command and
//! callback handlers. Rendering lives here so commands do not depend on the
//! callback dispatcher.

use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode,
};

use crate::db;
use crate::i18n::{self, Lang};
use crate::push::{no_preview, ContentMode, SourceDisplay};
use crate::state::AppState;

/// Items per page for the channel list and the per-channel source list.
pub(crate) const PAGE_SIZE: usize = 10;

/// Chat display title with fallback: when the title is absent, show the raw
/// chat id instead, so panels/headers never render an empty name. Exactly
/// matches the previous inline `…and_then(|c| c.title)…unwrap_or(chat_id)`
/// pattern (no extra trimming/empty-string handling) to keep behaviour
/// unchanged. (Previously six sites repeated it inline.)
pub(crate) fn chat_display_title(title: Option<&str>, chat_id: &str) -> String {
    title.unwrap_or(chat_id).to_string()
}

/// Clamp a requested page to the actual page count and compute the
/// `[start, end)` slice bounds for a PAGE_SIZE-sized window. Returns
/// `(total_pages, clamped_page, start, end)`. `total_pages` is at least 1
/// so an empty list still renders one blank page (never zero — that would
/// underflow `total_pages - 1` when clamping). Previously three sites
/// (render_channel_list / render_channel_page / render_unsub_panel) each
/// re-typed these four lines verbatim.
pub(crate) fn paginate(len: usize, page: usize) -> (usize, usize, usize, usize) {
    let total_pages = len.div_ceil(PAGE_SIZE).max(1);
    let page = page.min(total_pages - 1);
    let start = page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(len);
    (total_pages, page, start, end)
}

/// Build a row containing only the shared Close button. Panels that need to
/// push a plain close row into a multi-row keyboard use this instead of
/// re-typing the button construction. `close_kb` (empty-state pages) uses it
/// to build a single-row Markup.
pub(crate) fn close_row(lang: Lang) -> Vec<InlineKeyboardButton> {
    vec![InlineKeyboardButton::callback(
        i18n::t(lang, "close"),
        "c|cancelop",
    )]
}

/// Build a `[back, close]` row. Panels that repeat this pair (with a
/// domain-specific back label key + callback data) use this helper instead
/// of hand-writing the two-button vector at every site.
pub(crate) fn back_close_row(
    lang: Lang,
    back_key: &'static str,
    back_cb: String,
) -> Vec<InlineKeyboardButton> {
    vec![
        InlineKeyboardButton::callback(i18n::t(lang, back_key), back_cb),
        InlineKeyboardButton::callback(i18n::t(lang, "close"), "c|cancelop"),
    ]
}

/// Build the pagination nav row (prev / indicator / next). M6 fix: three
/// call sites duplicated this exact logic; only the callback URL differs,
/// supplied via `mk_url`. Returns None when there is only one page.
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
        "c|page",
    ));
    if page + 1 < total_pages {
        nav.push(InlineKeyboardButton::callback(
            i18n::t(lang, "next_page"),
            mk_url(page + 1),
        ));
    }
    Some(nav)
}

/// Where a rendered page should go: a brand-new message, an in-place edit of
/// the message the callback button was pressed on, or an in-place edit of an
/// explicitly addressed message (used by flows outside a callback, e.g. the
/// rename reply refreshing the source page it came from).
pub(crate) enum PageTarget<'a> {
    Send(ChatId),
    Edit(&'a CallbackQuery),
    EditMessage(ChatId, teloxide::types::MessageId),
}

/// #15: Fallback keyboard for empty-state/error pages — at least provide a "Close" button,
/// so users are not left facing a dead buttonless message they cannot exit.
pub(crate) fn close_kb(lang: Lang) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![close_row(lang)])
}

/// Deliver an HTML page to the target. For `Edit`/`EditMessage`, edits the
/// original message in place; falls back to sending a new message when the
/// original can no longer be edited (e.g. too old).
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

/// Render a generic error page (escaped message + close button).
/// `err_display` is the already-formatted error text (callers may use
/// `{:#}` for anyhow error chains); escaping happens here.
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

/// Edit a message in place; fall back to sending a new message when the
/// original can no longer be edited (too old / inaccessible).
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
            // Content identical — nothing to update. Prefer matching the typed
            // ApiError::MessageNotModified; keep the substring check as a
            // fallback in case the error surfaces without the typed variant, so
            // an edit-to-identical-content never falls through to sending a
            // duplicate panel message (spamming on every paginate/toggle).
            if matches!(
                e,
                teloxide::RequestError::Api(teloxide::ApiError::MessageNotModified)
            ) || e.to_string().contains("not modified")
            {
                return Ok(());
            }
            // Original message can't be edited (too old / inaccessible): send new.
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

/// Render the /cookies panel: title + one button per platform showing
/// cookie status (not configured / configured / expired). Global (admin-configured).
/// The header text is always the plain title — after a cookie update the
/// confirmation is carried by the retired prompt message, so the panel
/// itself only needs its button statuses refreshed.
///
/// Lang is resolved by the caller (same convention as render_channel_list):
/// re-resolving here with `None` would drop the Telegram client language_code
/// hint and force English on never-configured users while adjacent panels
/// render Chinese.
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
            // Cookie-less platforms (e.g. Telegram web preview): listed
            // read-only, no configuration flow.
            buttons.push(vec![InlineKeyboardButton::callback(
                i18n::t(lang, "ck_no_config").replacen("{}", p.label, 1),
                format!("c|ck|{}", p.id),
            )]);
            continue;
        }
        let current = state.get_cookie(p.id).await;
        // Status determination: not configured → expired (settings flag) → configured
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
            format!("c|ck|{}", p.id),
        )]);
    }

    buttons.push(close_row(lang));

    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

/// Render the channel list page (paginated, PAGE_SIZE items per page).
pub(crate) async fn render_channel_list(
    bot: &Bot,
    target: PageTarget<'_>,
    state: &AppState,
    user_id: i64,
    page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    // Lang is resolved by the caller: the callback top-level and the command
    // path both run it through resolve_lang *with the Telegram client
    // language_code*, so the list page can no longer diverge from sibling
    // panels (previously this function re-resolved with None, which dropped
    // the client hint and forced English on never-configured users while
    // adjacent panels rendered Chinese).
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

    // Buttons-only list: per-item text lines duplicated the buttons, so each
    // channel is now directly a clickable button.
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
    // Per-channel subscription counts for button labels. A DB failure must not
    // masquerade as "every channel has 0 subscriptions" — fall back to an
    // explicit "?" placeholder per chat in that case. Missing keys in a
    // successful result legitimately mean 0 (GROUP BY only returns chats with
    // at least one subscription).
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
        // The 4th segment passes through the channel-list page number so the channel page's
        // "back to channel list" button can restore the original page.
        row.push(InlineKeyboardButton::callback(
            format!(
                "{}（{}）",
                crate::htmltext::truncate_text(&title, 32),
                cnt_label
            ),
            format!("c|ch|{}|0|{}", chat.chat_id, page),
        ));
        if row.len() >= 2 {
            buttons.push(std::mem::take(&mut row));
        }
    }
    if !row.is_empty() {
        buttons.push(row);
    }
    if let Some(nav) = nav_row(page, total_pages, lang, |p| format!("c|chlist|{}", p)) {
        buttons.push(nav);
    }
    // Language picker entry (shows current language; opens the picker on tap)
    buttons.push(vec![InlineKeyboardButton::callback(
        i18n::t(lang, "lang_btn"),
        "c|lang",
    )]);
    buttons.push(close_row(lang));

    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}

/// Render the language picker. Every supported language is one button, always
/// labelled with that language's own name; the current one is marked with a
/// check. Selecting a language writes the user preference and returns to the
/// channel list in that language.
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
            format!("c|lang|{}", option.db_value()),
        )]);
    }
    buttons.push(back_close_row(lang, "back", "c|chlist|0".to_string()));
    deliver(
        bot,
        &target,
        i18n::t(lang, "lang_title"),
        Some(InlineKeyboardMarkup::new(buttons)),
    )
    .await
}

/// Render the channel page (subscription list + management buttons).
/// The source list is paginated, PAGE_SIZE items per page.
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

    // Each source is directly a button opening its feed page. Paused sources
    // get a ⏸ marker; error counts stay off the list (see feed page).
    // Sequence numbers stay global (not reset per page) for stable reference.
    // The trailing chlist_page keeps the channel-list position alive across the
    // whole feed/unsub round trip (fix: without it, "back" buttons defaulted
    // the channel list to page 0).
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
            format!("c|src|{}|{}|{}", sub.id, page, chlist_page),
        )]);
    }

    if let Some(nav) = nav_row(page, total_pages, lang, |p| {
        format!("c|ch|{}|{}|{}", chat_id, p, chlist_page)
    }) {
        buttons.push(nav);
    }

    buttons.push(vec![
        InlineKeyboardButton::callback(
            i18n::t(lang, "chpage_settings"),
            format!("c|cfg|{}|{}|{}", chat_id, page, chlist_page),
        ),
        InlineKeyboardButton::callback(
            i18n::t(lang, "chpage_unsub"),
            format!("c|unsubpg|{}|0|{}|{}", chat_id, page, chlist_page),
        ),
    ]);
    // When paused sources exist, offer a one-click enable-all (reset error counts and resume fetching immediately)
    // Layout: c|resumeall|<chat_id>|<chlist_page>|<ch_page> — ch_page preserves the source-list page
    // across the re-render, chlist_page preserves the channel-list page for its own "back" button.
    if paused_count > 0 {
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "chpage_resume_all").replacen("{}", &paused_count.to_string(), 1),
            format!("c|resumeall|{}|{}|{}", chat_id, chlist_page, page),
        )]);
    }
    buttons.push(back_close_row(
        lang,
        "chpage_back",
        format!("c|chlist|{}", chlist_page),
    ));

    let kb = InlineKeyboardMarkup::new(buttons);

    deliver(bot, &target, &text, Some(kb)).await?;

    Ok(())
}

/// Render the channel settings page (rsstt-style option buttons).
/// Loads the subscription; when it doesn't exist or the query errors, delivers an error page to the target and returns None.
/// Boilerplate shared by render_feed_page / render_feed_filter / load_feed_ctx.
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

/// Render the per-source (feed) page: status, effective config with override
/// markers, and management buttons (pause/resume, rename, feed-level overrides).
///
/// Public entry point kept for the command path (rename-reply refresh).
/// PendingOp stores both page positions now, so the refresh preserves the
/// channel-list page instead of resetting it.
pub(crate) async fn render_feed_page(
    bot: &Bot,
    target: PageTarget<'_>,
    sub_id: i64,
    state: &AppState,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    render_feed_page_impl(bot, target, sub_id, state, ch_page, chlist_page, lang).await
}

/// Internal feed page renderer carrying `chlist_page` (the channel-list page the
/// user came from) through every button's callback data.
async fn render_feed_page_impl(
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
    // Panels are sent/edited as one message; an unbounded feed title or long
    // URL would make both edit and send fail with "message is too long",
    // leaving the source page permanently unreachable.
    let name_html = crate::push::escape_html(&crate::htmltext::truncate_text(name, 200));
    let mut text = i18n::fill(
        i18n::t(lang, "feed_header"),
        &[&url_attr, &name_html, status],
    );
    // Show original title when manually renamed
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

    // Pause / resume
    if sub.active == 1 {
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_pause"),
            format!("c|srcpause|{}|{}|{}", sub_id, ch_page, chlist_page),
        )]);
    } else {
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_resume"),
            format!("c|srcresume|{}|{}|{}", sub_id, ch_page, chlist_page),
        )]);
    }

    // Rename / reset name
    buttons.push(vec![InlineKeyboardButton::callback(
        i18n::t(lang, "feed_rename"),
        format!("c|srcrename|{}|{}|{}", sub_id, ch_page, chlist_page),
    )]);

    // Feed-level overrides: interval/mode/source open second-level pages;
    // caption/link preview toggle in place (mirrors the channel settings
    // layout). Labels show the effective value; override state is shown in the
    // summary above.
    if let Some(c) = &cfg {
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_interval").replacen(
                "{}",
                &fmt_interval(lang, c.interval_minutes),
                1,
            ),
            format!("c|srcfip|{}|{}|{}", sub_id, ch_page, chlist_page),
        )]);
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_mode").replacen("{}", fmt_content_mode(lang, &c.content_mode), 1),
            format!("c|srcfmp|{}|{}|{}", sub_id, ch_page, chlist_page),
        )]);
        buttons.push(vec![InlineKeyboardButton::callback(
            i18n::t(lang, "feed_source").replacen(
                "{}",
                fmt_source_display(lang, &c.source_display),
                1,
            ),
            format!("c|srcfsp|{}|{}|{}", sub_id, ch_page, chlist_page),
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
            format!("c|srcfcp|{}|{}|{}", sub_id, ch_page, chlist_page),
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
            format!("c|srcflp|{}|{}|{}", sub_id, ch_page, chlist_page),
        )]);
        // Note: under Approach A, picking a value identical to the channel automatically clears
        // the override, so no global reset button is needed; each sub-page still keeps the
        // "↩️ 🔗 follow channel" button.
    }

    // Content filter button (shows status when active)
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
            format!("c|srcfilter|{}|{}|{}", sub_id, ch_page, chlist_page),
        )]);
    }

    buttons.push(back_close_row(
        lang,
        "unsub_back",
        format!("c|ch|{}|{}|{}", sub.chat_id, ch_page, chlist_page),
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
    // Normalize through the same legacy-tolerant parser used by rendering, so
    // an old `TitleSummary`/`MediaOnly` value does not display as Title+Link.
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

/// Second-level page: content filter settings for a feed.
///
/// Public entry point kept for the command path (filter-keyword reply refresh).
/// PendingOp stores both page positions now, so the refresh preserves the
/// channel-list page instead of resetting it.
pub(crate) async fn render_feed_filter(
    bot: &Bot,
    target: PageTarget<'_>,
    sub_id: i64,
    state: &AppState,
    ch_page: usize,
    chlist_page: usize,
    lang: Lang,
) -> ResponseResult<()> {
    render_feed_filter_impl(bot, target, sub_id, state, ch_page, chlist_page, lang).await
}

/// Internal filter renderer carrying `chlist_page` through every button.
async fn render_feed_filter_impl(
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
        // The switch is on but no keywords are configured: say so instead of
        // claiming "off" while the toggle button says "disable".
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
            format!("c|srcftoggle|{}|{}|{}", sub_id, ch_page, chlist_page),
        )],
        vec![InlineKeyboardButton::callback(
            mode_btn_label,
            format!("c|srcfmode|{}|{}|{}", sub_id, ch_page, chlist_page),
        )],
        vec![InlineKeyboardButton::callback(
            i18n::t(lang, "filter_set_kw"),
            format!("c|srcfkw|{}|{}|{}", sub_id, ch_page, chlist_page),
        )],
        back_close_row(
            lang,
            "back",
            format!("c|src|{}|{}|{}", sub_id, ch_page, chlist_page),
        ),
    ];
    let kb = InlineKeyboardMarkup::new(buttons);
    deliver(bot, &target, &text, Some(kb)).await?;
    Ok(())
}
