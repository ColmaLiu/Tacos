//! Kernel Threads

mod imp;
pub mod manager;
pub mod scheduler;
pub mod switch;

use crate::sbi;

pub use self::imp::*;
pub use self::manager::Manager;
pub(self) use self::scheduler::{Schedule, Scheduler};

use alloc::sync::Arc;
use core::sync::atomic::Ordering::SeqCst;

/// Create a new thread
pub fn spawn<F>(name: &'static str, f: F) -> Arc<Thread>
where
    F: FnOnce() + Send + 'static,
{
    Builder::new(f).name(name).spawn()
}

/// Get the current running thread
pub fn current() -> Arc<Thread> {
    Manager::get().current.lock().clone()
}

/// Yield the control to another thread (if there's another one ready to run).
pub fn schedule() {
    Manager::get().schedule()
}

/// Gracefully shut down the current thread, and schedule another one.
pub fn exit() -> ! {
    {
        let current = Manager::get().current.lock();

        #[cfg(feature = "debug")]
        kprintln!("Exit: {:?}", *current);

        current.set_status(Status::Dying);
    }

    schedule();

    unreachable!("An exited thread shouldn't be scheduled again");
}

/// Mark the current thread as [`Blocked`](Status::Blocked) and
/// yield the control to another thread
pub fn block() {
    let current = current();
    current.set_status(Status::Blocked);

    #[cfg(feature = "debug")]
    kprintln!("[THREAD] Block {:?}", current);

    schedule();
}

/// Wake up a previously blocked thread, mark it as [`Ready`](Status::Ready),
/// and register it into the scheduler.
pub fn wake_up(thread: Arc<Thread>) {
    assert_eq!(thread.status(), Status::Blocked);
    thread.set_status(Status::Ready);

    #[cfg(feature = "debug")]
    kprintln!("[THREAD] Wake up {:?}", thread);

    Manager::get().scheduler.lock().register(thread);
}

/// Sets the current thread's priority to a given value
pub fn set_priority(_priority: u32) {
    assert!(PRI_MIN <= _priority && _priority <= PRI_MAX);
    let old = sbi::interrupt::set(false);

    let cur = current();
    cur.priority.store(_priority, SeqCst);
    let mut prio = _priority;
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
        t = holder.clone();
    }

    #[cfg(feature = "thread-scheduler-priority")]
    Manager::get().scheduler.lock().rearrange();

    sbi::interrupt::set(old);
    schedule();
}

/// Returns the current thread's effective priority.
pub fn get_priority() -> u32 {
    current().effective_priority.load(SeqCst)
}

/// Make the current thread sleep for the given ticks.
pub fn sleep(ticks: i64) {
    let old = sbi::interrupt::set(false);

    let current = current();
    let start = sbi::timer::timer_ticks();
    let wakeup_tick = if ticks <= 0 { start } else { start + ticks };

    #[cfg(feature = "debug")]
    kprintln!(
        "[THREAD] {:?} sleeps at tick {} until tick {}",
        current,
        start,
        wakeup_tick
    );

    if ticks > 0 {
        Manager::get()
            .sleep_queue
            .lock()
            .sleep_until(current.clone(), wakeup_tick);
        current.set_status(Status::Blocked);
    }

    sbi::interrupt::set(old);

    schedule();
}
