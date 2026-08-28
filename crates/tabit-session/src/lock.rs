//! One poison-recovering lock helper, shared everywhere the crate takes
//! a `std::sync::Mutex` (no code panics while holding one, so poisoning
//! cannot happen; recovery is the honest cheap answer).

/// Lock, recovering from poisoning.
pub(crate) fn lock<T: ?Sized>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
