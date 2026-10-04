//! Small helpers shared by several modules.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks `mutex`, ignoring poisoning. Every state ferry keeps behind a mutex
/// stays valid when a holder panics, and a panic must not cascade.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Cuts `text` to at most `max` bytes, moving the cut back to a char boundary.
pub(crate) fn truncate_at_char_boundary(text: &mut String, max: usize) {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}
