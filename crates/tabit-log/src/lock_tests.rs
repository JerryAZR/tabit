//! The lock helper's diagnostics: bounded claims name their holder,
//! the registry tracks holds, and a re-entrant claim fails loud
//! before blocking. The public helpers read the environment (process
//! — and therefore test-host — global), so these tests drive the
//! `*_under` doors with explicit diagnostics.

use std::panic::Location;
use std::sync::{Arc, Barrier, Mutex, RwLock};
use std::time::Duration;

use super::{Diag, HELD, addr_of, lock_under, read_under, write_under};

fn diag(timeout: Option<Duration>) -> Diag {
    Diag {
        timeout,
        trace: false,
    }
}

fn recorded(addr: usize) -> bool {
    HELD.lock()
        .expect("the registry lock is never poisoned")
        .as_ref()
        .is_some_and(|table| table.contains_key(&addr))
}

#[test]
fn an_uncontended_bounded_claim_acquires_and_releases() {
    let mutex = Mutex::new(0);
    let addr = addr_of(&mutex);
    {
        let mut guard = lock_under(
            &mutex,
            diag(Some(Duration::from_secs(5))),
            Location::caller(),
        );
        *guard = 7;
        assert!(recorded(addr), "an active claim is in the registry");
    }
    assert!(!recorded(addr), "the registry entry leaves with the guard");
}

#[test]
fn an_uncontended_bounded_rw_claim_acquires_and_releases() {
    let lock = RwLock::new(0);
    let addr = addr_of(&lock);
    {
        let _read = read_under(
            &lock,
            diag(Some(Duration::from_secs(5))),
            Location::caller(),
        );
        assert!(recorded(addr));
    }
    {
        let mut _write = write_under(
            &lock,
            diag(Some(Duration::from_secs(5))),
            Location::caller(),
        );
        *_write = 1;
        assert!(recorded(addr));
    }
    assert!(!recorded(addr));
}

#[test]
#[should_panic(expected = "blocked on: held by")]
fn a_blocked_bounded_claim_panics_naming_the_holder() {
    let mutex = Arc::new(Mutex::new(0));
    let held = mutex.clone();
    let claimed = Arc::new(Barrier::new(2));
    let gate = claimed.clone();
    std::thread::spawn(move || {
        let _guard = lock_under(
            &held,
            diag(Some(Duration::from_secs(30))),
            Location::caller(),
        );
        gate.wait();
        // Hold past the blocker's bound so the report has a live holder.
        std::thread::sleep(Duration::from_millis(300));
    });
    claimed.wait(); // the holder is in the registry from here
    let _blocked = lock_under(
        &mutex,
        diag(Some(Duration::from_millis(50))),
        Location::caller(),
    );
    // panics above; the detached holder releases on its own after its nap
}

#[test]
#[should_panic(expected = "re-entrant claim")]
fn a_same_thread_second_claim_fails_loud_before_blocking() {
    let mutex = Mutex::new(0);
    let _outer = lock_under(
        &mutex,
        diag(Some(Duration::from_secs(30))),
        Location::caller(),
    );
    let _inner = lock_under(
        &mutex,
        diag(Some(Duration::from_secs(30))),
        Location::caller(),
    );
}

#[test]
fn an_inactive_claim_leaves_no_registry_trace() {
    let mutex = Mutex::new(0);
    let addr = addr_of(&mutex);
    let _guard = lock_under(&mutex, diag(None), Location::caller());
    assert!(!recorded(addr), "no bookkeeping when diagnostics are off");
}
