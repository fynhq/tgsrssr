//! Fetch/outbox scheduling, auto-disable/self-heal and platform health.
mod pending;
mod policy;
mod send_effects;

use send_effects::{apply_send_success, SendSuccessCtx, SendSuccessExtras};

use jiff::Timestamp;
use teloxide::prelude::*;
use tokio::sync::Semaphore;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{debug, error, info, warn};

use crate::db;
use crate::fetcher;
use crate::i18n::{self, Lang};
use crate::notify::{
    notify_admins_html, notify_download_failed, notify_oversized_videos, notify_owner_html,
    owner_lang, push_fail_msg, push_line,
};
use crate::push::{self, ContentMode};
use crate::sendpolicy;
use crate::state::AppState;

const MAX_SEND_FAILURES: i64 = 3;

const OUTBOX_PER_FEED_LIMIT: i64 = 20;

const SCHEDULER_INTERVAL_MINUTES: u64 = 5;

pub async fn register_fetch_job(
    sched: &JobScheduler,
    state: AppState,
    bot: Bot,
) -> anyhow::Result<()> {
    let interval = SCHEDULER_INTERVAL_MINUTES;
    let expr = format!("0 */{} * * * *", interval);

    sched
        .add(Job::new_async(expr.as_str(), move |_uuid, _l| {
            let state = state.clone();
            let bot = bot.clone();
            Box::pin(async move {
                if let Err(e) = run_fetch_cycle(&state, &bot).await {
                    error!(error = %e, "fetch cycle failed");
                }
            })
        })?)
        .await?;

    info!(
        interval_min = interval,
        tick_min = interval,
        "fetch job registered"
    );
    Ok(())
}

pub async fn register_heal_job(sched: &JobScheduler, state: AppState) -> anyhow::Result<()> {
    const AUTO_HEAL_COOLDOWN_HOURS: i64 = 3;
    let expr = "0 0 * * * *";

    sched
        .add(Job::new_async(expr, move |_uuid, _l| {
            let state = state.clone();
            Box::pin(async move {
                match db::reenable_due_auto_disabled(&state.db, AUTO_HEAL_COOLDOWN_HOURS).await {
                    Ok(0) => {}
                    Ok(n) => info!(
                        count = n,
                        "auto-heal: re-enabled subscriptions after cooldown"
                    ),
                    Err(e) => error!(error = %e, "auto-heal job failed"),
                }
            })
        })?)
        .await?;

    info!(
        cooldown_hours = AUTO_HEAL_COOLDOWN_HOURS,
        "auto-heal job registered"
    );
    Ok(())
}

// read_streak returns None on a transient DB error; callers must not write 0 back and reset the streak.
async fn read_streak(state: &AppState, key: &str, platform: &'static str) -> Option<i64> {
    match db::get_setting(&state.db, key).await {
        Ok(v) => Some(v.and_then(|s| s.parse().ok()).unwrap_or(0)),
        Err(e) => {
            warn!(platform = platform, error = %e, "streak read failed, deferring to next round");
            None
        }
    }
}

const STALE_ZERO_WARN_ROUNDS: i64 = 20;

fn stale_zero_next(prev: i64) -> (i64, bool) {
    let next = prev.saturating_add(1);
    (next, next % STALE_ZERO_WARN_ROUNDS == 0)
}

async fn note_zero_entry_round(state: &AppState, sub: &db::SubRow, round_zero: bool) {
    let key = db::setting_key::stale_zero(&sub.chat_id, &sub.feed_url);
    if !round_zero {
        if let Err(e) = db::delete_setting(&state.db, &key).await {
            warn!(feed = %sub.feed_url, error = %e, "stale-zero streak clear failed");
        }
        return;
    }
    let prev = match db::get_setting(&state.db, &key).await {
        Ok(v) => v.and_then(|s| s.parse().ok()).unwrap_or(0),
        Err(e) => {
            warn!(feed = %sub.feed_url, error = %e, "stale-zero streak read failed, skipping this round");
            return;
        }
    };
    let (next, should_warn) = stale_zero_next(prev);
    if let Err(e) = db::set_setting(&state.db, &key, &next.to_string()).await {
        warn!(feed = %sub.feed_url, error = %e, "stale-zero streak write failed");
        return;
    }
    if should_warn {
        warn!(
            feed = %sub.feed_url,
            chat_id = %sub.chat_id,
            rounds = next,
            "source returned zero entries for {} consecutive successful full fetches \
             (parser may have gone stale after a page redesign; platform-level zero-entry \
             warns did not fire this whole time)",
            next
        );
    }
}

#[derive(Default)]
struct PlatRoundStats {
    seen: tokio::sync::Mutex<std::collections::HashMap<&'static str, usize>>,
    failed: tokio::sync::Mutex<std::collections::HashSet<&'static str>>,
    ok: tokio::sync::Mutex<std::collections::HashSet<&'static str>>,
    last_err: tokio::sync::Mutex<std::collections::HashMap<&'static str, String>>,
    active_counts: std::collections::HashMap<&'static str, usize>,
    active_counts_valid: bool,
}

async fn run_fetch_cycle(state: &AppState, bot: &Bot) -> anyhow::Result<()> {
    if state
        .fetch_cycle_active
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::Acquire,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_err()
    {
        warn!("previous fetch cycle still running, skipping this tick");
        return Ok(());
    }
    let _guard = crate::state::ActiveFlagGuard(state.fetch_cycle_active.clone());
    let allowed_owner_ids = state.config.allowed_owner_ids();

    let (total_due, due) = db::get_due_subscriptions(&state.db, &allowed_owner_ids).await?;
    if due.is_empty() {
        return Ok(());
    }
    let selected_due = due.len();
    let cycle_started = std::time::Instant::now();
    info!(
        count = due.len(),
        due_total = total_due,
        skipped = total_due.saturating_sub(due.len() as i64),
        "fetch cycle: due subscriptions selected"
    );

    let (active_counts, active_counts_valid) = match db::active_subscription_urls(
        &state.db,
        &allowed_owner_ids,
    )
    .await
    {
        Ok(urls) => {
            let mut counts: std::collections::HashMap<&'static str, usize> =
                std::collections::HashMap::new();
            for url in urls {
                if let Some(p) = platform_of_url(&url) {
                    *counts.entry(p).or_insert(0) += 1;
                }
            }
            (counts, true)
        }
        Err(e) => {
            warn!(error = %e, "active subscription count failed; skipping platform health this round");
            (std::collections::HashMap::new(), false)
        }
    };
    let stats = std::sync::Arc::new(PlatRoundStats {
        active_counts,
        active_counts_valid,
        ..PlatRoundStats::default()
    });

    let (platform_subs, rss_subs): (Vec<_>, Vec<_>) = due
        .into_iter()
        .partition(|(sub, _)| platform_of_url(&sub.feed_url).is_some());

    let process_sem = std::sync::Arc::new(Semaphore::new(crate::config::FETCH_CONCURRENCY));
    let mut handles: Vec<(String, tokio::task::JoinHandle<anyhow::Result<FeedStatus>>)> =
        Vec::new();
    for (idx, (sub, cfg)) in rss_subs.into_iter().enumerate() {
        let state = state.clone();
        let bot = bot.clone();
        let stats = stats.clone();
        let process_sem = process_sem.clone();
        let feed_url = sub.feed_url.clone();
        handles.push((
            feed_url,
            tokio::spawn(async move {
                if idx > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(600 * idx as u64)).await;
                }
                let _permit = process_sem.acquire_owned().await?;
                process_feed(&state, &bot, sub, cfg, &stats).await
            }),
        ));
    }

    let mut first_platform = true;
    let mut plat_ok = 0usize;
    let mut plat_fail = 0usize;
    for (sub, cfg) in platform_subs {
        if !first_platform {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos();
            let delay_ms = 3000 + (nanos % 5000) as u64;
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        first_platform = false;
        let feed_url = sub.feed_url.clone();
        let chat_id = sub.chat_id.clone();
        match process_feed(state, bot, sub, cfg, &stats).await {
            Ok(FeedStatus::Ok) => {
                plat_ok += 1;
                record_platform_outcome(&stats, &feed_url, true).await;
            }
            Ok(FeedStatus::Failed) => {
                plat_fail += 1;
                record_platform_outcome(&stats, &feed_url, false).await;
            }
            Err(e) => {
                plat_fail += 1;
                record_platform_outcome(&stats, &feed_url, false).await;
                error!(feed = %feed_url, chat_id = %chat_id, error = %e, "platform feed processing failed");
            }
        }
    }

    let mut rss_ok = 0usize;
    let mut rss_fail = 0usize;
    for (feed_url, handle) in handles {
        match handle.await {
            Ok(Ok(FeedStatus::Ok)) => rss_ok += 1,
            Ok(Ok(FeedStatus::Failed)) => rss_fail += 1,
            Ok(Err(e)) => {
                rss_fail += 1;
                error!(feed = %feed_url, error = %e, "feed processing failed")
            }
            Err(e) => {
                rss_fail += 1;
                error!(feed = %feed_url, error = %e, "feed task join failed")
            }
        }
    }

    info!(
        due = total_due,
        selected = selected_due,
        rss_ok = rss_ok,
        rss_fail = rss_fail,
        plat_ok = plat_ok,
        plat_fail = plat_fail,
        elapsed_ms = cycle_started.elapsed().as_millis() as u64,
        "fetch cycle: summary"
    );

    const HYSTERESIS_ROUNDS: i64 = 2;
    if !stats.active_counts_valid {
        return Ok(());
    }
    let seen = stats.seen.lock().await.clone();
    let failed = stats.failed.lock().await.clone();
    let ok_set = stats.ok.lock().await.clone();
    let last_err = stats.last_err.lock().await.clone();
    for (&p, &seen_count) in &seen {
        let active_count = stats.active_counts.get(p).copied().unwrap_or(seen_count);
        let verdict = policy::platform_round_verdict(
            seen_count,
            active_count,
            ok_set.contains(&p),
            failed.contains(&p),
        );
        if verdict == policy::PlatformRound::Skip {
            tracing::debug!(
                platform = p,
                seen = seen_count,
                active = active_count,
                "platform health deferred: not all active sources ran this round"
            );
            continue;
        }
        let streak_key = db::setting_key::platform_streak(p);
        let health_key = db::setting_key::platform_health(p);
        let is_down = matches!(
            db::get_setting(&state.db, &health_key).await,
            Ok(Some(v)) if v == "down"
        );

        if verdict == policy::PlatformRound::AllFailed {
            if let Some(prev) = read_streak(state, &streak_key, p).await {
                let streak = if prev > 0 { prev + 1 } else { 1 };
                if let Err(e) = db::set_setting(&state.db, &streak_key, &streak.to_string()).await {
                    warn!(platform = p, error = %e, "streak write failed");
                }

                if streak >= HYSTERESIS_ROUNDS && !is_down {
                    let err = last_err
                        .get(&p)
                        .map(String::as_str)
                        .unwrap_or("unknown error");
                    maybe_notify_platform_down(bot, state, p, err).await;
                }
            }
        } else if verdict == policy::PlatformRound::AllOk {
            if let Some(streak) = read_streak(state, &streak_key, p).await {
                let ok_streak = if streak < 0 { streak - 1 } else { -1 };
                if let Err(e) =
                    db::set_setting(&state.db, &streak_key, &ok_streak.to_string()).await
                {
                    warn!(platform = p, error = %e, "streak write failed");
                }

                if ok_streak <= -HYSTERESIS_ROUNDS && is_down {
                    maybe_notify_platform_recovered(bot, state, p).await;
                }
            }
        } else {
            if let Err(e) = db::set_setting(&state.db, &streak_key, "0").await {
                warn!(platform = p, error = %e, "streak reset failed");
            }
        }
    }

    Ok(())
}

const FEED_DEADLINE_SECS: u64 = 240;

enum FeedStatus {
    Ok,
    Failed,
}

async fn entry_already_handled(
    state: &AppState,
    chat_id: &str,
    feed_url: &str,
    entry_id: &str,
) -> anyhow::Result<bool> {
    let (sent, queued) = db::entry_dedup_state(&state.db, chat_id, feed_url, entry_id).await?;
    Ok(sent || queued)
}

struct SendFailureCtx<'a> {
    state: &'a AppState,
    bot: &'a Bot,
    sub: &'a db::SubRow,
    cfg: &'a db::EffectiveConfig,
    lang: Lang,
    owner_id: Option<i64>,
    chat_title: &'a str,
    feed_title: &'a str,
    entry: &'a feed_rs::model::Entry,
    entry_id: &'a str,
    author: Option<&'a str>,
    notify_warnings: &'a [String],
    oversized: &'a [crate::media::OversizedVideo],
    notify_oversized: bool,
    dl_retry_note: &'a str,
    media_items: &'a [crate::media::MediaItem],
    meta: Option<&'a crate::entrymeta::EntryMeta>,
    error: &'a anyhow::Error,
}

async fn handle_send_failure(ctx: &SendFailureCtx<'_>) -> anyhow::Result<policy::FlowAction> {
    let state = ctx.state;
    let bot = ctx.bot;
    let sub = ctx.sub;
    let cfg = ctx.cfg;
    let lang = ctx.lang;
    let owner_id = ctx.owner_id;
    let chat_title = ctx.chat_title;
    let feed_title = ctx.feed_title;
    let entry = ctx.entry;
    let eid = ctx.entry_id;
    let author = ctx.author;
    let notify_warnings = ctx.notify_warnings;
    let oversized = ctx.oversized;
    let notify_oversized = ctx.notify_oversized;
    let dl_retry_note = ctx.dl_retry_note;
    let media_items = ctx.media_items;
    let meta = ctx.meta;
    let e = ctx.error;

    if let Some(new_id) = migrated_chat_id_from_error(e) {
        match db::migrate_chat_id(&state.db, &cfg.chat_id, &new_id).await {
            Ok(()) => {
                info!(
                    old_chat_id = %cfg.chat_id,
                    new_chat_id = %new_id,
                    "chat id migrated after send error; retrying on the next cycle"
                );
                return Ok(policy::FlowAction::DeferBreak);
            }
            Err(me) => warn!(
                old_chat_id = %cfg.chat_id,
                new_chat_id = %new_id,
                error = %me,
                "chat id migration after send error failed"
            ),
        }
    }
    if record_send_failure(bot, state, sub, cfg, lang, feed_title, e).await {
        return Ok(policy::FlowAction::DeferBreak);
    }

    let queued = match db::outbox_count_for_feed(&state.db, &cfg.chat_id, &cfg.feed_url).await {
        Ok(n) => n,
        Err(err) => {
            warn!(feed = %cfg.feed_url, error = %err, "outbox count failed, deferring entry");
            return Ok(policy::FlowAction::DeferContinue);
        }
    };
    if queued >= OUTBOX_PER_FEED_LIMIT {
        warn!(
            feed = %cfg.feed_url,
            queued,
            "outbox queue reached the per-source limit; disabling source"
        );
        match db::auto_disable_subscription(&state.db, sub.id).await {
            Ok(true) => {
                let name = if feed_title.is_empty() {
                    &cfg.feed_url
                } else {
                    feed_title
                };
                let msg = i18n::t(lang, "notif_outbox_full")
                    .replacen("{}", &push::escape_html(name), 1)
                    .replacen("{}", &OUTBOX_PER_FEED_LIMIT.to_string(), 1);
                notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
            }
            Ok(false) => {}
            Err(err) => {
                warn!(feed = %cfg.feed_url, error = %err, "outbox-full auto-disable failed")
            }
        }
        return Ok(policy::FlowAction::DeferBreak);
    }

    warn!(feed = %cfg.feed_url, error = %e, "send failed, queuing to outbox");
    // entry_json is the sole retry source; never enqueue an empty string (the row would be deleted without mark_sent).
    let entry_json = match serde_json::to_string(entry) {
        Ok(j) => j,
        Err(se) => {
            error!(feed = %cfg.feed_url, entry_id = %eid, error = %se, "entry_json serialization failed, marking sent");
            db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, eid).await?;
            let elink = fetcher::entry_link(entry).unwrap_or_default();
            let msg = push_fail_msg(
                lang,
                i18n::t(lang, "notif_serialize_failed"),
                feed_title,
                &format!("{:#}", e),
                "label_source",
                &elink,
            );
            notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
            return Ok(policy::FlowAction::Continue);
        }
    };
    let media_json = serde_json::to_string(media_items).unwrap_or_default();
    let meta_json = meta
        .map(|m| serde_json::to_string(m).unwrap_or_default())
        .unwrap_or_default();
    let now_ts = Timestamp::now().as_second();
    let next_retry = now_ts + outbox_backoff_secs(0);
    let entry_link = fetcher::entry_link(entry).unwrap_or_default();
    match db::outbox_insert(
        &state.db,
        &cfg.chat_id,
        &cfg.feed_url,
        eid,
        feed_title,
        &entry_json,
        &media_json,
        &meta_json,
        &cfg.content_mode,
        &cfg.source_display,
        cfg.link_preview,
        cfg.caption_above_media,
        author,
        next_retry,
        now_ts,
    )
    .await
    {
        Ok(true) => {
            let msg = push_fail_msg(
                lang,
                i18n::t(lang, "notif_queued"),
                feed_title,
                &format!("{:#}", e),
                "label_source",
                &entry_link,
            );
            notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
            if !notify_warnings.is_empty() {
                notify_download_failed(
                    bot,
                    lang,
                    owner_id,
                    feed_title,
                    notify_warnings,
                    dl_retry_note,
                    None,
                    &entry_link,
                )
                .await;
            }
            if notify_oversized {
                notify_oversized_videos(
                    bot,
                    lang,
                    owner_id,
                    chat_title,
                    &entry_link,
                    None,
                    false,
                    oversized,
                )
                .await;
            }
        }
        Ok(false) => {
            debug!(feed = %cfg.feed_url, entry_id = %eid, "already in outbox, skipping notice");
        }
        Err(insert_err) => {
            error!(feed = %cfg.feed_url, error = %insert_err, "outbox insert failed, marking sent");
            db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, eid).await?;
            let msg = push_fail_msg(
                lang,
                i18n::t(lang, "notif_enqueue_failed"),
                feed_title,
                &format!("{:#}", e),
                "label_source",
                &entry_link,
            );
            notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
        }
    }
    Ok(policy::FlowAction::Continue)
}

enum MediaGateAction {
    Continue,
    DeferContinue,
    Proceed { dl_retry_note: String },
}

struct MediaGateCtx<'a> {
    state: &'a AppState,
    bot: &'a Bot,
    cfg: &'a db::EffectiveConfig,
    lang: Lang,
    owner_id: Option<i64>,
    chat_title: &'a str,
    feed_title: &'a str,
    entry: &'a feed_rs::model::Entry,
    entry_id: &'a str,
    prepared: &'a crate::media::PreparedMedia,
    mode: ContentMode,
}

async fn media_gate(ctx: &MediaGateCtx<'_>) -> anyhow::Result<MediaGateAction> {
    let state = ctx.state;
    let bot = ctx.bot;
    let cfg = ctx.cfg;
    let lang = ctx.lang;
    let owner_id = ctx.owner_id;
    let chat_title = ctx.chat_title;
    let feed_title = ctx.feed_title;
    let entry = ctx.entry;
    let eid = ctx.entry_id;
    let prepared = ctx.prepared;
    let notify_warnings = prepared.notify_warnings(state.config.notify_oversized_media);
    let has_download_warnings = !prepared.warnings.is_empty();
    let notify_oversized = state.config.notify_oversized_media;
    let oversized = &prepared.oversized_videos;

    if ctx.mode == ContentMode::MediaOnly && prepared.items.is_empty() {
        let entry_link = fetcher::entry_link(entry).unwrap_or_default();
        if notify_oversized {
            notify_oversized_videos(
                bot,
                lang,
                owner_id,
                chat_title,
                &entry_link,
                None,
                false,
                oversized,
            )
            .await;
        }
        if has_download_warnings {
            let retry_key = db::setting_key::media_dl_fail(&cfg.chat_id, &cfg.feed_url, eid);
            let stored = match db::get_setting(&state.db, &retry_key).await {
                Ok(v) => v.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
                Err(e) => {
                    warn!(entry_id = %eid, feed = %cfg.feed_url, error = %e, "media_dl_fail counter read failed, deferring to next round");
                    return Ok(MediaGateAction::DeferContinue);
                }
            };
            let step = policy::media_retry_step(true, true, stored, !notify_warnings.is_empty());
            match step {
                policy::MediaRetryStep::Abandon { fail_count } => {
                    warn!(entry_id = %eid, feed = %cfg.feed_url, count = fail_count, "MediaOnly download retries exhausted, abandoning");
                    let msg = push_fail_msg(
                        lang,
                        i18n::t(lang, "notif_abandoned"),
                        feed_title,
                        &i18n::t(lang, "notif_media_dl_fail").replacen(
                            "{}",
                            &fail_count.to_string(),
                            1,
                        ),
                        "label_entry",
                        &entry_link,
                    );
                    notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                    db::delete_setting(&state.db, &retry_key).await.ok();
                    db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, eid).await?;
                }
                policy::MediaRetryStep::Retry { fail_count, notify } => {
                    if let Err(e) =
                        db::set_setting(&state.db, &retry_key, &fail_count.to_string()).await
                    {
                        warn!(entry_id = %eid, feed = %cfg.feed_url, error = %e, "media_dl_fail counter write failed, deferring without notice");
                        return Ok(MediaGateAction::DeferContinue);
                    }
                    if notify {
                        notify_download_failed(
                            bot,
                            lang,
                            owner_id,
                            feed_title,
                            &notify_warnings,
                            &i18n::t(lang, "notif_dl_note")
                                .replacen("{}", &fail_count.to_string(), 1)
                                .replacen("{}", &policy::MEDIA_DL_MAX_RETRIES.to_string(), 1),
                            None,
                            &entry_link,
                        )
                        .await;
                    }
                    return Ok(MediaGateAction::DeferContinue);
                }
                policy::MediaRetryStep::Proceed => {
                    unreachable!("media-only retry step cannot proceed")
                }
                policy::MediaRetryStep::Degrade { .. } => {
                    unreachable!("media-only retry step cannot degrade")
                }
            }
        }
        if !has_download_warnings {
            db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, eid).await?;
        }
        return Ok(MediaGateAction::Continue);
    }

    let mut dl_retry_note = String::new();
    if ctx.mode != ContentMode::MediaOnly && prepared.download_failed {
        let retry_key = db::setting_key::media_dl_fail(&cfg.chat_id, &cfg.feed_url, eid);
        let stored = match db::get_setting(&state.db, &retry_key).await {
            Ok(v) => v.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
            Err(e) => {
                warn!(entry_id = %eid, feed = %cfg.feed_url, error = %e, "media_dl_fail counter read failed, deferring to next round");
                return Ok(MediaGateAction::DeferContinue);
            }
        };
        match policy::media_retry_step(false, true, stored, false) {
            policy::MediaRetryStep::Retry { fail_count, .. } => {
                if let Err(e) =
                    db::set_setting(&state.db, &retry_key, &fail_count.to_string()).await
                {
                    warn!(entry_id = %eid, feed = %cfg.feed_url, error = %e, "media_dl_fail counter write failed, deferring without notice");
                    return Ok(MediaGateAction::DeferContinue);
                }
                info!(entry_id = %eid, feed = %cfg.feed_url, count = fail_count, "media download failed, deferring entry to next fetch round");
                return Ok(MediaGateAction::DeferContinue);
            }
            policy::MediaRetryStep::Degrade { fail_count } => {
                warn!(entry_id = %eid, feed = %cfg.feed_url, count = fail_count, "media download retries exhausted, degrading entry");
                db::delete_setting(&state.db, &retry_key).await.ok();
                dl_retry_note =
                    i18n::t(lang, "notif_dl_retried").replacen("{}", &fail_count.to_string(), 1);
            }
            policy::MediaRetryStep::Proceed => {
                unreachable!("download failure path cannot proceed")
            }
            policy::MediaRetryStep::Abandon { .. } => {
                unreachable!("non-media-only retry step cannot abandon")
            }
        }
    }

    if ctx.mode == ContentMode::MediaOnly || !prepared.download_failed {
        let retry_key = db::setting_key::media_dl_fail(&cfg.chat_id, &cfg.feed_url, eid);
        db::delete_setting(&state.db, &retry_key).await.ok();
    }
    Ok(MediaGateAction::Proceed { dl_retry_note })
}

async fn handle_fetch_failure(
    state: &AppState,
    bot: &Bot,
    sub: &db::SubRow,
    lang: Lang,
    e: &anyhow::Error,
    stats: &PlatRoundStats,
) -> anyhow::Result<()> {
    let err_msg = format!("{:#}", e);
    warn!(feed = %sub.feed_url, error = %err_msg, "fetch failed");

    if let Some(p) = platform_of_url(&sub.feed_url) {
        stats.last_err.lock().await.insert(p, err_msg.clone());
    }

    if marks_cookie_expired(e, &err_msg) {
        if let Some(p) = platform_of_url(&sub.feed_url) {
            db::set_setting(&state.db, &db::setting_key::cookie_expired_notified(p), "1")
                .await
                .ok();
        }
    }

    let count = db::increment_error_count(&state.db, sub.id, &err_msg).await?;

    if count >= state.config.max_fetch_errors && sub.active == 1 {
        if !db::auto_disable_subscription(&state.db, sub.id).await? {
            info!(
                feed = %sub.feed_url,
                "skip auto-disable: subscription paused by user during this round"
            );
            return Ok(());
        }
        info!(feed = %sub.feed_url, errors = count, "subscription auto-disabled");
        let notify_key = db::setting_key::auto_disable_notified(&sub.chat_id, &sub.feed_url);
        let already_notified = db::get_setting(&state.db, &notify_key)
            .await
            .ok()
            .flatten()
            .is_some();
        if !already_notified {
            db::set_setting(&state.db, &notify_key, "1").await.ok();
            let name = sub
                .feed_title
                .as_deref()
                .filter(|t| !t.is_empty())
                .map(|t| {
                    format!(
                        "<a href=\"{}\">{}</a>",
                        crate::push::escape_attr(&sub.feed_url),
                        crate::push::escape_html(t)
                    )
                })
                .unwrap_or_else(|| crate::push::escape_html(&sub.feed_url));
            let auto_disable_msg = i18n::fill(
                i18n::t(lang, "notif_auto_disable"),
                &[&name, &crate::push::escape_html(&sub.chat_id)],
            );
            notify_owner_html(bot, state, &sub.chat_id, &auto_disable_msg).await;
        }
    }
    Ok(())
}

struct EntryPrep {
    prepared: crate::media::PreparedMedia,
    dl_retry_note: String,
    notify_warnings: Vec<String>,
    notify_oversized: bool,
    oversized: Vec<crate::media::OversizedVideo>,
}

#[allow(clippy::too_many_arguments)]
async fn prepare_entry_for_send(
    state: &AppState,
    bot: &Bot,
    cfg: &db::EffectiveConfig,
    lang: Lang,
    owner_id: Option<i64>,
    chat_title: &str,
    feed_title: &str,
    entry: &feed_rs::model::Entry,
    fr: &fetcher::FetchResult,
    opts: push::RenderOpts,
    transport: &crate::media::MediaTransport,
    allow_trusted_private: bool,
    entry_id: &str,
    storage_notice_sent: &mut bool,
) -> anyhow::Result<Result<EntryPrep, policy::FlowAction>> {
    let eid = entry_id;
    let meta = fr.metas.get(eid);
    let prepared = if opts.mode.skip_media() {
        crate::media::PreparedMedia::empty()
    } else {
        crate::media::prepare_entry_media(
            state,
            entry,
            transport,
            meta,
            lang,
            &cfg.feed_url,
            opts.mode.single_media(),
            allow_trusted_private,
        )
        .await
    };
    if prepared.setup_failed {
        if !*storage_notice_sent {
            *storage_notice_sent = true;
            let name = if feed_title.is_empty() {
                &cfg.feed_url
            } else {
                feed_title
            };
            let msg =
                i18n::t(lang, "notif_media_storage").replacen("{}", &push::escape_html(name), 1);
            notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
        }
        return Ok(Err(policy::FlowAction::DeferContinue));
    }
    let gate = media_gate(&MediaGateCtx {
        state,
        bot,
        cfg,
        lang,
        owner_id,
        chat_title,
        feed_title,
        entry,
        entry_id: eid,
        prepared: &prepared,
        mode: opts.mode,
    })
    .await?;
    let dl_retry_note = match gate {
        MediaGateAction::Continue => return Ok(Err(policy::FlowAction::Continue)),
        MediaGateAction::DeferContinue => return Ok(Err(policy::FlowAction::DeferContinue)),
        MediaGateAction::Proceed { dl_retry_note } => dl_retry_note,
    };
    let notify_warnings = prepared.notify_warnings(state.config.notify_oversized_media);
    let notify_oversized = state.config.notify_oversized_media;
    let oversized = prepared.oversized_videos.clone();

    match entry_already_handled(state, &cfg.chat_id, &cfg.feed_url, eid).await {
        Ok(true) => return Ok(Err(policy::FlowAction::Continue)),
        Ok(false) => {}
        Err(e) => {
            warn!(
                feed = %cfg.feed_url,
                entry_id = %eid,
                error = %e,
                "pre-send dedup recheck failed, deferring remaining entries"
            );
            return Ok(Err(policy::FlowAction::DeferBreak));
        }
    }
    Ok(Ok(EntryPrep {
        prepared,
        dl_retry_note,
        notify_warnings,
        notify_oversized,
        oversized,
    }))
}

struct SendAndHandleCtx<'a> {
    state: &'a AppState,
    bot: &'a Bot,
    sub: &'a db::SubRow,
    cfg: &'a db::EffectiveConfig,
    lang: Lang,
    owner_id: Option<i64>,
    chat_id: i64,
    chat_title: &'a str,
    feed_title: &'a str,
    entry: &'a feed_rs::model::Entry,
    entry_id: &'a str,
    opts: push::RenderOpts,
    prepared: &'a crate::media::PreparedMedia,
    meta: Option<&'a crate::entrymeta::EntryMeta>,
    notify_warnings: &'a [String],
    notify_oversized: bool,
    oversized: &'a [crate::media::OversizedVideo],
    dl_retry_note: &'a str,
    transport: &'a crate::media::MediaTransport,
}

async fn send_and_handle(ctx: &SendAndHandleCtx<'_>) -> anyhow::Result<policy::FlowAction> {
    let state = ctx.state;
    let bot = ctx.bot;
    let sub = ctx.sub;
    let cfg = ctx.cfg;
    let lang = ctx.lang;
    let owner_id = ctx.owner_id;
    let chat_id = ctx.chat_id;
    let chat_title = ctx.chat_title;
    let feed_title = ctx.feed_title;
    let entry = ctx.entry;
    let eid = ctx.entry_id;
    let opts = ctx.opts;
    let prepared = ctx.prepared;
    let meta = ctx.meta;
    let notify_warnings = ctx.notify_warnings;
    let notify_oversized = ctx.notify_oversized;
    let oversized = ctx.oversized;
    let dl_retry_note = ctx.dl_retry_note;
    let transport = ctx.transport;

    let author = fetcher::entry_author(entry);
    let result: anyhow::Result<push::SendOutcome> = if prepared.items.is_empty() {
        push::send_text_only(
            bot,
            chat_id,
            entry,
            feed_title,
            author.as_deref(),
            opts,
            meta,
        )
        .await
    } else {
        push::send_entry_with_media(
            bot,
            &state.db,
            chat_id,
            push::PushCtx {
                entry,
                feed_title,
                author: author.as_deref(),
                opts,
                caption_above_media: cfg.caption_above_media,
                media: &prepared.items,
                meta,
            },
            transport,
        )
        .await
    };

    match result {
        Ok(send_outcome) => {
            let entry_link = fetcher::entry_link(entry).unwrap_or_default();
            let effects = SendSuccessCtx {
                bot,
                state,
                chat_id: &cfg.chat_id,
                feed_url: &cfg.feed_url,
                entry_id: eid,
                chat_title,
                lang,
            };
            apply_send_success(
                &effects,
                &send_outcome,
                &SendSuccessExtras {
                    owner_id,
                    feed_title,
                    entry_link: &entry_link,
                    download_warnings: notify_warnings,
                    oversized,
                    notify_oversized,
                    dl_retry_note,
                    outbox_row_id: None,
                },
                "mark_entry_sent failed after successful delivery (may re-push next cycle)",
            )
            .await;
            Ok(policy::FlowAction::Continue)
        }
        Err(e) => {
            let failure = SendFailureCtx {
                state,
                bot,
                sub,
                cfg,
                lang,
                owner_id,
                chat_title,
                feed_title,
                entry,
                entry_id: eid,
                author: author.as_deref(),
                notify_warnings,
                oversized,
                notify_oversized,
                dl_retry_note,
                media_items: &prepared.items,
                meta,
                error: &e,
            };
            handle_send_failure(&failure).await
        }
    }
}

struct FeedOwnerCtx {
    owner_id: Option<i64>,
    lang: Lang,
    allow_trusted_private: bool,
}

async fn resolve_feed_owner(
    state: &AppState,
    sub: &db::SubRow,
    stats: &PlatRoundStats,
) -> anyhow::Result<Option<FeedOwnerCtx>> {
    let owner_id = db::get_chat_owner(&state.db, &sub.chat_id).await?;
    if let Some(owner) = owner_id {
        if !state.config.is_allowed(owner) {
            info!(
                chat_id = %sub.chat_id,
                owner,
                feed = %sub.feed_url,
                "subscription owner is no longer whitelisted; skipping feed"
            );
            return Ok(None);
        }
    }
    let lang = match owner_id {
        Some(o) => owner_lang(state, o).await,
        None => Lang::En,
    };
    let allow_trusted_private = match owner_id {
        Some(o) => state.config.trusted_private_allowed(o),
        None => false,
    };
    if let Some(p) = platform_of_url(&sub.feed_url) {
        *stats.seen.lock().await.entry(p).or_insert(0) += 1;
    }
    Ok(Some(FeedOwnerCtx {
        owner_id,
        lang,
        allow_trusted_private,
    }))
}

enum FeedFetchError {
    Deferred,
    Failed(anyhow::Error),
}

async fn fetch_feed_with_retry(
    state: &AppState,
    sub: &db::SubRow,
    cfg: &db::EffectiveConfig,
    lang: Lang,
    allow_trusted_private: bool,
) -> Result<fetcher::FetchResult, FeedFetchError> {
    let is_rss_source = platform_of_url(&sub.feed_url).is_none();
    let known_sent_snapshot: Option<std::collections::HashSet<String>> =
        if crate::platforms::platform_for_url(&sub.feed_url).is_some_and(|p| p.known_sent) {
            match db::sent_entry_ids_recent(
                &state.db,
                &cfg.chat_id,
                &cfg.feed_url,
                db::SENT_ENTRY_LOOKBACK_DAYS,
                db::SENT_ENTRY_LOOKBACK_ROWS,
            )
            .await
            {
                Ok(s) => Some(s),
                Err(e) => {
                    warn!(
                        feed = %cfg.feed_url,
                        error = %e,
                        "sent-entry batch query failed, deferring feed"
                    );
                    return Err(FeedFetchError::Deferred);
                }
            }
        } else {
            None
        };
    let fetch_opts = fetcher::FetchOptions {
        etag: sub.http_etag.as_deref(),
        last_modified: sub.http_last_modified.as_deref(),
        known_sent: known_sent_snapshot.as_ref(),
        lang,
        allow_trusted_private,
        enrichment: fetcher::Enrichment::Full,
    };
    let result = fetcher::fetch_feed(state, &sub.feed_url, &fetch_opts).await;
    let result = match result {
        Err(ref e) if is_rss_source && sendpolicy::is_net_error(&format!("{:#}", e)) => {
            warn!(
                feed = %sub.feed_url,
                error = format!("{:#}", e),
                "fetch transient error, retrying once after 2s"
            );
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            fetcher::fetch_feed(state, &sub.feed_url, &fetch_opts).await
        }
        other => other,
    };
    drop(known_sent_snapshot);
    result.map_err(FeedFetchError::Failed)
}

async fn process_feed(
    state: &AppState,
    bot: &Bot,
    sub: db::SubRow,
    cfg: db::EffectiveConfig,
    stats: &PlatRoundStats,
) -> anyhow::Result<FeedStatus> {
    let transport = crate::media::MediaTransport::new(&state.config);
    let Some(owner) = resolve_feed_owner(state, &sub, stats).await? else {
        return Ok(FeedStatus::Ok);
    };
    let FeedOwnerCtx {
        owner_id,
        lang,
        allow_trusted_private,
    } = owner;

    let result = fetch_feed_with_retry(state, &sub, &cfg, lang, allow_trusted_private).await;
    let mut feed_failed = false;

    match result {
        Ok(fr) => {
            if let Err(e) = db::reset_error_count(&state.db, sub.id).await {
                warn!(feed = %sub.feed_url, error = %e, "reset error count failed");
            }
            let notify_key = db::setting_key::auto_disable_notified(&sub.chat_id, &sub.feed_url);
            db::delete_setting(&state.db, &notify_key).await.ok();

            if fr.not_modified {
                return Ok(FeedStatus::Ok);
            }

            note_zero_entry_round(state, &sub, fr.entries.is_empty()).await;

            if sub.title_overridden == 0 {
                if let Some(t) = &fr.feed_title {
                    if Some(t.as_str()) != sub.feed_title.as_deref() {
                        db::rename_subscription_auto(&state.db, sub.id, t)
                            .await
                            .ok();
                    }
                }
            }

            let feed_title = policy::effective_feed_title(
                sub.title_overridden != 0,
                fr.feed_title.as_deref(),
                sub.feed_title.as_deref(),
            );
            let opts = push::RenderOpts {
                mode: ContentMode::from_str(&cfg.content_mode),
                source: push::SourceDisplay::from_str(&cfg.source_display),
                link_preview: cfg.link_preview,
                lang,
                allow_rich: true,
            };
            let chat_id: i64 = match cfg.chat_id.parse() {
                Ok(id) => id,
                Err(_) => {
                    warn!(chat_id = %cfg.chat_id, feed = %cfg.feed_url, "invalid chat_id, skipping subscription");
                    return Ok(FeedStatus::Ok);
                }
            };
            let chat_title = db::get_chat(&state.db, &cfg.chat_id)
                .await
                .ok()
                .flatten()
                .and_then(|c| c.title)
                .unwrap_or_else(|| cfg.chat_id.clone());

            // A filter-config DB error is not "no filter": defer the entry instead of pushing blocked content.
            let filter_cfg = match db::get_feed_filter(&state.db, &cfg.chat_id, &cfg.feed_url).await
            {
                Ok(cfg) => cfg,
                Err(e) => {
                    warn!(feed = %cfg.feed_url, error = %e, "feed filter read failed, deferring feed");
                    return Ok(FeedStatus::Failed);
                }
            };
            let filter_keywords = filter_cfg.keyword_list();

            let wanted_ids: Vec<String> = fr.entries.iter().map(fetcher::entry_id).collect();
            let known_sent = match db::sent_entry_ids_in_window(
                &state.db,
                &cfg.chat_id,
                &cfg.feed_url,
                &wanted_ids,
            )
            .await
            {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        feed = %cfg.feed_url,
                        error = %e,
                        "sent-entry batch query failed after fetch, deferring feed"
                    );
                    return Ok(FeedStatus::Failed);
                }
            };
            let outbox_ids =
                match db::outbox_entry_ids(&state.db, &cfg.chat_id, &cfg.feed_url).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(
                            feed = %cfg.feed_url,
                            error = %e,
                            "outbox batch query failed after fetch, deferring feed"
                        );
                        return Ok(FeedStatus::Failed);
                    }
                };
            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_secs(FEED_DEADLINE_SECS);
            let mut storage_notice_sent = false;
            let selection = pending::select_pending(
                &fr.entries,
                &known_sent,
                &outbox_ids,
                &filter_cfg,
                &filter_keywords,
                lang,
                deadline,
            );
            if selection.deferred {
                warn!(
                    feed = %cfg.feed_url,
                    "feed filtering deadline reached, remaining entries deferred to next cycle"
                );
            }
            let mut deferred = selection.deferred;
            for eid in &selection.filtered_ids {
                db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, eid).await?;
            }
            let pending = selection.entries;
            drop(known_sent);
            drop(outbox_ids);

            // Save ETag/Last-Modified only when all entries drained; a premature 304 permanently swallows deferred entries.
            for entry in pending {
                if tokio::time::Instant::now() >= deadline {
                    warn!(
                        feed = %cfg.feed_url,
                        "feed processing deadline reached, remaining entries deferred to next cycle"
                    );
                    deferred = true;
                    break;
                }
                let eid = fetcher::entry_id(entry);
                match entry_already_handled(state, &cfg.chat_id, &cfg.feed_url, &eid).await {
                    Ok(true) => {
                        debug!(feed = %cfg.feed_url, entry_id = %eid, "entry already handled, skipping");
                        continue;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(
                            feed = %cfg.feed_url,
                            entry_id = %eid,
                            error = %e,
                            "entry dedup recheck failed, deferring remaining entries"
                        );
                        deferred = true;
                        break;
                    }
                }
                let prep = match prepare_entry_for_send(
                    state,
                    bot,
                    &cfg,
                    lang,
                    owner_id,
                    &chat_title,
                    &feed_title,
                    entry,
                    &fr,
                    opts,
                    &transport,
                    allow_trusted_private,
                    &eid,
                    &mut storage_notice_sent,
                )
                .await?
                {
                    Ok(p) => p,
                    Err(policy::FlowAction::Continue) => continue,
                    Err(policy::FlowAction::DeferContinue) => {
                        deferred = true;
                        continue;
                    }
                    Err(policy::FlowAction::DeferBreak) => {
                        deferred = true;
                        break;
                    }
                };
                let EntryPrep {
                    prepared,
                    dl_retry_note,
                    notify_warnings,
                    notify_oversized,
                    oversized,
                } = prep;
                let meta = fr.metas.get(&eid);

                let action = send_and_handle(&SendAndHandleCtx {
                    state,
                    bot,
                    sub: &sub,
                    cfg: &cfg,
                    lang,
                    owner_id,
                    chat_id,
                    chat_title: &chat_title,
                    feed_title: &feed_title,
                    entry,
                    entry_id: &eid,
                    opts,
                    prepared: &prepared,
                    meta,
                    notify_warnings: &notify_warnings,
                    notify_oversized,
                    oversized: &oversized,
                    dl_retry_note: &dl_retry_note,
                    transport: &transport,
                })
                .await?;
                match action {
                    policy::FlowAction::Continue => {}
                    policy::FlowAction::DeferContinue => {
                        deferred = true;
                    }
                    policy::FlowAction::DeferBreak => {
                        deferred = true;
                        break;
                    }
                }
            }

            if !deferred {
                db::update_fetch_headers(
                    &state.db,
                    sub.id,
                    fr.http_etag.as_deref(),
                    fr.http_last_modified.as_deref(),
                )
                .await
                .ok();
            }
        }
        Err(FeedFetchError::Deferred) => return Ok(FeedStatus::Failed),
        Err(FeedFetchError::Failed(e)) => {
            feed_failed = true;
            handle_fetch_failure(state, bot, &sub, lang, &e, stats).await?;
        }
    }

    Ok(if feed_failed {
        FeedStatus::Failed
    } else {
        FeedStatus::Ok
    })
}

fn platform_of_url(feed_url: &str) -> Option<&'static str> {
    crate::platforms::platform_for_url(feed_url).map(|p| p.id)
}

// One owner for the platform health sample: ok/failed both derive from the feed status.
async fn record_platform_outcome(stats: &PlatRoundStats, feed_url: &str, ok: bool) {
    let Some(p) = platform_of_url(feed_url) else {
        return;
    };
    if ok {
        stats.ok.lock().await.insert(p);
    } else {
        stats.failed.lock().await.insert(p);
    }
}

fn marks_cookie_expired(error: &anyhow::Error, err_msg: &str) -> bool {
    let mut auth_expired = false;
    let mut source_forbidden = false;
    for cause in error.chain() {
        match cause.downcast_ref::<crate::platforms::PlatformError>() {
            Some(crate::platforms::PlatformError::AuthExpired { .. }) => auth_expired = true,
            Some(crate::platforms::PlatformError::SourceForbidden { .. }) => {
                source_forbidden = true;
            }
            _ => {}
        }
    }
    auth_expired || (!source_forbidden && err_msg.to_lowercase().contains("cookie"))
}

async fn maybe_notify_platform_down(bot: &Bot, state: &AppState, platform: &str, err_msg: &str) {
    let health_key = db::setting_key::platform_health(platform);
    let flipped = match db::flip_setting_once(&state.db, &health_key, "down").await {
        Ok(f) => f,
        Err(e) => {
            warn!(platform = platform, error = %e, "platform down flip failed, alert may be lost this round");
            return;
        }
    };
    if !flipped {
        return;
    }

    let label = crate::platforms::find_platform(platform)
        .map(|p| p.label)
        .unwrap_or(platform);

    let delivered = notify_admins_html(bot, state, |lang| {
        i18n::t(lang, "notif_platform_down")
            .replacen("{}", label, 1)
            .replacen("{}", &crate::push::escape_html(err_msg), 1)
    })
    .await;
    if !delivered {
        if let Err(e) = db::set_setting(&state.db, &health_key, "ok").await {
            warn!(platform = platform, error = %e, "failed to restore health bit after undelivered alert");
        } else {
            warn!(
                platform = platform,
                "platform-down alert undelivered; will retry next round"
            );
        }
    }
}

async fn maybe_notify_platform_recovered(bot: &Bot, state: &AppState, platform: &str) {
    let health_key = db::setting_key::platform_health(platform);
    let was_down = matches!(
        db::get_setting(&state.db, &health_key).await,
        Ok(Some(v)) if v == "down"
    );
    if !was_down {
        return;
    }

    let flipped = match db::flip_setting_once(&state.db, &health_key, "ok").await {
        Ok(f) => f,
        Err(e) => {
            warn!(platform = platform, error = %e, "platform recovery flip failed");
            return;
        }
    };
    if !flipped {
        return;
    }

    db::set_setting(
        &state.db,
        &db::setting_key::cookie_expired_notified(platform),
        "0",
    )
    .await
    .ok();

    let label = crate::platforms::find_platform(platform)
        .map(|p| p.label)
        .unwrap_or(platform);
    let delivered = notify_admins_html(bot, state, |lang| {
        i18n::t(lang, "notif_platform_recovered").replacen("{}", label, 1)
    })
    .await;
    if !delivered {
        if let Err(e) = db::set_setting(&state.db, &health_key, "down").await {
            warn!(platform = platform, error = %e, "failed to restore health bit after undelivered recovery");
        } else {
            warn!(
                platform = platform,
                "platform-recovered alert undelivered; will retry next round"
            );
        }
    }
}

pub async fn register_outbox_retry_job(
    sched: &JobScheduler,
    state: AppState,
    bot: Bot,
) -> anyhow::Result<()> {
    sched
        .add(Job::new_async(
            "0 0,5,10,15,20,25,30,35,40,45,50,55 * * * *",
            move |_uuid, _l| {
                let state = state.clone();
                let bot = bot.clone();
                Box::pin(async move {
                    if let Err(e) = run_outbox_retry(&state, &bot).await {
                        error!(error = %e, "outbox retry cycle failed");
                    }
                })
            },
        )?)
        .await?;
    Ok(())
}

fn outbox_backoff_secs(attempts: i64) -> i64 {
    match attempts {
        0 => 60,
        1 => 300,
        2 => 900,
        3 => 1800,
        4 => 3600,
        _ => 7200,
    }
}

const OUTBOX_ABANDON_SECS: i64 = 72 * 3600;
const OUTBOX_UNATTEMPTED_MAX_SECS: i64 = 7 * 24 * 3600;

const OUTBOX_BATCH_LIMIT: i64 = 25;

fn migrated_chat_id_from_error(e: &anyhow::Error) -> Option<String> {
    if let Some(teloxide::RequestError::MigrateToChatId(id)) =
        e.downcast_ref::<teloxide::RequestError>()
    {
        return Some(id.0.to_string());
    }
    let text = format!("{:#}", e);
    let marker = "supergroup with ID #";
    let start = text.find(marker)? + marker.len();
    let id: String = text[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    (!id.is_empty()).then_some(id)
}

async fn record_send_failure(
    bot: &Bot,
    state: &AppState,
    sub: &db::SubRow,
    cfg: &db::EffectiveConfig,
    lang: Lang,
    feed_title: &str,
    err: &anyhow::Error,
) -> bool {
    let key = db::setting_key::send_fail(&cfg.chat_id, &cfg.feed_url);
    let stored = match db::get_setting(&state.db, &key).await {
        Ok(v) => v.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
        Err(e) => {
            warn!(feed = %cfg.feed_url, error = %e, "send-failure counter read failed");
            return false;
        }
    };
    let count = stored + 1;
    if let Err(e) = db::set_setting(&state.db, &key, &count.to_string()).await {
        warn!(feed = %cfg.feed_url, error = %e, "send-failure counter write failed");
        return false;
    }
    if count < MAX_SEND_FAILURES {
        return false;
    }

    let err_str = format!("{:#}", err);
    let permanent = sendpolicy::is_permanent_chat_error(&err_str);
    match db::auto_disable_subscription(&state.db, sub.id).await {
        Ok(true) => {
            let name = if feed_title.is_empty() {
                &cfg.feed_url
            } else {
                feed_title
            };
            let msg = i18n::t(lang, "notif_send_failed_disabled")
                .replacen("{}", &push::escape_html(name), 1)
                .replacen("{}", &count.to_string(), 1);
            notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
            warn!(
                feed = %cfg.feed_url,
                failures = count,
                permanent,
                error = %err_str,
                "source auto-disabled after repeated send failures"
            );
            true
        }
        Ok(false) => false,
        Err(e) => {
            warn!(feed = %cfg.feed_url, error = %e, "failed to auto-disable after send failures");
            false
        }
    }
}

async fn clear_send_failure(state: &AppState, chat_id: &str, feed_url: &str) {
    if let Err(e) =
        db::delete_setting(&state.db, &db::setting_key::send_fail(chat_id, feed_url)).await
    {
        debug!(feed = %feed_url, error = %e, "failed to clear send-failure counter");
    }
}

fn outbox_abandon_due(
    created_at: i64,
    first_attempt_at: Option<i64>,
    attempts: i64,
    now: i64,
) -> bool {
    if attempts <= 0 {
        return now - created_at >= OUTBOX_UNATTEMPTED_MAX_SECS;
    }
    let anchor = first_attempt_at.unwrap_or(created_at);
    now - anchor >= OUTBOX_ABANDON_SECS
}

async fn abandon_outbox_row(bot: &Bot, state: &AppState, row: &db::OutboxRow, reason_key: &str) {
    let lang = match db::get_chat_owner(&state.db, &row.chat_id).await {
        Ok(Some(owner)) => owner_lang(state, owner).await,
        _ => Lang::En,
    };
    let entry_link = extract_entry_link_from_json(&row.entry_json);
    let msg = push_fail_msg(
        lang,
        i18n::t(lang, "notif_abandoned"),
        &row.feed_title,
        i18n::t(lang, reason_key),
        "label_source",
        entry_link.as_deref().unwrap_or(""),
    );
    notify_owner_html(bot, state, &row.chat_id, &msg).await;
    if let Err(e) = db::mark_entry_sent(&state.db, &row.chat_id, &row.feed_url, &row.entry_id).await
    {
        warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to mark abandoned entry as sent; keeping the row");
        return;
    }
    if let Err(e) = db::outbox_delete(&state.db, row.id).await {
        warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete abandoned entry");
    }
    db::delete_setting(&state.db, &db::setting_key::outbox_media_lost(row.id))
        .await
        .ok();
}

async fn maybe_notify_outbox_media_lost(
    bot: &Bot,
    state: &AppState,
    row: &db::OutboxRow,
    original: usize,
    remaining: usize,
) {
    let key = db::setting_key::outbox_media_lost(row.id);
    match db::get_setting(&state.db, &key).await {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(e) => {
            warn!(entry_id = %row.entry_id, error = %e, "outbox media-lost flag read failed, skipping notice");
            return;
        }
    }
    let lang = match db::get_chat_owner(&state.db, &row.chat_id).await {
        Ok(Some(owner)) => owner_lang(state, owner).await,
        Ok(None) => {
            warn!(chat_id = %row.chat_id, "cannot notify media loss: chat has no owner");
            return;
        }
        Err(e) => {
            warn!(chat_id = %row.chat_id, error = %e, "cannot notify media loss: owner lookup failed");
            return;
        }
    };
    let mut msg = i18n::t(lang, "notif_outbox_media_expired")
        .replacen("{}", &original.to_string(), 1)
        .replacen("{}", &remaining.to_string(), 1)
        .replacen("{}", &push::escape_html(&row.feed_title), 1);
    let entry_link = extract_entry_link_from_json(&row.entry_json);
    push_line(
        &mut msg,
        lang,
        "label_source",
        entry_link.as_deref().unwrap_or(""),
    );
    notify_owner_html(bot, state, &row.chat_id, &msg).await;
    if let Err(e) = db::set_setting(&state.db, &key, "1").await {
        warn!(entry_id = %row.entry_id, error = %e, "outbox media-lost flag write failed, notice may repeat");
    }
}

async fn run_outbox_retry(state: &AppState, bot: &Bot) -> anyhow::Result<()> {
    if state
        .outbox_retry_active
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::Acquire,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_err()
    {
        return Ok(());
    }
    let _guard = crate::state::ActiveFlagGuard(state.outbox_retry_active.clone());

    let now = Timestamp::now().as_second();
    let allowed_owner_ids = state.config.allowed_owner_ids();
    match db::delete_ineligible_owner_outbox_before(
        &state.db,
        now.saturating_sub(OUTBOX_UNATTEMPTED_MAX_SECS),
        &allowed_owner_ids,
    )
    .await
    {
        Ok(0) => {}
        Ok(n) => debug!(
            count = n,
            "silently removed outbox rows for ineligible owners"
        ),
        Err(e) => warn!(error = %e, "failed to purge ineligible-owner outbox rows"),
    }
    let due_ids =
        db::outbox_due_ids(&state.db, now, OUTBOX_BATCH_LIMIT, &allowed_owner_ids).await?;
    if due_ids.is_empty() {
        return Ok(());
    }
    if due_ids.len() as i64 == OUTBOX_BATCH_LIMIT {
        warn!(
            batch = OUTBOX_BATCH_LIMIT,
            "outbox retry batch is full; a backlog may be forming"
        );
    }

    let due_count = due_ids.len();
    let round_started = std::time::Instant::now();
    let mut sent = 0usize;
    let mut retried = 0usize;
    let mut abandoned = 0usize;
    let mut dropped = 0usize;
    let mut skipped = 0usize;

    let transport = crate::media::MediaTransport::new(&state.config);

    for id in due_ids {
        let row = match db::outbox_get(&state.db, id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                dropped += 1;
                continue;
            }
            Err(e) => {
                warn!(outbox_id = id, error = %e, "outbox: row fetch failed, skipping");
                skipped += 1;
                continue;
            }
        };
        match db::is_entry_sent(&state.db, &row.chat_id, &row.feed_url, &row.entry_id).await {
            Ok(true) => {
                info!(entry_id = %row.entry_id, feed = %row.feed_url, "outbox entry already sent, dropping");
                if let Err(e) = db::outbox_delete(&state.db, row.id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete entry");
                }
                dropped += 1;
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                warn!(entry_id = %row.entry_id, error = %e, "outbox: is_entry_sent check failed, skipping this round (fail-closed)");
                skipped += 1;
                continue;
            }
        }

        let row_now = Timestamp::now().as_second();
        if outbox_abandon_due(row.created_at, row.first_attempt_at, row.attempts, row_now) {
            warn!(
                entry_id = %row.entry_id,
                feed = %row.feed_url,
                attempts = row.attempts,
                "outbox entry abandoned after 72h of retries"
            );
            abandon_outbox_row(bot, state, &row, "notif_timeout_72h").await;
            abandoned += 1;
            continue;
        }

        let entry: feed_rs::model::Entry = match serde_json::from_str(&row.entry_json) {
            Ok(e) => e,
            Err(e) => {
                error!(entry_id = %row.entry_id, error = %e, "outbox: failed to deserialize entry");
                abandon_outbox_row(bot, state, &row, "notif_entry_unreadable").await;
                abandoned += 1;
                continue;
            }
        };

        let all_media: Vec<crate::media::MediaItem> = if row.media_json.trim().is_empty() {
            warn!(entry_id = %row.entry_id, "outbox: media_json is empty");
            Vec::new()
        } else {
            serde_json::from_str(&row.media_json).unwrap_or_else(|e| {
                warn!(entry_id = %row.entry_id, error = %e, "outbox: media_json deserialize failed");
                Vec::new()
            })
        };
        let had_media = !all_media.is_empty();
        let original_count = all_media.len();
        let media_items: Vec<crate::media::MediaItem> =
            all_media.into_iter().filter(|m| m.path.exists()).collect();
        let media_lost = had_media && media_items.len() < original_count;

        let lang = match db::get_chat_owner(&state.db, &row.chat_id).await {
            Ok(Some(owner)) => owner_lang(state, owner).await,
            _ => Lang::En,
        };
        let opts = push::RenderOpts {
            mode: ContentMode::from_str(&row.content_mode),
            source: push::SourceDisplay::from_str(&row.source_display),
            link_preview: row.link_preview,
            lang,
            allow_rich: false,
        };

        if opts.mode == ContentMode::MediaOnly && media_items.is_empty() {
            let reason = if had_media {
                "notif_media_expired"
            } else {
                "notif_media_unreadable"
            };
            warn!(
                entry_id = %row.entry_id,
                feed = %row.feed_url,
                "outbox entry abandoned: MediaOnly without sendable media"
            );
            abandon_outbox_row(bot, state, &row, reason).await;
            abandoned += 1;
            continue;
        }

        if media_lost {
            if !media_items.is_empty() {
                warn!(
                    entry_id = %row.entry_id,
                    feed = %row.feed_url,
                    original = original_count,
                    remaining = media_items.len(),
                    "outbox retry: some media files expired, sending with reduced media"
                );
            } else {
                warn!(
                    entry_id = %row.entry_id,
                    feed = %row.feed_url,
                    original = original_count,
                    "outbox retry: all media files expired, sending as text"
                );
            }
            maybe_notify_outbox_media_lost(bot, state, &row, original_count, media_items.len())
                .await;
        }
        let chat_id: i64 = match row.chat_id.parse() {
            Ok(id) => id,
            Err(_) => {
                warn!(chat_id = %row.chat_id, entry_id = %row.entry_id, "outbox: invalid chat_id, dropping entry");
                if let Err(e) = db::outbox_delete(&state.db, row.id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete entry");
                }
                dropped += 1;
                continue;
            }
        };
        let author = row.author.as_deref();

        match db::is_entry_sent(&state.db, &row.chat_id, &row.feed_url, &row.entry_id).await {
            Ok(true) => {
                info!(entry_id = %row.entry_id, feed = %row.feed_url, "outbox entry sent between fetch and send, dropping");
                if let Err(e) = db::outbox_delete(&state.db, row.id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete entry");
                }
                dropped += 1;
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                warn!(entry_id = %row.entry_id, error = %e, "outbox: second is_entry_sent check failed, skipping this round (fail-closed)");
                skipped += 1;
                continue;
            }
        }

        let outbox_meta: Option<crate::entrymeta::EntryMeta> = if row.meta_json.is_empty() {
            None
        } else {
            serde_json::from_str(&row.meta_json).ok()
        };
        let meta_ref = outbox_meta.as_ref();

        let result = if media_items.is_empty() {
            push::send_text_only(
                bot,
                chat_id,
                &entry,
                &row.feed_title,
                author,
                opts,
                meta_ref,
            )
            .await
        } else {
            push::send_entry_with_media(
                bot,
                &state.db,
                chat_id,
                push::PushCtx {
                    entry: &entry,
                    feed_title: &row.feed_title,
                    author,
                    opts,
                    caption_above_media: row.caption_above_media,
                    media: &media_items,
                    meta: meta_ref,
                },
                &transport,
            )
            .await
        };

        match result {
            Ok(send_outcome) => {
                info!(entry_id = %row.entry_id, feed = %row.feed_url, "outbox retry succeeded");
                sent += 1;
                let needs_notice = send_outcome.text_partial
                    || send_outcome.media_failed
                    || send_outcome.uncertain;
                let (chat_title, lang, owner_id) = if needs_notice {
                    let chat_title = db::get_chat(&state.db, &row.chat_id)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|c| c.title.or(c.username))
                        .unwrap_or_else(|| row.chat_id.clone());
                    let (lang, owner_id) = match db::get_chat_owner(&state.db, &row.chat_id).await {
                        Ok(Some(owner)) => (owner_lang(state, owner).await, Some(owner)),
                        _ => (Lang::En, None),
                    };
                    (chat_title, lang, owner_id)
                } else {
                    (String::new(), Lang::En, None)
                };
                let entry_link = fetcher::entry_link(&entry).unwrap_or_default();
                let effects = SendSuccessCtx {
                    bot,
                    state,
                    chat_id: &row.chat_id,
                    feed_url: &row.feed_url,
                    entry_id: &row.entry_id,
                    chat_title: &chat_title,
                    lang,
                };
                apply_send_success(
                    &effects,
                    &send_outcome,
                    &SendSuccessExtras {
                        owner_id,
                        feed_title: &row.feed_title,
                        entry_link: &entry_link,
                        download_warnings: &[],
                        oversized: &[],
                        notify_oversized: false,
                        dl_retry_note: "",
                        outbox_row_id: Some(row.id),
                    },
                    "outbox: failed to mark entry sent (may re-push on re-subscribe)",
                )
                .await;
            }
            Err(e) => {
                if let Some(new_id) = migrated_chat_id_from_error(&e) {
                    match db::migrate_chat_id(&state.db, &row.chat_id, &new_id).await {
                        Ok(()) => {
                            info!(
                                old_chat_id = %row.chat_id,
                                new_chat_id = %new_id,
                                "outbox row migrated after send error"
                            );
                            skipped += 1;
                            continue;
                        }
                        Err(me) => warn!(
                            old_chat_id = %row.chat_id,
                            new_chat_id = %new_id,
                            error = %me,
                            "outbox chat id migration failed"
                        ),
                    }
                }
                let attempts = row.attempts + 1;
                retried += 1;
                let backoff = outbox_backoff_secs(attempts);
                let retry_now = Timestamp::now().as_second();
                let next_retry = retry_now + backoff;
                warn!(
                    entry_id = %row.entry_id,
                    feed = %row.feed_url,
                    attempts = attempts,
                    next_in_secs = backoff,
                    error = %e,
                    "outbox retry failed"
                );
                if let Err(e) = db::outbox_update_retry(
                    &state.db,
                    row.id,
                    attempts,
                    next_retry,
                    retry_now,
                    &format!("{:#}", e),
                )
                .await
                {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to update retry state");
                }
            }
        }
    }
    info!(
        due = due_count,
        sent,
        retried,
        abandoned,
        dropped,
        skipped,
        elapsed_ms = round_started.elapsed().as_millis() as u64,
        "outbox retry: summary"
    );
    Ok(())
}

fn extract_entry_link_from_json(entry_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(entry_json).ok()?;
    v.get("links")
        .and_then(|l| l.as_array())
        .and_then(|arr| arr.first())
        .and_then(|l| l.get("href"))
        .and_then(|h| h.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_expiry_classification_ignores_source_forbidden_details() {
        let forbidden = crate::platforms::source_forbidden("X", 403, "cookie challenge");
        assert!(!marks_cookie_expired(
            &forbidden,
            &format!("{:#}", forbidden)
        ));
        let expired = crate::platforms::auth_expired("X", "HTTP 401");
        assert!(marks_cookie_expired(&expired, &format!("{:#}", expired)));
        assert!(marks_cookie_expired(
            &anyhow::anyhow!("Weibo cookie invalid"),
            "Weibo cookie invalid"
        ));
    }

    #[test]
    fn outbox_abandon_requires_an_attempt_and_uses_first_attempt() {
        let now = 1_000_000_000;
        assert!(!outbox_abandon_due(now - 5 * 24 * 3600, None, 0, now));
        assert!(outbox_abandon_due(
            now - OUTBOX_UNATTEMPTED_MAX_SECS,
            None,
            0,
            now
        ));
        assert!(outbox_abandon_due(
            now - 10 * 24 * 3600,
            Some(now - OUTBOX_ABANDON_SECS - 1),
            5,
            now
        ));
        assert!(!outbox_abandon_due(
            now - 10 * 24 * 3600,
            Some(now - 3600),
            2,
            now
        ));
        assert!(outbox_abandon_due(now - OUTBOX_ABANDON_SECS, None, 1, now));
    }

    #[test]
    fn stale_zero_counts_up_and_warns_on_multiples() {
        assert_eq!(stale_zero_next(0), (1, false));
        assert_eq!(stale_zero_next(18), (19, false));
        assert_eq!(stale_zero_next(STALE_ZERO_WARN_ROUNDS - 1), (20, true));
        assert_eq!(stale_zero_next(39), (40, true));
        assert_eq!(stale_zero_next(40), (41, false));
        assert_eq!(stale_zero_next(60), (61, false));
    }
}
