//! Pure scheduling decisions extracted from `scheduler.rs`.
//!
//! These helpers contain no I/O and no AppState/Bot references, so the
//! branch-heavy parts of the fetch cycle can be tested directly.

/// Media-only download failure cap (per entry, in fetch rounds).
pub(super) const MEDIA_DL_MAX_RETRIES: i64 = 10;
/// Normal-mode download failure deferral cap (including the degrading round).
pub(super) const MEDIA_DL_MAX_DEFER_ROUNDS: i64 = 3;

/// Next step after reading the `media_dl_fail:` counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MediaRetryStep {
    /// No download failure: send normally and clear the counter.
    Proceed,
    /// Reading the counter failed: defer the entry, keep the old negotiation
    /// headers, and do not write a counter this round.
    ReadError,
    /// Record the incremented counter and defer the entry.
    Retry { fail_count: i64, notify: bool },
    /// MediaOnly cap reached: notify the owner and mark the entry sent.
    Abandon { fail_count: i64 },
    /// Normal-mode cap reached: delete the counter and send the entry with the
    /// media that survived.
    Degrade { fail_count: i64 },
}

/// Decide the next media-download retry step.
///
/// `stored` is the parsed counter; `Err(())` means the DB read failed. The
/// caller owns the write/delete/notification side effects so this stays pure.
pub(super) fn media_retry_step(
    media_only: bool,
    download_failed: bool,
    stored: Result<i64, ()>,
    notify_warnings: bool,
) -> MediaRetryStep {
    let stored = match stored {
        Ok(n) => n,
        Err(()) => return MediaRetryStep::ReadError,
    };
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

/// Platform-health verdict for one round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlatformRound {
    /// Not every active source was scheduled this round: no verdict.
    Skip,
    AllFailed,
    AllOk,
    /// Mixed results (or no valid signal): reset the streak.
    Mixed,
}

/// Classify one platform's round from the scheduled/active source counts and
/// the rounded ok/failed signals.
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

/// Effective feed title for this round: a non-overridden subscription prefers
/// the freshly fetched title, so a rename takes effect in the same round as
/// the DB update. An empty fetched title falls back to the stored value.
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
            media_retry_step(true, true, Ok(0), true),
            MediaRetryStep::Retry {
                fail_count: 1,
                notify: true
            }
        );
        assert_eq!(
            media_retry_step(true, true, Ok(8), true),
            MediaRetryStep::Retry {
                fail_count: 9,
                notify: true
            }
        );
        // 10th failure abandons; `notify` is irrelevant on this branch.
        assert_eq!(
            media_retry_step(true, true, Ok(9), false),
            MediaRetryStep::Abandon { fail_count: 10 }
        );
        // Saturating add keeps an absurd stored value bounded.
        assert_eq!(
            media_retry_step(true, true, Ok(i64::MAX), true),
            MediaRetryStep::Abandon {
                fail_count: i64::MAX
            }
        );
    }

    #[test]
    fn normal_mode_defers_then_degrades_on_the_third_failure() {
        assert_eq!(
            media_retry_step(false, true, Ok(0), false),
            MediaRetryStep::Retry {
                fail_count: 1,
                notify: false
            }
        );
        assert_eq!(
            media_retry_step(false, true, Ok(1), true),
            MediaRetryStep::Retry {
                fail_count: 2,
                notify: false
            }
        );
        assert_eq!(
            media_retry_step(false, true, Ok(2), true),
            MediaRetryStep::Degrade { fail_count: 3 }
        );
    }

    #[test]
    fn no_download_failure_proceeds_without_counting() {
        assert_eq!(
            media_retry_step(false, false, Ok(0), false),
            MediaRetryStep::Proceed
        );
        assert_eq!(
            media_retry_step(false, false, Ok(5), true),
            MediaRetryStep::Proceed
        );
    }

    #[test]
    fn counter_read_error_is_never_treated_as_zero() {
        assert_eq!(
            media_retry_step(true, true, Err(()), true),
            MediaRetryStep::ReadError
        );
        assert_eq!(
            media_retry_step(false, true, Err(()), false),
            MediaRetryStep::ReadError
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
        // No valid signal is a mixed round: reset the streak.
        assert_eq!(
            platform_round_verdict(2, 2, false, false),
            PlatformRound::Mixed
        );
    }

    #[test]
    fn effective_title_prefers_fetched_value_unless_overridden() {
        assert_eq!(effective_feed_title(false, Some("new"), Some("old")), "new");
        assert_eq!(effective_feed_title(true, Some("new"), Some("old")), "old");
        // Missing or blank fetched title falls back to the stored title.
        assert_eq!(effective_feed_title(false, None, Some("old")), "old");
        assert_eq!(effective_feed_title(false, Some("  "), Some("old")), "old");
        assert_eq!(effective_feed_title(false, None, None), "");
    }
}
