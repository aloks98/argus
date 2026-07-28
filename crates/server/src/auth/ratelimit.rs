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
//! 4. **Hand-rolled, not `governor`.** Considered and rejected: GCRA answers
//!    "too many requests per period?", but this state machine is keyed to
//!    *outcomes* -- escalating delay per consecutive failure, reservation
//!    unwound on success -- which a `Quota` cannot express. Governor's real
//!    value-add is keyed (per-IP) limiting, which decision 1 rules out here.
//!
//! This is a backstop, not the primary defence: the password is 24 random
//! characters (arithmetically unguessable online -- design §7) and argon2id's
//! ~100ms cost is the second layer. Accordingly this module stays pure logic
//! plus a mutex: no I/O, no database, no async, and it takes `now: Instant`
//! explicitly rather than reading the clock, so tests can drive time without
//! sleeping.
//!
//! **`check` RESERVES the slot it grants, atomically, before the caller ever
//! runs the (slow) argon2 verify.** Reading the count and recording the
//! failure as two separate steps would let N concurrent callers all observe
//! "under budget" before any of their ~100ms verifies finish, making the
//! real limit "N concurrent" instead of `BURST` total. Incrementing under
//! the same lock as the read closes that gap; `record_success` unwinds the
//! reservation the moment a real success is confirmed, so a legitimate login
//! is never left paying for its own reservation.

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
/// `std::sync::Mutex` rather than `&mut self`. Constructed once at startup
/// (`http::serve`) and consulted by the login handler (`auth::local::login`)
/// before it ever touches the database.
#[derive(Default)]
pub struct LoginLimiter {
    bucket: Mutex<Bucket>,
}

impl LoginLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// `None` means the caller may proceed immediately -- and this call has
    /// ALREADY counted that as a pessimistic failure, incrementing
    /// `consecutive_failures` and stamping `last_attempt` under the same lock
    /// acquisition that read them, so no concurrent caller can observe the
    /// pre-increment count. `record_success` unwinds that the moment a real
    /// success is confirmed; a caller that never calls back in simply leaves
    /// one reservation spent, the conservative direction to fail in.
    ///
    /// `Some(d)` means wait `d` longer, and makes NO state change at all --
    /// hammering while already delayed must not itself escalate the delay
    /// further, or an attacker could extend their own penalty into ours.
    ///
    /// Never blocks, sleeps, or touches the database -- the handler decides
    /// what a `429` looks like, and the (blocking) argon2 verify happens
    /// entirely after this returns `None`.
    pub fn check(&self, now: Instant) -> Option<Duration> {
        let mut bucket = self.lock();
        if bucket.consecutive_failures < BURST {
            bucket.consecutive_failures += 1;
            bucket.last_attempt = Some(now);
            return None;
        }
        if bucket.consecutive_failures == BURST {
            // Logged once per streak so a sustained brute force stays visible
            // even though the throttled path below skips the audit log
            // (consulting the limiter first is what keeps a hammering caller
            // from costing the database). Fires only when `consecutive_failures`
            // first equals `BURST` -- later admissions push it past `BURST`,
            // and calls that arrive while still delayed (the `else` arm) leave
            // it untouched -- so this can't re-fire on every poll, only once
            // per fresh streak.
            tracing::warn!("local admin login rate limiter: burst exhausted, now throttling");
        }
        let delay = Self::delay_for(bucket.consecutive_failures);
        let Some(last) = bucket.last_attempt else {
            // Unreachable in practice (`consecutive_failures >= BURST > 0`
            // implies some earlier call already set `last_attempt`), but if
            // it ever were reached, failing OPEN (no delay) rather than
            // unwrapping is the direction that can never become a lockout.
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

    /// A successful login clears the penalty entirely: the escalating delay
    /// punishes a consecutive run of *failures* (or reservations that never
    /// panned out), not lifetime attempts, so the legitimate user who
    /// eventually gets the (generated, unmemorable) password right is not
    /// left paying for earlier typos -- including the reservation `check`
    /// took out for THIS successful attempt itself.
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

    /// Drives the limiter through exactly `attempts` allowed reservations,
    /// advancing the simulated clock only as far as each one's own reported
    /// delay requires -- the fastest an attacker who never once succeeds
    /// could possibly go. Returns the `Instant` at which the last one was
    /// admitted, so callers can inspect the state immediately afterward.
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

    /// The concurrency property this module exists to guarantee (see the
    /// module doc): many requests arriving at (approximately) the same
    /// instant -- modelled here as the same identical `Instant`, i.e. zero
    /// interleaved verify latency at all, the worst case -- must not all be
    /// admitted. `check` reserves the slot itself, under the lock, before the
    /// caller ever runs the slow argon2 verify, so calling it far more than
    /// `BURST` times at one identical instant still only lets `BURST` through.
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

    /// A hard lockout would be a denial of service on the one credential that
    /// exists to rescue the operator. However many failed/reserved attempts
    /// occur, waiting must eventually allow another attempt. Driven through
    /// `check` itself now (rather than a separate `record_failure`), since
    /// `check` is the only way to add to the count at all -- this proves the
    /// property against the real, single entry point.
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

    /// The one mutation that turns "no permanent lockout" into a REAL
    /// permanent lockout: if the delayed (`Some(d)`) branch of `check` ever
    /// stamped `last_attempt = now`, every poll from a caller hammering the
    /// endpoint would push the countdown's origin forward by the poll
    /// interval, so `elapsed >= delay` would never become true again.
    ///
    /// `exhaust` (used by every other test here) cannot catch this: it calls
    /// `check` exactly once per delay period, jumping the simulated clock
    /// straight to the boundary, so it never exercises a caller that polls
    /// FASTER than the delay -- which is exactly what a real attacker (or an
    /// impatient legitimate user) does. This test polls in small steps
    /// instead, driven first well past the burst so the delay is pinned at
    /// its cap (`MAX_DELAY`) -- the worst, most realistic case -- and asserts
    /// admission happens within `MAX_DELAY` plus a little slack.
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
