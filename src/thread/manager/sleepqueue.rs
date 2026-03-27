use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use crate::thread::Thread;

pub struct SleepQueue(BTreeMap<i64, Vec<Arc<Thread>>>);

impl SleepQueue {
    pub fn new() -> Self {
        SleepQueue(BTreeMap::new())
    }

    pub fn sleep_until(&mut self, thread: Arc<Thread>, wakeup_tick: i64) {
        self.0.entry(wakeup_tick).or_default().push(thread);
    }

    pub fn wake_expired(&mut self, current_tick: i64) -> Vec<Arc<Thread>> {
        let mut to_wake = Vec::new();
        while let Some((&tick, _)) = self.0.first_key_value() {
            if tick > current_tick {
                break;
            }
            if let Some(threads) = self.0.remove(&tick) {
                to_wake.extend(threads);
            }
        }
        to_wake
    }
}
