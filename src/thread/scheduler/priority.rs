use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering::SeqCst;

use crate::thread::{Manager, Schedule, Status, Thread, PRI_MIN};

/// Priority Scheduler
#[derive(Default)]
pub struct Priority(BTreeMap<u32, VecDeque<Arc<Thread>>>);

impl Schedule for Priority {
    fn register(&mut self, thread: Arc<Thread>) {
        let priority = thread.effective_priority.load(SeqCst);
        self.0.entry(priority).or_default().push_front(thread);
    }

    fn schedule(&mut self) -> Option<Arc<Thread>> {
        let current = Manager::get().current.lock().clone();
        let current_priority = if current.status() == Status::Running {
            current.effective_priority.load(SeqCst)
        } else {
            PRI_MIN
        };
        let highest_priority = match self.0.last_key_value() {
            Some((priority, _)) => *priority,
            None => return None,
        };
        let queue = self.0.get_mut(&highest_priority).unwrap();
        if queue.back().unwrap().priority.load(SeqCst) >= current_priority {
            let next = queue.pop_back();
            if queue.is_empty() {
                self.0.remove(&highest_priority);
            }
            return next;
        }
        None
    }
}

impl Priority {
    pub fn rearrange(&mut self) {
        let mut rearrange_threads = Vec::new();
        for (&prio, queue) in self.0.iter_mut() {
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
            self.register(t);
        }
        self.0.retain(|_, queue| !queue.is_empty());
    }
}
