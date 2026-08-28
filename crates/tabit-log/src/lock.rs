//! One poison-recovering lock helper, shared everywhere a mutex is
//! taken (no code panics while holding one, so poisoning cannot
//! happen; recovery is the honest cheap answer).

/// Lock, recovering from poisoning.
pub fn lock<T: ?Sized>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
