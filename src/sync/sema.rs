use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::sync::atomic::Ordering::SeqCst;

use crate::sbi;
use crate::thread::{self, Thread};

/// Atomic counting semaphore
///
/// # Examples
/// ```
/// let sema = Semaphore::new(0);
/// sema.down();
/// sema.up();
/// ```
#[derive(Clone)]
pub struct Semaphore {
    value: Cell<usize>,
    waiters: RefCell<BTreeMap<u32, VecDeque<Arc<Thread>>>>,
}

unsafe impl Sync for Semaphore {}
unsafe impl Send for Semaphore {}

impl Semaphore {
    /// Creates a new semaphore of initial value n.
    pub fn new(n: usize) -> Self {
        Semaphore {
            value: Cell::new(n),
            waiters: RefCell::new(BTreeMap::new()),
        }
    }

    /// P operation
    pub fn down(&self) {
        let old = sbi::interrupt::set(false);

        // Is semaphore available?
        while self.value() == 0 {
            let current = thread::current();
            let priority = current.effective_priority.load(SeqCst);
            // `push_front` ensures to wake up threads in a fifo manner
            self.waiters
                .borrow_mut()
                .entry(priority)
                .or_default()
                .push_front(current);

            // Block the current thread until it's awakened by an `up` operation
            thread::block();
        }
        self.value.set(self.value() - 1);

        sbi::interrupt::set(old);
    }

    /// V operation
    pub fn up(&self) {
        let old = sbi::interrupt::set(false);
        let count = self.value.replace(self.value() + 1);

        // Check if we need to wake up a sleeping waiter
        self.rearrange();
        if self.waiters.borrow().last_key_value().is_some() {
            assert_eq!(count, 0);
            let highest_priority = *self.waiters.borrow().last_key_value().unwrap().0;
            let mut waiters = self.waiters.borrow_mut();
            let queue = waiters.get_mut(&highest_priority).unwrap();
            let waiter = queue.pop_back().unwrap();
            if queue.is_empty() {
                waiters.remove(&highest_priority);
            }
            thread::wake_up(waiter);
        }

        sbi::interrupt::set(old);

        thread::schedule();
    }

    /// Get the current value of a semaphore
    pub fn value(&self) -> usize {
        self.value.get()
    }

    pub fn waiters(&self) -> Vec<Arc<Thread>> {
        self.waiters
            .borrow()
            .values()
            .flat_map(|queue| queue.iter().cloned())
            .collect()
    }

    fn rearrange(&self) {
        let mut rearrange_threads = Vec::new();
        for (&prio, queue) in self.waiters.borrow_mut().iter_mut() {
            let len = queue.len();
            for _ in 0..len {
                let t = queue.pop_back().unwrap();
                if t.effective_priority.load(SeqCst) == prio {
                    queue.push_front(t);
                } else {
                    rearrange_threads.push(t);
                }
            }
        }
        for t in rearrange_threads {
            let priority = t.effective_priority.load(SeqCst);
            self.waiters
                .borrow_mut()
                .entry(priority)
                .or_default()
                .push_front(t);
        }
        self.waiters
            .borrow_mut()
            .retain(|_, queue| !queue.is_empty());
    }
}
