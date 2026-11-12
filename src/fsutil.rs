//! Filesystem helpers shared by the media budget and cleanup paths.
//!
//! `dir_size` used to live in `cleanup.rs`, which made `media.rs` depend on the
//! cleanup module while cleanup also depends on media item types. Moving the
//! neutral helper here keeps the dependency one-way.

use std::path::Path;

use tokio::fs;

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
