//! Pending-entry selection for the fetch cycle.
//!
//! Extracted from `process_feed` so the dedup/filter/deadline/sort rules can be
//! tested without a database or Telegram client. The caller still owns the
//! sent/outbox queries and the async `mark_entry_sent` writes; this function is
//! pure apart from the cooperative deadline clock.

use std::collections::HashSet;

use feed_rs::model::Entry;

use crate::db;
use crate::fetcher;
use crate::i18n::Lang;

/// Result of selecting this round's pending entries.
pub(super) struct PendingSelection<'a> {
    /// Entries to push this round, in push order (oldest first when all have
    /// timestamps).
    pub entries: Vec<&'a Entry>,
    /// Entry ids rejected by the content filter. The caller marks these sent
    /// so the same blocked entry does not consume the window every round.
    pub filtered_ids: Vec<String>,
    /// True when the cooperative deadline stopped the scan; the caller keeps
    /// the previous conditional-request headers so the next round re-fetches.
    pub deferred: bool,
}

/// Select unsent entries for one source.
///
/// Rules kept identical to `process_feed`:
/// - repeated entry ids within one response are deduped;
/// - entries already in `known_sent` (64-bit marker hash) or in the outbox are
///   skipped (the outbox has sole retry authority);
/// - content-filtered entries are returned separately for mark-sent;
/// - the deadline is checked every 256 entries; a hit defers the rest;
/// - pending entries are sorted oldest-first only when every entry has a
///   timestamp (sources with missing timestamps keep their original order).
pub(super) fn select_pending<'a>(
    entries: &'a [Entry],
    known_sent: &HashSet<u64>,
    outbox_ids: &HashSet<String>,
    filter_cfg: &db::FilterConfig,
    filter_keywords: &[String],
    lang: Lang,
    deadline: tokio::time::Instant,
) -> PendingSelection<'a> {
    let mut pending: Vec<&'a Entry> = Vec::new();
    let mut filtered_ids: Vec<String> = Vec::new();
    let mut deferred = false;
    let mut seen_eids: HashSet<String> = HashSet::new();

    for (idx, entry) in entries.iter().enumerate() {
        if idx % 256 == 0 && tokio::time::Instant::now() >= deadline {
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
        if !filter_cfg
            .allows_title_with_keywords(&fetcher::entry_title(entry, lang), filter_keywords)
        {
            filtered_ids.push(eid);
            continue;
        }
        pending.push(entry);
    }

    if pending.len() > 1 && pending.iter().all(|e| e.published.or(e.updated).is_some()) {
        pending.sort_by_key(|e| e.published.or(e.updated));
    }

    PendingSelection {
        entries: pending,
        filtered_ids,
        deferred,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn entry(id: &str, title: &str, published: Option<chrono::DateTime<Utc>>) -> Entry {
        crate::platforms::make_entry(
            id.to_string(),
            title.to_string(),
            String::new(),
            String::new(),
            "text/html",
            format!("https://example.com/{id}"),
            published,
        )
    }

    fn future() -> tokio::time::Instant {
        tokio::time::Instant::now() + std::time::Duration::from_secs(60)
    }

    fn past() -> tokio::time::Instant {
        tokio::time::Instant::from_std(
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        )
    }

    fn filter(enabled: bool, mode: &str, keywords: &str) -> db::FilterConfig {
        db::FilterConfig {
            enabled,
            mode: mode.to_string(),
            keywords: keywords.to_string(),
        }
    }

    #[test]
    fn dedupes_repeated_ids_within_one_response() {
        let entries = vec![entry("e1", "first", None), entry("e1", "duplicate", None)];
        let out = select_pending(
            &entries,
            &HashSet::new(),
            &HashSet::new(),
            &filter(false, "", ""),
            &[],
            Lang::En,
            future(),
        );
        assert_eq!(out.entries.len(), 1);
        assert!(out.filtered_ids.is_empty());
        assert!(!out.deferred);
    }

    #[test]
    fn skips_sent_and_outbox_entries() {
        let entries = vec![
            entry("sent", "sent", None),
            entry("queued", "queued", None),
            entry("new", "new", None),
        ];
        let known = HashSet::from([db::entry_marker_hash("sent")]);
        let outbox = HashSet::from(["queued".to_string()]);
        let out = select_pending(
            &entries,
            &known,
            &outbox,
            &filter(false, "", ""),
            &[],
            Lang::En,
            future(),
        );
        assert_eq!(out.entries.len(), 1);
        assert_eq!(fetcher::entry_id(out.entries[0]), "new");
    }

    #[test]
    fn blacklist_returns_filtered_ids_for_mark_sent() {
        let entries = vec![entry("bad", "bad news", None), entry("ok", "good news", None)];
        let cfg = filter(true, "blacklist", "bad");
        let keywords = cfg.keyword_list();
        let out = select_pending(
            &entries,
            &HashSet::new(),
            &HashSet::new(),
            &cfg,
            &keywords,
            Lang::En,
            future(),
        );
        assert_eq!(out.filtered_ids, vec!["bad".to_string()]);
        assert_eq!(out.entries.len(), 1);
        assert_eq!(fetcher::entry_id(out.entries[0]), "ok");
    }

    #[test]
    fn whitelist_keeps_only_matching_titles() {
        let entries = vec![
            entry("keep", "keep this", None),
            entry("other", "something else", None),
        ];
        let cfg = filter(true, "whitelist", "keep");
        let keywords = cfg.keyword_list();
        let out = select_pending(
            &entries,
            &HashSet::new(),
            &HashSet::new(),
            &cfg,
            &keywords,
            Lang::En,
            future(),
        );
        assert_eq!(out.filtered_ids, vec!["other".to_string()]);
        assert_eq!(out.entries.len(), 1);
        assert_eq!(fetcher::entry_id(out.entries[0]), "keep");
    }

    #[test]
    fn pending_is_sorted_oldest_first_when_all_have_timestamps() {
        let old = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let new = Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();
        // Feed order is newest-first (the fetch layer sorts that way).
        let entries = vec![
            entry("new", "new", Some(new)),
            entry("old", "old", Some(old)),
        ];
        let out = select_pending(
            &entries,
            &HashSet::new(),
            &HashSet::new(),
            &filter(false, "", ""),
            &[],
            Lang::En,
            future(),
        );
        assert_eq!(fetcher::entry_id(out.entries[0]), "old");
        assert_eq!(fetcher::entry_id(out.entries[1]), "new");
    }

    #[test]
    fn missing_timestamps_keep_the_feed_order() {
        let entries = vec![entry("a", "a", None), entry("b", "b", None)];
        let out = select_pending(
            &entries,
            &HashSet::new(),
            &HashSet::new(),
            &filter(false, "", ""),
            &[],
            Lang::En,
            future(),
        );
        assert_eq!(fetcher::entry_id(out.entries[0]), "a");
        assert_eq!(fetcher::entry_id(out.entries[1]), "b");
    }

    #[test]
    fn past_deadline_defers_the_whole_batch() {
        let entries = vec![entry("a", "a", None)];
        let out = select_pending(
            &entries,
            &HashSet::new(),
            &HashSet::new(),
            &filter(false, "", ""),
            &[],
            Lang::En,
            past(),
        );
        assert!(out.entries.is_empty());
        assert!(out.deferred);
    }
}
