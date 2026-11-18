use std::collections::HashSet;

use feed_rs::model::Entry;

use crate::db;
use crate::fetcher;
use crate::i18n::Lang;
use crate::timeutil::entry_timestamp;

pub(super) struct PendingSelection<'a> {
    pub entries: Vec<&'a Entry>,
    pub filtered_ids: Vec<String>,
    pub deferred: bool,
}

pub(super) fn select_pending<'a>(
    entries: &'a [Entry],
    known_sent: &HashSet<String>,
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
        if known_sent.contains(&eid) {
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

    if pending.len() > 1 && pending.iter().all(|e| entry_timestamp(e).is_some()) {
        pending.sort_by_key(|e| entry_timestamp(e));
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
    use jiff::Timestamp;

    fn utc_at(year: i16, month: i8, day: i8) -> Timestamp {
        jiff::tz::TimeZone::UTC
            .to_zoned(jiff::civil::DateTime::new(year, month, day, 0, 0, 0, 0).unwrap())
            .unwrap()
            .timestamp()
    }

    fn entry(id: &str, title: &str, published: Option<Timestamp>) -> Entry {
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
        let known = HashSet::from(["sent".to_string()]);
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
        let entries = vec![
            entry("bad", "bad news", None),
            entry("ok", "good news", None),
        ];
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
        let old = utc_at(2026, 1, 1);
        let new = utc_at(2026, 1, 2);
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
