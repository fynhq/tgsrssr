#![forbid(unsafe_code)]

mod callback_protocol;
mod callbacks;
mod cleanup;
mod commands;
mod config;
mod db;
mod entrymeta;
mod errors;
mod fetcher;
mod fsutil;
mod htmlentities;
mod htmltable;
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
mod timeutil;
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
    // reqwest 0.13 uses rustls-no-provider; install ring before any client is built.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cfg = Config::from_env()?;

    let _log_guard = logging::init_logging(&cfg.log_dir)?;

    info!("starting tgsrssr v{}", commands::VERSION);

    if cfg.rss_proxy.is_some() {
        info!(
            "RSS proxy enabled: SSRF veto allows a local DNS lookup failure to proceed via the proxy; platform APIs skip local DNS checks behind the proxy. Scope the proxy ACL accordingly."
        );
    }
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

    std::fs::create_dir_all(cfg.media_dir.join("outbound"))?;

    let db_pool = db::connect(&cfg.db_path).await?;
    info!(db = %cfg.db_path.display(), "database connected");
    match db::normalize_telegram_entry_ids(&db_pool).await {
        Ok(0) => {}
        Ok(rows) => info!(rows, "normalized legacy Telegram entry ids"),
        Err(e) => {
            tracing::warn!(error = %e, "legacy Telegram entry id normalization failed; first fetch may re-push the visible window")
        }
    }

    let http_client = build_bot_client(bot_proxy.as_deref(), std::time::Duration::from_secs(600))?;
    let mut bot = Bot::with_client(&cfg.token, http_client);
    if let Some(api_url) = &cfg.bot_api_url {
        let url = reqwest::Url::parse(api_url)?;
        bot = bot.set_api_url(url);
        info!(api_url = %api_url, "using custom Bot API endpoint");
    }

    let probe_client = build_bot_client(bot_proxy.as_deref(), std::time::Duration::from_secs(15))?;
    let mut probe_bot = Bot::with_client(&cfg.token, probe_client);
    if let Some(api_url) = &cfg.bot_api_url {
        probe_bot = probe_bot.set_api_url(reqwest::Url::parse(api_url)?);
    }
    let bot_username = wait_for_bot_api(&probe_bot).await?;
    drop(probe_bot);

    let state = AppState::new(cfg.clone(), db_pool.clone(), bot_username)?;

    platforms::migrate_legacy_cookies(&state).await;
    platforms::load_persisted_cookies(&state).await;

    cleanup::run_startup_cleanup(&cfg, &db_pool).await;
    state.media_budget.invalidate();

    let all_cmds: Vec<teloxide::types::BotCommand> = Command::bot_commands()
        .into_iter()
        .filter(|c| !is_hidden_from_menu(&c.command))
        .collect();
    let user_cmds: Vec<teloxide::types::BotCommand> = all_cmds
        .iter()
        .filter(|c| !is_admin_only_command(&c.command))
        .cloned()
        .collect();
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

    let mut sched = JobScheduler::new().await?;
    scheduler::register_fetch_job(&sched, state.clone(), bot.clone()).await?;
    scheduler::register_heal_job(&sched, state.clone()).await?;
    register_cleanup_job(&sched, state.clone()).await?;
    scheduler::register_outbox_retry_job(&sched, state.clone(), bot.clone()).await?;
    register_pending_ops_sweep_job(&sched, state.clone(), bot.clone()).await?;
    sched.start().await?;
    info!("scheduler started");

    let handler = dptree::entry()
        .branch(Update::filter_message().branch(dptree::endpoint(handle_message)))
        .branch(Update::filter_callback_query().branch(dptree::endpoint(handle_callback_query)))
        .branch(
            Update::filter_my_chat_member().branch(dptree::endpoint(handle_my_chat_member_update)),
        );

    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .build();

    let shutdown_token = dispatcher.shutdown_token();
    let dispatch = dispatcher.dispatch();
    tokio::pin!(dispatch);
    tokio::select! {
        _ = &mut dispatch => {}
        signal = wait_for_shutdown_signal() => {
            info!(signal, "shutdown signal received, stopping dispatcher");
            loop {
                match shutdown_token.shutdown() {
                    Ok(done) => {
                        tokio::join!(done, &mut dispatch);
                        break;
                    }
                    Err(_) => tokio::select! {
                        _ = &mut dispatch => break,
                        _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
                    },
                }
            }
        }
    }

    info!("shutting down scheduler");
    if let Err(e) = sched.shutdown().await {
        tracing::warn!(error = %e, "scheduler shutdown returned an error");
    }

    Ok(())
}

fn build_bot_client(
    proxy: Option<&str>,
    timeout: std::time::Duration,
) -> anyhow::Result<reqwest_bot::Client> {
    let builder = reqwest_bot::Client::builder()
        .timeout(timeout)
        .pool_max_idle_per_host(2)
        .pool_idle_timeout(std::time::Duration::from_secs(30));
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

async fn handle_message(bot: Bot, msg: Message, state: AppState) -> ResponseResult<()> {
    if let Some(new_chat_id) = msg.migrate_to_chat_id() {
        let old_id = msg.chat.id.0.to_string();
        let new_id = new_chat_id.0.to_string();
        match db::migrate_chat_id(&state.db, &old_id, &new_id).await {
            Ok(()) => {
                info!(old_chat_id = %old_id, new_chat_id = %new_id, "chat id migrated after group upgrade")
            }
            Err(e) => {
                error!(old_chat_id = %old_id, new_chat_id = %new_id, error = format!("{:#}", e), "chat id migration failed")
            }
        }
        return Ok(());
    }

    let text = msg.text().unwrap_or_default();

    let chat_id = msg.chat.id.0;
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    let is_command = text.starts_with('/');
    let is_channel_forward = matches!(
        msg.forward_origin(),
        Some(teloxide::types::MessageOrigin::Channel { .. })
    );
    let pending = {
        let mut pending_ops = state.pending_ops.lock().await;
        let key = (chat_id, user_id);
        let keep_for_media = pending_ops.get(&key).is_some_and(|op| {
            state.config.is_allowed(user_id)
                && !op.is_expired()
                && msg.text().is_none()
                && !is_command
                && !is_channel_forward
        });
        if keep_for_media {
            None
        } else {
            pending_ops.remove(&key)
        }
    };
    if let Some(op) = pending {
        let lang = commands::user_lang(&state, user_id, Some(&msg)).await;
        if !state.config.is_allowed(user_id) {
            crate::commands::retire_prompt(
                &bot,
                msg.chat.id,
                op.prompt_msg_id(),
                i18n::t(lang, "cancelled"),
            )
            .await;
            return Ok(());
        }
        let expired = op.is_expired();
        if expired {
            let prompt_msg_id = op.prompt_msg_id();
            crate::commands::retire_prompt(
                &bot,
                msg.chat.id,
                prompt_msg_id,
                i18n::t(lang, "timeout_cancelled"),
            )
            .await;
        } else if is_command || is_channel_forward {
            let prompt_msg_id = op.prompt_msg_id();
            crate::commands::retire_prompt(
                &bot,
                msg.chat.id,
                prompt_msg_id,
                i18n::t(lang, "cancelled"),
            )
            .await;
        } else {
            return commands::handle_pending_op(&bot, &msg, &state, op, text).await;
        }
    }

    match Command::parse(text, &state.bot_username) {
        Ok(cmd) => commands::handle_command(bot, msg, cmd, state).await,
        Err(_) => {
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

async fn handle_callback_query(bot: Bot, q: CallbackQuery, state: AppState) -> ResponseResult<()> {
    callbacks::handle_callback(bot, q, state).await
}

async fn handle_my_chat_member_update(
    mcm: ChatMemberUpdated,
    state: AppState,
) -> ResponseResult<()> {
    callbacks::handle_my_chat_member(mcm, state).await
}

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

const CLEANUP_INTERVAL_MINUTES: u64 = 60;

async fn register_cleanup_job(sched: &JobScheduler, state: AppState) -> anyhow::Result<()> {
    let interval = CLEANUP_INTERVAL_MINUTES;
    let (expr, gate_secs) = ("0 * * * * *".to_string(), Some(interval.saturating_mul(60)));
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

async fn register_pending_ops_sweep_job(
    sched: &JobScheduler,
    state: AppState,
    bot: Bot,
) -> anyhow::Result<()> {
    let expr = "0 */5 * * * *";
    sched
        .add(Job::new_async(expr, move |_uuid, _l| {
            let state = state.clone();
            let bot = bot.clone();
            Box::pin(async move {
                let mut ops = state.pending_ops.lock().await;
                let expired: Vec<((i64, i64), crate::state::PendingOp)> = ops
                    .iter()
                    .filter(|(_, op)| op.is_expired())
                    .map(|(k, op)| (*k, op.clone()))
                    .collect();
                for (key, _) in &expired {
                    ops.remove(key);
                }
                drop(ops);
                for ((chat_id, user_id), op) in expired {
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

fn is_hidden_from_menu(cmd: &str) -> bool {
    matches!(cmd.trim_start_matches('/'), "start")
}

fn is_admin_only_command(cmd: &str) -> bool {
    matches!(cmd.trim_start_matches('/'), "cookies" | "loget")
}

fn menu_language_codes() -> impl Iterator<Item = &'static str> {
    Lang::ALL
        .into_iter()
        .filter(|lang| *lang != Lang::En)
        .map(Lang::db_value)
}

type MenuVariant = (Option<String>, Vec<teloxide::types::BotCommand>);

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
        assert_eq!(variants.len(), Lang::ALL.len());

        let (lang, en) = &variants[0];
        assert!(lang.is_none());
        assert_eq!(en[0].description, "Subscriptions");

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

        let (_, zh) = variants
            .iter()
            .find(|(code, _)| code.as_deref() == Some("zh"))
            .expect("zh menu");
        assert_eq!(zh[0].description, "订阅管理");
        assert_eq!(zh[1].description, "查询消息源");
    }

    #[test]
    fn menu_filtering_hides_start_and_admin_commands() {
        assert!(is_hidden_from_menu("/start"));
        assert!(is_hidden_from_menu("start"));
        assert!(!is_hidden_from_menu("/set"));

        assert!(is_admin_only_command("/cookies"));
        assert!(is_admin_only_command("/loget"));
        assert!(is_admin_only_command("cookies"));
        assert!(is_admin_only_command("loget"));
        assert!(!is_admin_only_command("/find"));
        assert!(!is_admin_only_command("/about"));

        let names: Vec<String> = Command::bot_commands()
            .into_iter()
            .map(|c| c.command)
            .collect();
        assert!(names.contains(&"/cookies".to_string()));
        assert!(names.contains(&"/loget".to_string()));
        assert!(names.contains(&"/start".to_string()));

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
        assert_eq!(all_cmds.last().map(|c| c.command.as_str()), Some("/about"));
    }

    #[test]
    fn rustls_provider_install_allows_reqwest_client_build() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("reqwest client builds with the installed provider");
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
