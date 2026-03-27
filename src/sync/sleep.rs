use alloc::sync::Arc;
use core::cell::RefCell;
use core::sync::atomic::Ordering::SeqCst;

use crate::sbi;
use crate::sync::{Lock, Semaphore};
use crate::thread::{self, Thread};

#[cfg(feature = "thread-scheduler-priority")]
use crate::thread::Manager;

/// Sleep lock. Uses [`Semaphore`] under the hood.
#[derive(Clone)]
pub struct Sleep {
    inner: Semaphore,
    holder: RefCell<Option<Arc<Thread>>>,
}

impl Default for Sleep {
    fn default() -> Self {
        Self {
            inner: Semaphore::new(1),
            holder: Default::default(),
        }
    }
}

impl Lock for Sleep {
    fn acquire(&self) {
        let old = sbi::interrupt::set(false);
        let cur = thread::current();
        if self.holder.borrow().is_some() {
            *cur.waiting_thread.lock() = self.holder.borrow().clone();
            let mut t = cur;
            let mut prio;
            while t.waiting_thread.lock().is_some() {
                let holder = t.waiting_thread.lock().clone().unwrap();
                prio = t.effective_priority.load(SeqCst);
                if holder.effective_priority.load(SeqCst) < prio {
                    holder.effective_priority.store(prio, SeqCst);
                }
                holder.donors.lock().push(t);
                t = holder;
            }
        }

        #[cfg(feature = "thread-scheduler-priority")]
        Manager::get().scheduler.lock().rearrange();

        self.inner.down();
        self.holder.borrow_mut().replace(thread::current());
        sbi::interrupt::set(old);
    }

    fn release(&self) {
        assert!(Arc::ptr_eq(
            self.holder.borrow().as_ref().unwrap(),
            &thread::current()
        ));

        let old = sbi::interrupt::set(false);

        let cur = thread::current();
        let waiters = self.inner.waiters();
        cur.donors.lock().retain(|t| {
            let flag = waiters.iter().any(|waiter| Arc::ptr_eq(t, waiter));
            if flag {
                *t.waiting_thread.lock() = None;
            }
            !flag
        });
        let mut prio = cur.priority.load(SeqCst);
        for t in cur.donors.lock().iter() {
            let donor_prio = t.effective_priority.load(SeqCst);
            if donor_prio > prio {
                prio = donor_prio;
            }
        }
        cur.effective_priority.store(prio, SeqCst);
        let mut t = cur;
        while t.waiting_thread.lock().is_some() {
            let holder = t.waiting_thread.lock().clone().unwrap();
            let mut prio = holder.priority.load(SeqCst);
            for t in holder.donors.lock().iter() {
                let donor_prio = t.effective_priority.load(SeqCst);
                if donor_prio > prio {
                    prio = donor_prio;
                }
            }
            holder.effective_priority.store(prio, SeqCst);
            t = holder;
        }

        #[cfg(feature = "thread-scheduler-priority")]
        Manager::get().scheduler.lock().rearrange();

        sbi::interrupt::set(old);

        self.holder.borrow_mut().take().unwrap();
        self.inner.up();
    }
}

unsafe impl Sync for Sleep {}
