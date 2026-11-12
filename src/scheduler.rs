use teloxide::prelude::*;
use tokio::sync::Semaphore;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{debug, error, info, warn};

use crate::db;
use crate::fetcher;
use crate::i18n::{self, Lang};
use crate::notify::{
    notify_admins_html, notify_download_failed, notify_oversized_video, notify_owner_html,
    owner_lang, push_fail_msg, push_line,
};
use crate::push::{self, ContentMode};
use crate::sendpolicy;
use crate::state::AppState;

/// Consecutive send failures for one source before it is auto-disabled.
/// Fetch failures already have their own counter; this covers the
/// "fetch succeeds, every send fails" case (removed from channel, missing
/// posting rights), which previously kept enqueueing until the 72h abandon.
const MAX_SEND_FAILURES: i64 = 3;

/// Upper bound of queued outbox rows per (chat, feed). Reaching it disables
/// the source instead of letting an unreachable chat grow the queue without
/// bound.
const OUTBOX_PER_FEED_LIMIT: i64 = 20;

/// Fixed scheduler polling tick. Actual fetch intervals still come from each
/// source/channel's `interval_minutes`.
const SCHEDULER_INTERVAL_MINUTES: u64 = 5;

/// Register the RSS fetch job on the scheduler.
pub async fn register_fetch_job(
    sched: &JobScheduler,
    state: AppState,
    bot: Bot,
) -> anyhow::Result<()> {
    let interval = SCHEDULER_INTERVAL_MINUTES;
    // The cron minute field only accepts steps of 1-59: large intervals (such
    // as the 3/6/12-hour options in the panel) cannot be written directly as
    // */180 — that is an invalid expression. A fixed 5-minute tick is used;
    // whether each source is due is decided precisely by the
    // get_due_subscriptions SQL using its own interval.
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

    info!(interval_min = interval, tick_min = interval, "fetch job registered");
    Ok(())
}

/// Register the auto-heal job: periodically re-enable subscriptions that were
/// auto-disabled by consecutive fetch failures, once the cooldown has passed.
/// A network-wide outage (proxy down, firewall blocking, etc.) causes all
/// sources to be auto-disabled one after another; once the network recovers,
/// this job brings them back automatically with no manual intervention.
/// Manually paused sources (auto_disabled=0) are unaffected. Silent when
/// healthy — only writes logs.
pub async fn register_heal_job(sched: &JobScheduler, state: AppState) -> anyhow::Result<()> {
    // Check once at the top of every hour; cooldown is 3 hours (see
    // AUTO_HEAL_COOLDOWN_HOURS).
    const AUTO_HEAL_COOLDOWN_HOURS: i64 = 3;
    let expr = "0 0 * * * *";

    sched
        .add(Job::new_async(expr, move |_uuid, _l| {
            let state = state.clone();
            Box::pin(async move {
                match db::reenable_due_auto_disabled(&state.db, AUTO_HEAL_COOLDOWN_HOURS).await {
                    Ok(0) => {}
                    Ok(n) => info!(count = n, "auto-heal: re-enabled subscriptions after cooldown"),
                    Err(e) => error!(error = %e, "auto-heal job failed"),
                }
            })
        })?)
        .await?;

    info!(cooldown_hours = AUTO_HEAL_COOLDOWN_HOURS, "auto-heal job registered");
    Ok(())
}

/// Read the platform hysteresis streak counter (positive = failure streak,
/// negative = success streak). Returns Some(n) for a valid stored value (or
/// 0 when the key is absent — that genuinely means "no streak yet"), or None
/// on a transient DB read error. Callers must NOT write the streak back when
/// this returns None: collapsing a read error to 0 and then writing 0 would
/// reset an accumulated streak on a single DB hiccup (a DB read error must
/// not unwrap_or to a neutral value — for counters, skip the round instead). A missing key still yields Some(0) so the normal
/// "first failure → 1" flow proceeds.
async fn read_streak(state: &AppState, key: &str, platform: &'static str) -> Option<i64> {
    match db::get_setting(&state.db, key).await {
        Ok(v) => Some(v.and_then(|s| s.parse().ok()).unwrap_or(0)),
        Err(e) => {
            warn!(platform = platform, error = %e, "streak read failed, deferring to next round");
            None
        }
    }
}

/// Consecutive full-body fetches that parsed zero entries before the stale-zero
/// warn fires. Default fetch interval is 30 min, so 20 rounds ≈ 10 hours of
/// complete silence — long enough that even a low-activity feed with a healthy
/// parser rarely trips it, short enough to catch a page redesign within a day.
const STALE_ZERO_WARN_ROUNDS: i64 = 20;

/// Pure next-step computation of the stale-zero streak (unit-testable):
/// returns (new_streak, should_warn). Warn fires on every exact multiple of
/// the threshold while the streak persists — not once per edge — so a
/// source that stays broken keeps reminding without per-round spam.
fn stale_zero_next(prev: i64) -> (i64, bool) {
    let next = prev.saturating_add(1);
    (next, next % STALE_ZERO_WARN_ROUNDS == 0)
}

/// Structure-independent "silence detector" for a successful fetch round.
///
/// The per-platform zero-entry warns (weibo cards non-empty / X non-cursor
/// entries present / telegram blocks present) all require the platform's
/// response structure to still be recognized. If a redesign removes or renames
/// the container keys, the parser reports a *legitimately* empty result: those
/// warns never fire, the round counts as success, and the source goes silent
/// with zero log lines. This guard needs nothing from the response shape: an
/// HTTP-200-class fetch (not a 304) that yields zero entries repeatedly is the
/// only evidence it uses.
///
/// Behavior: streak persisted in settings (`stale_zero:<chat>:<feed>`); a
/// round with entries clears it; a DB read error skips the round rather than
/// collapsing to 0 (same counter contract as read_streak / media_dl_fail).
/// Log-only — no user notification: a genuinely quiet source (rarely-posting
/// account, emptied feed) is legitimate, so paging a human on false positives
/// would train everyone to ignore it.
async fn note_zero_entry_round(state: &AppState, sub: &db::SubRow, round_zero: bool) {
    let key = db::setting_key::stale_zero(&sub.chat_id, &sub.feed_url);
    if !round_zero {
        // Source produced entries again — streak restarts from scratch.
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

/// Per-round platform-level stats (global — no longer isolated per owner
/// since cookies were globalized). Bundled into a struct to avoid too many
/// arguments on process_feed (clippy too_many_arguments).
#[derive(Default)]
struct PlatRoundStats {
    /// Platforms scheduled this round and how many of their sources ran
    /// (regardless of success/failure).
    seen: tokio::sync::Mutex<std::collections::HashMap<&'static str, usize>>,
    /// Platforms that had failures this round
    failed: tokio::sync::Mutex<std::collections::HashSet<&'static str>>,
    /// Platforms that had successes this round
    ok: tokio::sync::Mutex<std::collections::HashSet<&'static str>>,
    /// Last error message (for display in anomaly notifications)
    last_err: tokio::sync::Mutex<std::collections::HashMap<&'static str, String>>,
    /// Number of active sources per platform. A platform-level health verdict
    /// is only valid when every active source was scheduled this round; with
    /// mixed per-source intervals a single due source is not a whole-platform
    /// sample.
    active_counts: std::collections::HashMap<&'static str, usize>,
}

/// One full fetch cycle: find due subscriptions, fetch, push new entries.
/// Media/pipeline warnings notify the channel owner immediately
/// (notify_owner_html) instead of being batched until the end of the cycle.
async fn run_fetch_cycle(state: &AppState, bot: &Bot) -> anyhow::Result<()> {
    // If the previous round has not finished (a round can exceed the trigger
    // interval when media downloads are slow), skip this tick to avoid two
    // concurrent rounds both passing the is_entry_sent check and pushing the
    // same entry twice.
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

    // At most MAX_DUE_PER_CYCLE sources are processed per round; the rest
    // stay due and are picked up by later rounds (oldest-first rotation).
    // `total_due` is the unbounded count for observability only.
    let total_due = db::count_due_subscriptions(&state.db).await?;
    let due = db::get_due_subscriptions(&state.db).await?;
    if due.is_empty() {
        return Ok(());
    }
    let selected_due = due.len();
    // Cycle-level timing + totals, reported by the summary log at the end of
    // the round (see below). Kept local to run_fetch_cycle — no signature
    // change to process_feed.
    let cycle_started = std::time::Instant::now();
    info!(
        count = due.len(),
        due_total = total_due,
        skipped = total_due.saturating_sub(due.len() as i64),
        "fetch cycle: due subscriptions selected"
    );

    // Per-round platform-level stats: seen = platforms scheduled this round,
    // failed = platforms with failures this round, ok = platforms with
    // successes, last_err = last error message.
    // Used for whole-round hysteresis decisions, avoiding anomaly/recovery
    // notification spam caused by single-round jitter.
    // Criterion: a round counts as a failure round only if ALL sources failed,
    // and as a success round only if ALL succeeded (mixed rounds reset the
    // streak), preventing one permanently failing source (when an owner has
    // multiple sources) from dragging the whole platform into a persistent
    // down state that never recovers.
    let active_counts = match db::active_subscription_urls(&state.db).await {
        Ok(urls) => {
            let mut counts: std::collections::HashMap<&'static str, usize> =
                std::collections::HashMap::new();
            for url in urls {
                if let Some(p) = platform_of_url(&url) {
                    *counts.entry(p).or_insert(0) += 1;
                }
            }
            counts
        }
        Err(e) => {
            warn!(error = %e, "active subscription count failed; platform health uses the due sample only");
            std::collections::HashMap::new()
        }
    };
    let stats = std::sync::Arc::new(PlatRoundStats {
        active_counts,
        ..PlatRoundStats::default()
    });

    // Separate platform sources (weibo/X) from plain RSS sources:
    // platform sources run serially with random gaps (reducing the chance of
    // triggering risk control); plain sources run in parallel.
    let (platform_subs, rss_subs): (Vec<_>, Vec<_>) = due
        .into_iter()
        .partition(|(sub, _)| platform_of_url(&sub.feed_url).is_some());

    // Plain RSS sources: the fetch request itself is bounded by
    // `state.fetch_semaphore` inside fetcher::fetch_feed
    // (config::FETCH_CONCURRENCY, which also covers /sub and /test).
    // `process_sem` bounds the whole per-source processing (fetch + media
    // downloads + sends): a task keeps its parsed FeedResult alive until its
    // last entry is pushed, so without this cap the retained memory would
    // scale with the number of due subscriptions instead of a fixed limit.
    // Platform sources stay serial and are deliberately not gated so they keep
    // their priority.
    let process_sem = std::sync::Arc::new(Semaphore::new(crate::config::FETCH_CONCURRENCY));
    // Each handle is paired with a copy of the feed_url: once sub is moved
    // into the spawn closure, the source identifier can no longer be obtained
    // at the join site and failure logs would lose their locator info
    // (added in v1.10.7).
    let mut handles: Vec<(String, tokio::task::JoinHandle<anyhow::Result<FeedStatus>>)> = Vec::new();
    for (idx, (sub, cfg)) in rss_subs.into_iter().enumerate() {
        let state = state.clone();
        let bot = bot.clone();
        let stats = stats.clone();
        let process_sem = process_sem.clone();
        let feed_url = sub.feed_url.clone();
        handles.push((feed_url, tokio::spawn(async move {
            // Staggered start: avoid all sources hitting their respective
            // servers/proxies concurrently at the same instant, forming a
            // momentary burst (inspired by rsstt's sharded staggered
            // scheduling, simplified to sequential delays at this scale).
            if idx > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(600 * idx as u64)).await;
            }
            let _permit = process_sem.acquire_owned().await?;
            process_feed(&state, &bot, sub, cfg, &stats).await
        })));
    }

    // Platform sources (weibo/X): strictly serial + random 3-8 second wait
    // before each request. Weibo risk control is sensitive to burst
    // concurrency; serial execution + random gaps effectively lowers the 403
    // probability.
    let mut first_platform = true;
    let mut plat_ok = 0usize;
    let mut plat_fail = 0usize;
    for (sub, cfg) in platform_subs {
        // Non-first platform source: random 3-8 second delay (pseudo-random
        // via nanosecond modulo, no rand dependency needed)
        if !first_platform {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos();
            let delay_ms = 3000 + (nanos % 5000) as u64;
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        first_platform = false;
        // sub is moved into process_feed; failure logs need the source
        // identifier, so extract it first (v1.10.7).
        let feed_url = sub.feed_url.clone();
        let chat_id = sub.chat_id.clone();
        match process_feed(state, bot, sub, cfg, &stats).await {
            Ok(FeedStatus::Ok) => plat_ok += 1,
            Ok(FeedStatus::Failed) => plat_fail += 1,
            Err(e) => {
                plat_fail += 1;
                error!(feed = %feed_url, chat_id = %chat_id, error = %e, "platform feed processing failed");
            }
        }
    }

    // Wait for the plain-source tasks to finish
    let mut rss_ok = 0usize;
    let mut rss_fail = 0usize;
    for (feed_url, handle) in handles {
        match handle.await {
            Ok(Ok(FeedStatus::Ok)) => rss_ok += 1,
            // The failure was already logged and handled inside process_feed.
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

    // One-line cycle summary at info level. Rationale: the individual
    // per-feed failures above each already emit their own error!, but there
    // was no aggregate view — an operator looking at logs after the fact
    // could not tell at a glance "how many subs ran, how many succeeded, how
    // long did the round take". This is the cheapest place to expose that
    // (counters already collected above; no extra DB or platform queries).
    // Deliberately NOT logging per-feed success info! lines to avoid volume.
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

    // Platform-level health hysteresis (whole-round view, serial so no race,
    // global per platform):
    // report an anomaly only after 2 consecutive all-failure rounds; report
    // recovery only after 2 consecutive all-success rounds.
    // This avoids "anomaly → recovery" notification spam caused by single-round
    // network jitter.
    // Criterion (v1.4.3 fix): a "failure round" = all of this platform's
    // sources failed this round; a "success round" = all succeeded; mixed
    // rounds reset the streak. Previously any single source failure marked the
    // round as failed, so with multiple sources one permanently failing source
    // (e.g. account deleted) would drag the whole platform into a persistent
    // down state that the other healthy sources could never pull back.
    // v1.11.0: after cookies were globalized, the health stats were globalized
    // accordingly (no longer isolated per owner); anomaly/recovery
    // notifications go to the single admin (v1.11.3).
    const HYSTERESIS_ROUNDS: i64 = 2;
    let seen = stats.seen.lock().await.clone();
    let failed = stats.failed.lock().await.clone();
    let ok_set = stats.ok.lock().await.clone();
    let last_err = stats.last_err.lock().await.clone();
    for (&p, &seen_count) in &seen {
        // Only judge a platform when every active source was scheduled this
        // round. With mixed per-source intervals, a single due source (which
        // happens to fail) is not evidence that the whole platform is down.
        // If the active-count query failed, fall back to the due sample.
        let active_count = stats.active_counts.get(p).copied().unwrap_or(seen_count);
        if seen_count < active_count {
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

        let has_failed = failed.contains(&p);
        let has_ok = ok_set.contains(&p);
        // Nature of this round: all-failed / all-ok / mixed (or no valid signal)
        let all_failed = has_failed && !has_ok;
        let all_ok = has_ok && !has_failed;

        if all_failed {
            // All failed this round: increment the failure streak counter
            // (positive = failure streak, starting from 1; if the previous
            // direction was success (negative), reset to 1).
            // On a transient read error (None), skip both the write-back and
            // the down-notification this round — the next round re-reads and
            // continues accumulating, preserving the streak value.
            if let Some(prev) = read_streak(state, &streak_key, p).await {
                let streak = if prev > 0 { prev + 1 } else { 1 };
                if let Err(e) = db::set_setting(&state.db, &streak_key, &streak.to_string()).await {
                    warn!(platform = p, error = %e, "streak write failed");
                }

                // Hysteresis threshold reached and not currently down: report
                // the anomaly (notify the single admin)
                if streak >= HYSTERESIS_ROUNDS && !is_down {
                    let err = last_err.get(&p).map(String::as_str).unwrap_or("unknown error");
                    maybe_notify_platform_down(bot, state, p, err).await;
                }
            }
        } else if all_ok {
            // All succeeded this round: increment the success streak counter.
            // Same "defer on read error" rule as the failure branch.
            if let Some(streak) = read_streak(state, &streak_key, p).await {
                // The success direction counts with negative numbers (-1, -2, ...)
                let ok_streak = if streak < 0 { streak - 1 } else { -1 };
                if let Err(e) = db::set_setting(&state.db, &streak_key, &ok_streak.to_string()).await {
                    warn!(platform = p, error = %e, "streak write failed");
                }

                // Hysteresis threshold reached and currently down: report recovery
                // (notify the single admin)
                if ok_streak <= -HYSTERESIS_ROUNDS && is_down {
                    maybe_notify_platform_recovered(bot, state, p).await;
                }
            }
        } else {
            // Mixed round (both successes and failures): reset the streak,
            // leave the health state unchanged.
            if let Err(e) = db::set_setting(&state.db, &streak_key, "0").await {
                warn!(platform = p, error = %e, "streak reset failed");
            }
        }
    }

    Ok(())
}

/// Cooperative deadline for processing a single source (seconds). After the
/// timeout, no new entry starts processing; remaining entries are left to the
/// next round (not marked sent, so the next round's re-fetch naturally picks
/// them up). This is "cooperative" rather than a hard cancellation: an entry
/// already being processed is allowed to finish (its internal download/send
/// each have their own timeout bounds), avoiding cutting off an in-flight
/// send and causing a duplicate push. The main purpose is protecting platform
/// sources' sliding windows — one stuck source slows the whole round, and if
/// it drags on long enough, other sources' entries roll out of the sliding
/// window, which is the real permanent loss.
const FEED_DEADLINE_SECS: u64 = 240;

/// Outcome of processing one feed. The failure branch handles errors and
/// notifications internally, so the cycle summary can count real successes
/// instead of treating every handled error as a success.
enum FeedStatus {
    Ok,
    Failed,
}

/// True when another sender already owns this entry: either it was delivered
/// (possibly by the outbox retry, after the batch snapshot was taken) or it is
/// queued in the outbox, which has sole retry authority. A DB error is an
/// error, never "not handled": the caller must defer rather than send.
async fn entry_already_handled(
    state: &AppState,
    chat_id: &str,
    feed_url: &str,
    entry_id: &str,
) -> anyhow::Result<bool> {
    let (sent, queued) = db::entry_dedup_state(&state.db, chat_id, feed_url, entry_id).await?;
    Ok(sent || queued)
}

/// Process a single source: fetch + push new entries in chronological order.
/// Parallel across sources, serial within a source.
/// All failure notifications are sent immediately, no longer batched until the
/// end of the cycle.
/// `stats` records this round's platform-level scheduling and success/failure
/// outcomes, for run_fetch_cycle to perform hysteresis evaluation after the
/// whole round ends.
async fn process_feed(
    state: &AppState,
    bot: &Bot,
    sub: db::SubRow,
    cfg: db::EffectiveConfig,
    stats: &PlatRoundStats,
) -> anyhow::Result<FeedStatus> {
    let transport = crate::media::MediaTransport::new(&state.config);

    // Resolve the chat owner early: used for owner-routed notifications
    // (auto-disable / media failures / oversized video) and language preference.
    // On a DB query error, log a warn and treat it as "no owner" (do not
    // silently swallow the error).
    // v1.11.0: after cookies were globalized, platform-source fetching no
    // longer depends on the owner; channels without an owner can also fetch
    // normally.
    let owner_id = match db::get_chat_owner(&state.db, &sub.chat_id).await {
        Ok(o) => o,
        Err(e) => {
            warn!(chat_id = %sub.chat_id, error = %e, "get_chat_owner failed, treating as no owner");
            None
        }
    };
    // Removing a user from ALLOWED_USER_IDS must revoke the data channel too,
    // not just interactive commands. Their subscriptions stop before any
    // fetch/media work; the row is kept so re-adding the user resumes it.
    if let Some(owner) = owner_id {
        if !state.config.is_allowed(owner) {
            info!(
                chat_id = %sub.chat_id,
                owner,
                feed = %sub.feed_url,
                "subscription owner is no longer whitelisted; skipping feed"
            );
            return Ok(FeedStatus::Ok);
        }
    }

    // Owner's language preference (for notification copy i18n); no owner
    // defaults to English.
    let lang = match owner_id {
        Some(o) => owner_lang(state, o).await,
        None => Lang::En,
    };

    // Trusted private RSSHub allowlist is scoped to the subscription owner:
    // background fetches use the same caller contract as interactive commands.
    // No owner resolves to false (allowlist is an explicit private bypass).
    let allow_trusted_private = match owner_id {
        Some(o) => state.config.trusted_private_allowed(o),
        None => false,
    };

    // Record the platforms scheduled this round (regardless of outcome), for
    // whole-round recovery evaluation (global criterion).
    if let Some(p) = platform_of_url(&sub.feed_url) {
        *stats.seen.lock().await.entry(p).or_insert(0) += 1;
    }

    // v1.10.3: debounce retry for transient errors on RSS sources (1 retry,
    // 2s backoff). Applies only to non-platform sources — platform sources
    // (weibo/x/telegram) have serial delays and risk-control strategies, and
    // blind retries may trigger rate limiting. Typical scenario: transient
    // proxy/network connection errors that recover on their own after a few
    // seconds; a single retry can save the round.
    let is_rss_source = platform_of_url(&sub.feed_url).is_none();
    // Batch sent-entry snapshot for platform enrichment only (Weibo detail /
    // vote). X/TG/RSS ignore the parameter entirely, so only Weibo pays for
    // this full-history query. If another platform starts consuming
    // `known_sent`, extend this gate instead of loading it unconditionally.
    // It is deliberately not used for the final push
    // filter: it is taken before the fetch and can go stale while an
    // overlapping outbox retry finishes the same entry. A failed read must
    // also fail closed — treating it as "nothing sent" would re-push the whole
    // source window.
    let known_sent_snapshot: Option<std::collections::HashSet<String>> =
        if platform_of_url(&sub.feed_url) == Some("weibo") {
            match db::sent_entry_ids(&state.db, &cfg.chat_id, &cfg.feed_url).await {
                Ok(s) => Some(s),
                Err(e) => {
                    warn!(
                        feed = %cfg.feed_url,
                        error = %e,
                        "sent-entry batch query failed, deferring feed"
                    );
                    return Ok(FeedStatus::Failed);
                }
            }
        } else {
            None
        };
    let result = fetcher::fetch_feed(
        state,
        &sub.feed_url,
        sub.http_etag.as_deref(),
        sub.http_last_modified.as_deref(),
        known_sent_snapshot.as_ref(),
        lang,
        allow_trusted_private,
    )
    .await;
    let result = match result {
        // Render the anyhow error chain with `{:#}` (alternates Display):
        // fetcher::fetch_feed wraps reqwest errors via `.with_context(...)`
        // so plain `e.to_string()` returns only the outer "request failed:
        // <url>" and the inner reqwest source string ("error sending request
        // for url ...", "connection closed", etc.) is dropped — is_net_error
        // would then never match on RSS connection-class failures. `{:#}`
        // appends the full source chain separated by ": ", letting the
        // substring check see the underlying reqwest error text.
        Err(ref e) if is_rss_source && sendpolicy::is_net_error(&format!("{:#}", e)) => {
            warn!(
                feed = %sub.feed_url,
                error = format!("{:#}", e),
                "fetch transient error, retrying once after 2s"
            );
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            fetcher::fetch_feed(
                state,
                &sub.feed_url,
                sub.http_etag.as_deref(),
                sub.http_last_modified.as_deref(),
                known_sent_snapshot.as_ref(),
                lang,
                allow_trusted_private,
            )
            .await
        }
        other => other,
    };
    // The enrichment snapshot is no longer needed once fetching (including the
    // one-shot retry) is done; release it before the much slower send loop.
    drop(known_sent_snapshot);

    let mut feed_failed = false;
    match result {
        Ok(fr) => {
            // Reset error count on success (a 304 "source not updated" also
            // counts as success).
            // Do not use `?`: a single DB hiccup should not invalidate the
            // whole round's fetch results (otherwise this round's entries
            // would all go unpushed and health stats would be missing); a
            // failed reset merely leaves a trace and is retried next round.
            if let Err(e) = db::reset_error_count(&state.db, sub.id).await {
                warn!(feed = %sub.feed_url, error = %e, "reset error count failed");
            }
            // After a successful fetch, clear the disable-notification flag so
            // the next failure can notify again (#9 fix)
            let notify_key = db::setting_key::auto_disable_notified(&sub.chat_id, &sub.feed_url);
            db::delete_setting(&state.db, &notify_key).await.ok();

            // Record that this platform had a success this round (for the
            // whole-round hysteresis all-success/mixed criterion).
            if let Some(p) = platform_of_url(&sub.feed_url) {
                stats.ok.lock().await.insert(p);
            }

            // 304 Not Modified: the source has no updates, nothing to push
            if fr.not_modified {
                return Ok(FeedStatus::Ok);
            }

            // Stale-zero detector (structure-independent): a 200 full-body
            // fetch that parses to zero entries extends the streak; any entry
            // produced clears it. A 304 round above never touches it (an
            // unchanged source is neither evidence of staleness nor health).
            note_zero_entry_round(state, &sub, fr.entries.is_empty()).await;

            // Backfill/refresh feed title when not manually overridden.
            // Covers subscriptions added while fetching was broken (title fell
            // back to the raw URL) and feeds whose title changes later.
            if sub.title_overridden == 0 {
                if let Some(t) = &fr.feed_title {
                    if Some(t.as_str()) != sub.feed_title.as_deref() {
                        db::rename_subscription_auto(&state.db, sub.id, t).await.ok();
                    }
                }
            }

            // Push new entries (skip already-sent)
            let feed_title = sub.feed_title.clone().unwrap_or_default();
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
                    // chat_id is not numeric (e.g. legacy dirty data): silently
                    // sending to chat 0 would fail forever and be hard to
                    // debug, so warn explicitly and skip this subscription's
                    // push for this round.
                    warn!(chat_id = %cfg.chat_id, feed = %cfg.feed_url, "invalid chat_id, skipping subscription");
                    return Ok(FeedStatus::Ok);
                }
            };
            // Channel title for notices (owner already resolved at function top).
            let chat_title = db::get_chat(&state.db, &cfg.chat_id)
                .await
                .ok()
                .flatten()
                .and_then(|c| c.title)
                .unwrap_or_else(|| cfg.chat_id.clone());

            // Content filter config (feed level): takes effect when the switch
            // is on and the keywords are non-empty. A DB error must not be
            // treated as "no filter": that would push blocked content and mark
            // it sent, making the blocklist silently lose an entry. Defer the
            // whole feed this round, consistent with the sent/outbox reads.
            let filter_cfg = match db::get_feed_filter(&state.db, &cfg.chat_id, &cfg.feed_url).await {
                Ok(cfg) => cfg,
                Err(e) => {
                    warn!(feed = %cfg.feed_url, error = %e, "feed filter read failed, deferring feed");
                    return Ok(FeedStatus::Failed);
                }
            };
            // Parse the keyword list once per feed, not once per entry.
            let filter_keywords = filter_cfg.keyword_list();

            // First filter out unsent entries, then push in ascending order of
            // publish time (old → new). RSS feeds are mostly "newest first";
            // pushing in the original order would invert the timeline of the
            // channel's message stream.
            // Cross-cycle double-send guard: an entry queued in the outbox but
            // not yet in sent_entries must be skipped — the outbox has sole
            // retry authority. These sets are queried again after the fetch:
            // the pre-fetch snapshot is only for enrichment and may be stale
            // (an overlapping outbox retry can finish while the fetch runs).
            // Sent ids are held as 64-bit marker hashes: a long-lived source's
            // history stays compact and is streamed row by row. The query is
            // also restricted to this feed window's markers, so memory is
            // bounded by the fetched entry count instead of by the
            // subscription's entire (never-pruned) sent history.
            // A read error must fail closed and leave the headers untouched,
            // so the whole feed is retried next round instead of being pushed.
            let wanted_hashes: std::collections::HashSet<u64> = fr
                .entries
                .iter()
                .map(|e| db::entry_marker_hash(&fetcher::entry_id(e)))
                .collect();
            let known_sent = match db::sent_entry_id_hashes(
                &state.db,
                &cfg.chat_id,
                &cfg.feed_url,
                Some(&wanted_hashes),
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
            let outbox_ids = match db::outbox_entry_ids(&state.db, &cfg.chat_id, &cfg.feed_url).await {
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
            // Cooperative deadline covers filtering too: a 20MB feed can hold
            // tens of thousands of entries, and the old code created the
            // deadline only after the filter loop.
            let deadline = tokio::time::Instant::now()
                + std::time::Duration::from_secs(FEED_DEADLINE_SECS);
            let mut deferred = false;
            // One storage-failure notice per feed per round: a global volume
            // outage would otherwise send one notice per media entry.
            let mut storage_notice_sent = false;
            let mut pending: Vec<&feed_rs::model::Entry> = Vec::new();
            // The same source can repeat an entry id within one response; the
            // sent-entry sets cannot know about the copy sent earlier in this
            // loop, so dedupe explicitly.
            let mut seen_eids: std::collections::HashSet<String> = std::collections::HashSet::new();
            for (idx, entry) in fr.entries.iter().enumerate() {
                if idx % 256 == 0 && tokio::time::Instant::now() >= deadline {
                    warn!(
                        feed = %cfg.feed_url,
                        "feed filtering deadline reached, remaining entries deferred to next cycle"
                    );
                    deferred = true;
                    break;
                }
                let eid = fetcher::entry_id(entry);
                if !seen_eids.insert(eid.clone()) {
                    continue;
                }
                if known_sent.contains(&db::entry_marker_hash(&eid)) {
                    continue;
                }
                if outbox_ids.contains(&eid) {
                    continue;
                }
                // Content filter: entries whose title matches a rule are marked
                // as sent directly (never pushed), avoiding repeated checks
                // every round eating into the fetch window.
                if !filter_cfg
                    .allows_title_with_keywords(&fetcher::entry_title(entry, lang), &filter_keywords)
                {
                    db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, &eid).await?;
                    continue;
                }
                pending.push(entry);
            }
            // Sort only when every entry has a timestamp (stable sort, equal
            // timestamps keep the feed's original order); sources missing
            // timestamps keep the feed's original order — no risky reordering.
            if pending.len() > 1
                && pending.iter().all(|e| e.published.or(e.updated).is_some())
            {
                pending.sort_by_key(|e| e.published.or(e.updated));
            }
            // The filter sets are not needed during the (much slower) send
            // loop; release them now instead of holding a long-lived
            // subscription's history for the whole entry-processing phase.
            drop(known_sent);
            drop(outbox_ids);
            drop(seen_eids);

            // True when an entry loop was cut short (filtering or pushing).
            // The response's negotiation headers are only saved when everything
            // drained fully (see after the loop): saving them while some
            // entries were deferred would let the next round's conditional
            // request return 304 and permanently swallow the unpushed entries.
            // A `?` abort returns before this point too, so headers stay at
            // the previous round's value and the next round re-fetches the
            // full body (sent_entries dedup prevents double-push).
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
                // Recheck right before doing any work: the batch sets were
                // queried after the fetch but an overlapping outbox retry can
                // still finish between that query and this entry. Skipping a
                // delivered/queued entry is safe; a DB error is not, because
                // sending without dedup could double-post.
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
                // Group-C sidecar: look up structured metadata by entry_id
                // (populated by platform sources, always empty for RSS sources).
                let meta = fr.metas.get(&eid);

                // Title+link / title-only modes: never fetch media, always take
                // the pure-text path.
                let prepared = if opts.mode.skip_media() {
                    crate::media::PreparedMedia::empty()
                } else {
                    // Reuse the round-level owner language resolved once at the
                    // top of process_feed; re-querying the settings row per
                    // entry wasted a DB round-trip and fell back to En via a
                    // different (owner_id.unwrap_or(0)) path.
                    crate::media::prepare_entry_media(
                        state,
                        entry,
                        &transport,
                        meta,
                        lang,
                        &cfg.feed_url,
                        opts.mode.single_media(),
                        allow_trusted_private,
                    )
                    .await
                };
                // The media root/year-level directory could not be created.
                // This is an environment failure, not "the entry has no
                // media": defer the entry so nothing is marked sent as text,
                // keep the negotiation headers stale, and notify the owner
                // once for this feed. Text-only entries continue normally.
                if prepared.setup_failed {
                    deferred = true;
                    if !storage_notice_sent {
                        storage_notice_sent = true;
                        let name = if feed_title.is_empty() { &cfg.feed_url } else { &feed_title };
                        let msg = i18n::t(lang, "notif_media_storage")
                            .replacen("{}", &push::escape_html(name), 1);
                        notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                    }
                    continue;
                }
                // Owner-facing warning texts. Per-file oversized skips are
                // muted when NOTIFY_OVERSIZED_MEDIA=false; the raw warning
                // list still drives MediaOnly retry / mark-sent control flow
                // below, so the gate never changes delivery behavior.
                let notify_warnings = prepared.notify_warnings(state.config.notify_oversized_media);
                let has_download_warnings = !prepared.warnings.is_empty();
                let notify_oversized = state.config.notify_oversized_media;

                // Oversized videos: stash for now, record after a successful
                // send (the TG message link is needed).
                let oversized = prepared.oversized_videos;

                // Media-only mode: silently skip when there is no media.
                // Mark as sent only when the entry itself has no media at all
                // (no download warnings), avoiding repeated re-checks later;
                // if there was media but all downloads failed (warnings
                // non-empty), do not mark it — leave it for the next round's
                // retry, preventing the entry from being silently lost.
                // Retry cap is 10: once exceeded, mark as sent and notify the
                // owner to prevent an infinite loop.
                if opts.mode == ContentMode::MediaOnly && prepared.items.is_empty() {
                    // Oversized videos: notify the owner immediately (media-only
                    // mode, entry not pushed).
                    let entry_link = fetcher::entry_link(entry).unwrap_or_default();
                    if notify_oversized {
                        for ov in &oversized {
                            notify_oversized_video(bot, lang, owner_id, &chat_title, &entry_link, None, false, ov).await;
                        }
                    }
                    // Download failures: notify immediately.
                    if has_download_warnings {
                        const MEDIA_DL_MAX_RETRIES: i64 = 10;
                        let retry_key = db::setting_key::media_dl_fail(&cfg.chat_id, &cfg.feed_url, &eid);
                        // Read the current counter, distinguishing a genuine
                        // DB error from a missing/invalid value. On a DB error we
                        // must NOT treat it as 0: that would reset the failure
                        // streak every time the DB hiccups, so the 10-retry cap
                        // is never reached and the entry retries forever. Skip
                        // the counter step this round and let the next round
                        // re-read (the entry is not marked sent, so it is safe).
                        let stored = match db::get_setting(&state.db, &retry_key).await {
                            Ok(v) => v.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
                            Err(e) => {
                                warn!(entry_id = %eid, feed = %cfg.feed_url, error = %e, "media_dl_fail counter read failed, deferring to next round");
                                // Keep the previous conditional-request headers:
                                // saving this round's ETag would freeze the
                                // retry behind a 304 on the next round.
                                deferred = true;
                                continue;
                            }
                        };
                        let fail_count: i64 = stored + 1;
                        if fail_count >= MEDIA_DL_MAX_RETRIES {
                            // Cap reached: abandon this entry and notify the
                            // owner.
                            warn!(entry_id = %eid, feed = %cfg.feed_url, count = fail_count, "MediaOnly download retries exhausted, abandoning");
                            let msg = push_fail_msg(
                                lang,
                                i18n::t(lang, "notif_abandoned"),
                                &feed_title,
                                &i18n::t(lang, "notif_media_dl_fail").replacen("{}", &fail_count.to_string(), 1),
                                "label_entry",
                                &entry_link,
                            );
                            notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                            // Delete the counter key BEFORE marking sent: if the
                            // mark fails (and `?` returns), the counter is
                            // already cleared, so the next round re-counts from 0
                            // instead of re-triggering the abandon branch every
                            // round and spamming the owner with "abandoned".
                            db::delete_setting(&state.db, &retry_key).await.ok();
                            db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, &eid).await?;
                        } else {
                            // A failed counter write must not be swallowed: the
                            // next round would re-read the old value and notify
                            // forever while the retry cap is never reached.
                            if let Err(e) = db::set_setting(&state.db, &retry_key, &fail_count.to_string()).await {
                                warn!(entry_id = %eid, feed = %cfg.feed_url, error = %e, "media_dl_fail counter write failed, deferring without notice");
                                deferred = true;
                                continue;
                            }
                            // The entry is still unresolved: keep the old
                            // conditional-request headers so the next round
                            // re-fetches the body and retries the media
                            // instead of returning 304.
                            deferred = true;
                            if !notify_warnings.is_empty() {
                                notify_download_failed(
                                    bot,
                                    lang,
                                    owner_id,
                                    &feed_title,
                                    &notify_warnings,
                                    &i18n::t(lang, "notif_dl_note")
                                        .replacen("{}", &fail_count.to_string(), 1)
                                        .replacen("{}", &MEDIA_DL_MAX_RETRIES.to_string(), 1),
                                    None,
                                    &entry_link,
                                )
                                .await;
                            }
                        }
                    }
                    if !has_download_warnings {
                        db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, &eid).await?;
                    }
                    continue;
                }

                // Owner notification note for the degrade-after-retries case
                // below; empty for a first-time (non-deferred) failure.
                let mut dl_retry_note = String::new();

                // Other modes: a download error is usually a transient network
                // fault, and the per-download retries only span a few seconds —
                // far shorter than the multi-minute outage windows actually
                // observed. Instead of degrading to text immediately (permanent
                // media loss), defer the entry to the next fetch round and
                // re-download; marking `deferred` keeps the negotiation headers
                // stale so the next round re-fetches the full body and the
                // entry reappears (sent_entries dedup prevents double-push).
                // Deterministic warnings alone (oversized skip / empty response)
                // do not defer: `download_failed` is only set on a real
                // download error. MediaOnly keeps its own counter branch above;
                // a MediaOnly partial success (items non-empty) preserves the
                // historical send-what-we-have behavior.
                if opts.mode != ContentMode::MediaOnly && prepared.download_failed {
                    // Cap on failed fetch rounds, including the final degrading
                    // round; rounds in between defer the entry. With the 30 min
                    // default feed interval, 3 rounds span roughly one hour of
                    // transient outages. Kept low on purpose: every deferred
                    // round also delays the entry's text, and an entry that
                    // rolls out of the source's sliding window mid-retry is
                    // lost the same way MediaOnly deferrals are.
                    const MEDIA_DL_MAX_DEFER_ROUNDS: i64 = 3;
                    let retry_key = db::setting_key::media_dl_fail(&cfg.chat_id, &cfg.feed_url, &eid);
                    // Same DB-error discipline as the MediaOnly branch: a read
                    // error must not reset the streak (cap would never fire),
                    // and a write error must not be swallowed (the counter
                    // would never advance).
                    let stored = match db::get_setting(&state.db, &retry_key).await {
                        Ok(v) => v.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
                        Err(e) => {
                            warn!(entry_id = %eid, feed = %cfg.feed_url, error = %e, "media_dl_fail counter read failed, deferring to next round");
                            deferred = true;
                            continue;
                        }
                    };
                    let fail_count: i64 = stored + 1;
                    if fail_count < MEDIA_DL_MAX_DEFER_ROUNDS {
                        if let Err(e) = db::set_setting(&state.db, &retry_key, &fail_count.to_string()).await {
                            warn!(entry_id = %eid, feed = %cfg.feed_url, error = %e, "media_dl_fail counter write failed, deferring without notice");
                            deferred = true;
                            continue;
                        }
                        info!(entry_id = %eid, feed = %cfg.feed_url, count = fail_count, "media download failed, deferring entry to next fetch round");
                        deferred = true;
                        continue;
                    }
                    // Cap reached: fall through to the normal send path — the
                    // entry goes out with whatever media survived (cover
                    // fallback or none) and the owner notification below
                    // carries the retry note. Delete the counter first so a
                    // later `?` abort cannot leave it behind.
                    warn!(entry_id = %eid, feed = %cfg.feed_url, count = fail_count, "media download retries exhausted, degrading entry");
                    db::delete_setting(&state.db, &retry_key).await.ok();
                    dl_retry_note = i18n::t(lang, "notif_dl_retried")
                        .replacen("{}", &fail_count.to_string(), 1);
                }

                // Reaching here means the entry is sent this round (a MediaOnly
                // entry with items, or a normal-mode entry whose media either
                // succeeded or exhausted its defer budget): clear the retry
                // counter key, preventing keys written by earlier failed rounds
                // from lingering in the settings table forever.
                if opts.mode == ContentMode::MediaOnly || !prepared.download_failed {
                    let retry_key = db::setting_key::media_dl_fail(&cfg.chat_id, &cfg.feed_url, &eid);
                    db::delete_setting(&state.db, &retry_key).await.ok();
                }

                // Second recheck directly before the send: media preparation
                // can take minutes (downloads + ffprobe), long enough for an
                // overlapping outbox retry to deliver this same entry. The
                // batch sets are already stale by now; this is the last cheap
                // guard before the Telegram call.
                match entry_already_handled(state, &cfg.chat_id, &cfg.feed_url, &eid).await {
                    Ok(true) => {
                        debug!(feed = %cfg.feed_url, entry_id = %eid, "entry handled during preparation, skipping send");
                        continue;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(
                            feed = %cfg.feed_url,
                            entry_id = %eid,
                            error = %e,
                            "pre-send dedup recheck failed, deferring remaining entries"
                        );
                        deferred = true;
                        break;
                    }
                }

                let author = fetcher::entry_author(entry);
                // No whole-entry retry for sending (#11 fix: removed the
                // SEND_ATTEMPTS=1 infinite loop): send_entry_msg (text
                // segments) and send_entry_with_media (media ops) already have
                // segment-level/op-level resume internally (only the failed
                // current segment is resent, delivered segments are not).
                // A whole-entry retry would resend already-delivered
                // segments/ops under compound failures, producing duplicate
                // messages. Persistent failures are caught by the outbox
                // persistent retry queue (exponential backoff, abandon at 72h).
                let result: anyhow::Result<push::SendOutcome> = if prepared.items.is_empty() {
                    // The pure-text path uniformly goes through send_text_only
                    // (H1 fix: it used to be a verbatim duplicate of the outbox
                    // retry path; the poll-resend logic was fixed twice because
                    // of this, v1.9.3/v1.9.6).
                    push::send_text_only(bot, chat_id, entry, &feed_title, author.as_deref(), opts, meta).await
                } else {
                    push::send_entry_with_media(
                        bot,
                        &state.db,
                        chat_id,
                        push::PushCtx {
                            entry,
                            feed_title: &feed_title,
                            author: author.as_deref(),
                            opts,
                            caption_above_media: cfg.caption_above_media,
                            aspect_threshold: crate::config::LONG_IMAGE_ASPECT_RATIO,
                            media: &prepared.items,
                            meta,
                        },
                        &transport,
                    )
                    .await
                };

                match result {
                    Ok(send_outcome) => {
                        let entry_link = fetcher::entry_link(entry).unwrap_or_default();
                        // A1 policy: the rich main message may have been
                        // delivered even though the API call reported an error
                        // (timeout/reset/5xx). The entry is still marked sent
                        // below (no outbox, no duplicate resend); tell the
                        // owner to verify the channel manually.
                        if send_outcome.uncertain {
                            let mut msg = i18n::t(lang, "notif_send_uncertain")
                                .replacen("{}", &push::escape_html(&chat_title), 1);
                            if let Some(el) = &send_outcome.entry_link {
                                push_line(&mut msg, lang, "label_entry", el);
                            }
                            notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                        }
                        if send_outcome.text_partial {
                            let mut msg = i18n::t(lang, "notif_text_partial")
                                .replacen("{}", &push::escape_html(&chat_title), 1);
                            if let Some(tg) = &send_outcome.delivered_tg_link {
                                push_line(&mut msg, lang, "label_message", tg);
                            }
                            if let Some(el) = &send_outcome.entry_link {
                                push_line(&mut msg, lang, "label_entry", el);
                            }
                            notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                        }
                        // A successful send resets the per-source failure
                        // streak; the next isolated failure starts from 1.
                        clear_send_failure(state, &cfg.chat_id, &cfg.feed_url).await;
                        // Download failures: notify the owner immediately
                        // (including the delivered-message link and the source
                        // entry link).
                        if !notify_warnings.is_empty() {
                            notify_download_failed(
                                bot,
                                lang,
                                owner_id,
                                &feed_title,
                                &notify_warnings,
                                &dl_retry_note,
                                send_outcome.delivered_tg_link.as_deref(),
                                &entry_link,
                            )
                            .await;
                        }
                        // Media send failed (text delivered but media not
                        // attached): notify immediately.
                        if send_outcome.media_failed {
                            let mut msg = i18n::t(lang, "notif_media_failed")
                                .replacen("{}", &push::escape_html(&chat_title), 1);
                            if let Some(tg) = &send_outcome.delivered_tg_link {
                                push_line(&mut msg, lang, "label_message", tg);
                            }
                            if let Some(el) = &send_outcome.entry_link {
                                push_line(&mut msg, lang, "label_entry", el);
                            }
                            notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                        }
                        // Oversized videos: notify immediately (unless muted
                        // by NOTIFY_OVERSIZED_MEDIA).
                        if notify_oversized {
                            for ov in &oversized {
                                notify_oversized_video(
                                    bot,
                                    lang,
                                    owner_id,
                                    &chat_title,
                                    &entry_link,
                                    send_outcome.delivered_tg_link.as_deref(),
                                    send_outcome.text_only,
                                    ov,
                                )
                                .await;
                            }
                        }
                        // Register the sent message: TG message id → RSS entry,
                        // for /find reverse lookup. Registration failure does
                        // not affect the main push flow.
                        if let Some(mid) = send_outcome.delivered_message_id {
                            db::record_sent_message(
                                &state.db,
                                &cfg.chat_id,
                                mid,
                                &cfg.feed_url,
                                &eid,
                                send_outcome.entry_link.as_deref(),
                            )
                            .await
                            .ok();
                        }
                        // Message already delivered to TG: a persistence failure
                        // must not abort (the original `?` would discard the
                        // "delivered" fact, and the next round's re-fetch would
                        // push it again, producing duplicate messages).
                        // Best-effort + warn.
                        if let Err(e) = db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, &eid).await {
                            warn!(feed = %cfg.feed_url, entry_id = %eid, error = %e, "mark_entry_sent failed after successful delivery (may re-push next cycle)");
                        }
                    }
                    Err(e) => {
                        // Group upgraded to a supergroup: migrate every
                        // chat-scoped row and skip the outbox insert; the next
                        // fetch uses the new chat id. Without this, the old
                        // subscription would keep failing into outbox until
                        // abandoned after 72h.
                        if let Some(new_id) = migrated_chat_id_from_error(&e) {
                            match db::migrate_chat_id(&state.db, &cfg.chat_id, &new_id).await {
                                Ok(()) => {
                                    info!(
                                        old_chat_id = %cfg.chat_id,
                                        new_chat_id = %new_id,
                                        "chat id migrated after send error; retrying on the next cycle"
                                    );
                                    continue;
                                }
                                Err(me) => warn!(
                                    old_chat_id = %cfg.chat_id,
                                    new_chat_id = %new_id,
                                    error = %me,
                                    "chat id migration after send error failed"
                                ),
                            }
                        }
                        // A broken chat (bot removed / no posting rights) would
                        // otherwise succeed at fetching forever while every
                        // entry is queued and abandoned after 72h. Count
                        // consecutive send failures and disable the source or
                        // chat once the threshold is reached.
                        if record_send_failure(bot, state, &sub, &cfg, lang, &feed_title, &e).await {
                            deferred = true;
                            break;
                        }

                        // Per-source queue cap: even intermittent failures can
                        // accumulate rows indefinitely. Stop fetching before
                        // the queue grows without bound; the source is
                        // auto-disabled and can be resumed after fixing it.
                        let queued = match db::outbox_count_for_feed(&state.db, &cfg.chat_id, &cfg.feed_url).await {
                            Ok(n) => n,
                            Err(err) => {
                                warn!(feed = %cfg.feed_url, error = %err, "outbox count failed, deferring entry");
                                deferred = true;
                                continue;
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
                                    let name = if feed_title.is_empty() { &cfg.feed_url } else { &feed_title };
                                    let msg = i18n::t(lang, "notif_outbox_full")
                                        .replacen("{}", &push::escape_html(name), 1)
                                        .replacen("{}", &OUTBOX_PER_FEED_LIMIT.to_string(), 1);
                                    notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                                }
                                Ok(false) => {}
                                Err(err) => warn!(feed = %cfg.feed_url, error = %err, "outbox-full auto-disable failed"),
                            }
                            deferred = true;
                            break;
                        }

                        warn!(feed = %cfg.feed_url, error = %e, "send failed, queuing to outbox");
                        // Push failed: write into the outbox persistent retry
                        // queue (exponential backoff, abandon at 72h).
                        // entry_json is the sole source for deserialization on
                        // retry: if serialization failed and an empty string
                        // were enqueued, the retry would fail to deserialize,
                        // only delete the row without mark_sent, and the entry
                        // would be permanently lost once it rolls out of the
                        // window. So on failure we take the "enqueue failed"
                        // branch (mark_sent + notify owner), leaving no hidden
                        // hazard.
                        let entry_json = match serde_json::to_string(entry) {
                            Ok(j) => j,
                            Err(se) => {
                                error!(feed = %cfg.feed_url, entry_id = %eid, error = %se, "entry_json serialization failed, marking sent");
                                db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, &eid).await?;
                                let elink = fetcher::entry_link(entry).unwrap_or_default();
                                let msg = push_fail_msg(
                                    lang,
                                    i18n::t(lang, "notif_serialize_failed"),
                                    &feed_title,
                                    &format!("{:#}", e),
                                    "label_source",
                                    &elink,
                                );
                                notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                                continue;
                            }
                        };
                        // A media_json failure degrades to an empty array (the
                        // retry proceeds as text without media); a meta_json
                        // failure degrades to no meta (the retry loses the
                        // poll/prerendered markers) — neither loses the entry,
                        // which is acceptable.
                        let media_json = serde_json::to_string(&prepared.items).unwrap_or_default();
                        // Group-C sidecar: serialize EntryMeta so it can be
                        // restored on retry.
                        let meta_json = meta
                            .map(|m| serde_json::to_string(m).unwrap_or_default())
                            .unwrap_or_default();
                        let now_ts = chrono::Utc::now().timestamp();
                        let next_retry = now_ts + outbox_backoff_secs(0);
                        let entry_link = fetcher::entry_link(entry).unwrap_or_default();
                        match db::outbox_insert(
                            &state.db,
                            &cfg.chat_id,
                            &cfg.feed_url,
                            &eid,
                            &feed_title,
                            &entry_json,
                            &media_json,
                            &meta_json,
                            &cfg.content_mode,
                            &cfg.source_display,
                            cfg.link_preview,
                            cfg.caption_above_media,
                            author.as_deref(),
                            next_retry,
                            now_ts,
                        )
                        .await
                        {
                            Ok(true) => {
                                // Newly enqueued: notify the owner (only notify
                                // on the first failure).
                                let msg = push_fail_msg(
                                    lang,
                                    i18n::t(lang, "notif_queued"),
                                    &feed_title,
                                    &format!("{:#}", e),
                                    "label_source",
                                    &entry_link,
                                );
                                notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                                // Download-failure/oversized-video alerts are
                                // not persisted: send them at enqueue time,
                                // otherwise these alerts would be lost forever
                                // once the outbox retry succeeds.
                                if !notify_warnings.is_empty() {
                                    notify_download_failed(
                                        bot,
                                        lang,
                                        owner_id,
                                        &feed_title,
                                        &notify_warnings,
                                        &dl_retry_note,
                                        None,
                                        &entry_link,
                                    )
                                    .await;
                                }
                                if notify_oversized {
                                    for ov in &oversized {
                                        notify_oversized_video(
                                            bot,
                                            lang,
                                            owner_id,
                                            &chat_title,
                                            &entry_link,
                                            None,
                                            false,
                                            ov,
                                        )
                                        .await;
                                    }
                                }
                            }
                            Ok(false) => {
                                // Already in the queue (enqueued by a previous
                                // fetch round): do not notify again, avoiding
                                // the same notice being spammed every round
                                // during a persistent outage.
                                debug!(feed = %cfg.feed_url, entry_id = %eid, "already in outbox, skipping notice");
                            }
                            Err(insert_err) => {
                                // outbox write failed: fall back to marking sent
                                // (prevents infinite retries)
                                error!(feed = %cfg.feed_url, error = %insert_err, "outbox insert failed, marking sent");
                                db::mark_entry_sent(&state.db, &cfg.chat_id, &cfg.feed_url, &eid).await?;
                                let msg = push_fail_msg(
                                    lang,
                                    i18n::t(lang, "notif_enqueue_failed"),
                                    &feed_title,
                                    &format!("{:#}", e),
                                    "label_source",
                                    &entry_link,
                                );
                                notify_owner_html(bot, state, &cfg.chat_id, &msg).await;
                            }
                        }
                    }
                }
                // Send rate limiting is handled by the push module's ChatPacer,
                // which queues uniformly per channel (≥2s/message); no extra
                // sleep is needed here.
            }

            // Save this response's negotiation headers for the next
            // conditional request — but only when every entry in this round
            // was fully processed. On deadline deferral (or a `?` abort that
            // skips this code) the headers stay stale, forcing the next round
            // to re-fetch the full body and pick up the leftovers (sent_entries
            // dedup keeps already-pushed entries from being re-pushed).
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
        Err(e) => {
            feed_failed = true;
            let err_msg = format!("{:#}", e);
            warn!(feed = %sub.feed_url, error = %err_msg, "fetch failed");

            // Record that this platform had a failure this round + save the
            // last error message (for whole-round hysteresis evaluation).
            if let Some(p) = platform_of_url(&sub.feed_url) {
                stats.failed.lock().await.insert(p);
                stats.last_err.lock().await.insert(p, err_msg.clone());
            }

            // Cookie-expired flag (for the /cookies panel status display):
            // set immediately (global); this is NOT a push notification. The
            // admin alert follows the platform-health hysteresis (two
            // consecutive all-failed rounds), so one jitter round stays quiet.
            // Detection is typed first (PlatformError::AuthExpired); the
            // legacy "cookie" substring is kept as a fallback for "cookie not
            // configured" messages that predate the typed error.
            let auth_expired = e.chain().any(|cause| {
                matches!(
                    cause.downcast_ref::<crate::platforms::PlatformError>(),
                    Some(crate::platforms::PlatformError::AuthExpired { .. })
                )
            });
            if auth_expired || err_msg.to_lowercase().contains("cookie") {
                if let Some(p) = platform_of_url(&sub.feed_url) {
                    db::set_setting(&state.db, &db::setting_key::cookie_expired_notified(p), "1")
                        .await
                        .ok();
                }
            }

            let count = db::increment_error_count(&state.db, sub.id, &err_msg).await?;

            if count >= state.config.max_fetch_errors && sub.active == 1 {
                // Auto-disable (marked as system-auto-disabled; the self-heal
                // job auto-recovers it after the cooldown). The UPDATE carries
                // an `active = 1` guard closing the manual-pause race: the
                // round-start snapshot above may be stale if the user paused
                // the source mid-round, in which case the disable is skipped
                // (rows_affected = 0) — a manual pause must not be converted
                // into an auto-disable that the self-heal job would undo.
                if !db::auto_disable_subscription(&state.db, sub.id).await? {
                    info!(
                        feed = %sub.feed_url,
                        "skip auto-disable: subscription paused by user during this round"
                    );
                    return Ok(FeedStatus::Failed);
                }
                info!(feed = %sub.feed_url, errors = count, "subscription auto-disabled");
                // Edge deduplication (#9 fix): no duplicate notification when
                // the same source goes disable → self-heal → fail again.
                // Notify only on the first disable; a successful fetch clears
                // the flag so the next failure can notify again.
                let notify_key = db::setting_key::auto_disable_notified(&sub.chat_id, &sub.feed_url);
                let already_notified = db::get_setting(&state.db, &notify_key)
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                if !already_notified {
                    db::set_setting(&state.db, &notify_key, "1").await.ok();
                    // Format: title = source-name hyperlink; body = check hint
                    // + /set command (inline code).
                    let name = sub
                        .feed_title
                        .as_deref()
                        .filter(|t| !t.is_empty())
                        .map(|t| format!(
                            "<a href=\"{}\">{}</a>",
                            crate::push::escape_attr(&sub.feed_url),
                            crate::push::escape_html(t)
                        ))
                        .unwrap_or_else(|| crate::push::escape_html(&sub.feed_url));
                    // i18n::fill: single-pass substitution, so a source name
                    // containing literal `{}` cannot swallow the chat-id slot.
                    let auto_disable_msg = i18n::fill(
                        i18n::t(lang, "notif_auto_disable"),
                        &[&name, &crate::push::escape_html(&sub.chat_id)],
                    );
                    notify_owner_html(
                        bot,
                        state,
                        &sub.chat_id,
                        &auto_disable_msg,
                    )
                    .await;
                }
            }
        }
    }

    Ok(if feed_failed {
        FeedStatus::Failed
    } else {
        FeedStatus::Ok
    })
}

/// Determine which platform (weibo / x / telegram) a feed_url belongs to;
/// returns None for non-platform sources. Registry-driven (platform_for_url),
/// so adding a new platform requires no changes to this file.
fn platform_of_url(feed_url: &str) -> Option<&'static str> {
    crate::platforms::platform_for_url(feed_url).map(|p| p.id)
}

/// Platform-level health detection (failure direction): called after the
/// hysteresis evaluation in run_fetch_cycle. The atomic flip guarantees a
/// single notification on ok → down only; no repeat while persistently down.
/// v1.11.0: after cookies were globalized, notifications go to the single admin (v1.11.3).
async fn maybe_notify_platform_down(bot: &Bot, state: &AppState, platform: &str, err_msg: &str) {
    // Atomic edge trigger: returns true only when the state flips from
    // non-down to down. DB errors and "already down" are handled differently:
    // errors log a warn and leave a trace (it will be retried next round if
    // the streak still meets the threshold, so it self-heals), instead of
    // being silently swallowed and letting the alert vanish without a trace.
    let health_key = db::setting_key::platform_health(platform);
    let flipped = match db::flip_setting_once(&state.db, &health_key, "down").await {
        Ok(f) => f,
        Err(e) => {
            warn!(platform = platform, error = %e, "platform down flip failed, alert may be lost this round");
            return;
        }
    };
    if !flipped {
        return; // Already down, do not notify again
    }

    let label = crate::platforms::find_platform(platform)
        .map(|p| p.label)
        .unwrap_or(platform);

    // Unified format: title + detailed error wrapped in a blockquote.
    // Whether it is folded is decided by the unified rule inside
    // notify_user_html (fold when >200 characters).
    let delivered = notify_admins_html(bot, state, |lang| {
        i18n::t(lang, "notif_platform_down")
            .replacen("{}", label, 1)
            .replacen("{}", &crate::push::escape_html(err_msg), 1)
    })
    .await;
    if !delivered {
        // The health bit was flipped before delivery; without reverting it,
        // this edge would never fire again and the alert would be lost for
        // the whole outage. Restore "ok" so the next qualifying round retries.
        if let Err(e) = db::set_setting(&state.db, &health_key, "ok").await {
            warn!(platform = platform, error = %e, "failed to restore health bit after undelivered alert");
        } else {
            warn!(platform = platform, "platform-down alert undelivered; will retry next round");
        }
    }
}

/// Platform-level health detection (recovery direction): called by
/// run_fetch_cycle at the end of the whole round, triggered only when all of
/// this platform's sources succeeded this round (avoiding flapping where a
/// partial success reports recovery). Edge-triggered: notify only on
/// down → ok; also clears the cookie-expired flag.
/// v1.11.0: after cookies were globalized, notifications go to the single admin (v1.11.3).
async fn maybe_notify_platform_recovered(bot: &Bot, state: &AppState, platform: &str) {
    let health_key = db::setting_key::platform_health(platform);
    // Pre-check: attempt recovery only when clearly in the down state.
    // Avoids mistakenly sending a "recovered" notification on a fresh install
    // (key does not exist).
    let was_down = matches!(
        db::get_setting(&state.db, &health_key).await,
        Ok(Some(v)) if v == "down"
    );
    if !was_down {
        return;
    }

    // Atomic edge trigger: returns true only when the state flips from non-ok
    // to ok. DB errors log a warn and leave a trace (retried next round if
    // everything still succeeds); not silently swallowed.
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

    // Clear the cookie-expired flag (the /cookies panel no longer shows
    // "expired")
    db::set_setting(&state.db, &db::setting_key::cookie_expired_notified(platform), "0")
        .await
        .ok();

    let label = crate::platforms::find_platform(platform)
        .map(|p| p.label)
        .unwrap_or(platform);
    notify_admins_html(bot, state, |lang| {
        i18n::t(lang, "notif_platform_recovered").replacen("{}", label, 1)
    }).await;
}

// ===== Outbox retry (persistent retry queue for failed sends) =====

/// Register the outbox retry job: runs every 5 minutes, retries failed sends
/// with exponential backoff (capped at 2h intervals), and abandons after 72h
/// from the first actual retry attempt (never-attempted rows are not abandoned).
pub async fn register_outbox_retry_job(
    sched: &JobScheduler,
    state: AppState,
    bot: Bot,
) -> anyhow::Result<()> {
    sched
        .add(Job::new_async("0 0,5,10,15,20,25,30,35,40,45,50,55 * * * *", move |_uuid, _l| {
            let state = state.clone();
            let bot = bot.clone();
            Box::pin(async move {
                if let Err(e) = run_outbox_retry(&state, &bot).await {
                    error!(error = %e, "outbox retry cycle failed");
                }
            })
        })?)
        .await?;
    Ok(())
}

/// Exponential backoff intervals (seconds) for outbox retries.
/// Index = attempts (0-based after first failure). Capped at 2h.
fn outbox_backoff_secs(attempts: i64) -> i64 {
    match attempts {
        0 => 60,       // 1 min
        1 => 300,      // 5 min
        2 => 900,      // 15 min
        3 => 1800,     // 30 min
        4 => 3600,     // 1 hour
        _ => 7200,     // 2 hours (cap)
    }
}

/// Total elapsed time before abandoning an outbox entry (72 hours), measured
/// from the first real retry attempt (not from enqueue time). Rows that never
/// received a turn during a long backlog are retried instead of being silently
/// abandoned.
const OUTBOX_ABANDON_SECS: i64 = 72 * 3600;
/// Hard cap for rows whose attempts counter never advanced (persistent DB
/// write failures). Without it, `attempts <= 0` made the 72h abandon guard
/// unreachable and the row was retried every cron tick forever.
const OUTBOX_UNATTEMPTED_MAX_SECS: i64 = 7 * 24 * 3600;

/// Rows fetched per retry round (every 5 minutes). Raised from 10 to 25 so a
/// long Telegram outage cannot leave the oldest rows permanently at the back
/// of the queue.
const OUTBOX_BATCH_LIMIT: i64 = 25;

/// Extract the new supergroup id from a send failure. The typed teloxide
/// variant is preferred; media paths stringify errors, so the Display text
/// ("migrated to a supergroup with ID #-100…") is the fallback.
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

/// Record one send failure for a source. Returns true when the caller must not
/// enqueue the current entry (the source/chat has just been disabled).
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
    // A permanent chat error (bot removed / no rights / chat gone) cannot be
    // fixed by retrying this source, but disabling the chat row would also
    // hide it from `/set` and block the owner from inspecting/resuming after
    // fixing permissions. Disable only this source; a real bot removal is
    // handled by my_chat_member (which soft-disables the whole chat).
    let permanent = sendpolicy::is_permanent_chat_error(&err_str);
    match db::auto_disable_subscription(&state.db, sub.id).await {
        Ok(true) => {
            let name = if feed_title.is_empty() { &cfg.feed_url } else { feed_title };
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
    if let Err(e) = db::delete_setting(&state.db, &db::setting_key::send_fail(chat_id, feed_url)).await {
        debug!(feed = %feed_url, error = %e, "failed to clear send-failure counter");
    }
}

/// Whether a queue row is past its 72h retry window. The clock starts at the
/// first actual attempt, not at enqueue time; a row that has never been tried
/// is never abandoned (it may simply have been behind a long backlog). Legacy
/// rows without `first_attempt_at` fall back to `created_at`, matching the old
/// behavior for rows that had already been attempted.
///
/// Do not anchor at the most recent attempt: continuous retries would refresh
/// the anchor forever and the row would never be abandoned.
fn outbox_abandon_due(
    created_at: i64,
    first_attempt_at: Option<i64>,
    attempts: i64,
    now: i64,
) -> bool {
    if attempts <= 0 {
        // A never-attempted row is usually behind a backlog and should not be
        // abandoned early. But if the attempts update itself keeps failing,
        // this branch would otherwise be permanent; apply a long hard cap.
        return now - created_at >= OUTBOX_UNATTEMPTED_MAX_SECS;
    }
    let anchor = first_attempt_at.unwrap_or(created_at);
    now - anchor >= OUTBOX_ABANDON_SECS
}

/// Abandon one outbox entry: notify the owner + mark sent + delete the queue
/// row. Shared by the 72h-timeout abandonment and the MediaOnly media-expiry
/// abandonment (reason_key is the i18n key for the blockquote copy).
/// Marking sent prevents the entry from being re-pushed by the next fetch
/// while it is still in the source's sliding window, which would form an
/// infinite loop of "re-push → fail → re-enqueue → abandon again" and
/// duplicate notifications.
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
    if let Err(e) = db::mark_entry_sent(&state.db, &row.chat_id, &row.feed_url, &row.entry_id).await {
        // Keep the row: deleting it without the sent marker would let the
        // next fetch re-push the entry, fail again, and re-enqueue it, turning
        // the abandon path into a periodic notification loop. The next retry
        // attempts the mark again before abandoning.
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

/// Notify the owner once when media files expired while an outbox row was
/// waiting for retry. The flag is keyed by outbox row id: a persistent retry
/// loop must not repeat this notice every backoff, and the row is deleted at
/// 72h anyway. A DB read/write error skips/sends without dedup rather than
/// hiding the loss; both paths leave a warn.
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
    push_line(&mut msg, lang, "label_source", entry_link.as_deref().unwrap_or(""));
    notify_owner_html(bot, state, &row.chat_id, &msg).await;
    if let Err(e) = db::set_setting(&state.db, &key, "1").await {
        warn!(entry_id = %row.entry_id, error = %e, "outbox media-lost flag write failed, notice may repeat");
    }
}

async fn run_outbox_retry(state: &AppState, bot: &Bot) -> anyhow::Result<()> {
    // Reentrancy protection: one retry round can exceed the 5-minute cron
    // interval due to large media uploads; without a lock, the next tick
    // would concurrently pick up the same batch of rows and double-send.
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

    let now = chrono::Utc::now().timestamp();
    let due_ids = db::outbox_due_ids(&state.db, now, OUTBOX_BATCH_LIMIT).await?;
    if due_ids.is_empty() {
        return Ok(());
    }
    if due_ids.len() as i64 == OUTBOX_BATCH_LIMIT {
        warn!(
            batch = OUTBOX_BATCH_LIMIT,
            "outbox retry batch is full; a backlog may be forming"
        );
    }

    let transport = crate::media::MediaTransport::new(&state.config);

    for id in due_ids {
        // Fetch only this row: a batch of full rows can carry very large
        // entry_json values, so the id list is the only shared batch state.
        let row = match db::outbox_get(&state.db, id).await {
            Ok(Some(row)) => row,
            Ok(None) => continue,
            Err(e) => {
                warn!(outbox_id = id, error = %e, "outbox: row fetch failed, skipping");
                continue;
            }
        };
        // Dedup guard first: if the entry was already delivered via the normal
        // fetch path (e.g. network recovered and the next cycle re-pushed it),
        // drop the queued copy instead of double-posting. Checking this before
        // the 72h-abandon branch avoids a false "abandoned" notice for an entry
        // that was actually delivered while sitting in the queue.
        // Fail-closed: when the DB query errors, skip this round (keep the
        // queue and re-check next round); do not treat a transient error as
        // "not sent" and keep sending (prevents double-sends).
        match db::is_entry_sent(&state.db, &row.chat_id, &row.feed_url, &row.entry_id).await {
            Ok(true) => {
                info!(entry_id = %row.entry_id, feed = %row.feed_url, "outbox entry already sent, dropping");
                if let Err(e) = db::outbox_delete(&state.db, row.id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete entry");
                }
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                warn!(entry_id = %row.entry_id, error = %e, "outbox: is_entry_sent check failed, skipping this round (fail-closed)");
                continue;
            }
        }

        // Check if 72h elapsed since the first actual attempt -> abandon.
        // Measuring from created_at silently abandoned rows that sat behind a
        // long outage backlog and were never retried even once. Use a per-row
        // clock: a batch of large uploads can outlive several backoff steps,
        // and the round-start timestamp would make the next retry look due
        // immediately.
        let row_now = chrono::Utc::now().timestamp();
        if outbox_abandon_due(row.created_at, row.first_attempt_at, row.attempts, row_now) {
            warn!(
                entry_id = %row.entry_id,
                feed = %row.feed_url,
                attempts = row.attempts,
                "outbox entry abandoned after 72h of retries"
            );
            abandon_outbox_row(bot, state, &row, "notif_timeout_72h").await;
            continue;
        }

        // Deserialize entry
        let entry: feed_rs::model::Entry = match serde_json::from_str(&row.entry_json) {
            Ok(e) => e,
            Err(e) => {
                error!(entry_id = %row.entry_id, error = %e, "outbox: failed to deserialize entry");
                if let Err(e) = db::outbox_delete(&state.db, row.id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete entry");
                }
                continue;
            }
        };

        // Deserialize media items (filter out files that no longer exist)
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
        let media_items: Vec<crate::media::MediaItem> = all_media
            .into_iter()
            .filter(|m| m.path.exists())
            .collect();
        let media_lost = had_media && media_items.len() < original_count;

        // Reconstruct render opts BEFORE the MediaOnly check: content_mode is
        // a legacy-tolerant string (`MediaOnly`/`mediaonly`), so a raw string
        // compare here would bypass the abandon guard for older rows.
        // Owner language drives user-visible fallbacks (missing title).
        let lang = match db::get_chat_owner(&state.db, &row.chat_id).await {
            Ok(Some(owner)) => owner_lang(state, owner).await,
            _ => Lang::En,
        };
        let opts = push::RenderOpts {
            mode: ContentMode::from_str(&row.content_mode),
            source: push::SourceDisplay::from_str(&row.source_display),
            link_preview: row.link_preview,
            lang,
            // Persistent retries stay on the proven standard path. A rich
            // failure must never be repeated on every outbox backoff.
            allow_rich: false,
        };

        // MediaOnly mode with no sendable media left (files cleaned up, or a
        // legacy/corrupt media_json): the text renderer returns None for this
        // mode, so send_text_only would report a silent success and the entry
        // would be marked sent without delivering anything. Abandon instead.
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
            // The enqueue-time notice only covered the original send failure;
            // losing files while queued is a separate degradation and must be
            // visible to the owner. One-shot per outbox row (a retry loop can
            // run for 72h); the flag is removed on success/abandon and swept
            // by cleanup if the row is removed elsewhere.
            maybe_notify_outbox_media_lost(
                bot,
                state,
                &row,
                original_count,
                media_items.len(),
            )
            .await;
        }
        let chat_id: i64 = match row.chat_id.parse() {
            Ok(id) => id,
            Err(_) => {
                // chat_id is not numeric: this entry can never be delivered,
                // delete it to avoid infinite retries.
                warn!(chat_id = %row.chat_id, entry_id = %row.entry_id, "outbox: invalid chat_id, dropping entry");
                if let Err(e) = db::outbox_delete(&state.db, row.id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete entry");
                }
                continue;
            }
        };
        let author = row.author.as_deref();

        // Second dedup check: the fetch cycle may have pushed the same entry
        // between fetch_due and this row (check-then-act window); querying
        // once more right before sending shrinks the window.
        // Likewise fail-closed: on a DB error, skip this round rather than
        // risking a send.
        match db::is_entry_sent(&state.db, &row.chat_id, &row.feed_url, &row.entry_id).await {
            Ok(true) => {
                info!(entry_id = %row.entry_id, feed = %row.feed_url, "outbox entry sent between fetch and send, dropping");
                if let Err(e) = db::outbox_delete(&state.db, row.id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete entry");
                }
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                warn!(entry_id = %row.entry_id, error = %e, "outbox: second is_entry_sent check failed, skipping this round (fail-closed)");
                continue;
            }
        }

        // Group-C sidecar: the meta for outbox retries comes from the
        // meta_json column. An empty string or a deserialization failure is
        // treated as no meta (compatibility with historical rows; behavior
        // degrades to pure text).
        let outbox_meta: Option<crate::entrymeta::EntryMeta> = if row.meta_json.is_empty() {
            None
        } else {
            serde_json::from_str(&row.meta_json).ok()
        };
        let meta_ref = outbox_meta.as_ref();

        // Attempt send
        let result = if media_items.is_empty() {
            // The pure-text path uniformly goes through send_text_only (H1 fix:
            // converged with the main push path into a single function; the
            // poll-resend logic had previously been fixed twice due to dual-path
            // drift, v1.9.3/v1.9.6).
            push::send_text_only(bot, chat_id, &entry, &row.feed_title, author, opts, meta_ref).await
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
                    aspect_threshold: crate::config::LONG_IMAGE_ASPECT_RATIO,
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
                clear_send_failure(state, &row.chat_id, &row.feed_url).await;
                if send_outcome.text_partial {
                    let chat_title = db::get_chat(&state.db, &row.chat_id)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|c| c.title.or(c.username))
                        .unwrap_or_else(|| row.chat_id.clone());
                    let lang = match db::get_chat_owner(&state.db, &row.chat_id).await {
                        Ok(Some(owner)) => owner_lang(state, owner).await,
                        _ => Lang::En,
                    };
                    let mut msg = i18n::t(lang, "notif_text_partial")
                        .replacen("{}", &push::escape_html(&chat_title), 1);
                    if let Some(tg) = &send_outcome.delivered_tg_link {
                        push_line(&mut msg, lang, "label_message", tg);
                    }
                    if let Some(el) = &send_outcome.entry_link {
                        push_line(&mut msg, lang, "label_entry", el);
                    }
                    notify_owner_html(bot, state, &row.chat_id, &msg).await;
                }
                // The retry may deliver the text while its media is
                // permanently lost (expired/rejected). Surface the same
                // media_failed notice as the main path; otherwise the owner
                // only sees the earlier "queued" message and never learns that
                // the media was dropped.
                if send_outcome.media_failed {
                    let chat_title = db::get_chat(&state.db, &row.chat_id)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|c| c.title.or(c.username))
                        .unwrap_or_else(|| row.chat_id.clone());
                    let lang = match db::get_chat_owner(&state.db, &row.chat_id).await {
                        Ok(Some(owner)) => owner_lang(state, owner).await,
                        _ => Lang::En,
                    };
                    let mut msg = i18n::t(lang, "notif_media_failed")
                        .replacen("{}", &push::escape_html(&chat_title), 1);
                    if let Some(tg) = &send_outcome.delivered_tg_link {
                        push_line(&mut msg, lang, "label_message", tg);
                    }
                    if let Some(el) = &send_outcome.entry_link {
                        push_line(&mut msg, lang, "label_entry", el);
                    }
                    notify_owner_html(bot, state, &row.chat_id, &msg).await;
                }
                if let Some(mid) = send_outcome.delivered_message_id {
                    db::record_sent_message(
                        &state.db,
                        &row.chat_id,
                        mid,
                        &row.feed_url,
                        &row.entry_id,
                        send_outcome.entry_link.as_deref(),
                    )
                    .await
                    .ok();
                }
                // Mark sent so a later re-fetch/re-subscribe won't double-post.
                if let Err(e) = db::mark_entry_sent(&state.db, &row.chat_id, &row.feed_url, &row.entry_id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to mark entry sent (may re-push on re-subscribe)");
                }
                if let Err(e) = db::outbox_delete(&state.db, row.id).await {
                    warn!(entry_id = %row.entry_id, error = %e, "outbox: failed to delete entry");
                }
                db::delete_setting(&state.db, &db::setting_key::outbox_media_lost(row.id))
                    .await
                    .ok();
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
                let backoff = outbox_backoff_secs(attempts);
                let retry_now = chrono::Utc::now().timestamp();
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
    Ok(())
}

/// Extract entry link from serialized entry JSON (best-effort).
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
    fn outbox_abandon_requires_an_attempt_and_uses_first_attempt() {
        let now = 1_000_000_000;
        // Never attempted: held for a long grace period, then hard-capped so a
        // persistently failing attempts update cannot retry forever.
        assert!(!outbox_abandon_due(now - 5 * 24 * 3600, None, 0, now));
        assert!(outbox_abandon_due(
            now - OUTBOX_UNATTEMPTED_MAX_SECS,
            None,
            0,
            now
        ));
        // First attempt past 72h, even if retries kept happening since:
        // this is the regression the last_attempt_at anchor missed.
        assert!(outbox_abandon_due(
            now - 10 * 24 * 3600,
            Some(now - OUTBOX_ABANDON_SECS - 1),
            5,
            now
        ));
        // First attempt recent even though enqueued long ago: still retried.
        assert!(!outbox_abandon_due(
            now - 10 * 24 * 3600,
            Some(now - 3600),
            2,
            now
        ));
        // Legacy row (attempts>0, no first_attempt_at): use created_at.
        assert!(outbox_abandon_due(
            now - OUTBOX_ABANDON_SECS,
            None,
            1,
            now
        ));
    }

    #[test]
    fn stale_zero_counts_up_and_warns_on_multiples() {
        // First round: no warn (streak 1 of 20).
        assert_eq!(stale_zero_next(0), (1, false));
        assert_eq!(stale_zero_next(18), (19, false));
        // Exactly the threshold warns, then every 20 consecutive silent rounds.
        assert_eq!(stale_zero_next(STALE_ZERO_WARN_ROUNDS - 1), (20, true));
        assert_eq!(stale_zero_next(39), (40, true));
        // Between the multiples, counting continues silently.
        assert_eq!(stale_zero_next(40), (41, false));
        assert_eq!(stale_zero_next(60), (61, false));
    }
}
