//! Global, in-memory rate limiter guarding `POST /auth/local` (design §10).
//! Four decisions here must not be quietly reversed:
//!
//! 1. **Global, not per-IP.** Behind Traefik the peer address is the
//!    proxy's; per-IP would require trusting the spoofable `X-Forwarded-For`.
//! 2. **No permanent lockout, ever.** A hard lock would DoS the one
//!    credential that rescues the operator; the delay escalates to
//!    `MAX_DELAY` and stops (see `no_sequence_of_failures_produces_a_permanent_lock`).
//! 3. **In memory, not a table.** One replica, no attacker-inducible restart
//!    to reset the counter -- a table would only buy durability against a
//!    crash nobody can cause, at the cost of a write per failed attempt.
//! 4. **Hand-rolled, not `governor`.** GCRA answers "too many per period?";
//!    this is keyed to *outcomes* (escalating delay per failure, unwound on
//!    success), which a `Quota` can't express -- and governor's real
//!    value-add, per-IP limiting, is what decision 1 rules out.
//!
//! Backstop, not the primary defence (the generated password + argon2id
//! are); stays pure logic plus a mutex -- no I/O, no async -- and takes
//! `now: Instant` explicitly so tests can drive time without sleeping.
//!
//! `check` RESERVES the slot it grants, atomically, before the caller's
//! (slow) argon2 verify runs -- reading the count and recording failure as
//! two steps would let N concurrent callers all see "under budget" first,
//! making the real limit "N concurrent" not `BURST` total.

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Attempts allowed before any delay is imposed.
pub const BURST: u32 = 5;

/// The escalating delay never exceeds this (design §10.2).
pub const MAX_DELAY: Duration = Duration::from_secs(30);

/// The mutable state, guarded by a single `Mutex` so `consecutive_failures`
/// and `last_attempt` are always updated together and never observed
/// half-written.
#[derive(Default)]
struct Bucket {
    consecutive_failures: u32,
    last_attempt: Option<Instant>,
}

/// Guards `POST /auth/local`, shared via `AppState` -- `&self` because state
/// lives behind a `Mutex`, not `&mut self`.
#[derive(Default)]
pub struct LoginLimiter {
    bucket: Mutex<Bucket>,
}

impl LoginLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// `None` = proceed -- ALREADY counted as a pessimistic failure under the
    /// same lock that read the count, so no concurrent caller sees the
    /// pre-increment value. `record_success` unwinds it; an uncalled-back
    /// caller just leaves one reservation spent (fails conservative).
    ///
    /// `Some(d)` = wait `d` longer, NO state change (hammering while delayed
    /// must not itself escalate the delay). Never blocks, sleeps, or touches the DB.
    pub fn check(&self, now: Instant) -> Option<Duration> {
        let mut bucket = self.lock();
        if bucket.consecutive_failures < BURST {
            bucket.consecutive_failures += 1;
            bucket.last_attempt = Some(now);
            return None;
        }
        if bucket.consecutive_failures == BURST {
            // Fires once per fresh streak (only when `consecutive_failures`
            // first hits `BURST`) since the throttled path skips the audit
            // log -- limiter-before-audit is what keeps a hammering caller
            // off the database.
            tracing::warn!("local admin login rate limiter: burst exhausted, now throttling");
        }
        let delay = Self::delay_for(bucket.consecutive_failures);
        let Some(last) = bucket.last_attempt else {
            // Unreachable in practice (`consecutive_failures >= BURST > 0`
            // implies `last_attempt` was already set), but if reached, fail
            // OPEN (no delay) -- never toward a lockout.
            bucket.consecutive_failures += 1;
            bucket.last_attempt = Some(now);
            return None;
        };
        let elapsed = now.saturating_duration_since(last);
        if elapsed >= delay {
            bucket.consecutive_failures = bucket.consecutive_failures.saturating_add(1);
            bucket.last_attempt = Some(now);
            None
        } else {
            Some(delay - elapsed)
        }
    }

    /// Clears the penalty entirely: the delay punishes a consecutive run of
    /// *failures*, not lifetime attempts, so a legitimate user who eventually
    /// gets the password right isn't left paying -- including for THIS
    /// attempt's own reservation.
    pub fn record_success(&self) {
        let mut bucket = self.lock();
        bucket.consecutive_failures = 0;
        bucket.last_attempt = None;
    }

    /// A panic elsewhere while holding this lock must not permanently break
    /// the login endpoint (a self-inflicted version of decision 2's
    /// lockout). The guarded state is two plain fields with no invariant a
    /// torn write can violate (worst case: one stale counter), so
    /// recovering on poison is safe.
    fn lock(&self) -> MutexGuard<'_, Bucket> {
        self.bucket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `2^(failures - BURST)` seconds, capped at `MAX_DELAY`. `checked_pow`
    /// (not a shift) saturates an arbitrarily long failure run to
    /// `u64::MAX` instead of overflowing/panicking; `.min` then collapses
    /// it to the cap.
    fn delay_for(consecutive_failures: u32) -> Duration {
        let exponent = consecutive_failures - BURST;
        let seconds = 2u64.checked_pow(exponent).unwrap_or(u64::MAX);
        Duration::from_secs(seconds).min(MAX_DELAY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives the limiter through `attempts` reservations, advancing the
    /// clock only as far as each delay requires -- the fastest a
    /// never-succeeding attacker could go. Returns the `Instant` of the last admission.
    fn exhaust(l: &LoginLimiter, mut now: Instant, attempts: u32) -> Instant {
        for _ in 0..attempts {
            while let Some(d) = l.check(now) {
                now += d;
            }
        }
        now
    }

    #[test]
    fn allows_until_the_burst_is_spent_then_delays() {
        let l = LoginLimiter::new();
        let t0 = Instant::now();
        for i in 0..BURST {
            assert!(
                l.check(t0).is_none(),
                "attempt {i} within burst must be allowed"
            );
        }
        assert!(
            l.check(t0).is_some(),
            "the attempt after the burst must be delayed"
        );
    }

    /// The property this module exists to guarantee: many requests at the
    /// same `Instant` (zero interleaved verify latency, the worst case) must
    /// not all be admitted -- `check` reserves under the lock before the
    /// caller's argon2 verify ever runs.
    #[test]
    fn concurrent_checks_at_the_same_instant_cannot_exceed_the_burst() {
        let l = LoginLimiter::new();
        let t0 = Instant::now();
        let allowed = (0..BURST + 10).filter(|_| l.check(t0).is_none()).count();
        assert_eq!(
            allowed, BURST as usize,
            "only the burst may pass at a single instant, no matter how many arrive"
        );
    }

    #[test]
    fn the_delay_escalates_but_is_capped() {
        let l = LoginLimiter::new();
        let now = exhaust(&l, Instant::now(), BURST + 20);
        let d = l.check(now).expect("should be delaying");
        assert!(
            d <= MAX_DELAY,
            "delay {d:?} must not exceed the cap {MAX_DELAY:?}"
        );
    }

    /// A hard lockout would DoS the recovery credential -- however many
    /// attempts occur, waiting must eventually allow another. Driven through
    /// `check` itself (the only way to add to the count), proving the
    /// property against the real entry point.
    #[test]
    fn no_sequence_of_failures_produces_a_permanent_lock() {
        let l = LoginLimiter::new();
        let now = exhaust(&l, Instant::now(), 10_000);
        let later = now + MAX_DELAY + Duration::from_secs(1);
        assert!(
            l.check(later).is_none(),
            "after waiting out the capped delay, an attempt must be allowed"
        );
    }

    /// The one mutation that would turn "no permanent lockout" into a REAL
    /// one: if the delayed branch ever stamped `last_attempt = now`, a caller
    /// polling faster than the delay would push the countdown's origin
    /// forward forever. `exhaust` can't catch this (it jumps straight to
    /// each boundary); this test polls in small steps instead, pinned at
    /// `MAX_DELAY`, and asserts admission within it plus slack.
    #[test]
    fn hammering_while_delayed_does_not_extend_the_delay() {
        let l = LoginLimiter::new();
        let mut now = exhaust(&l, Instant::now(), BURST + 20);

        let deadline = now + MAX_DELAY + Duration::from_secs(5);
        let mut admitted = false;
        while now < deadline {
            now += Duration::from_millis(100);
            if l.check(now).is_none() {
                admitted = true;
                break;
            }
        }
        assert!(
            admitted,
            "polling every 100ms must still be admitted within MAX_DELAY plus slack -- \
             if this fails, the delayed branch is mutating state (e.g. `last_attempt`) \
             it must leave alone, which is a real permanent lockout under a sustained flood"
        );
    }

    #[test]
    fn success_clears_the_penalty() {
        let l = LoginLimiter::new();
        let now = exhaust(&l, Instant::now(), BURST + 5);
        assert!(l.check(now).is_some());
        l.record_success();
        assert!(
            l.check(now).is_none(),
            "a successful login resets the limiter"
        );
    }
}
