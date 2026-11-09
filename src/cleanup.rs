use std::collections::{BinaryHeap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use futures_util::TryStreamExt;
use sqlx::Row;
use tokio::fs;
use tracing::{info, warn};
use walkdir::WalkDir;

use crate::config::Config;
use crate::db::{self, Db};

/// Fixed retention for outbound media date directories.
const OUTBOUND_TTL_HOURS: u64 = 80;
/// Fixed retention for orphaned sent-entry markers.
const SENT_ENTRIES_RETENTION_DAYS: i64 = 180;
/// Fixed retention for the file_id upload dedup cache.
const FILE_ID_CACHE_RETENTION_DAYS: i64 = 30;
/// Fixed row cap for the sent-messages registry.
const SENT_MESSAGES_MAX_ROWS: i64 = 5000;

/// Run all cleanup tasks once. `ctx` is a log prefix distinguishing the
/// periodic run ("cleanup") from the startup run ("startup cleanup").
/// L6 fix: run_all_cleanup and run_startup_cleanup duplicated the same
/// 5-task sequence; collapsed into one function parameterized by context.
async fn run_cleanup_tasks(config: &Config, db_pool: &Db, ctx: &str) {
    // The two media-cleanup passes below both need the outbox protection set.
    // Computing it once here (instead of once per pass) halves the full-table
    // outbox scan cost. On query failure both media passes are skipped (an
    // empty protection set would delete retry-window media), but the
    // unrelated log/database/backup passes still run.
    match outbox_protected_paths(db_pool).await {
        Ok(protected) => {
            if let Err(e) = cleanup_outbound(config, &protected).await {
                warn!(error = %e, ctx = ctx, "outbound cleanup failed");
            }
            if let Err(e) = enforce_media_quota(config, &protected).await {
                warn!(error = %e, ctx = ctx, "media quota enforcement failed");
            }
        }
        Err(e) => {
            warn!(error = %e, ctx = ctx, "outbox query failed, skipping media cleanup passes to protect retry media");
        }
    }
    if let Err(e) = cleanup_logs(config).await {
        warn!(error = %e, ctx = ctx, "log cleanup failed");
    }
    if let Err(e) = cleanup_database(db_pool).await {
        warn!(error = %e, ctx = ctx, "database cleanup failed");
    }
    if let Err(e) = backup_database(db_pool, config).await {
        warn!(error = %e, ctx = ctx, "database backup failed");
    }
}

/// Run all cleanup tasks once (periodic schedule).
pub async fn run_all_cleanup(config: &Config, db_pool: &Db) {
    run_cleanup_tasks(config, db_pool, "cleanup").await;
}

/// Collect all media paths referenced by the outbox (path + thumb_path), used
/// to protect entries that have not finished retrying from being mistakenly
/// deleted by cleanup. Returns Err on query failure (the caller should skip
/// this cleanup round: an empty protection set would cause media still inside
/// the retry window to be mistakenly deleted).
/// Full query (no limit): during long outages with backlog, a limited query
/// would miss entries that need protection.
async fn outbox_protected_paths(db_pool: &Db) -> anyhow::Result<std::collections::HashSet<PathBuf>> {
    let mut set = std::collections::HashSet::new();
    let mut media_jsons = db::outbox_media_json_stream(db_pool);
    while let Some(mj) = media_jsons.try_next().await? {
        match serde_json::from_str::<Vec<crate::media::MediaItem>>(&mj) {
            Ok(items) => {
                for item in &items {
                    set.insert(item.path.clone());
                    if let Some(tp) = &item.thumb_path {
                        set.insert(tp.clone());
                    }
                }
            }
            Err(e) => {
                // Fail closed: without the paths this row's media would look
                // unreferenced and could be deleted while the retry still
                // needs it. Skipping the media passes this round is safer.
                warn!(error = %e, "outbox media_json parse failed; skipping media cleanup to protect retry media");
                anyhow::bail!("outbox media_json parse failed: {}", e);
            }
        }
    }
    Ok(set)
}

/// Directory-granularity retention cutoff for outbound media.
///
/// Media is archived into `outbound/<UTC date>/` directories, so a nominal
/// TTL of N hours deletes a directory only after `ceil(N/24)+1` full days —
/// an 80h TTL therefore keeps roughly 5–7 days of media. This is intentional
/// (lower-bound semantics) and documented in README/docs.
///
/// Pure and checked: extreme config values cannot panic or wrap.
pub(crate) fn outbound_cutoff(today: chrono::NaiveDate, ttl_hours: u64) -> chrono::NaiveDate {
    let ttl_days = ((ttl_hours as f64) / 24.0).ceil() as u64;
    let days = ttl_days.saturating_add(1);
    today
        .checked_sub_days(chrono::Days::new(days))
        .unwrap_or(today)
}

/// Remove outbound date directories older than TTL.
/// Protects media referenced by the outbox (protection set precomputed by the
/// caller): even if a directory is past its TTL, deletion is skipped as long
/// as outbox entries still reference files under that directory (prevents
/// media within the 72h retry window from being cleaned up prematurely).
pub async fn cleanup_outbound(
    config: &Config,
    outbox_protected: &std::collections::HashSet<PathBuf>,
) -> anyhow::Result<u64> {
    let outbound = config.media_dir.join("outbound");
    if !outbound.exists() {
        return Ok(0);
    }
    // UTC cutoff, paired with the UTC date_dir written by media.rs: the
    // comparison only stays correct if both sides use the same clock.
    let cutoff = outbound_cutoff(chrono::Utc::now().date_naive(), OUTBOUND_TTL_HOURS);
    let mut removed = 0u64;

    let mut entries = fs::read_dir(&outbound).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Ok(date) = chrono::NaiveDate::parse_from_str(&name_str, "%Y-%m-%d") {
            if date < cutoff {
                // Check whether any files under this directory are referenced by the outbox
                let dir_path = entry.path();
                let has_protected = outbox_protected.iter().any(|p| p.starts_with(&dir_path));
                if has_protected {
                    info!(dir = %name_str, "skipping outbound dir: outbox entries still reference files");
                    continue;
                }
                let size = dir_size(&dir_path).await.unwrap_or(0);
                // Only count freed space on successful deletion: counting on
                // failure would over-report (diagnostic only, but keep the
                // accounting accurate), and the directory wasn't actually
                // freed — it will be retried next round.
                if fs::remove_dir_all(dir_path).await.is_ok() {
                    removed += size;
                    info!(dir = %name_str, bytes = size, "removed expired outbound dir");
                } else {
                    warn!(dir = %name_str, "failed to remove expired outbound dir");
                }
            }
        }
    }
    Ok(removed)
}

/// Maximum number of candidate files kept in memory by one quota pass (the
/// oldest ones). When the overage needs more, another pass re-walks the tree.
const QUOTA_BATCH_FILES: usize = 8192;
/// Maximum re-walks per cleanup round. A heavily oversized tree is trimmed
/// across multiple hourly rounds instead of scanning unboundedly.
const QUOTA_MAX_PASSES: usize = 4;

/// Keep the `k` oldest entries from `items`. The max-heap evicts the newest
/// entry whenever it holds more than `k`, so memory stays bounded while the
/// selection remains oldest-first (ties fall back to path order).
fn keep_oldest<I>(items: I, k: usize) -> Vec<(SystemTime, PathBuf, u64)>
where
    I: IntoIterator<Item = (SystemTime, PathBuf, u64)>,
{
    let mut heap: BinaryHeap<(SystemTime, PathBuf, u64)> = BinaryHeap::with_capacity(k + 1);
    for item in items {
        heap.push(item);
        if heap.len() > k {
            heap.pop();
        }
    }
    let mut out = heap.into_vec();
    out.sort_by_key(|(t, _, _)| *t);
    out
}

/// Oldest deletable files under `root`, bounded to `k` entries. Outbox-
/// protected paths and files modified within the recent cutoff are skipped;
/// the recursive walk is blocking filesystem IO and is called from
/// `spawn_blocking`.
fn oldest_candidates(
    root: &Path,
    k: usize,
    protected: &HashSet<PathBuf>,
    recent_cutoff: SystemTime,
) -> Vec<(SystemTime, PathBuf, u64)> {
    let candidates = WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            if !entry.file_type().is_file() {
                return None;
            }
            let path = entry.path();
            if protected.contains(path) {
                return None;
            }
            let meta = entry.metadata().ok()?;
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if mtime >= recent_cutoff {
                return None;
            }
            Some((mtime, path.to_path_buf(), meta.len()))
        });
    keep_oldest(candidates, k)
}

/// Enforce total media directory size limit.
pub async fn enforce_media_quota(
    config: &Config,
    outbox_protected: &std::collections::HashSet<PathBuf>,
) -> anyhow::Result<u64> {
    let max_bytes = config.media_max_bytes();
    let total = dir_size(&config.media_dir).await?;
    if total <= max_bytes {
        return Ok(0);
    }

    // outbox_protected (paths referenced by pending retries) must not be
    // deleted, otherwise MediaOnly retries lose their media.
    let protected = Arc::new(outbox_protected.clone());
    let mut freed = 0u64;
    let mut deleted_files = 0u64;
    let mut remaining = total;
    // Protect recently written files: media files from a fetch cycle that is
    // still running have not entered the outbox yet (in-flight); quota
    // cleanup deletes oldest-first by mtime and could delete a file that was
    // just downloaded and about to be sent. The grace window should cover
    // only the download→send handoff (outbox paths are protected separately),
    // not an hour of unconstrained writes. 30 minutes covers the 600s Bot API
    // timeout plus queueing for a large local-API upload; the pre-download
    // byte budget is what actually prevents a full disk.
    let recent_cutoff = SystemTime::now() - Duration::from_secs(1800);

    for _pass in 0..QUOTA_MAX_PASSES {
        if remaining <= max_bytes {
            break;
        }
        let media_dir = config.media_dir.clone();
        let protected = Arc::clone(&protected);
        let batch = tokio::task::spawn_blocking(move || {
            oldest_candidates(&media_dir, QUOTA_BATCH_FILES, &protected, recent_cutoff)
        })
        .await?;
        if batch.is_empty() {
            break;
        }
        let mut progressed = false;
        for (_mtime, path, size) in batch {
            if remaining <= max_bytes {
                break;
            }
            if outbox_protected.contains(&path) {
                continue;
            }
            // Only deduct from the quota on successful deletion: deducting on
            // failure would over-report freed space and stop cleanup prematurely.
            match fs::remove_file(&path).await {
                Ok(()) => {
                    // saturating_sub: `remaining` is a snapshot of the initial
                    // total; concurrent rewrites can make the walked size
                    // smaller than the accumulated deductions, and an
                    // underflow would wrap and stop cleanup prematurely.
                    remaining = remaining.saturating_sub(size);
                    freed += size;
                    deleted_files += 1;
                    progressed = true;
                }
                Err(e) => {
                    warn!(path = ?path, error = %e, "failed to remove file for quota");
                }
            }
        }
        if !progressed {
            break;
        }
    }
    if remaining > max_bytes {
        warn!(
            remaining,
            max_bytes,
            "media quota still exceeded after cleanup passes; remaining files are retried next round"
        );
    }
    // One aggregated line per round instead of a per-file warn: on instances
    // where the quota is persistently saturated this fires hundreds of times
    // a day and drowns out real warnings. Deletion failures above keep their
    // per-file warn so individual problems remain locatable.
    if deleted_files > 0 {
        info!(
            files = deleted_files,
            bytes = freed,
            mb = (freed as f64 / 1_048_576.0 * 10.0).round() / 10.0,
            "removed files due to media quota"
        );
    }
    Ok(freed)
}

/// Remove log files older than retention period.
pub async fn cleanup_logs(config: &Config) -> anyhow::Result<u64> {
    let log_dir = &config.log_dir;
    if !log_dir.exists() {
        return Ok(0);
    }
    let ttl = Duration::from_secs((config.log_retention_days as u64) * 86400);
    let now = SystemTime::now();
    let mut removed = 0u64;

    let mut entries = fs::read_dir(log_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let meta = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue, // A single failure does not abort the whole round
        };
        if meta.is_file() {
            // Never delete arbitrary files that happen to sit in LOG_DIR: a
            // misconfigured LOG_DIR pointing at the data directory would
            // otherwise remove the DB or backups.
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with("rss-bot.log") {
                continue;
            }
            if let Ok(modified) = meta.modified() {
                if now.duration_since(modified).unwrap_or_default() > ttl {
                    let size = meta.len();
                    // Only count freed space on success (consistent with
                    // cleanup_outbound); failures leave a warning trace.
                    match fs::remove_file(entry.path()).await {
                        Ok(()) => {
                            removed += size;
                            info!(path = ?entry.path(), "removed old log file");
                        }
                        Err(e) => {
                            warn!(path = ?entry.path(), error = %e, "failed to remove old log file");
                        }
                    }
                }
            }
        }
    }
    Ok(removed)
}

/// Clean up database tables.
pub async fn cleanup_database(db_pool: &Db) -> anyhow::Result<()> {
    // Each step is independent: a transient DB error in one sweep must not
    // cancel the remaining maintenance for this round.
    async fn step<F, Fut>(name: &'static str, f: F) -> u64
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<u64>>,
    {
        match f().await {
            Ok(n) => n,
            Err(e) => {
                warn!(step = name, error = %e, "database cleanup step failed; continuing");
                0
            }
        }
    }

    // Only orphaned markers expire; markers for existing subscriptions are
    // kept regardless of age (see db::cleanup_sent_entries).
    let r1 = step("sent_entries", || {
        db::cleanup_sent_entries(db_pool, SENT_ENTRIES_RETENTION_DAYS)
    })
    .await;
    // The file_id cache is cleaned by retention period (30 days);
    // sent_messages is trimmed by row-count cap, so no time-based cleanup needed.
    let r2 = step("file_id_cache", || {
        db::cleanup_file_id_cache(db_pool, FILE_ID_CACHE_RETENTION_DAYS)
    })
    .await;
    // media_dl_fail: keys are only removed at two points — "download
    // succeeded" and "gave up after hitting the limit". If an entry rolls
    // out of the source's rolling window before hitting the limit, neither
    // point ever fires and the key lingers forever. A retention-period
    // cleanup on updated_at acts as the safety net (keys actively being
    // retried refresh updated_at on every failed round, so they're unaffected).
    let r3 = step("media_dl_fail", || {
        db::cleanup_stale_settings(db_pool, "media_dl_fail:", 7)
    })
    .await;
    // stale_zero: streak counters refresh updated_at on every zero round and
    // vanish with the unsubscribe cascade; this retention sweep only mops up
    // orphans left by subscriptions removed outside those paths.
    let r4 = step("stale_zero", || {
        db::cleanup_stale_settings(db_pool, "stale_zero:", 14)
    })
    .await;
    // outbox_media_lost: one-shot notice flags keyed by outbox row id. The
    // normal success/abandon paths delete them; this sweep catches rows
    // removed outside those paths.
    let r5 = step("outbox_media_lost", || {
        db::cleanup_stale_settings(db_pool, "outbox_media_lost:", 7)
    })
    .await;
    // sent_messages is capped by row count; trim here instead of on every
    // delivered message (the old per-insert COUNT was a full scan).
    let r6 = step("sent_messages", || {
        db::trim_sent_messages(db_pool, SENT_MESSAGES_MAX_ROWS)
    })
    .await;
    // Note: teloxide_dialogues table has no timestamp column.
    // Dialogue cleanup is handled via chat purge events instead.
    info!(
        sent_entries_deleted = r1,
        file_id_cache_deleted = r2,
        stale_settings_deleted = r3,
        stale_zero_deleted = r4,
        outbox_media_lost_deleted = r5,
        sent_messages_trimmed = r6,
        "database cleanup done"
    );
    Ok(())
}

/// Daily database backup: first run a WAL checkpoint(TRUNCATE) to flush data
/// back into the main database file, then copy the main file to obtain a
/// consistent snapshot. At most one backup per day; keep the last N days.
/// Backups are placed under backups/ in the same directory as the DB
/// (inside the container: /app/data/backups, persisted with the data
/// volume).
pub async fn backup_database(db_pool: &Db, config: &Config) -> anyhow::Result<()> {
    let Some(data_dir) = config.db_path.parent() else {
        return Ok(());
    };
    let backups = data_dir.join("backups");
    fs::create_dir_all(&backups).await?;
    crate::db::restrict_permissions(&backups, true);

    // Clean up orphaned .tmp files left behind by a crash (the normal flow
    // leaves none after the rename).
    if let Ok(mut rd) = fs::read_dir(&backups).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let n = e.file_name();
            if n.to_string_lossy().ends_with(".db.tmp") {
                fs::remove_file(e.path()).await.ok();
                info!(file = %n.to_string_lossy(), "removed stale backup tmp file");
            }
        }
    }

    // UTC date in the backup name, consistent with the UTC retention
    // cutoff below and with log/media date semantics across the bot.
    let today = chrono::Utc::now().format("%Y%m%d").to_string();
    let target = backups.join(format!("rss_bot_{}.db", today));
    if !target.exists() {
        // Best-effort WAL truncate for disk hygiene. It is NOT the consistency
        // mechanism anymore: a checkpoint can complete with busy=0 and a
        // concurrent writer can then trigger another checkpoint while the
        // main file is being copied (the old approach), producing a torn
        // backup. `VACUUM INTO` below reads a consistent snapshot (including
        // the WAL) regardless of concurrent activity, so a busy checkpoint
        // must not skip the backup.
        match sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(db_pool)
            .await
        {
            Ok(row) => {
                let busy: i64 = row.try_get("busy").unwrap_or(1);
                if busy != 0 {
                    warn!("wal_checkpoint reported busy; continuing with VACUUM INTO snapshot");
                }
            }
            Err(e) => warn!(error = %e, "wal_checkpoint failed; continuing with VACUUM INTO snapshot"),
        }

        // Write to a temp path first, then rename (atomic replacement): a
        // failure won't leave a half-written target behind, otherwise
        // target.exists() would make all retries for the day skip.
        let tmp = backups.join(format!("rss_bot_{}.db.tmp", today));
        if tmp.exists() {
            fs::remove_file(&tmp).await.ok();
        }
        // VACUUM INTO takes a string literal (not a bind parameter); the path
        // is local config data, and single quotes are escaped.
        let tmp_literal = tmp.to_string_lossy().replace('\'', "''");
        let vacuum = format!("VACUUM INTO '{}'", tmp_literal);
        if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(vacuum)).execute(db_pool).await {
            fs::remove_file(&tmp).await.ok();
            return Err(e.into());
        }
        // Backup files contain the full DB (cookies, user ids): owner-only,
        // like the live DB and its WAL/SHM.
        crate::db::restrict_permissions(&tmp, false);
        fs::rename(&tmp, &target).await?;
        crate::db::restrict_permissions(&target, false);
        info!(backup = %target.display(), "database backed up");
    }

    // Clean up old backups past the retention period (judged by the date in
    // the filename). Retention days must be at least 1: a negative value
    // would push the cutoff into the future and delete all backups.
    let retention = config.backup_retention_days.max(1);
    // UTC cutoff, same clock as the UTC `today` used in the backup name above.
    let cutoff = chrono::Utc::now().date_naive() - chrono::Duration::days(retention);
    let mut entries = fs::read_dir(&backups).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let date_part = name_str
            .strip_prefix("rss_bot_")
            .and_then(|s| s.strip_suffix(".db"));
        if let Some(d) = date_part {
            if let Ok(date) = chrono::NaiveDate::parse_from_str(d, "%Y%m%d") {
                if date < cutoff {
                    match fs::remove_file(entry.path()).await {
                        Ok(()) => info!(backup = %name_str, "removed old database backup"),
                        Err(e) => warn!(backup = %name_str, error = %e, "failed to remove old database backup"),
                    }
                }
            }
        }
    }
    Ok(())
}

/// Startup cleanup: run housekeeping once so a long-stopped bot starts tidy.
/// Each failure logs a warn (consistent with run_all_cleanup): the first
/// startup after a long shutdown is exactly when these cleanups most need to
/// be visible, so they must not be silently swallowed with let _ =. A backup
/// is also taken at startup (automatically skipped if one already exists for
/// the day), without waiting for the hourly cleanup job.
pub async fn run_startup_cleanup(config: &Config, db_pool: &Db) {
    // Crash-orphaned media temp files: the normal flow renames or deletes
    // `.tmp_*` after each attempt, but a crash can leave them behind. At
    // startup nothing is downloading, so every such file under the media dir
    // is stale and can be swept before size accounting runs.
    sweep_media_tmp_files(&config.media_dir).await;
    run_cleanup_tasks(config, db_pool, "startup cleanup").await;
}

/// Remove crash-orphaned `.tmp_*` files under the media root. Only safe at
/// startup (no in-flight downloads); the periodic cleanup must not call it.
async fn sweep_media_tmp_files(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = fs::read_dir(&dir).await else { continue };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(meta) = entry.metadata().await else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if entry.file_name().to_string_lossy().starts_with(".tmp_") {
                match fs::remove_file(entry.path()).await {
                    Ok(()) => info!(file = %entry.path().display(), "removed stale media tmp file"),
                    Err(e) => warn!(file = %entry.path().display(), error = %e, "failed to remove stale media tmp file"),
                }
            }
        }
    }
}

/// Recursive directory size. In-progress download temp files (`.tmp_*`) are
/// excluded: their bytes are already tracked by the MediaBudget reservation,
/// so counting them again would double-book the same file (and a crash
/// orphan would otherwise inflate the quota until the date-directory TTL).
pub async fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = match fs::read_dir(&dir).await {
            Ok(entries) => entries,
            // A directory disappearing (cleanup) or not existing yet is not a
            // failure; per-entry races below are skipped for the same reason.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(_) => break,
            };
            // A file can be renamed/deleted between readdir and metadata;
            // skipping it avoids turning a normal cleanup race into a failed
            // whole-directory scan (which would fail-close the media budget).
            let meta = match entry.metadata().await {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if !entry.file_name().to_string_lossy().starts_with(".tmp_") {
                total += meta.len();
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64, name: &str, size: u64) -> (SystemTime, PathBuf, u64) {
        (SystemTime::UNIX_EPOCH + Duration::from_secs(secs), PathBuf::from(name), size)
    }

    #[test]
    fn keep_oldest_keeps_the_oldest_k_in_order() {
        let items = (0..10).map(|i| at(i, &format!("f{i}"), i));
        let out = keep_oldest(items, 3);
        let names: Vec<String> = out
            .iter()
            .map(|(_, p, _)| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["f0", "f1", "f2"]);
    }

    #[test]
    fn keep_oldest_handles_fewer_items_than_the_cap() {
        let out = keep_oldest(vec![at(1, "only", 1)], 8);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, PathBuf::from("only"));
    }

    #[test]
    fn outbound_cutoff_is_date_granular_and_checked() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 3).unwrap();
        // 1h -> ceil(1/24)=1 day, +1 protection day => 2 days.
        assert_eq!(outbound_cutoff(today, 1), today - chrono::Days::new(2));
        // 24h -> 1+1.
        assert_eq!(outbound_cutoff(today, 24), today - chrono::Days::new(2));
        // 80h -> ceil(80/24)=4, +1 => 5 days (keeps 5-7 days of date dirs).
        assert_eq!(outbound_cutoff(today, 80), today - chrono::Days::new(5));
        // 168h -> 7+1.
        assert_eq!(outbound_cutoff(today, 168), today - chrono::Days::new(8));
        // Extreme (bounded by config) values must not panic.
        assert_eq!(outbound_cutoff(today, 87_600), today - chrono::Days::new(3651));
        assert_eq!(outbound_cutoff(today, u64::MAX), today);
    }
}
