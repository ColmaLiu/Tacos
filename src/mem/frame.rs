//! Global frame table.
//!
//! Tracks physical frames allocated from UserPool: which process owns them,
//! which virtual address they back, and clock-algorithm metadata.

use alloc::collections::BTreeMap;

use crate::sync::{Intr, Lazy, Mutex};

/// Per-frame metadata
#[derive(Debug, Clone)]
pub struct FrameEntry {
    /// Thread id that owns this frame, or -1 if unowned
    pub owner_tid: isize,
    /// User virtual address this frame backs
    pub user_va: usize,
    /// Second-chance reference bit for clock algorithm
    pub ref_bit: bool,
    /// True if frame must not be evicted
    pub pinned: bool,
}

/// Global frame table singleton
pub struct FrameTable {
    frames: BTreeMap<usize, FrameEntry>, // phys_addr → entry
    clock_hand: usize,
    keys: alloc::vec::Vec<usize>,
    keys_dirty: bool,
}

impl FrameTable {
    pub fn instance() -> &'static Mutex<FrameTable, Intr> {
        static FTABLE: Lazy<Mutex<FrameTable, Intr>> =
            Lazy::new(|| Mutex::new(FrameTable::new()));
        &FTABLE
    }

    fn new() -> Self {
        Self {
            frames: BTreeMap::new(),
            clock_hand: 0,
            keys: alloc::vec::Vec::new(),
            keys_dirty: true,
        }
    }

    fn refresh_keys(&mut self) {
        if self.keys_dirty {
            self.keys = self.frames.keys().copied().collect();
            self.keys_dirty = false;
        }
    }

    pub fn register(&mut self, phys: usize, owner_tid: isize, user_va: usize) {
        self.frames.insert(
            phys,
            FrameEntry {
                owner_tid,
                user_va,
                ref_bit: true,
                pinned: false,
            },
        );
        self.keys_dirty = true;
    }

    pub fn unregister(&mut self, phys: usize) -> Option<FrameEntry> {
        let result = self.frames.remove(&phys);
        self.keys_dirty = true;
        result
    }

    pub fn update_owner(&mut self, phys: usize, new_tid: isize) {
        if let Some(e) = self.frames.get_mut(&phys) {
            e.owner_tid = new_tid;
        }
    }

    pub fn mark_accessed(&mut self, phys: usize) {
        if let Some(e) = self.frames.get_mut(&phys) {
            e.ref_bit = true;
        }
    }

    pub fn pin(&mut self, phys: usize) {
        if let Some(e) = self.frames.get_mut(&phys) {
            e.pinned = true;
        }
    }

    pub fn unpin(&mut self, phys: usize) {
        if let Some(e) = self.frames.get_mut(&phys) {
            e.pinned = false;
        }
    }

    pub fn get(&self, phys: usize) -> Option<&FrameEntry> {
        self.frames.get(&phys)
    }

    /// Clock / second-chance eviction. Returns the physical address of the
    /// victim frame, or None if no evictable frame exists.
    pub fn evict_one(&mut self) -> Option<(usize, FrameEntry)> {
        self.evict_one_for(-1)
    }

    /// Clock eviction that only considers frames belonging to `owner_tid`.
    /// Pass -1 to match any owner.
    pub fn evict_one_for(&mut self, owner_tid: isize) -> Option<(usize, FrameEntry)> {
        if self.frames.is_empty() {
            return None;
        }

        self.refresh_keys();
        if self.keys.is_empty() {
            return None;
        }

        let n = self.keys.len();
        let match_any = owner_tid < 0;
        for _ in 0..(n * 2) {
            if self.clock_hand >= self.keys.len() {
                self.clock_hand = 0;
            }
            let idx = self.clock_hand;
            self.clock_hand = (self.clock_hand + 1) % self.keys.len();

            let phys = self.keys[idx];
            let entry = match self.frames.get(&phys) {
                Some(e) => e,
                None => {
                    self.keys_dirty = true;
                    continue;
                }
            };

            if entry.pinned {
                continue;
            }

            if !match_any && entry.owner_tid != owner_tid {
                continue;
            }

            if entry.ref_bit {
                if let Some(e) = self.frames.get_mut(&phys) {
                    e.ref_bit = false;
                }
                continue;
            }

            self.keys_dirty = true;
            return self.frames.remove(&phys).map(|e| (phys, e));
        }

        // Fallback: try any unpinned frame (still respecting owner filter)
        for &phys in &self.keys {
            if let Some(e) = self.frames.get(&phys) {
                if !e.pinned && (match_any || e.owner_tid == owner_tid) {
                    self.keys_dirty = true;
                    return self.frames.remove(&phys).map(|e| (phys, e));
                }
            }
        }

        None
    }

    /// Refresh A/D bits from PTEs for the given process's page table.
    /// Walk all frames owned by `tid`, read the PTE A/D bits, update ref_bit,
    /// and clear the hardware A/D bits for the next round.
    pub fn refresh_ad_bits(
        &mut self,
        tid: isize,
        pagetable: &crate::mem::pagetable::PageTable,
    ) {
        for (&_phys, entry) in self.frames.iter_mut() {
            if entry.owner_tid != tid {
                continue;
            }
            if let Some(pte) = pagetable.get_pte(entry.user_va) {
                if pte.is_accessed() {
                    entry.ref_bit = true;
                }
            }
        }
    }

    /// Iterate over all frames and return pairs of (phys_addr, &FrameEntry)
    pub fn iter(&self) -> impl Iterator<Item = (&usize, &FrameEntry)> {
        self.frames.iter()
    }
}
