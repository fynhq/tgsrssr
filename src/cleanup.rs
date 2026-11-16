//! Periodic/startup cleanup: media quota, logs and DB backups.
use futures_util::TryStreamExt;
use sqlx::Row;
use std::collections::{BinaryHeap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::fs;
use tracing::{info, warn};
use walkdir::WalkDir;

use crate::config::Config;
use crate::db::{self, Db};
use crate::fsutil::dir_size;

const OUTBOUND_TTL_HOURS: u64 = 80;
const SENT_ENTRIES_RETENTION_DAYS: i64 = 180;
const FILE_ID_CACHE_RETENTION_DAYS: i64 = 30;
const SENT_MESSAGES_MAX_ROWS: i64 = 5000;

async fn run_cleanup_tasks(config: &Config, db_pool: &Db, ctx: &str) {
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

pub async fn run_all_cleanup(config: &Config, db_pool: &Db) {
    run_cleanup_tasks(config, db_pool, "cleanup").await;
}

async fn outbox_protected_paths(
    db_pool: &Db,
) -> anyhow::Result<std::collections::HashSet<PathBuf>> {
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
                warn!(error = %e, "outbox media_json parse failed; skipping media cleanup to protect retry media");
                anyhow::bail!("outbox media_json parse failed: {}", e);
            }
        }
    }
    Ok(set)
}

pub(crate) fn outbound_cutoff(today: chrono::NaiveDate, ttl_hours: u64) -> chrono::NaiveDate {
    let ttl_days = ((ttl_hours as f64) / 24.0).ceil() as u64;
    let days = ttl_days.saturating_add(1);
    today
        .checked_sub_days(chrono::Days::new(days))
        .unwrap_or(today)
}

pub async fn cleanup_outbound(
    config: &Config,
    outbox_protected: &std::collections::HashSet<PathBuf>,
) -> anyhow::Result<u64> {
    let outbound = config.media_dir.join("outbound");
    if !outbound.exists() {
        return Ok(0);
    }
    let cutoff = outbound_cutoff(chrono::Utc::now().date_naive(), OUTBOUND_TTL_HOURS);
    let mut removed = 0u64;

    let mut entries = fs::read_dir(&outbound).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Ok(date) = chrono::NaiveDate::parse_from_str(&name_str, "%Y-%m-%d") {
            if date < cutoff {
                let dir_path = entry.path();
                let has_protected = outbox_protected.iter().any(|p| p.starts_with(&dir_path));
                if has_protected {
                    info!(dir = %name_str, "skipping outbound dir: outbox entries still reference files");
                    continue;
                }
                let size = dir_size(&dir_path).await.unwrap_or(0);
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

const QUOTA_BATCH_FILES: usize = 8192;
const QUOTA_MAX_PASSES: usize = 4;

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

pub async fn enforce_media_quota(
    config: &Config,
    outbox_protected: &std::collections::HashSet<PathBuf>,
) -> anyhow::Result<u64> {
    let max_bytes = config.media_max_bytes();
    let total = dir_size(&config.media_dir).await?;
    if total <= max_bytes {
        return Ok(0);
    }

    // outbox_protected paths must never be deleted; if the set cannot be read, skip media cleanup (fail closed).
    let protected = Arc::new(outbox_protected.clone());
    let mut freed = 0u64;
    let mut deleted_files = 0u64;
    let mut remaining = total;
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
            match fs::remove_file(&path).await {
                Ok(()) => {
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
            Err(_) => continue,
        };
        if meta.is_file() {
            // Only known filenames under LOG_DIR may be touched; never delete arbitrary files (a misconfigured path could erase the DB or backups).
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with("rss-bot.log") {
                continue;
            }
            if let Ok(modified) = meta.modified() {
                if now.duration_since(modified).unwrap_or_default() > ttl {
                    let size = meta.len();
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

pub async fn cleanup_database(db_pool: &Db) -> anyhow::Result<()> {
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

    let r1 = step("sent_entries", || {
        db::cleanup_sent_entries(db_pool, SENT_ENTRIES_RETENTION_DAYS)
    })
    .await;
    let r2 = step("file_id_cache", || {
        db::cleanup_file_id_cache(db_pool, FILE_ID_CACHE_RETENTION_DAYS)
    })
    .await;
    let r3 = step("media_dl_fail", || {
        db::cleanup_stale_settings(db_pool, "media_dl_fail:", 7)
    })
    .await;
    let r4 = step("stale_zero", || {
        db::cleanup_stale_settings(db_pool, "stale_zero:", 14)
    })
    .await;
    let r5 = step("outbox_media_lost", || {
        db::cleanup_stale_settings(db_pool, "outbox_media_lost:", 7)
    })
    .await;
    let r6 = step("sent_messages", || {
        db::trim_sent_messages(db_pool, SENT_MESSAGES_MAX_ROWS)
    })
    .await;
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

pub async fn backup_database(db_pool: &Db, config: &Config) -> anyhow::Result<()> {
    let Some(data_dir) = config.db_path.parent() else {
        return Ok(());
    };
    let backups = data_dir.join("backups");
    fs::create_dir_all(&backups).await?;
    crate::db::restrict_permissions(&backups, true);

    if let Ok(mut rd) = fs::read_dir(&backups).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let n = e.file_name();
            if n.to_string_lossy().ends_with(".db.tmp") {
                fs::remove_file(e.path()).await.ok();
                info!(file = %n.to_string_lossy(), "removed stale backup tmp file");
            }
        }
    }

    let today = chrono::Utc::now().format("%Y%m%d").to_string();
    let target = backups.join(format!("rss_bot_{}.db", today));
    if !target.exists() {
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
            Err(e) => {
                warn!(error = %e, "wal_checkpoint failed; continuing with VACUUM INTO snapshot")
            }
        }

        let tmp = backups.join(format!("rss_bot_{}.db.tmp", today));
        if tmp.exists() {
            fs::remove_file(&tmp).await.ok();
        }
        let tmp_literal = tmp.to_string_lossy().replace('\'', "''");
        let vacuum = format!("VACUUM INTO '{}'", tmp_literal);
        if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(vacuum))
            .execute(db_pool)
            .await
        {
            fs::remove_file(&tmp).await.ok();
            return Err(e.into());
        }
        crate::db::restrict_permissions(&tmp, false);
        fs::rename(&tmp, &target).await?;
        crate::db::restrict_permissions(&target, false);
        info!(backup = %target.display(), "database backed up");
    }

    let retention = config.backup_retention_days.max(1);
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
                        Err(e) => {
                            warn!(backup = %name_str, error = %e, "failed to remove old database backup")
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// Backup failures must stay visible; do not swallow them with let _ =.
pub async fn run_startup_cleanup(config: &Config, db_pool: &Db) {
    sweep_media_tmp_files(&config.media_dir).await;
    run_cleanup_tasks(config, db_pool, "startup cleanup").await;
}

// Orphan .tmp_* cleanup is startup-only; the periodic sweep must not run while downloads may be in flight.
async fn sweep_media_tmp_files(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if entry.file_name().to_string_lossy().starts_with(".tmp_") {
                match fs::remove_file(entry.path()).await {
                    Ok(()) => info!(file = %entry.path().display(), "removed stale media tmp file"),
                    Err(e) => {
                        warn!(file = %entry.path().display(), error = %e, "failed to remove stale media tmp file")
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64, name: &str, size: u64) -> (SystemTime, PathBuf, u64) {
        (
            SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            PathBuf::from(name),
            size,
        )
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
        assert_eq!(outbound_cutoff(today, 1), today - chrono::Days::new(2));
        assert_eq!(outbound_cutoff(today, 24), today - chrono::Days::new(2));
        assert_eq!(outbound_cutoff(today, 80), today - chrono::Days::new(5));
        assert_eq!(outbound_cutoff(today, 168), today - chrono::Days::new(8));
        assert_eq!(
            outbound_cutoff(today, 87_600),
            today - chrono::Days::new(3651)
        );
        assert_eq!(outbound_cutoff(today, u64::MAX), today);
    }
}
