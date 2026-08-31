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
//! Protection is layered: a username bucket slows attacks on one account, an
//! origin bucket stops rotating usernames from one socket, and a global window
//! plus semaphore caps distributed CPU/memory cost. Proxy headers are ignored
//! unless the operator explicitly enables them behind a proxy that overwrites
//! those headers.
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
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cluster_core::Millis;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Failures a single username may collect inside the window before its
/// attempts start being refused.
pub const MAX_FAILURES: u32 = 8;

/// How long the count takes to clear, and how long a username stays refused
/// once it is over the limit.
pub const WINDOW_MS: u64 = 5 * 60 * 1000;
pub const MAX_ORIGIN_ATTEMPTS: u32 = 20;
pub const MAX_GLOBAL_ATTEMPTS: u32 = 100;
pub const MAX_ARGON2_CONCURRENCY: usize = 4;

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

/// Process-wide protection around every unauthenticated password hash.
#[derive(Debug)]
pub struct AuthGate {
    hashes: Arc<Semaphore>,
    origins: Mutex<HashMap<IpAddr, Window>>,
    global: Mutex<Option<Window>>,
}

impl Default for AuthGate {
    fn default() -> Self {
        Self::new(MAX_ARGON2_CONCURRENCY)
    }
}

impl AuthGate {
    pub fn new(max_hashes: usize) -> Self {
        Self {
            hashes: Arc::new(Semaphore::new(max_hashes.max(1))),
            origins: Mutex::new(HashMap::new()),
            global: Mutex::new(None),
        }
    }

    pub fn take(&self, origin: IpAddr, now: Millis) -> bool {
        let global_ok = take_window(&self.global, now, MAX_GLOBAL_ATTEMPTS);
        if !global_ok {
            // Do not retain attacker-chosen origins once the global budget is
            // exhausted. Otherwise the limit would cap CPU while leaving an
            // unbounded IP-keyed memory allocation behind it.
            return false;
        }
        let mut origins = self.origins.lock().unwrap_or_else(|e| e.into_inner());
        origins.retain(|_, window| now.get().saturating_sub(window.started.get()) < WINDOW_MS);
        let window = origins.entry(origin).or_insert(Window {
            started: now,
            attempts: 0,
        });
        window.attempts = window.attempts.saturating_add(1);
        window.attempts <= MAX_ORIGIN_ATTEMPTS
    }

    /// Immediate backpressure: requests above the memory-hard concurrency
    /// budget are rejected instead of forming an unbounded wait queue.
    pub fn try_hash(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.hashes).try_acquire_owned().ok()
    }
}

fn take_window(cell: &Mutex<Option<Window>>, now: Millis, maximum: u32) -> bool {
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    match *guard {
        Some(mut window) if now.get().saturating_sub(window.started.get()) < WINDOW_MS => {
            window.attempts = window.attempts.saturating_add(1);
            *guard = Some(window);
            window.attempts <= maximum
        }
        _ => {
            *guard = Some(Window {
                started: now,
                attempts: 1,
            });
            true
        }
    }
}

#[derive(Debug)]
pub struct SseGate {
    total: AtomicUsize,
    origins: Mutex<HashMap<IpAddr, usize>>,
    max_total: usize,
    max_per_origin: usize,
}

impl Default for SseGate {
    fn default() -> Self {
        Self {
            total: AtomicUsize::new(0),
            origins: Mutex::new(HashMap::new()),
            max_total: 256,
            max_per_origin: 8,
        }
    }
}

impl SseGate {
    pub fn enter(self: &Arc<Self>, origin: IpAddr) -> Option<SsePermit> {
        let previous = self.total.fetch_add(1, Ordering::AcqRel);
        if previous >= self.max_total {
            self.total.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        let mut origins = self.origins.lock().unwrap_or_else(|e| e.into_inner());
        let count = origins.entry(origin).or_default();
        if *count >= self.max_per_origin {
            self.total.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        *count += 1;
        Some(SsePermit {
            gate: self.clone(),
            origin,
        })
    }
}

pub struct SsePermit {
    gate: Arc<SseGate>,
    origin: IpAddr,
}

impl Drop for SsePermit {
    fn drop(&mut self) {
        self.gate.total.fetch_sub(1, Ordering::AcqRel);
        let mut origins = self.gate.origins.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = origins.get_mut(&self.origin) {
            *count -= 1;
            if *count == 0 {
                origins.remove(&self.origin);
            }
        }
    }
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
    fn rotating_usernames_cannot_evade_origin_or_global_limits() {
        let gate = AuthGate::new(2);
        let origin = "192.0.2.1".parse().unwrap();
        for i in 0..MAX_ORIGIN_ATTEMPTS {
            assert!(gate.take(origin, at(i as u64)));
        }
        assert!(!gate.take(origin, at(100)));

        let gate = AuthGate::new(2);
        for i in 0..MAX_GLOBAL_ATTEMPTS {
            let ip = std::net::Ipv4Addr::new(198, 51, (i / 250) as u8, (i % 250 + 1) as u8);
            assert!(gate.take(ip.into(), at(i as u64)));
        }
        assert!(!gate.take("203.0.113.9".parse().unwrap(), at(200)));
        for i in 0..1_000u16 {
            let ip = std::net::Ipv4Addr::new(203, (i / 250) as u8, 113, (i % 250) as u8);
            assert!(!gate.take(ip.into(), at(201 + u64::from(i))));
        }
        assert!(
            gate.origins.lock().unwrap().len() <= MAX_GLOBAL_ATTEMPTS as usize,
            "refused distributed traffic cannot grow the origin map"
        );
    }

    #[test]
    fn memory_hard_hashes_have_a_strict_concurrency_budget() {
        let gate = AuthGate::new(2);
        let first = gate.try_hash().unwrap();
        let second = gate.try_hash().unwrap();
        assert!(gate.try_hash().is_none());
        drop(first);
        assert!(gate.try_hash().is_some());
        drop(second);
    }

    #[test]
    fn sse_slots_are_bounded_per_origin_and_released_on_drop() {
        let gate = Arc::new(SseGate {
            total: AtomicUsize::new(0),
            origins: Mutex::new(HashMap::new()),
            max_total: 2,
            max_per_origin: 1,
        });
        let first = gate.enter("192.0.2.1".parse().unwrap()).unwrap();
        assert!(gate.enter("192.0.2.1".parse().unwrap()).is_none());
        let second = gate.enter("192.0.2.2".parse().unwrap()).unwrap();
        assert!(gate.enter("192.0.2.3".parse().unwrap()).is_none());
        drop(first);
        assert!(gate.enter("192.0.2.1".parse().unwrap()).is_some());
        drop(second);
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
