//! Dispatch handlers for callback config/source operations.
//!
//! Split out of `callbacks/mod.rs` so the router stays small. The ctx aliases
//! keep the original branch bodies unchanged.

use super::data::CallbackOp;
use super::*;

pub(super) fn is_config_op(op: CallbackOp) -> bool {
    matches!(
        op,
        CallbackOp::Config
            | CallbackOp::ConfigInterval
            | CallbackOp::ConfigMode
            | CallbackOp::ConfigSource
            | CallbackOp::ConfigLinkPreview
            | CallbackOp::ConfigCaption
            | CallbackOp::ResumeAll
            | CallbackOp::ConfigIntervalValue
            | CallbackOp::ConfigModeValue
            | CallbackOp::ConfigSourceValue
    )
}

pub(super) async fn config(ctx: &CallbackCtx<'_>) -> ResponseResult<()> {
    let bot = ctx.bot;
    let q = ctx.q;
    let state = ctx.state;
    let lang = ctx.lang;
    let qid = ctx.qid.clone();
    let parts = ctx.parts;

    match ctx.op {
        CallbackOp::Config => {
            // c|cfg|<chat_id>|<ch_page>|<chlist_page> — open settings page
            if let Some(cid) = parts.get(2) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                render_settings_page(
                    bot,
                    PageTarget::Edit(q),
                    cid,
                    state,
                    ch_page,
                    chlist_page,
                    lang,
                )
                .await?;
            }
        }
        CallbackOp::ConfigInterval => {
            if let Some(cid) = parts.get(2) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                render_settings_interval(
                    bot,
                    PageTarget::Edit(q),
                    cid,
                    state,
                    ch_page,
                    chlist_page,
                    lang,
                )
                .await?;
            }
        }
        CallbackOp::ConfigMode => {
            if let Some(cid) = parts.get(2) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                render_settings_mode(
                    bot,
                    PageTarget::Edit(q),
                    cid,
                    state,
                    ch_page,
                    chlist_page,
                    lang,
                )
                .await?;
            }
        }
        CallbackOp::ConfigSource => {
            if let Some(cid) = parts.get(2) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                render_settings_source(
                    bot,
                    PageTarget::Edit(q),
                    cid,
                    state,
                    ch_page,
                    chlist_page,
                    lang,
                )
                .await?;
            }
        }
        CallbackOp::ConfigLinkPreview => {
            // Toggle link preview (chat-level only)
            if let Some(cid) = parts.get(2) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                // Strict baseline read: a transient DB error aborts the toggle
                // with a toast instead of flipping from a wrong default and
                // persisting a contradictory override.
                let cfg = match load_chat_config_checked(&state.db, cid).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(chat = %cid, error = %e, "cfglp: config read failed, toggle aborted");
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "op_failed_retry"))
                            .await
                            .ok();
                        return Ok(());
                    }
                };
                let next = if cfg.link_preview != 0 { "0" } else { "1" };
                match db::set_chat_config_field(&state.db, cid, "link_preview", next).await {
                    Ok(_) => {
                        render_settings_page(
                            bot,
                            PageTarget::Edit(q),
                            cid,
                            state,
                            ch_page,
                            chlist_page,
                            lang,
                        )
                        .await?;
                    }
                    Err(e) => {
                        bot.answer_callback_query(qid.clone())
                            .text(toast_text(
                                i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e)),
                            ))
                            .await
                            .ok();
                    }
                }
            }
        }
        CallbackOp::ConfigCaption => {
            // Toggle caption position (chat-level): Telegram's
            // show_caption_above_media only applies to photo/video/animation.
            if let Some(cid) = parts.get(2) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let cfg = match load_chat_config_checked(&state.db, cid).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(chat = %cid, error = %e, "cfgcp: config read failed, toggle aborted");
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "op_failed_retry"))
                            .await
                            .ok();
                        return Ok(());
                    }
                };
                let next = if cfg.caption_above_media != 0 {
                    "0"
                } else {
                    "1"
                };
                match db::set_chat_config_field(&state.db, cid, "caption_above_media", next).await {
                    Ok(_) => {
                        render_settings_page(
                            bot,
                            PageTarget::Edit(q),
                            cid,
                            state,
                            ch_page,
                            chlist_page,
                            lang,
                        )
                        .await?;
                    }
                    Err(e) => {
                        bot.answer_callback_query(qid.clone())
                            .text(toast_text(
                                i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e)),
                            ))
                            .await
                            .ok();
                    }
                }
            }
        }
        CallbackOp::ResumeAll => {
            // c|resumeall|<chat_id>|<chlist_page>[|<ch_page>] — re-enable all paused sources under this channel in one click
            if let Some(cid) = parts.get(2) {
                let chlist_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let ch_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                match db::resume_all_subscriptions(&state.db, cid).await {
                    Ok(n) => {
                        info!(chat_id = %cid, count = n, "resume-all: subscriptions re-enabled");
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "resumeall_ok").replacen("{}", &n.to_string(), 1))
                            .await
                            .ok();
                        // Keep the source-list page (ch_page) instead of resetting to 0, so the
                        // "enable all" action does not jump the user back to the first page.
                        render_channel_page(
                            bot,
                            PageTarget::Edit(q),
                            cid,
                            state,
                            ch_page,
                            chlist_page,
                            lang,
                        )
                        .await?;
                    }
                    Err(e) => {
                        bot.answer_callback_query(qid.clone())
                            .text(toast_text(
                                i18n::t(lang, "op_failed").replace("{:#}", &format!("{:#}", e)),
                            ))
                            .await
                            .ok();
                    }
                }
            }
        }
        CallbackOp::ConfigIntervalValue
        | CallbackOp::ConfigModeValue
        | CallbackOp::ConfigSourceValue => {
            // c|cfgiv|<chat_id>|<value>|<ch_page>|<chlist_page>
            if let (Some(cid), Some(val)) = (parts.get(2), parts.get(3)) {
                let ch_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(5)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
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
                    let ok = val
                        .parse::<i64>()
                        .map(|n| (1..=crate::db::MAX_INTERVAL_MINUTES).contains(&n))
                        .unwrap_or(false);
                    if !ok {
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "invalid_interval"))
                            .await
                            .ok();
                        return Ok(());
                    }
                }
                match db::set_chat_config_field(&state.db, cid, field, val).await {
                    Ok(_) => {
                        info!(chat_id = %cid, field = field, value = %val, "chat config updated");
                        render_settings_page(
                            bot,
                            PageTarget::Edit(q),
                            cid,
                            state,
                            ch_page,
                            chlist_page,
                            lang,
                        )
                        .await?;
                    }
                    Err(e) => {
                        bot.answer_callback_query(qid.clone())
                            .text(toast_text(
                                i18n::t(lang, "set_failed").replace("{:#}", &format!("{:#}", e)),
                            ))
                            .await
                            .ok();
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn is_source_op(op: CallbackOp) -> bool {
    matches!(
        op,
        CallbackOp::Source
            | CallbackOp::SourcePause
            | CallbackOp::SourceResume
            | CallbackOp::SourceRename
            | CallbackOp::SourceIntervalPage
            | CallbackOp::SourceModePage
            | CallbackOp::SourceSourcePage
            | CallbackOp::SourceCaption
            | CallbackOp::SourceLinkPreview
            | CallbackOp::SourceFilter
            | CallbackOp::SourceFilterToggle
            | CallbackOp::SourceFilterMode
            | CallbackOp::SourceFilterKeywords
            | CallbackOp::SourceIntervalValue
            | CallbackOp::SourceModeValue
            | CallbackOp::SourceSourceValue
    )
}

pub(super) async fn source(ctx: &CallbackCtx<'_>) -> ResponseResult<()> {
    let bot = ctx.bot;
    let q = ctx.q;
    let state = ctx.state;
    let lang = ctx.lang;
    let qid = ctx.qid.clone();
    let parts = ctx.parts;
    let guard_chat_id = ctx.guard_chat_id;

    match ctx.op {
        CallbackOp::Source => {
            // c|src|<sub_id>[|<ch_page>[|<chlist_page>]] — feed (source) page.
            // ch_page is the source-list page on the channel page the user came
            // from; chlist_page is the channel-list page. Both are passed back
            // through the "back to channel" button so a full round trip keeps
            // the user's position.
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                render_feed_page(
                    bot,
                    PageTarget::Edit(q),
                    id,
                    state,
                    ch_page,
                    chlist_page,
                    lang,
                )
                .await?;
            }
        }
        CallbackOp::SourcePause | CallbackOp::SourceResume => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let active = parts[1] == "srcresume";
                // Fail closed when the guard could not resolve an owner (the
                // subscription did not exist at guard time). The update also
                // carries chat_id, so an id recreated between the guard SELECT
                // and this UPDATE cannot be toggled.
                let Some(expected_chat) = guard_chat_id else {
                    bot.answer_callback_query(qid.clone())
                        .text(i18n::t(lang, "sub_not_found"))
                        .await
                        .ok();
                    return Ok(());
                };
                match db::set_subscription_active(&state.db, id, expected_chat, active).await {
                    Ok(true) => {
                        info!(sub_id = id, active = active, "subscription active toggled");
                        render_feed_page(
                            bot,
                            PageTarget::Edit(q),
                            id,
                            state,
                            ch_page,
                            chlist_page,
                            lang,
                        )
                        .await?;
                    }
                    Ok(false) => {
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "sub_not_found"))
                            .await
                            .ok();
                    }
                    Err(e) => {
                        bot.answer_callback_query(qid.clone())
                            .text(toast_text(
                                i18n::t(lang, "op_failed").replace("{:#}", &format!("{:#}", e)),
                            ))
                            .await
                            .ok();
                    }
                }
            }
        }
        CallbackOp::SourceRename => {
            // Ask for a new name via plain text prompt; the reply is handled in
            // commands. Both page positions are stored so the post-rename
            // refresh keeps the channel-list page too.
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(state, bot, qid.clone(), id, lang).await {
                    let chat = q.message.as_ref().map(|m| m.chat().id);
                    // The feed page this button lives on; refreshed in place
                    // after a successful rename.
                    let panel_msg_id = q.message.as_ref().map(|m| m.id().0).unwrap_or(0);
                    if let Some(chat) = chat {
                        let user_id = q.from.id.0 as i64;
                        bot.answer_callback_query(qid.clone()).await.ok();
                        let prompt_text = i18n::t(lang, "rename_prompt").replacen(
                            "{}",
                            sub.feed_title.as_deref().unwrap_or(&sub.feed_url),
                            1,
                        );
                        start_pending_op(
                            bot,
                            state,
                            chat,
                            user_id,
                            prompt_text,
                            false,
                            lang,
                            |prompt_msg_id| crate::state::PendingOp::RenameSub {
                                sub_id: id,
                                prompt_msg_id,
                                panel_msg_id,
                                ch_page,
                                chlist_page,
                                created_at: std::time::Instant::now(),
                            },
                        )
                        .await?;
                    }
                }
            }
        }
        // Second-level feed pages.
        CallbackOp::SourceIntervalPage => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                render_feed_interval(
                    bot,
                    PageTarget::Edit(q),
                    id,
                    state,
                    ch_page,
                    chlist_page,
                    lang,
                )
                .await?;
            }
        }
        CallbackOp::SourceModePage => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                render_feed_mode(
                    bot,
                    PageTarget::Edit(q),
                    id,
                    state,
                    ch_page,
                    chlist_page,
                    lang,
                )
                .await?;
            }
        }
        CallbackOp::SourceSourcePage => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                render_feed_source(
                    bot,
                    PageTarget::Edit(q),
                    id,
                    state,
                    ch_page,
                    chlist_page,
                    lang,
                )
                .await?;
            }
        }
        CallbackOp::SourceCaption => {
            // Toggle feed-level caption position directly (mirrors channel cfgcp).
            // Approach A: when the toggled value equals the channel default, clear the override (marker goes back to 🔗).
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(state, bot, qid.clone(), id, lang).await {
                    let (cfg, chat_cfg) = match (
                        db::get_effective_config(&state.db, &sub.chat_id, &sub.feed_url).await,
                        load_chat_config_checked(&state.db, &sub.chat_id).await,
                    ) {
                        (Ok(cfg), Ok(chat_cfg)) => (cfg, chat_cfg),
                        (Err(e), _) | (_, Err(e)) => {
                            tracing::warn!(sub_id = id, error = %e, "srcfcp: config read failed, toggle aborted");
                            bot.answer_callback_query(qid.clone())
                                .text(i18n::t(lang, "op_failed_retry"))
                                .await
                                .ok();
                            return Ok(());
                        }
                    };
                    let next = if cfg.caption_above_media { "0" } else { "1" };
                    let channel_default = if chat_cfg.caption_above_media != 0 {
                        "1"
                    } else {
                        "0"
                    };
                    let value: Option<&str> = if next == channel_default {
                        None
                    } else {
                        Some(next)
                    };
                    match db::set_feed_config_field(
                        &state.db,
                        &sub.chat_id,
                        &sub.feed_url,
                        "caption_above_media",
                        value,
                    )
                    .await
                    {
                        Ok(_) => {
                            info!(
                                sub_id = id,
                                caption_above_media = next,
                                "feed caption position toggled"
                            );
                            render_feed_page(
                                bot,
                                PageTarget::Edit(q),
                                id,
                                state,
                                ch_page,
                                chlist_page,
                                lang,
                            )
                            .await?;
                        }
                        Err(e) => {
                            bot.answer_callback_query(qid.clone())
                                .text(toast_text(
                                    i18n::t(lang, "set_failed")
                                        .replace("{:#}", &format!("{:#}", e)),
                                ))
                                .await
                                .ok();
                        }
                    }
                }
            }
        }
        CallbackOp::SourceLinkPreview => {
            // Toggle feed-level link preview directly (mirrors channel cfglp).
            // Approach A: when the toggled value equals the channel default, clear the override (marker goes back to 🔗).
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(state, bot, qid.clone(), id, lang).await {
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
                            bot.answer_callback_query(qid.clone())
                                .text(i18n::t(lang, "op_failed_retry"))
                                .await
                                .ok();
                            return Ok(());
                        }
                    };
                    let next = if cfg.link_preview { "0" } else { "1" };
                    let channel_default = if chat_cfg.link_preview != 0 { "1" } else { "0" };
                    let value: Option<&str> = if next == channel_default {
                        None
                    } else {
                        Some(next)
                    };
                    match db::set_feed_config_field(
                        &state.db,
                        &sub.chat_id,
                        &sub.feed_url,
                        "link_preview",
                        value,
                    )
                    .await
                    {
                        Ok(_) => {
                            info!(
                                sub_id = id,
                                link_preview = next,
                                "feed link preview toggled"
                            );
                            render_feed_page(
                                bot,
                                PageTarget::Edit(q),
                                id,
                                state,
                                ch_page,
                                chlist_page,
                                lang,
                            )
                            .await?;
                        }
                        Err(e) => {
                            bot.answer_callback_query(qid.clone())
                                .text(toast_text(
                                    i18n::t(lang, "set_failed")
                                        .replace("{:#}", &format!("{:#}", e)),
                                ))
                                .await
                                .ok();
                        }
                    }
                }
            }
        }
        CallbackOp::SourceFilter => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                render_feed_filter(
                    bot,
                    PageTarget::Edit(q),
                    id,
                    state,
                    ch_page,
                    chlist_page,
                    lang,
                )
                .await?;
            }
        }
        CallbackOp::SourceFilterToggle => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(state, bot, qid.clone(), id, lang).await {
                    // Strict baseline read (same contract as cfglp/srcflp): a
                    // transient DB error must abort the toggle instead of
                    // flipping from the FilterConfig::default() and persisting
                    // a state that contradicts what the user actually set.
                    let fc = match db::get_feed_filter(&state.db, &sub.chat_id, &sub.feed_url).await
                    {
                        Ok(fc) => fc,
                        Err(e) => {
                            tracing::warn!(sub_id = id, error = %e, "srcftoggle: filter read failed, toggle aborted");
                            bot.answer_callback_query(qid.clone())
                                .text(i18n::t(lang, "op_failed_retry"))
                                .await
                                .ok();
                            return Ok(());
                        }
                    };
                    match db::set_feed_filter_enabled(
                        &state.db,
                        &sub.chat_id,
                        &sub.feed_url,
                        !fc.enabled,
                    )
                    .await
                    {
                        Ok(_) => {
                            render_feed_filter(
                                bot,
                                PageTarget::Edit(q),
                                id,
                                state,
                                ch_page,
                                chlist_page,
                                lang,
                            )
                            .await?;
                        }
                        Err(e) => {
                            bot.answer_callback_query(qid.clone())
                                .text(toast_text(
                                    i18n::t(lang, "set_failed")
                                        .replace("{:#}", &format!("{:#}", e)),
                                ))
                                .await
                                .ok();
                        }
                    }
                }
            }
        }
        CallbackOp::SourceFilterMode => {
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(state, bot, qid.clone(), id, lang).await {
                    // Strict baseline read (see srcftoggle): abort on a DB
                    // error rather than persisting a mode flipped from a
                    // wrong default.
                    let fc = match db::get_feed_filter(&state.db, &sub.chat_id, &sub.feed_url).await
                    {
                        Ok(fc) => fc,
                        Err(e) => {
                            tracing::warn!(sub_id = id, error = %e, "srcfmode: filter read failed, toggle aborted");
                            bot.answer_callback_query(qid.clone())
                                .text(i18n::t(lang, "op_failed_retry"))
                                .await
                                .ok();
                            return Ok(());
                        }
                    };
                    let next = if fc.mode == "whitelist" {
                        "blacklist"
                    } else {
                        "whitelist"
                    };
                    match db::set_feed_filter_mode(&state.db, &sub.chat_id, &sub.feed_url, next)
                        .await
                    {
                        Ok(_) => {
                            render_feed_filter(
                                bot,
                                PageTarget::Edit(q),
                                id,
                                state,
                                ch_page,
                                chlist_page,
                                lang,
                            )
                            .await?;
                        }
                        Err(e) => {
                            bot.answer_callback_query(qid.clone())
                                .text(toast_text(
                                    i18n::t(lang, "set_failed")
                                        .replace("{:#}", &format!("{:#}", e)),
                                ))
                                .await
                                .ok();
                        }
                    }
                }
            }
        }
        CallbackOp::SourceFilterKeywords => {
            // Open keyword-input flow (plain text prompt; reply handled in commands)
            if let Some(id) = parts.get(2).and_then(|s| s.parse::<i64>().ok()) {
                let ch_page = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                if let Some(sub) = fetch_sub_or_toast(state, bot, qid.clone(), id, lang).await {
                    let chat = q.message.as_ref().map(|m| m.chat().id);
                    let panel_msg_id = q.message.as_ref().map(|m| m.id().0).unwrap_or(0);
                    if let Some(chat) = chat {
                        let user_id = q.from.id.0 as i64;
                        // A failed read must abort the flow: showing the
                        // default "(none)" baseline would make the user's
                        // reply overwrite the real keywords.
                        let fc = match db::get_feed_filter(&state.db, &sub.chat_id, &sub.feed_url)
                            .await
                        {
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
                        start_pending_op(
                            bot,
                            state,
                            chat,
                            user_id,
                            prompt_text,
                            true,
                            lang,
                            |prompt_msg_id| crate::state::PendingOp::SetFilterKeywords {
                                sub_id: id,
                                prompt_msg_id,
                                panel_msg_id,
                                ch_page,
                                chlist_page,
                                created_at: std::time::Instant::now(),
                            },
                        )
                        .await?;
                    }
                }
            }
        }
        CallbackOp::SourceIntervalValue
        | CallbackOp::SourceModeValue
        | CallbackOp::SourceSourceValue => {
            // Feed-level overrides: c|srcfi|<sub_id>|<value>[|<ch_page>] etc.
            // A value of "inherit" clears the override (NULL → fall back to channel).
            // Approach A: when the selected value equals the channel default, the override is
            // cleared automatically (marker goes back to 🔗); an override record (marker ✏️) is
            // only written when the value truly differs from the channel.
            if let (Some(id), Some(val)) = (
                parts.get(2).and_then(|s| s.parse::<i64>().ok()),
                parts.get(3),
            ) {
                let ch_page = parts
                    .get(4)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let chlist_page = parts
                    .get(5)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                // Range guard mirroring the chat-level cfgiv path: a forged or
                // edge callback with a 0/negative interval makes the due check
                // always true, hammering the source every tick (rate-limit /
                // IP-ban risk). "inherit" clears the override and is allowed.
                if parts[1] == "srcfi" && *val != "inherit" {
                    let ok = val
                        .parse::<i64>()
                        .map(|n| (1..=crate::db::MAX_INTERVAL_MINUTES).contains(&n))
                        .unwrap_or(false);
                    if !ok {
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "invalid_interval"))
                            .await
                            .ok();
                        return Ok(());
                    }
                }
                if let Some(sub) = fetch_sub_or_toast(state, bot, qid.clone(), id, lang).await {
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
                            bot.answer_callback_query(qid.clone())
                                .text(i18n::t(lang, "op_failed_retry"))
                                .await
                                .ok();
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
                    match db::set_feed_config_field(
                        &state.db,
                        &sub.chat_id,
                        &sub.feed_url,
                        field,
                        value,
                    )
                    .await
                    {
                        Ok(_) => {
                            info!(sub_id = id, field = field, value = %val, "feed config updated");
                            // Return to the feed (source) page after a selection.
                            render_feed_page(
                                bot,
                                PageTarget::Edit(q),
                                id,
                                state,
                                ch_page,
                                chlist_page,
                                lang,
                            )
                            .await?;
                        }
                        Err(e) => {
                            bot.answer_callback_query(qid.clone())
                                .text(toast_text(
                                    i18n::t(lang, "set_failed")
                                        .replace("{:#}", &format!("{:#}", e)),
                                ))
                                .await
                                .ok();
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}
