//! Counting reservations for detached threads and open pipes.
//!
//! Capture readers, handshake writers and subprocess output readers all run on
//! detached threads that can outlive the call that started them (a descendant
//! holding a pipe write end keeps the reader blocked). Each family therefore
//! has a fixed slot count: reserve before spawning, release when the resource
//! is really gone, and fail with `WouldBlock` instead of growing without bound.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};

/// An active reservation against a process-global counter. Dropping it
/// releases the slot.
pub(crate) struct BudgetGuard {
    counter: &'static AtomicUsize,
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        let previous = self.counter.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "budget released without a reservation");
    }
}

/// Takes one slot from `counter`, or reports `WouldBlock` when `max` are in
/// use. `what` names the resource in the error message.
pub(crate) fn reserve(
    counter: &'static AtomicUsize,
    max: usize,
    what: &str,
) -> Result<BudgetGuard, io::Error> {
    let mut active = counter.load(Ordering::Acquire);
    loop {
        if active >= max {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{what} resource budget exhausted: {active} active, maximum {max}"),
            ));
        }
        match counter.compare_exchange_weak(active, active + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return Ok(BudgetGuard { counter }),
            Err(next) => active = next,
        }
    }
}

#[cfg(test)]
pub(crate) fn active(counter: &'static AtomicUsize) -> usize {
    counter.load(Ordering::Acquire)
}
