//! # Condition Variable
//!
//! [`Condvar`] are able to block a thread so that it consumes no CPU time
//! while waiting for an event to occur. It is typically associated with a
//! boolean predicate (a condition) and a mutex. The predicate is always verified
//! inside of the mutex before determining that a thread must block.
//!
//! ## Usage
//!
//! Suppose there are two threads A and B, and thread A is waiting for some events
//! in thread B to happen.
//!
//! Here is the common practice of thread A:
//! ```rust
//! let pair = Arc::new(Mutex::new(false), Condvar::new());
//!
//! let (lock, cvar) = &*pair;
//! let condition = lock.lock();
//! while !condition {
//!     cvar.wait(&condition);
//! }
//! ```
//!
//! Here is a good practice of thread B:
//! ```rust
//! let (lock, cvar) = &*pair;
//!
//! // Lock must be held during a call to `Condvar.notify_one()`. Therefore, `guard` has to bind
//! // to a local variable so that it won't be dropped too soon.
//!
//! let guard = lock.lock(); // Bind `guard` to a local variable
//! *guard = true;           // Condition change
//! cvar.notify_one();       // Notify (`guard` will overlive this line)
//! ```
//!
//! Here is a bad practice of thread B:
//! ```rust
//! let (lock, cvar) = &*pair;
//!
//! *lock.lock() = true;     // Lock won't be held after this line.
//! cvar.notify_one();       // Buggy: notify another thread without holding the Lock
//! ```
//!

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use core::cell::RefCell;
use core::sync::atomic::Ordering::SeqCst;

use crate::sync::{Lock, MutexGuard, Semaphore};
use crate::thread;

pub struct Condvar(RefCell<BTreeMap<u32, VecDeque<Arc<Semaphore>>>>);

unsafe impl Sync for Condvar {}
unsafe impl Send for Condvar {}

impl Condvar {
    pub fn new() -> Self {
        Condvar(Default::default())
    }

    pub fn wait<T, L: Lock>(&self, guard: &mut MutexGuard<'_, T, L>) {
        let current = thread::current();
        let priority = current.priority.load(SeqCst);
        let sema = Arc::new(Semaphore::new(0));
        self.0
            .borrow_mut()
            .entry(priority)
            .or_default()
            .push_front(sema.clone());

        guard.release();
        sema.down();
        guard.acquire();
    }

    /// Wake up one thread from the waiting list
    pub fn notify_one(&self) {
        if self.0.borrow().last_key_value().is_some() {
            let highest_priority = *self.0.borrow().last_key_value().unwrap().0;
            let mut waiters = self.0.borrow_mut();
            let queue = waiters.get_mut(&highest_priority).unwrap();
            let sema = queue.pop_back().unwrap();
            if queue.is_empty() {
                waiters.remove(&highest_priority);
            }
            sema.up();
        }
    }

    /// Wake up all waiting threads
    pub fn notify_all(&self) {
        self.0
            .borrow()
            .values()
            .for_each(|semas| semas.iter().for_each(|sema| sema.up()));
        self.0.borrow_mut().clear();
    }
}
