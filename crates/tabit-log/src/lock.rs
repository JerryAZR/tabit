//! The one lock helper every sync mutex/rwlock claim in the session
//! stack funnels through (tabit-session re-exports it), for two
//! reasons: poison recovery (no code panics while holding one, so
//! poisoning cannot happen; recovery is the honest cheap answer), and
//! hang diagnosis — every claim can be traced and bounded from the
//! environment, turning a silent hang into a loud, located report.
//!
//! ## The claim contract (lock review, 2026-08)
//!
//! - **Order.** Nested claims run in one direction only:
//!   `conversation → buffer` (the engine's folds commit under the
//!   conversation's write hold) and `selection → buffer` (a model
//!   register write). Nothing claims the conversation or the selection
//!   while holding the buffer. Every other lock in the stack
//!   (mailbox queue, abort token, interaction pending map, workers
//!   table, …) is a leaf: claimed alone, never under another.
//! - **No re-entrancy.** A thread never claims a lock it already
//!   holds — std locks are not reentrant, and a same-thread
//!   read-then-read can deadlock against a waiting writer. With
//!   diagnostics on, a re-entrant claim is detected and panicked
//!   before it blocks.
//! - **No guard across an await.** Every hold is brief and
//!   synchronous; a std guard parked across an await deadlocks a
//!   single-threaded runtime — the classic silent hang.
//!
//! ## Hang diagnosis
//!
//! `TABIT_LOCK_TRACE=1` prints every acquire and release (lock
//! address, claim site, thread) to stderr. `TABIT_LOCK_TIMEOUT=<secs>`
//! bounds every claim; one that exceeds it panics with a full
//! waiting-for report: which lock and claim site blocked, which
//! thread holds it and from where, what the blocked thread itself
//! holds, and every lock held at that moment — a deadlock cycle is
//! visible in the table directly. Both default off; the cost when off
//! is one `OnceLock` read per claim.

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::Location;
use std::sync::{Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

/// Whether claim tracing is on (`TABIT_LOCK_TRACE`).
fn trace_enabled() -> bool {
    *TRACE.get_or_init(|| std::env::var("TABIT_LOCK_TRACE").is_ok_and(|value| value != "0"))
}

/// The claim bound (`TABIT_LOCK_TIMEOUT`, seconds).
fn claim_timeout() -> Option<Duration> {
    *TIMEOUT.get_or_init(|| {
        std::env::var("TABIT_LOCK_TIMEOUT")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs)
    })
}

static TRACE: OnceLock<bool> = OnceLock::new();
static TIMEOUT: OnceLock<Option<Duration>> = OnceLock::new();

/// The diagnostics one claim runs under. Production claims build it
/// from the environment; tests inject it directly (the environment is
/// process-global and tests share a process).
#[derive(Clone, Copy)]
pub(crate) struct Diag {
    timeout: Option<Duration>,
    trace: bool,
}

impl Diag {
    fn from_env() -> Self {
        Self {
            timeout: claim_timeout(),
            trace: trace_enabled(),
        }
    }

    fn active(&self) -> bool {
        self.timeout.is_some() || self.trace
    }
}

/// One held lock, as the report tells it.
struct Held {
    kind: &'static str,
    site: &'static Location<'static>,
    thread: String,
    since: Instant,
}

/// Every lock currently held, anywhere (`None` until diagnostics first
/// activate). A leaf: claimed only at claim bookends, never held while
/// a user lock is claimed or vice versa.
static HELD: Mutex<Option<HashMap<usize, Held>>> = Mutex::new(None);

// What this thread holds, by address — the re-entrancy check and the
// blocked-thread half of the report.
thread_local! {
    static HELD_HERE: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// The stable address of a lock, as its registry identity.
fn addr_of<T: ?Sized>(value: &T) -> usize {
    value as *const T as *const () as usize
}

fn thread_label() -> String {
    let current = std::thread::current();
    match current.name() {
        Some(name) => format!("{name} ({:?})", current.id()),
        None => format!("{:?}", current.id()),
    }
}

/// One claim's identity, carried by its guard for release bookkeeping.
#[derive(Clone, Copy)]
struct Claim {
    addr: usize,
    kind: &'static str,
    site: &'static Location<'static>,
    diag: Diag,
}

impl Claim {
    /// The same-thread re-entrancy check, before any blocking: a
    /// std lock claimed twice by one thread is a self-deadlock —
    /// fail loud instead of hanging.
    #[allow(clippy::panic)] // sanctioned crash: a detected self-deadlock (AGENTS.md doctrine)
    fn pre_claim(&self) {
        if !self.diag.active() {
            return;
        }
        let held = HELD_HERE.with(|stack| stack.borrow().contains(&self.addr));
        if held {
            panic!(
                "re-entrant claim: this thread already holds {} @ {:#x} (from an earlier \
                 claim); std locks are not reentrant — see the claim contract in \
                 tabit-log's lock module",
                self.kind, self.addr
            );
        }
    }

    /// Claim a mutex under this claim's diagnostics.
    fn acquire_mutex<'a, T: ?Sized>(&self, mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
        let Some(timeout) = self.diag.timeout else {
            return match mutex.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
        };
        let deadline = Instant::now() + timeout;
        loop {
            match mutex.try_lock() {
                Ok(guard) => return guard,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => return poisoned.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => self.check_deadline(deadline, timeout),
            }
        }
    }

    /// Read-claim an rwlock under this claim's diagnostics.
    fn acquire_read<'a, T: ?Sized>(&self, lock: &'a RwLock<T>) -> RwLockReadGuard<'a, T> {
        let Some(timeout) = self.diag.timeout else {
            return match lock.read() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
        };
        let deadline = Instant::now() + timeout;
        loop {
            match lock.try_read() {
                Ok(guard) => return guard,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => return poisoned.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => self.check_deadline(deadline, timeout),
            }
        }
    }

    /// Write-claim an rwlock under this claim's diagnostics.
    fn acquire_write<'a, T: ?Sized>(&self, lock: &'a RwLock<T>) -> RwLockWriteGuard<'a, T> {
        let Some(timeout) = self.diag.timeout else {
            return match lock.write() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
        };
        let deadline = Instant::now() + timeout;
        loop {
            match lock.try_write() {
                Ok(guard) => return guard,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => return poisoned.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => self.check_deadline(deadline, timeout),
            }
        }
    }

    /// Sleep-poll deadline check; a claim past its bound reports and
    /// panics (fail loud: the alternative is the silent hang this
    /// module exists to kill).
    fn check_deadline(&self, deadline: Instant, timeout: Duration) {
        if Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
            return;
        }
        self.report(timeout);
    }

    /// The waiting-for report. Built while holding the registry only;
    /// the panic unwinds without it, and the releasing guards' cleanup
    /// never panics back.
    #[allow(clippy::panic)] // sanctioned crash: the hang this module kills, made loud (AGENTS.md doctrine)
    fn report(&self, timeout: Duration) -> ! {
        let mut report = format!(
            "lock claim exceeded {timeout:?}: {} @ {:#x} from {}",
            self.kind, self.addr, self.site
        );
        if let Ok(registry) = HELD.lock()
            && let Some(table) = registry.as_ref()
        {
            if let Some(held) = table.get(&self.addr) {
                report.push_str(&format!(
                    "\n  blocked on: held by {} from {} for {:?}",
                    held.thread,
                    held.site,
                    held.since.elapsed()
                ));
            }
            let mine = HELD_HERE.with(|stack| stack.borrow().clone());
            if !mine.is_empty() {
                report.push_str("\n  this thread holds:");
                for addr in mine {
                    let line = table
                        .get(&addr)
                        .map(|held| format!("{} @ {:#x} from {}", held.kind, addr, held.site))
                        .unwrap_or_else(|| format!("{addr:#x}"));
                    report.push_str(&format!("\n    {line}"));
                }
            }
            if !table.is_empty() {
                report.push_str("\n  every lock held right now:");
                for (addr, held) in table {
                    report.push_str(&format!(
                        "\n    {} @ {addr:#x} held by {} from {} for {:?}",
                        held.kind,
                        held.thread,
                        held.site,
                        held.since.elapsed()
                    ));
                }
            }
        }
        panic!("{report}")
    }

    /// Post-acquire bookkeeping: the registry entry and this thread's
    /// held stack.
    fn on_held(&self) {
        if !self.diag.active() {
            return;
        }
        if self.diag.trace {
            eprintln!(
                "[lock] held     {} @ {:#x} from {}",
                self.kind, self.addr, self.site
            );
        }
        if let Ok(mut registry) = HELD.lock() {
            registry.get_or_insert_with(HashMap::new).insert(
                self.addr,
                Held {
                    kind: self.kind,
                    site: self.site,
                    thread: thread_label(),
                    since: Instant::now(),
                },
            );
        }
        HELD_HERE.with(|stack| stack.borrow_mut().push(self.addr));
    }

    /// Release bookkeeping. Never panics: a guard dropping during an
    /// unwind must not turn the report into an abort.
    fn on_release(&self) {
        if !self.diag.active() {
            return;
        }
        if let Ok(mut registry) = HELD.lock()
            && let Some(table) = registry.as_mut()
        {
            table.remove(&self.addr);
        }
        HELD_HERE.with(|stack| {
            let mut stack = stack.borrow_mut();
            if let Some(at) = stack.iter().rposition(|addr| *addr == self.addr) {
                stack.remove(at);
            }
        });
        if self.diag.trace {
            eprintln!(
                "[lock] released {} @ {:#x} from {}",
                self.kind, self.addr, self.site
            );
        }
    }
}

/// A traced [`MutexGuard`]; the claimed lock is released on drop.
pub struct Guard<'a, T: ?Sized> {
    inner: MutexGuard<'a, T>,
    claim: Claim,
}

impl<T: ?Sized> std::ops::Deref for Guard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> std::ops::DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: ?Sized> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.claim.on_release();
    }
}

/// A traced [`RwLockReadGuard`]; the claimed lock is released on drop.
pub struct ReadGuard<'a, T: ?Sized> {
    inner: RwLockReadGuard<'a, T>,
    claim: Claim,
}

impl<T: ?Sized> std::ops::Deref for ReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        self.claim.on_release();
    }
}

/// A traced [`RwLockWriteGuard`]; the claimed lock is released on drop.
pub struct WriteGuard<'a, T: ?Sized> {
    inner: RwLockWriteGuard<'a, T>,
    claim: Claim,
}

impl<T: ?Sized> std::ops::Deref for WriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> std::ops::DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: ?Sized> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        self.claim.on_release();
    }
}

/// Lock, recovering from poisoning. The claim is traced and bounded
/// when diagnostics are on (see the module docs).
///
/// # Panics
///
/// Under `TABIT_LOCK_TIMEOUT`, panics with a waiting-for report when
/// the claim exceeds the bound; under either diagnostic, panics on a
/// same-thread re-entrant claim before blocking.
#[track_caller]
pub fn lock<T: ?Sized>(mutex: &Mutex<T>) -> Guard<'_, T> {
    lock_under(mutex, Diag::from_env(), Location::caller())
}

/// Read-lock, recovering from poisoning. See [`lock`] for the
/// diagnostics contract.
#[track_caller]
pub fn read<T: ?Sized>(lock: &RwLock<T>) -> ReadGuard<'_, T> {
    read_under(lock, Diag::from_env(), Location::caller())
}

/// Write-lock, recovering from poisoning. See [`lock`] for the
/// diagnostics contract.
#[track_caller]
pub fn write<T: ?Sized>(lock: &RwLock<T>) -> WriteGuard<'_, T> {
    write_under(lock, Diag::from_env(), Location::caller())
}

/// [`lock`] with explicit diagnostics (the test door — the environment
/// is process-global).
pub(crate) fn lock_under<'a, T: ?Sized>(
    mutex: &'a Mutex<T>,
    diag: Diag,
    site: &'static Location<'static>,
) -> Guard<'a, T> {
    let claim = Claim {
        addr: addr_of(mutex),
        kind: "mutex",
        site,
        diag,
    };
    claim.pre_claim();
    let inner = claim.acquire_mutex(mutex);
    claim.on_held();
    Guard { inner, claim }
}

/// [`read`] with explicit diagnostics.
pub(crate) fn read_under<'a, T: ?Sized>(
    lock: &'a RwLock<T>,
    diag: Diag,
    site: &'static Location<'static>,
) -> ReadGuard<'a, T> {
    let claim = Claim {
        addr: addr_of(lock),
        kind: "read",
        site,
        diag,
    };
    claim.pre_claim();
    let inner = claim.acquire_read(lock);
    claim.on_held();
    ReadGuard { inner, claim }
}

/// [`write`] with explicit diagnostics.
pub(crate) fn write_under<'a, T: ?Sized>(
    lock: &'a RwLock<T>,
    diag: Diag,
    site: &'static Location<'static>,
) -> WriteGuard<'a, T> {
    let claim = Claim {
        addr: addr_of(lock),
        kind: "write",
        site,
        diag,
    };
    claim.pre_claim();
    let inner = claim.acquire_write(lock);
    claim.on_held();
    WriteGuard { inner, claim }
}

#[cfg(test)]
#[path = "lock_tests.rs"]
mod tests;
