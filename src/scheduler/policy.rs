pub(super) const MEDIA_DL_MAX_RETRIES: i64 = 10;
pub(super) const MEDIA_DL_MAX_DEFER_ROUNDS: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FlowAction {
    Continue,
    DeferContinue,
    DeferBreak,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MediaRetryStep {
    Proceed,
    Retry { fail_count: i64, notify: bool },
    Abandon { fail_count: i64 },
    Degrade { fail_count: i64 },
}

pub(super) fn media_retry_step(
    media_only: bool,
    download_failed: bool,
    stored: i64,
    notify_warnings: bool,
) -> MediaRetryStep {
    if media_only {
        let fail_count = stored.saturating_add(1);
        if fail_count >= MEDIA_DL_MAX_RETRIES {
            MediaRetryStep::Abandon { fail_count }
        } else {
            MediaRetryStep::Retry {
                fail_count,
                notify: notify_warnings,
            }
        }
    } else if !download_failed {
        MediaRetryStep::Proceed
    } else {
        let fail_count = stored.saturating_add(1);
        if fail_count < MEDIA_DL_MAX_DEFER_ROUNDS {
            MediaRetryStep::Retry {
                fail_count,
                notify: false,
            }
        } else {
            MediaRetryStep::Degrade { fail_count }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlatformRound {
    Skip,
    AllFailed,
    AllOk,
    Mixed,
}

pub(super) fn platform_round_verdict(
    seen: usize,
    active: usize,
    has_ok: bool,
    has_failed: bool,
) -> PlatformRound {
    if seen < active {
        return PlatformRound::Skip;
    }
    if has_failed && !has_ok {
        PlatformRound::AllFailed
    } else if has_ok && !has_failed {
        PlatformRound::AllOk
    } else {
        PlatformRound::Mixed
    }
}

pub(super) fn effective_feed_title(
    title_overridden: bool,
    fetched: Option<&str>,
    stored: Option<&str>,
) -> String {
    if title_overridden {
        return stored.unwrap_or_default().to_string();
    }
    fetched
        .filter(|s| !s.trim().is_empty())
        .or(stored)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_only_counts_up_to_the_cap_then_abandons() {
        assert_eq!(
            media_retry_step(true, true, 0, true),
            MediaRetryStep::Retry {
                fail_count: 1,
                notify: true
            }
        );
        assert_eq!(
            media_retry_step(true, true, 8, true),
            MediaRetryStep::Retry {
                fail_count: 9,
                notify: true
            }
        );
        assert_eq!(
            media_retry_step(true, true, 9, false),
            MediaRetryStep::Abandon { fail_count: 10 }
        );
        assert_eq!(
            media_retry_step(true, true, i64::MAX, true),
            MediaRetryStep::Abandon {
                fail_count: i64::MAX
            }
        );
    }

    #[test]
    fn normal_mode_defers_then_degrades_on_the_third_failure() {
        assert_eq!(
            media_retry_step(false, true, 0, false),
            MediaRetryStep::Retry {
                fail_count: 1,
                notify: false
            }
        );
        assert_eq!(
            media_retry_step(false, true, 1, true),
            MediaRetryStep::Retry {
                fail_count: 2,
                notify: false
            }
        );
        assert_eq!(
            media_retry_step(false, true, 2, true),
            MediaRetryStep::Degrade { fail_count: 3 }
        );
    }

    #[test]
    fn no_download_failure_proceeds_without_counting() {
        assert_eq!(
            media_retry_step(false, false, 0, false),
            MediaRetryStep::Proceed
        );
        assert_eq!(
            media_retry_step(false, false, 5, true),
            MediaRetryStep::Proceed
        );
    }

    #[test]
    fn platform_round_requires_every_active_source() {
        assert_eq!(
            platform_round_verdict(1, 2, true, false),
            PlatformRound::Skip
        );
        assert_eq!(
            platform_round_verdict(2, 2, true, true),
            PlatformRound::Mixed
        );
    }

    #[test]
    fn platform_round_classifies_all_ok_and_all_failed() {
        assert_eq!(
            platform_round_verdict(2, 2, true, false),
            PlatformRound::AllOk
        );
        assert_eq!(
            platform_round_verdict(2, 2, false, true),
            PlatformRound::AllFailed
        );
        assert_eq!(
            platform_round_verdict(2, 2, false, false),
            PlatformRound::Mixed
        );
    }

    #[test]
    fn effective_title_prefers_fetched_value_unless_overridden() {
        assert_eq!(effective_feed_title(false, Some("new"), Some("old")), "new");
        assert_eq!(effective_feed_title(true, Some("new"), Some("old")), "old");
        assert_eq!(effective_feed_title(false, None, Some("old")), "old");
        assert_eq!(effective_feed_title(false, Some("  "), Some("old")), "old");
        assert_eq!(effective_feed_title(false, None, None), "");
    }
}
