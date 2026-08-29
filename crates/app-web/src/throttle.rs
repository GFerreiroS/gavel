//! A cap on how fast passwords can be guessed.
//!
//! Two reasons, and the second is the one that bites first:
//!
//! 1. Argon2id with the reference parameters is deliberately expensive. There
//!    was nothing stopping anyone spending that cost as often as they liked.
//! 2. That expense is 19 MiB and roughly 400ms of CPU **per attempt**. A
//!    handful of concurrent guesses is not a break-in attempt, it is a denial
//!    of service against a one-server deployment, and it arrives at the same
//!    endpoint.
//!
//! Keyed by **username**, not by address. The address is whatever the reverse
//! proxy in front of us decided to pass along; a header the client controls is
//! not an identity, and the socket address is the proxy's. The username is in
//! the request body and cannot be forged into someone else's bucket without
//! actually attacking that account.
//!
//! The known cost of that choice: an attacker can hold a username at its limit
//! and keep its owner out for as long as they keep guessing. That is the
//! accepted trade -- the window is short, it clears itself, and an unbounded
//! guess rate against one Argon2id hash is the worse of the two.
//!
//! In-process, like the SSE bus: correct for the one web replica this app
//! supports, and one more thing that needs shared state before there can be
//! two. See CLAUDE.md §6.

use std::collections::HashMap;
use std::sync::Mutex;

use cluster_core::Millis;

/// Failures a single username may collect inside the window before its
/// attempts start being refused.
pub const MAX_FAILURES: u32 = 8;

/// How long the count takes to clear, and how long a username stays refused
/// once it is over the limit.
pub const WINDOW_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, Copy)]
struct Bucket {
    failures: u32,
    /// When the last failure landed. The window is measured from here, so a
    /// run of guesses keeps pushing its own release back.
    last: Millis,
}

/// Recent failed sign-ins, by username.
#[derive(Debug, Default)]
pub struct LoginThrottle {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl LoginThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this username may be tried right now.
    ///
    /// Called before the password is hashed, which is the whole point: a
    /// refused attempt must not cost what an allowed one costs.
    pub fn allows(&self, username: &str, now: Millis) -> bool {
        let key = key(username);
        let mut buckets = self.lock();
        match buckets.get(&key) {
            Some(bucket) if expired(bucket, now) => {
                buckets.remove(&key);
                true
            }
            Some(bucket) => bucket.failures < MAX_FAILURES,
            None => true,
        }
    }

    /// Record a failure and, with it, the arrival that pays for the sweep.
    ///
    /// The sweep is here rather than on a timer because this is the only path
    /// that grows the map: a login nobody is attacking leaves nothing behind
    /// to collect.
    pub fn failed(&self, username: &str, now: Millis) {
        let key = key(username);
        let mut buckets = self.lock();
        buckets.retain(|_, bucket| !expired(bucket, now));
        let bucket = buckets.entry(key).or_insert(Bucket {
            failures: 0,
            last: now,
        });
        bucket.failures = bucket.failures.saturating_add(1);
        bucket.last = now;
    }

    /// A correct password clears the count. Someone who knows their own
    /// password is not the attacker the window was drawn around.
    pub fn succeeded(&self, username: &str) {
        self.lock().remove(&key(username));
    }

    /// How many usernames are currently being tracked, for the test that says
    /// the map does not grow without bound.
    #[cfg(test)]
    pub fn tracked(&self) -> usize {
        self.lock().len()
    }

    /// A poisoned mutex means a previous holder panicked while holding it.
    /// The map is two plain fields and cannot be left half-written, so the
    /// data is fine and refusing every sign-in from here on would be the
    /// worse failure.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Bucket>> {
        self.buckets.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn expired(bucket: &Bucket, now: Millis) -> bool {
    now.get().saturating_sub(bucket.last.get()) >= WINDOW_MS
}

/// Case-folded, so `Alice` and `alice` share a bucket. Usernames are ASCII by
/// [`app_core::auth::validate_username`], so this is the whole of it.
fn key(username: &str) -> String {
    username.to_ascii_lowercase()
}

/// New accounts this instance will create in a window, counting attempts
/// rather than successes.
///
/// Generous for what this is -- one server, whose first account is its
/// administrator and whose second is probably a friend. Ten in five minutes is
/// more registrations than a personal instance sees in a year.
pub const MAX_SIGN_UPS: u32 = 10;

/// A cap on registration, for the whole instance rather than per name.
///
/// Two problems, and per-username counting solves neither, because an attacker
/// working through a dictionary never repeats a name:
///
/// 1. **Registration answers "does this account exist".** It has to -- a
///    signup form that will not say the name is taken is not a signup form.
///    What it must not do is answer that question ten thousand times, which is
///    the difference between a design trade-off and an account list.
/// 2. **A free username costs two Argon2 passes**, or did: one to hash the new
///    password and one because registering used to sign you in by verifying
///    the password it had just hashed. That is ~800ms of CPU and 38 MiB, for
///    anyone, with no account required.
///
/// The cost of a global limit is that one attacker can stop everybody else
/// signing up for five minutes. On an instance with one administrator and no
/// open signup queue, that is the cheaper of the two.
#[derive(Debug)]
pub struct SignUpThrottle {
    window: Mutex<Option<Window>>,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    started: Millis,
    attempts: u32,
}

impl Default for SignUpThrottle {
    fn default() -> Self {
        Self::new()
    }
}

impl SignUpThrottle {
    pub fn new() -> Self {
        Self {
            window: Mutex::new(None),
        }
    }

    /// Count one attempt and say whether it may proceed.
    ///
    /// Counting on the way in, before the username is looked up, is what makes
    /// this cap enumeration: a name that turns out to be taken is refused in
    /// microseconds, so charging it only on success would leave the cheap
    /// probe unlimited.
    pub fn take(&self, now: Millis) -> bool {
        let mut guard = self.window.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = Window {
            started: now,
            attempts: 1,
        };
        match *guard {
            Some(window) if now.get().saturating_sub(window.started.get()) < WINDOW_MS => {
                let attempts = window.attempts.saturating_add(1);
                *guard = Some(Window { attempts, ..window });
                attempts <= MAX_SIGN_UPS
            }
            _ => {
                *guard = Some(fresh);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Millis {
        Millis(ms)
    }

    #[test]
    fn registration_stops_after_the_windows_worth() {
        let throttle = SignUpThrottle::new();
        for i in 0..MAX_SIGN_UPS {
            assert!(throttle.take(at(i as u64)), "attempt {i}");
        }
        assert!(!throttle.take(at(100)));
        assert!(!throttle.take(at(101)));
    }

    #[test]
    fn the_registration_window_reopens() {
        let throttle = SignUpThrottle::new();
        for i in 0..MAX_SIGN_UPS + 5 {
            throttle.take(at(i as u64));
        }
        assert!(!throttle.take(at(WINDOW_MS - 1)));
        assert!(throttle.take(at(WINDOW_MS)));
    }

    /// A refused attempt must not extend the window it was refused by, or a
    /// steady trickle of probes keeps everybody locked out for ever.
    #[test]
    fn a_refused_attempt_does_not_push_the_window_back() {
        let throttle = SignUpThrottle::new();
        for i in 0..MAX_SIGN_UPS + 20 {
            throttle.take(at(i as u64));
        }
        assert!(throttle.take(at(WINDOW_MS)), "the window still expires");
    }

    #[test]
    fn a_run_of_failures_closes_the_door() {
        let throttle = LoginThrottle::new();
        for i in 0..MAX_FAILURES {
            assert!(throttle.allows("alice", at(i as u64)), "attempt {i}");
            throttle.failed("alice", at(i as u64));
        }
        assert!(!throttle.allows("alice", at(MAX_FAILURES as u64)));
    }

    #[test]
    fn one_username_does_not_close_another() {
        let throttle = LoginThrottle::new();
        for i in 0..MAX_FAILURES {
            throttle.failed("alice", at(i as u64));
        }
        assert!(!throttle.allows("alice", at(10)));
        assert!(throttle.allows("bob", at(10)));
    }

    #[test]
    fn the_window_clears_itself() {
        let throttle = LoginThrottle::new();
        for i in 0..MAX_FAILURES {
            throttle.failed("alice", at(i as u64));
        }
        // Measured from the last failure, which landed at MAX_FAILURES - 1.
        let last = MAX_FAILURES as u64 - 1;
        assert!(!throttle.allows("alice", at(last + WINDOW_MS - 1)));
        assert!(throttle.allows("alice", at(last + WINDOW_MS)));
    }

    #[test]
    fn signing_in_clears_the_count() {
        let throttle = LoginThrottle::new();
        for i in 0..MAX_FAILURES - 1 {
            throttle.failed("alice", at(i as u64));
        }
        throttle.succeeded("alice");
        for i in 0..MAX_FAILURES {
            assert!(throttle.allows("alice", at(100 + i as u64)));
            throttle.failed("alice", at(100 + i as u64));
        }
        assert!(!throttle.allows("alice", at(200)));
    }

    #[test]
    fn case_does_not_buy_a_fresh_bucket() {
        let throttle = LoginThrottle::new();
        for i in 0..MAX_FAILURES {
            throttle.failed("ALICE", at(i as u64));
        }
        assert!(!throttle.allows("alice", at(10)));
    }

    #[test]
    fn stale_buckets_are_swept_rather_than_kept() {
        let throttle = LoginThrottle::new();
        for i in 0..500u64 {
            throttle.failed(&format!("user{i}"), at(i));
        }
        assert_eq!(throttle.tracked(), 500);

        // One arrival after the window, and everything older goes with it.
        throttle.failed("late", at(WINDOW_MS + 1_000));
        assert_eq!(throttle.tracked(), 1);
    }
}
