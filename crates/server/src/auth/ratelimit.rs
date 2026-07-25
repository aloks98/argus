//! Global, in-memory rate limiter guarding `POST /auth/local` (design §10).
//!
//! Three decisions here are deliberate and must not be quietly reversed:
//!
//! 1. **Global, not per-IP.** Behind Traefik the peer address seen by the
//!    control plane is the proxy's, so per-IP limiting either does nothing or
//!    requires trusting `X-Forwarded-For` -- a client-controlled header an
//!    attacker rotates freely, which would add a security dependency on
//!    untrusted input. A global limit cannot be evaded that way, and there is
//!    exactly one legitimate user to inconvenience.
//! 2. **No permanent lockout, ever.** A hard lock would be a denial of
//!    service on the one credential that exists to rescue the operator: an
//!    attacker who cannot guess the password could still deny the recovery
//!    path. The delay escalates to `MAX_DELAY` and stops there -- see
//!    `no_sequence_of_failures_produces_a_permanent_lock` below, which proves
//!    it for an arbitrarily long run of failures, not just a plausible one.
//! 3. **In memory, not a table.** There is one replica, and an attacker
//!    cannot force the restart that would reset the counter. A table would
//!    buy durability against a crash nobody can induce, at the cost of a
//!    write on every failed attempt.
//!
//! This is a backstop, not the primary defence: the password is 24 random
//! characters (arithmetically unguessable online -- design §7) and argon2id's
//! ~100ms cost is the second layer. Accordingly this module stays pure logic
//! plus a mutex: no I/O, no database, no async, and it takes `now: Instant`
//! explicitly rather than reading the clock, so tests can drive time without
//! sleeping.

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Attempts allowed before any delay is imposed.
pub const BURST: u32 = 5;

/// The escalating delay never exceeds this (design §10.2): a hard lock would
/// deny the one credential that exists to rescue the operator, so the delay
/// is capped rather than ever becoming permanent.
pub const MAX_DELAY: Duration = Duration::from_secs(30);

/// The mutable state, guarded by a single `Mutex` so `consecutive_failures`
/// and `last_attempt` are always updated together and never observed
/// half-written.
#[derive(Default)]
struct Bucket {
    consecutive_failures: u32,
    last_attempt: Option<Instant>,
}

/// Guards `POST /auth/local`. Shared across concurrent requests via
/// `AppState`, so every method takes `&self` -- interior mutability via
/// `std::sync::Mutex` rather than `&mut self`. Not yet consumed: the login
/// handler (`auth::local`, a later task of the same slice) is the intended
/// caller, hence the per-item `#[allow(dead_code)]` below rather than a
/// module-level one, matching the convention already used by
/// `auth/password.rs` and `repo.rs`'s `local_admin` items.
#[derive(Default)]
pub struct LoginLimiter {
    bucket: Mutex<Bucket>,
}

impl LoginLimiter {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// `None` means the caller may proceed immediately. `Some(d)` means wait
    /// `d` longer. Never blocks, sleeps, or touches the database -- the
    /// handler decides what a `429` looks like.
    #[allow(dead_code)]
    pub fn check(&self, now: Instant) -> Option<Duration> {
        let bucket = self.lock();
        if bucket.consecutive_failures < BURST {
            return None;
        }
        let delay = Self::delay_for(bucket.consecutive_failures);
        let last = bucket.last_attempt?;
        let elapsed = now.saturating_duration_since(last);
        if elapsed >= delay {
            None
        } else {
            Some(delay - elapsed)
        }
    }

    /// Record a failed attempt. Escalates the delay for the *next* `check`;
    /// it does not itself reject anything.
    #[allow(dead_code)]
    pub fn record_failure(&self, now: Instant) {
        let mut bucket = self.lock();
        bucket.consecutive_failures = bucket.consecutive_failures.saturating_add(1);
        bucket.last_attempt = Some(now);
    }

    /// A successful login clears the penalty entirely: the escalating delay
    /// punishes a consecutive run of *failures*, not lifetime attempts, so
    /// the legitimate user who eventually gets the (generated, unmemorable)
    /// password right is not left paying for earlier typos.
    #[allow(dead_code)]
    pub fn record_success(&self) {
        let mut bucket = self.lock();
        bucket.consecutive_failures = 0;
        bucket.last_attempt = None;
    }

    /// A panic in some unrelated request while holding this lock must not
    /// turn into a permanently broken login endpoint -- every subsequent
    /// request would see `.lock()` fail forever, which is a self-inflicted,
    /// worse version of the permanent lockout this module exists to avoid
    /// (decision 2 above). The guarded state is two plain fields with no
    /// invariant a half-finished write can violate (a torn update is at
    /// worst one stale counter), so recovering the inner value on poison and
    /// continuing is safe, and strictly better than wedging the endpoint.
    fn lock(&self) -> MutexGuard<'_, Bucket> {
        self.bucket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `2^(failures - BURST)` seconds, capped at `MAX_DELAY`. `checked_pow`
    /// (rather than a shift) means an arbitrarily long run of failures -- see
    /// the 10,000-failure test below -- saturates to `u64::MAX` seconds
    /// instead of overflowing or panicking, and the subsequent `.min`
    /// collapses that to the real cap either way.
    fn delay_for(consecutive_failures: u32) -> Duration {
        let exponent = consecutive_failures - BURST;
        let seconds = 2u64.checked_pow(exponent).unwrap_or(u64::MAX);
        Duration::from_secs(seconds).min(MAX_DELAY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_until_the_burst_is_spent_then_delays() {
        let l = LoginLimiter::new();
        let t0 = Instant::now();
        for i in 0..BURST {
            assert!(
                l.check(t0).is_none(),
                "attempt {i} within burst must be allowed"
            );
            l.record_failure(t0);
        }
        assert!(
            l.check(t0).is_some(),
            "the attempt after the burst must be delayed"
        );
    }

    #[test]
    fn the_delay_escalates_but_is_capped() {
        let l = LoginLimiter::new();
        let t0 = Instant::now();
        for _ in 0..(BURST + 20) {
            l.record_failure(t0);
        }
        let d = l.check(t0).expect("should be delaying");
        assert!(
            d <= MAX_DELAY,
            "delay {d:?} must not exceed the cap {MAX_DELAY:?}"
        );
    }

    /// A hard lockout would be a denial of service on the one credential that
    /// exists to rescue the operator. However many failures occur, waiting
    /// must eventually allow another attempt.
    #[test]
    fn no_sequence_of_failures_produces_a_permanent_lock() {
        let l = LoginLimiter::new();
        let t0 = Instant::now();
        for _ in 0..10_000 {
            l.record_failure(t0);
        }
        let later = t0 + MAX_DELAY + Duration::from_secs(1);
        assert!(
            l.check(later).is_none(),
            "after waiting out the capped delay, an attempt must be allowed"
        );
    }

    #[test]
    fn success_clears_the_penalty() {
        let l = LoginLimiter::new();
        let t0 = Instant::now();
        for _ in 0..(BURST + 5) {
            l.record_failure(t0);
        }
        assert!(l.check(t0).is_some());
        l.record_success();
        assert!(
            l.check(t0).is_none(),
            "a successful login resets the limiter"
        );
    }
}
