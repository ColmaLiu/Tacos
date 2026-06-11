//! Supplementary page table: per-process virtual-address -> backing store mapping.
//!
//! When a page fault occurs, the kernel looks up the faulting virtual address
//! here to determine where the page's data actually lives (RAM frame, swap slot,
//! executable file, or mmap'd file).

use alloc::collections::BTreeMap;

use crate::fs::File;
use crate::mem::pagetable::PTEFlags;
use crate::mem::PG_SIZE;

/// Describes where a page's data resides
#[derive(Clone)]
pub enum PageSource {
    /// Page is in a physical frame (V=1 in PTE)
    InFrame {
        phys_addr: usize,
        /// PTE flags that were set for this page
        flags: PTEFlags,
    },
    /// Page was evicted to swap
    InSwap { swap_index: usize, flags: PTEFlags },
    /// Page is demand-loaded from the executable file
    InFile {
        file: File,
        offset: usize,
        filesz: usize,
        flags: PTEFlags,
    },
    /// Page is demand-loaded from an mmap'd file
    MmapFile {
        file: File,
        offset: usize,
        mapid: usize,
        writable: bool,
    },
    /// Mmap'd page evicted to swap
    MmapSwap {
        swap_index: usize,
        mapid: usize,
        flags: PTEFlags,
    },
    /// Anonymous page (stack growth etc.) that was evicted to swap
    AnonSwap { swap_index: usize, flags: PTEFlags },
    /// Anonymous page (stack growth etc.) in a frame
    AnonFrame { phys_addr: usize, flags: PTEFlags },
}

impl PageSource {
    /// Whether this page was allocated from UserPool (vs. backed by file data)
    pub fn is_owned(&self) -> bool {
        matches!(
            self,
            PageSource::InSwap { .. }
                | PageSource::AnonSwap { .. }
                | PageSource::AnonFrame { .. }
                | PageSource::MmapSwap { .. }
        )
    }

    pub fn flags(&self) -> Option<PTEFlags> {
        match self {
            PageSource::InFrame { flags, .. } => Some(*flags),
            PageSource::InSwap { flags, .. } => Some(*flags),
            PageSource::InFile { flags, .. } => Some(*flags),
            PageSource::MmapFile { writable, .. } => {
                let mut f = PTEFlags::R | PTEFlags::U | PTEFlags::V;
                if *writable {
                    f |= PTEFlags::W;
                }
                Some(f)
            }
            PageSource::MmapSwap { flags, .. } => Some(*flags),
            PageSource::AnonSwap { flags, .. } => Some(*flags),
            PageSource::AnonFrame { flags, .. } => Some(*flags),
        }
    }
}

/// Supplementary page table entry
#[derive(Clone)]
pub struct SuppEntry {
    pub source: PageSource,
}

/// Per-process supplementary page table
#[derive(Clone)]
pub struct SuppPageTable {
    entries: BTreeMap<usize, SuppEntry>, // page-aligned va → entry
}

impl SuppPageTable {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, va: usize, source: PageSource) {
        let page = va & !(PG_SIZE - 1);
        self.entries.insert(page, SuppEntry { source });
    }

    pub fn get(&self, va: usize) -> Option<&SuppEntry> {
        let page = va & !(PG_SIZE - 1);
        self.entries.get(&page)
    }

    pub fn get_mut(&mut self, va: usize) -> Option<&mut SuppEntry> {
        let page = va & !(PG_SIZE - 1);
        self.entries.get_mut(&page)
    }

    pub fn remove(&mut self, va: usize) -> Option<SuppEntry> {
        let page = va & !(PG_SIZE - 1);
        self.entries.remove(&page)
    }

    pub fn contains(&self, va: usize) -> bool {
        let page = va & !(PG_SIZE - 1);
        self.entries.contains_key(&page)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&usize, &SuppEntry)> {
        self.entries.iter()
    }

    /// Find `pages` consecutive unmapped pages starting from `start`.
    /// Returns None if any page in the range is already in the table.
    pub fn is_range_free(&self, start: usize, pages: usize) -> bool {
        let base = start & !(PG_SIZE - 1);
        for p in 0..pages {
            if self.entries.contains_key(&(base + p * PG_SIZE)) {
                return false;
            }
        }
        true
    }

    /// Writes back dirty mmap pages and cleans up all entries for a given mapid.
    /// The actual file I/O is done outside locks — callers must split the work:
    /// 1. Call `collect_dirty_writebacks` to gather pages that need writeback
    /// 2. Release frame_table / supp_page locks
    /// 3. Perform the file writes
    /// 4. Call `cleanup_mapid` to free frames, unmap PTEs, and remove entries
    pub fn collect_dirty_writebacks(
        &self,
        mapid: usize,
        pagetable: &crate::mem::pagetable::PageTable,
    ) -> alloc::vec::Vec<(crate::fs::File, usize, [u8; PG_SIZE])> {
        let mut writebacks: alloc::vec::Vec<(crate::fs::File, usize, [u8; PG_SIZE])> =
            alloc::vec::Vec::new();

        for (&va, entry) in self.entries.iter() {
            let is_target = match &entry.source {
                PageSource::MmapFile { mapid: mid, .. }
                | PageSource::MmapSwap { mapid: mid, .. } => *mid == mapid,
                _ => false,
            };
            if !is_target {
                continue;
            }

            if let Some(pte) = pagetable.get_pte(va) {
                if pte.is_valid() && pte.is_dirty() {
                    if let PageSource::MmapFile { file, offset, .. } = &entry.source {
                        let phys = pte.pa().value();
                        let mut data = [0u8; PG_SIZE];
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                (phys + crate::mem::layout::VM_OFFSET) as *const u8,
                                data.as_mut_ptr(),
                                PG_SIZE,
                            );
                        }
                        writebacks.push((file.clone(), *offset, data));
                    }
                }
            }
        }

        writebacks
    }

    /// Free frames, unmap PTEs, and remove supp entries for all pages of a given mapid.
    pub fn cleanup_mapid(
        &mut self,
        mapid: usize,
        pagetable: &mut crate::mem::pagetable::PageTable,
        frame_table: &mut crate::mem::frame::FrameTable,
    ) {
        let pages_to_remove: alloc::vec::Vec<usize> = self
            .entries
            .iter()
            .filter(|(_, entry)| match &entry.source {
                PageSource::MmapFile { mapid: mid, .. }
                | PageSource::MmapSwap { mapid: mid, .. }
                    if *mid == mapid =>
                {
                    true
                }
                _ => false,
            })
            .map(|(&va, _)| va)
            .collect();

        for va in pages_to_remove {
            if let Some(pte) = pagetable.get_pte(va) {
                if pte.is_valid() {
                    let phys = pte.pa().value();
                    frame_table.unregister(phys);
                    unsafe {
                        crate::mem::palloc::UserPool::dealloc_pages(
                            (phys + crate::mem::layout::VM_OFFSET) as *mut u8,
                            1,
                        );
                    }
                }
            }

            pagetable.unmap(va);
            self.remove(va);
        }
    }

}
