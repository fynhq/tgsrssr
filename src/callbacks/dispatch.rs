//! Callback config/source operations.

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
    let args = ctx.args;

    match ctx.op {
        CallbackOp::Config => {
            if let Some(cid) = args.id {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
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
            if let Some(cid) = args.id {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
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
            if let Some(cid) = args.id {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
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
            if let Some(cid) = args.id {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
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
            if let Some(cid) = args.id {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
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
            if let Some(cid) = args.id {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
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
            if let Some(cid) = args.id {
                let chlist_page = args.chlist_page;
                let ch_page = args.ch_page;
                match db::resume_all_subscriptions(&state.db, cid).await {
                    Ok(n) => {
                        info!(chat_id = %cid, count = n, "resume-all: subscriptions re-enabled");
                        bot.answer_callback_query(qid.clone())
                            .text(i18n::t(lang, "resumeall_ok").replacen("{}", &n.to_string(), 1))
                            .await
                            .ok();
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
            if let (Some(cid), Some(val)) = (args.id, args.value) {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
                let field = match ctx.op {
                    CallbackOp::ConfigIntervalValue => "interval_minutes",
                    CallbackOp::ConfigModeValue => "content_mode",
                    _ => "source_display",
                };
                // Clamp forged/edge intervals: 0 or negative makes the due check always true (rate-limit / IP-ban risk).
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
    let args = ctx.args;
    let guard_chat_id = ctx.guard_chat_id;

    match ctx.op {
        CallbackOp::Source => {
            if let Some(id) = args.sub_id() {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
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
            if let Some(id) = args.sub_id() {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
                let active = ctx.op == CallbackOp::SourceResume;
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
            if let Some(id) = args.sub_id() {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
                if let Some(sub) = fetch_sub_or_toast(state, bot, qid.clone(), id, lang).await {
                    let chat = q.message.as_ref().map(|m| m.chat().id);
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
        CallbackOp::SourceIntervalPage => {
            if let Some(id) = args.sub_id() {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
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
            if let Some(id) = args.sub_id() {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
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
            if let Some(id) = args.sub_id() {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
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
            if let Some(id) = args.sub_id() {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
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
            if let Some(id) = args.sub_id() {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
                if let Some(sub) = fetch_sub_or_toast(state, bot, qid.clone(), id, lang).await {
                    // A transient DB read error must abort a toggle; toggling from the default persists a wrong override.
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
            if let Some(id) = args.sub_id() {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
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
            if let Some(id) = args.sub_id() {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
                if let Some(sub) = fetch_sub_or_toast(state, bot, qid.clone(), id, lang).await {
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
            if let Some(id) = args.sub_id() {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
                if let Some(sub) = fetch_sub_or_toast(state, bot, qid.clone(), id, lang).await {
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
            if let Some(id) = args.sub_id() {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
                if let Some(sub) = fetch_sub_or_toast(state, bot, qid.clone(), id, lang).await {
                    let chat = q.message.as_ref().map(|m| m.chat().id);
                    let panel_msg_id = q.message.as_ref().map(|m| m.id().0).unwrap_or(0);
                    if let Some(chat) = chat {
                        let user_id = q.from.id.0 as i64;
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
            if let (Some(id), Some(val)) = (args.sub_id(), args.value) {
                let ch_page = args.ch_page;
                let chlist_page = args.chlist_page;
                if ctx.op == CallbackOp::SourceIntervalValue && val != "inherit" {
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
                    let field = match ctx.op {
                        CallbackOp::SourceIntervalValue => "interval_minutes",
                        CallbackOp::SourceModeValue => "content_mode",
                        _ => "source_display",
                    };
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
                    let value: Option<&str> = if val == "inherit" || val == channel_default {
                        None
                    } else {
                        Some(val)
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
