//! In-memory command rate limiting and quota helpers.
//!
//! `/sub` and `/test` share one sliding-window limiter per non-admin user
//! (10 calls per minute, no daily cap). The limiter is intentionally in-memory:
//! a restart resets it, which is acceptable for an anti-abuse guard on a
//! closed admin/whitelist deployment.
//!
//! Subscription quotas (per channel / per user / platform sources) are
//! enforced from DB counts in the command layer; see `commands::handle_sub`.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `/sub` + `/test` calls allowed per non-admin user per window.
pub const COMMANDS_PER_MINUTE: usize = 10;
/// Sliding window length.
pub const COMMAND_WINDOW: Duration = Duration::from_secs(60);
/// Upper bound on tracked users. Allowed users are finite (admin + whitelist),
/// but the map is capped anyway so a future access-control regression cannot
/// grow it without bound.
const MAX_TRACKED_USERS: usize = 4096;
/// Maximum URLs accepted by a single `/sub` invocation (all users).
pub const MAX_URLS_PER_SUB: usize = 10;

/// Sliding-window rate limiter keyed by user ID.
///
/// `allow_at` takes an explicit `Instant` so tests can simulate window
/// expiry without sleeping.
pub struct CommandRateLimiter {
    inner: Mutex<HashMap<i64, VecDeque<Instant>>>,
}

impl CommandRateLimiter {
    pub fn new() -> Self {
        Self { inner: Mutex::new(HashMap::new()) }
    }

    /// Record a call for `user_id` and report whether it is allowed.
    pub fn allow(&self, user_id: i64) -> bool {
        self.allow_at(user_id, Instant::now())
    }

    fn allow_at(&self, user_id: i64, now: Instant) -> bool {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if map.len() >= MAX_TRACKED_USERS {
            // Drop queues whose calls have all left the window. If every
            // tracked user is still active, fail closed for a new user instead
            // of growing the map past the cap; silently exceeding the bound
            // defeats the purpose of MAX_TRACKED_USERS.
            map.retain(|_, q| {
                q.back()
                    .map(|t| now.duration_since(*t) < COMMAND_WINDOW)
                    .unwrap_or(false)
            });
            if map.len() >= MAX_TRACKED_USERS && !map.contains_key(&user_id) {
                return false;
            }
        }
        let q = map.entry(user_id).or_default();
        while q
            .front()
            .map(|t| now.duration_since(*t) >= COMMAND_WINDOW)
            .unwrap_or(false)
        {
            q.pop_front();
        }
        if q.len() >= COMMANDS_PER_MINUTE {
            return false;
        }
        q.push_back(now);
        true
    }
}

impl Default for CommandRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// One decision point shared by `/sub` and `/test`: admins are exempt.
pub fn allow_command(is_admin: bool, limiter: &CommandRateLimiter, user_id: i64) -> bool {
    is_admin || limiter.allow(user_id)
}

/// Subscription quota predicate: `cap <= 0` means unlimited; otherwise the
/// post-add count must not exceed the cap.
pub fn within_cap(cap: i64, current: i64, adding: i64) -> bool {
    cap <= 0 || current.saturating_add(adding) <= cap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_ten_then_rejects_eleventh() {
        let limiter = CommandRateLimiter::new();
        let t0 = Instant::now();
        for i in 0..COMMANDS_PER_MINUTE {
            assert!(limiter.allow_at(7, t0), "call {} should be allowed", i + 1);
        }
        assert!(!limiter.allow_at(7, t0), "11th call in the window must be rejected");
    }

    #[test]
    fn window_expiry_releases_capacity() {
        let limiter = CommandRateLimiter::new();
        let t0 = Instant::now();
        for _ in 0..COMMANDS_PER_MINUTE {
            assert!(limiter.allow_at(7, t0));
        }
        let later = t0 + COMMAND_WINDOW + Duration::from_secs(1);
        assert!(limiter.allow_at(7, later), "calls older than the window must not count");
    }

    #[test]
    fn users_are_independent() {
        let limiter = CommandRateLimiter::new();
        let t0 = Instant::now();
        for _ in 0..COMMANDS_PER_MINUTE {
            assert!(limiter.allow_at(1, t0));
        }
        assert!(!limiter.allow_at(1, t0));
        assert!(limiter.allow_at(2, t0), "another user must not inherit the limit");
    }

    #[test]
    fn admin_is_exempt() {
        let limiter = CommandRateLimiter::new();
        let t0 = Instant::now();
        for _ in 0..COMMANDS_PER_MINUTE {
            assert!(limiter.allow_at(7, t0));
        }
        assert!(!allow_command(false, &limiter, 7), "non-admin is rate limited");
        // Admin bypasses the limiter entirely.
        for _ in 0..100 {
            assert!(allow_command(true, &limiter, 1), "admin calls must always pass");
        }
    }

    #[test]
    fn stale_users_are_pruned_at_capacity() {
        let limiter = CommandRateLimiter::new();
        let t0 = Instant::now();
        {
            let mut map = limiter.inner.lock().unwrap();
            for uid in 0..MAX_TRACKED_USERS as i64 {
                let mut q = VecDeque::new();
                q.push_back(t0);
                map.insert(uid, q);
            }
        }
        let later = t0 + COMMAND_WINDOW + Duration::from_secs(1);
        assert!(limiter.allow_at(99_999, later), "new user should be tracked after pruning stale queues");
        let map = limiter.inner.lock().unwrap();
        assert!(map.len() < MAX_TRACKED_USERS, "stale queues must be dropped at capacity");
    }

    #[test]
    fn active_users_at_capacity_reject_new_tracking() {
        let limiter = CommandRateLimiter::new();
        let t0 = Instant::now();
        {
            let mut map = limiter.inner.lock().unwrap();
            for uid in 0..MAX_TRACKED_USERS as i64 {
                let mut q = VecDeque::new();
                q.push_back(t0);
                map.insert(uid, q);
            }
        }
        assert!(
            !limiter.allow_at(99_999, t0),
            "a full map of active users must reject a new user"
        );
        let map = limiter.inner.lock().unwrap();
        assert_eq!(map.len(), MAX_TRACKED_USERS);
        assert!(!map.contains_key(&99_999));
    }

    #[test]
    fn quota_caps_allow_exact_limit_and_zero_is_unlimited() {
        assert!(within_cap(99, 98, 1), "reaching the cap exactly is allowed");
        assert!(!within_cap(99, 99, 1), "exceeding by one is rejected");
        assert!(!within_cap(99, 90, 10), "batch larger than remaining capacity is rejected");
        assert!(within_cap(0, 10_000, 10_000), "0 means unlimited");
        assert!(within_cap(20, 19, 1));
        assert!(!within_cap(20, 20, 1));
    }
}
