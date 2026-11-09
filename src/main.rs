mod callbacks;
mod cleanup;
mod commands;
mod config;
mod db;
mod entrymeta;
mod errors;
mod fetcher;
mod htmlentities;
mod htmltext;
mod httpfollow;
mod i18n;
mod langpref;
mod limits;
mod logging;
mod media;
mod notify;
mod platforms;
mod push;
mod richmsg;
mod scheduler;
mod sendpolicy;
mod ssrf;
mod state;
mod ui;

use teloxide::dispatching::UpdateFilterExt;
use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, ChatMemberUpdated, Message, Update};
use teloxide::utils::command::BotCommands;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{error, info};

use commands::Command;
use config::Config;
use i18n::Lang;
use state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load config first (log_dir comes from config)
    let cfg = Config::from_env()?;

    // Initialize logging (console + daily rotating file). Guard must live for program lifetime.
    let _log_guard = logging::init_logging(&cfg.log_dir)?;

    info!("starting tgsrssr v{}", commands::VERSION);

    // Network/SSRF posture (one line each).
    if cfg.rss_proxy.is_some() {
        info!(
            "RSS proxy enabled: SSRF veto allows a local DNS lookup failure to proceed via the proxy; platform APIs skip local DNS checks behind the proxy. Scope the proxy ACL accordingly."
        );
    }
    // Bot API and RSS proxies are independent. A custom/local Bot API endpoint
    // is contacted directly; its own connectivity to Telegram is out of scope
    // for BOT_PROXY.
    let bot_proxy = if cfg.bot_api_url.is_some() {
        if cfg.bot_proxy.is_some() {
            tracing::warn!(
                "BOT_PROXY ignored because BOT_API_URL is set; the custom Bot API endpoint is contacted directly"
            );
        }
        None
    } else {
        cfg.bot_proxy.clone()
    };
    if bot_proxy.is_some() {
        info!("Bot API proxy enabled (BOT_PROXY); RSS traffic keeps using RSS_PROXY/RSS_NO_PROXY");
    }
    if !cfg.rss_trusted_private_hosts.is_empty() {
        tracing::warn!(
            hosts = ?cfg.rss_trusted_private_hosts,
            scope = ?cfg.rss_trusted_private_scope,
            "trusted private feed hosts enabled: an explicit private-network bypass for the admin/trusted users, NOT a general private allowlist"
        );
    }

    // Ensure media directories exist
    std::fs::create_dir_all(cfg.media_dir.join("outbound"))?;

    // Connect to database
    let db_pool = db::connect(&cfg.db_path).await?;
    info!(db = %cfg.db_path.display(), "database connected");
    match db::normalize_telegram_entry_ids(&db_pool).await {
        Ok(0) => {}
        Ok(rows) => info!(rows, "normalized legacy Telegram entry ids"),
        Err(e) => tracing::warn!(error = %e, "legacy Telegram entry id normalization failed; first fetch may re-push the visible window"),
    }

    // Build bot (optionally pointing at a local Bot API server).
    // Custom reqwest client with a long timeout: media uploads through the
    // local Bot API (proxy → Telegram) can take minutes; teloxide's default
    // 30s client timeout was cutting SendMediaGroup off mid-upload.
    // 10 minutes: gives large media a fair shot without hanging forever.
    let http_client = build_bot_client(bot_proxy.as_deref(), std::time::Duration::from_secs(600))?;
    let mut bot = Bot::with_client(&cfg.token, http_client);
    if let Some(api_url) = &cfg.bot_api_url {
        let url = reqwest::Url::parse(api_url)?;
        bot = bot.set_api_url(url);
        info!(api_url = %api_url, "using custom Bot API endpoint");
    }

    // Wait for Bot API readiness and capture bot username (needed for command parsing).
    // Use a dedicated short-timeout client: the main client's 600s timeout
    // exists for large media uploads and would make a dead endpoint hang
    // startup for ten minutes per probe.
    let probe_client = build_bot_client(bot_proxy.as_deref(), std::time::Duration::from_secs(15))?;
    let mut probe_bot = Bot::with_client(&cfg.token, probe_client);
    if let Some(api_url) = &cfg.bot_api_url {
        probe_bot = probe_bot.set_api_url(reqwest::Url::parse(api_url)?);
    }
    let bot_username = wait_for_bot_api(&probe_bot).await?;

    // Shared state
    let state = AppState::new(cfg.clone(), db_pool.clone(), bot_username)?;

    // Load persisted platform cookies (set via /cookies) into the runtime
    // store. Cookies come only from the /cookies command (persisted in the
    // settings table); there is no env injection.
    // Migrate legacy per-user cookies to the global format first, then load
    // (the migration converts old keys into new keys).
    platforms::migrate_legacy_cookies(&state).await;
    platforms::load_persisted_cookies(&state).await;

    // Startup cleanup: delete legacy global platform_health/streak keys from
    // before v1.0.0 that lack a user_id segment (two-segment format). Since
    // v1.11.0 these keys are back to the two-segment global format, but the
    // platform-name segment of old keys may differ from current platform
    // ids, and migrate_legacy_cookies already cleans up three-segment
    // per-user keys; this only sweeps up leftover two-segment orphan keys
    // (those whose platform segment is not a known platform id).
    {
        let known_platforms: Vec<&str> = platforms::PLATFORMS.iter().map(|p| p.id).collect();
        for prefix in ["platform_health:", "platform_streak:", "cookie_expired_notified:"] {
            let rows = match db::get_settings_by_prefix(&db_pool, prefix).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(prefix = %prefix, error = %e, "orphan-key cleanup: prefix query failed, skipping");
                    continue;
                }
            };
            for (key, _) in &rows {
                let rest = &key[prefix.len()..];
                // Two-segment keys (prefix:<platform>) are the v1.11.0 global
                // format; if the platform segment is not a known platform id,
                // treat it as a historical leftover and delete it. Three-segment
                // keys (containing user_id) are cleaned up by the migration.
                if !rest.contains(':') && !known_platforms.contains(&rest) {
                    db::delete_setting(&db_pool, key).await.ok();
                    info!(key = %key, "removed orphan settings key");
                }
            }
        }
    }

    // Startup cleanup
    cleanup::run_startup_cleanup(&cfg, &db_pool).await;
    // Cleanup may have freed media since any earlier scan; make the first
    // download reserve re-read the directory.
    state.media_budget.invalidate();

    // Register bot command menu (/start remains triggerable but is hidden from the menu)
    // Note: commands generated by teloxide 0.17 derive carry a "/" prefix,
    // so both spellings must be handled.
    // Tiered menu (v1.11.0): /cookies and /loget are admin-only; the default
    // menu hides these two items from regular users, and the admin gets the
    // full menu registered separately via a Chat scope.
    let all_cmds: Vec<teloxide::types::BotCommand> = Command::bot_commands()
        .into_iter()
        .filter(|c| !is_hidden_from_menu(&c.command))
        .collect();
    let user_cmds: Vec<teloxide::types::BotCommand> = all_cmds
        .iter()
        .filter(|c| !is_admin_only_command(&c.command))
        .cloned()
        .collect();
    // Per-language menus: Telegram keys the command menu on the language each
    // client reports. The derive literals are English and are registered with
    // no language_code, which is the fallback for every client whose language
    // has no dedicated list. Every other `Lang` registers its two-letter code;
    // the Bot API rejects subtag forms like `zh-hans` ("invalid language
    // code"), so a single `zh` list serves both scripts. Descriptions come
    // from locales via `commands::menu_desc`.
    //
    // Commands are private-chat only, so the user menu is registered under
    // AllPrivateChats and stale group/default menus are deleted below. The
    // admin's full menu still uses a Chat scope (not ChatMember, which
    // Telegram rejects in private chats; in a private chat chat_id == user_id,
    // so this targets the admin's DM).
    let admin_scope = teloxide::types::BotCommandScope::Chat {
        chat_id: teloxide::types::Recipient::Id(teloxide::types::ChatId(cfg.admin_user_id)),
    };
    let private_scope = teloxide::types::BotCommandScope::AllPrivateChats;
    for (scope_desc, cmds, scope) in [
        ("private", &user_cmds, private_scope),
        ("admin", &all_cmds, admin_scope),
    ] {
        for (lang, list) in menu_variants(cmds) {
            let mut req = bot.set_my_commands(list).scope(scope.clone());
            if let Some(l) = &lang {
                req = req.language_code(l.clone());
            }
            if let Err(e) = req.await {
                error!(
                    menu = scope_desc,
                    lang = %lang.unwrap_or_else(|| "en".into()),
                    error = %e,
                    "failed to set bot command menu"
                );
            }
        }
    }
    // Remove menus inherited by groups/channels from older versions (the
    // default scope is also cleared; private users are covered by the
    // AllPrivateChats registrations above). Clear each language-specific
    // menu as well: an older version may have registered it under the
    // default scope, and deleting only the language-less list would leave
    // those clients seeing the old menu in groups.
    for (scope_desc, scope) in [
        ("groups", teloxide::types::BotCommandScope::AllGroupChats),
        ("default", teloxide::types::BotCommandScope::Default),
    ] {
        for lang in menu_language_codes().map(Some).chain([None]) {
            let mut req = bot.delete_my_commands().scope(scope.clone());
            if let Some(code) = lang {
                req = req.language_code(code);
            }
            if let Err(e) = req.await {
                error!(
                    menu = scope_desc,
                    lang = lang.unwrap_or("en"),
                    error = %e,
                    "failed to clear bot command menu"
                );
            }
        }
    }

    // Scheduler: fetch job + cleanup job + auto-heal job
    let mut sched = JobScheduler::new().await?;
    scheduler::register_fetch_job(&sched, state.clone(), bot.clone()).await?;
    scheduler::register_heal_job(&sched, state.clone()).await?;
    register_cleanup_job(&sched, state.clone()).await?;
    scheduler::register_outbox_retry_job(&sched, state.clone(), bot.clone()).await?;
    register_pending_ops_sweep_job(&sched, state.clone(), bot.clone()).await?;
    sched.start().await?;
    info!("scheduler started");

    // ===== Dispatching handler tree =====
    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .branch(dptree::endpoint(handle_message)),
        )
        .branch(
            Update::filter_callback_query()
                .branch(dptree::endpoint(handle_callback_query)),
        )
        .branch(
            Update::filter_my_chat_member()
                .branch(dptree::endpoint(handle_my_chat_member_update)),
        );

    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .build();

    // Graceful shutdown for both SIGINT (Ctrl-C) and SIGTERM (`docker stop`,
    // systemd). As container PID 1 the process would otherwise ignore SIGTERM
    // and Docker would SIGKILL it after its 10s grace period. Driving the
    // dispatcher shutdown token lets worker tasks and in-flight handlers
    // finish before the scheduler is stopped.
    let shutdown_token = dispatcher.shutdown_token();
    let dispatch = dispatcher.dispatch();
    tokio::pin!(dispatch);
    tokio::select! {
        _ = &mut dispatch => {} // update listener ended on its own
        signal = wait_for_shutdown_signal() => {
            info!(signal, "shutdown signal received, stopping dispatcher");
            loop {
                match shutdown_token.shutdown() {
                    Ok(done) => {
                        tokio::join!(done, &mut dispatch);
                        break;
                    }
                    // Dispatcher is still starting up (its get_me call) or
                    // already idle: wait for it to start or finish, then retry.
                    Err(_) => tokio::select! {
                        _ = &mut dispatch => break,
                        _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
                    },
                }
            }
        }
    }

    // Dispatcher returns on shutdown or a fatal update-stream error. The
    // JobScheduler's ticker is an independent task and keeps firing cron jobs
    // (fetch/outbox notifications included) unless explicitly shut down, so
    // stop it before exiting rather than relying on process teardown.
    info!("shutting down scheduler");
    if let Err(e) = sched.shutdown().await {
        tracing::warn!(error = %e, "scheduler shutdown returned an error");
    }

    Ok(())
}

/// Builds a Bot API HTTP client with an explicit proxy decision.
///
/// BOT_PROXY is the only proxy setting for Bot API traffic: `None` disables
/// system proxy environment variables, so an unset BOT_PROXY always means a
/// direct connection. RSS clients apply the same explicit pattern in
/// `AppState::new`.
fn build_bot_client(
    proxy: Option<&str>,
    timeout: std::time::Duration,
) -> anyhow::Result<reqwest_bot::Client> {
    let builder = reqwest_bot::Client::builder().timeout(timeout);
    let builder = match proxy {
        Some(p) => {
            let proxy = reqwest_bot::Proxy::all(p).map_err(|e| {
                anyhow::anyhow!(
                    "invalid BOT_PROXY (expected http://, socks5:// or socks5h://): {e}"
                )
            })?;
            builder.proxy(proxy)
        }
        None => builder.no_proxy(),
    };
    Ok(builder.build()?)
}

/// Resolves when the process is asked to stop, returning which signal arrived.
///
/// SIGTERM is not covered by teloxide's built-in Ctrl-C handler, and the kernel
/// does not apply the default terminate action to PID 1 (the container init),
/// so without an explicit handler `docker stop` would wait out the grace period
/// and SIGKILL the process.
async fn wait_for_shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => tokio::select! {
                _ = tokio::signal::ctrl_c() => "SIGINT",
                _ = sigterm.recv() => "SIGTERM",
            },
            Err(e) => {
                tracing::warn!(error = %e, "failed to install SIGTERM handler, only Ctrl-C will stop the bot");
                let _ = tokio::signal::ctrl_c().await;
                "SIGINT"
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "SIGINT"
    }
}

/// Message handler: pending multi-step ops first, then commands; ignore the rest.
async fn handle_message(bot: Bot, msg: Message, state: AppState) -> ResponseResult<()> {
    // Group -> supergroup upgrade: Telegram posts a service message carrying
    // the new chat id in the old group. Move subscriptions/configs/sent
    // records before any command handling; otherwise the old chat id remains
    // bound and every push fails after the upgrade.
    if let Some(new_chat_id) = msg.migrate_to_chat_id() {
        let old_id = msg.chat.id.0.to_string();
        let new_id = new_chat_id.0.to_string();
        match db::migrate_chat_id(&state.db, &old_id, &new_id).await {
            Ok(()) => info!(old_chat_id = %old_id, new_chat_id = %new_id, "chat id migrated after group upgrade"),
            Err(e) => error!(old_chat_id = %old_id, new_chat_id = %new_id, error = format!("{:#}", e), "chat id migration failed"),
        }
        return Ok(());
    }

    let text = msg.text().unwrap_or_default();

    // Pending op (e.g. rename reply) consumes plain-text messages.
    let chat_id = msg.chat.id.0;
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    let pending = state.pending_ops.lock().await.remove(&(chat_id, user_id));
    if let Some(op) = pending {
        // Resolve user language for prompt retirement messages.
        let lang = commands::user_lang(&state, user_id, Some(&msg)).await;
        // Whitelist re-check: a user removed from the whitelist after
        // starting a flow no longer has their replies consumed.
        if !state.config.is_allowed(user_id) {
            crate::commands::retire_prompt(&bot, msg.chat.id, op.prompt_msg_id(), i18n::t(lang, "cancelled")).await;
            return Ok(());
        }
        // Expired pending ops (user walked away): clean up the stale prompt
        // and process the message normally instead of swallowing it.
        let expired = op.created_at().elapsed() > crate::state::PENDING_OP_TIMEOUT;
        // #14: receiving a command mid-flow → cancel the flow and proceed
        // with the command (don't swallow it).
        let is_command = Command::parse(text, &state.bot_username).is_ok();
        // Forwarded channel messages should go through the implicit /find,
        // not be treated as rename/cookie/keyword input.
        let is_channel_forward = matches!(
            msg.forward_origin(),
            Some(teloxide::types::MessageOrigin::Channel { .. })
        );
        if expired {
            let prompt_msg_id = op.prompt_msg_id();
            // Retire the stale prompt (edit → status) instead of deleting,
            // so the client's reply-draft target stays alive.
            crate::commands::retire_prompt(&bot, msg.chat.id, prompt_msg_id, i18n::t(lang, "timeout_cancelled")).await;
            // Fall through to normal handling below.
        } else if is_command {
            let prompt_msg_id = op.prompt_msg_id();
            crate::commands::retire_prompt(&bot, msg.chat.id, prompt_msg_id, i18n::t(lang, "cancelled")).await;
            // Fall through to command handling below.
        } else if is_channel_forward {
            let prompt_msg_id = op.prompt_msg_id();
            crate::commands::retire_prompt(&bot, msg.chat.id, prompt_msg_id, i18n::t(lang, "cancelled")).await;
            // Fall through to implicit /find handling below.
        } else {
            return commands::handle_pending_op(&bot, &msg, &state, op, text).await;
        }
    }

    match Command::parse(text, &state.bot_username) {
        Ok(cmd) => commands::handle_command(bot, msg, cmd, state).await,
        Err(_) => {
            // Forwarding a channel message directly to the bot (without a
            // command) = implicit /find: no need to copy-paste the message
            // link, forwarding is the query.
            // Whitelist check: consistent with the command-style /find —
            // non-whitelisted users are fully ignored silently.
            if state.config.is_allowed(user_id)
                && msg.chat.is_private()
                && matches!(
                    msg.forward_origin(),
                    Some(teloxide::types::MessageOrigin::Channel { .. })
                )
            {
                let lang = commands::user_lang(&state, user_id, Some(&msg)).await;
                return commands::handle_find(&bot, &msg, &state, "", lang).await;
            }
            Ok(())
        }
    }
}

/// Callback query handler.
async fn handle_callback_query(bot: Bot, q: CallbackQuery, state: AppState) -> ResponseResult<()> {
    callbacks::handle_callback(bot, q, state).await
}

/// My-chat-member handler (auto-register / purge on removal).
async fn handle_my_chat_member_update(mcm: ChatMemberUpdated, state: AppState) -> ResponseResult<()> {
    callbacks::handle_my_chat_member(mcm, state).await
}

/// Poll getMe until the Bot API is reachable (handles slow local Bot API startup).
/// Returns the bot username.
async fn wait_for_bot_api(bot: &Bot) -> anyhow::Result<String> {
    for attempt in 1..=30 {
        match bot.get_me().await {
            Ok(me) => {
                let username = me.username.clone().unwrap_or_default();
                info!(username = %username, "bot api ready");
                return Ok(username);
            }
            Err(e) => {
                info!(attempt = attempt, error = %e, "bot api not ready, retrying");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
    anyhow::bail!("Bot API not reachable after 30 attempts");
}

/// Fixed periodic cleanup interval.
const CLEANUP_INTERVAL_MINUTES: u64 = 60;

/// Register the periodic cleanup job.
async fn register_cleanup_job(sched: &JobScheduler, state: AppState) -> anyhow::Result<()> {
    let interval = CLEANUP_INTERVAL_MINUTES;
    // Tick every minute and gate with the fixed interval. The gate keeps the
    // first run anchored to process start rather than the wall clock.
    let (expr, gate_secs) = (
        "0 * * * * *".to_string(),
        Some(interval.saturating_mul(60)),
    );
    // Startup cleanup already ran; count it as the last run so a long
    // interval is honored from process start.
    let last_run = std::sync::Arc::new(std::sync::Mutex::new(Some(std::time::Instant::now())));

    sched
        .add(Job::new_async(expr.as_str(), move |_uuid, _l| {
            let state = state.clone();
            let last_run = last_run.clone();
            Box::pin(async move {
                if let Some(gate_secs) = gate_secs {
                    let now = std::time::Instant::now();
                    let mut last = match last_run.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    if last.is_some_and(|prev| now.duration_since(prev).as_secs() < gate_secs) {
                        return;
                    }
                    *last = Some(now);
                    drop(last);
                }
                // Reentrancy protection: a single cleanup round (scanning
                // multi-GB media dirs + quota + backup) can outlive the
                // trigger interval; overlapping rounds waste IO. Mirrors the
                // AtomicBool pattern used by fetch/outbox.
                if state
                    .cleanup_active
                    .compare_exchange(
                        false,
                        true,
                        std::sync::atomic::Ordering::Acquire,
                        std::sync::atomic::Ordering::Acquire,
                    )
                    .is_err()
                {
                    return;
                }
                let _guard = crate::state::ActiveFlagGuard(state.cleanup_active.clone());
                let cfg = (*state.config).clone();
                cleanup::run_all_cleanup(&cfg, &state.db).await;
                state.media_budget.invalidate();
            })
        })?)
        .await?;

    info!(interval_min = interval, "cleanup job registered");
    Ok(())
}

/// Register the pending-ops sweeper: periodically expires abandoned multi-step
/// flows (rename / cookie / filter-keyword). Without this, a user who walks
/// away mid-flow leaves the map entry and the prompt message lingering
/// forever (the lazy check only fires on that user's next message).
async fn register_pending_ops_sweep_job(
    sched: &JobScheduler,
    state: AppState,
    bot: Bot,
) -> anyhow::Result<()> {
    // Every 5 minutes: cheap scan, retires any prompt older than the timeout.
    let expr = "0 */5 * * * *";
    sched
        .add(Job::new_async(expr, move |_uuid, _l| {
            let state = state.clone();
            let bot = bot.clone();
            Box::pin(async move {
                let mut ops = state.pending_ops.lock().await;
                let expired: Vec<((i64, i64), crate::state::PendingOp)> = ops
                    .iter()
                    .filter(|(_, op)| op.created_at().elapsed() > crate::state::PENDING_OP_TIMEOUT)
                    .map(|(k, op)| (*k, op.clone()))
                    .collect();
                for (key, _) in &expired {
                    ops.remove(key);
                }
                drop(ops);
                for ((chat_id, user_id), op) in expired {
                    // Background sweep: no client language_code available, so
                    // resolve via the shared helper with None (DB preference
                    // first, English otherwise).
                    let lang = langpref::resolve_lang(&state, user_id, None).await;
                    crate::commands::retire_prompt(
                        &bot,
                        teloxide::types::ChatId(chat_id),
                        op.prompt_msg_id(),
                        i18n::t(lang, "timeout_cancelled"),
                    )
                    .await;
                }
            })
        })?)
        .await?;

    info!("pending-ops sweep job registered");
    Ok(())
}

/// Commands hidden from the command menu entirely (still triggerable).
/// teloxide 0.17 derive emits command names with a "/" prefix, but match
/// both spellings to stay robust across versions.
fn is_hidden_from_menu(cmd: &str) -> bool {
    matches!(cmd.trim_start_matches('/'), "start")
}

/// Admin-only commands excluded from the default (non-admin) menu.
fn is_admin_only_command(cmd: &str) -> bool {
    matches!(cmd.trim_start_matches('/'), "cookies" | "loget")
}

/// Telegram language codes that have a dedicated command-menu translation.
/// English is the language-less fallback, so it is excluded. The Bot API
/// accepts only two-letter ISO 639-1 codes here and rejects subtag forms such
/// as `zh-hans` ("invalid language code specified", verified against a live
/// server) — Telegram normalizes Simplified and Traditional Chinese clients
/// onto the single `zh` list.
fn menu_language_codes() -> impl Iterator<Item = &'static str> {
    Lang::ALL
        .into_iter()
        .filter(|lang| *lang != Lang::En)
        .map(Lang::db_value)
}

/// One menu variant: `(language_code, commands)`. `language_code = None`
/// means "no dedicated language" — the fallback list every client uses when
/// its reported language has no registered list.
type MenuVariant = (Option<String>, Vec<teloxide::types::BotCommand>);

/// Build the per-language menu variants for one command list: the English
/// base (registered with no language code, acting as the fallback) plus one
/// translated copy per non-English `Lang`. The command order is preserved;
/// only descriptions are replaced. An unknown command name keeps its English
/// description rather than being dropped.
fn menu_variants(cmds: &[teloxide::types::BotCommand]) -> Vec<MenuVariant> {
    let mut out = Vec::with_capacity(Lang::ALL.len());
    out.push((None, cmds.to_vec()));
    for lang in Lang::ALL {
        if lang == Lang::En {
            continue;
        }
        let localized = cmds
            .iter()
            .map(|c| {
                let desc = commands::menu_desc(lang, &c.command).unwrap_or(c.description.as_str());
                teloxide::types::BotCommand::new(c.command.clone(), desc.to_string())
            })
            .collect();
        out.push((Some(lang.db_value().to_string()), localized));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_variants_cover_en_fallback_and_every_translation() {
        let base = vec![
            teloxide::types::BotCommand::new("/set", "Subscriptions"),
            teloxide::types::BotCommand::new("/find", "Find message source"),
        ];
        let variants = menu_variants(&base);
        // English fallback + one translated list per non-English Lang.
        assert_eq!(variants.len(), Lang::ALL.len());

        // English fallback carries the derive descriptions untouched.
        let (lang, en) = &variants[0];
        assert!(lang.is_none());
        assert_eq!(en[0].description, "Subscriptions");

        // Every non-English language registers its two-letter code and
        // translates every command; the command order/prefix is preserved.
        for option in Lang::ALL {
            if option == Lang::En {
                continue;
            }
            let (code, localized) = variants
                .iter()
                .find(|(code, _)| code.as_deref() == Some(option.db_value()))
                .unwrap_or_else(|| panic!("missing menu for {option:?}"));
            assert_eq!(code.as_deref(), Some(option.db_value()));
            assert_eq!(localized.len(), base.len());
            for (source, translated) in base.iter().zip(localized) {
                assert_eq!(
                    translated.description,
                    commands::menu_desc(option, &source.command).expect("menu description"),
                    "wrong {option:?} description for {}",
                    source.command
                );
                assert_eq!(translated.command, source.command);
            }
        }

        // Representative Chinese texts (the locale value, not a code table).
        let (_, zh) = variants
            .iter()
            .find(|(code, _)| code.as_deref() == Some("zh"))
            .expect("zh menu");
        assert_eq!(zh[0].description, "订阅管理");
        assert_eq!(zh[1].description, "查询消息源");
    }

    #[test]
    fn menu_filtering_hides_start_and_admin_commands() {
        // /start is hidden from every menu but remains triggerable.
        assert!(is_hidden_from_menu("/start"));
        assert!(is_hidden_from_menu("start"));
        assert!(!is_hidden_from_menu("/set"));

        // /cookies and /loget are excluded from the default user menu.
        // Regression guard: teloxide 0.17 derive emits the "/"-prefixed
        // form, which the old equality checks ("cookies"/"loget") missed.
        assert!(is_admin_only_command("/cookies"));
        assert!(is_admin_only_command("/loget"));
        assert!(is_admin_only_command("cookies"));
        assert!(is_admin_only_command("loget"));
        assert!(!is_admin_only_command("/find"));
        assert!(!is_admin_only_command("/about"));

        // The derived command list must actually carry the "/" prefix;
        // if this ever changes the filters above still work, but this
        // assertion documents the current behaviour.
        let names: Vec<String> = Command::bot_commands()
            .into_iter()
            .map(|c| c.command)
            .collect();
        assert!(names.contains(&"/cookies".to_string()));
        assert!(names.contains(&"/loget".to_string()));
        assert!(names.contains(&"/start".to_string()));

        // End-to-end shape of the two menus.
        let all_cmds: Vec<_> = Command::bot_commands()
            .into_iter()
            .filter(|c| !is_hidden_from_menu(&c.command))
            .collect();
        let user_cmds: Vec<_> = all_cmds
            .iter()
            .filter(|c| !is_admin_only_command(&c.command))
            .cloned()
            .collect();
        assert!(!user_cmds.iter().any(|c| c.command == "/cookies"));
        assert!(!user_cmds.iter().any(|c| c.command == "/loget"));
        assert!(!user_cmds.iter().any(|c| c.command == "/start"));
        assert!(all_cmds.iter().any(|c| c.command == "/cookies"));
        assert!(all_cmds.iter().any(|c| c.command == "/loget"));
        // loget sits between cookies and about; about stays last.
        assert_eq!(all_cmds.last().map(|c| c.command.as_str()), Some("/about"));
    }

    #[test]
    fn bot_client_accepts_explicit_proxy_schemes() {
        for proxy in [
            "http://127.0.0.1:8080",
            "socks5://127.0.0.1:1080",
            "socks5h://127.0.0.1:1080",
        ] {
            let client = build_bot_client(Some(proxy), std::time::Duration::from_secs(1));
            assert!(client.is_ok(), "BOT_PROXY {proxy} should be accepted");
        }
        assert!(build_bot_client(None, std::time::Duration::from_secs(1)).is_ok());
    }
}
