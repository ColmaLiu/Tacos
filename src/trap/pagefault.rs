use crate::fs::disk::SwapManager;
use crate::io::{Read, Seek, Write};
use crate::mem::frame::FrameTable;
use crate::mem::layout::{MAX_STACK_SIZE, USER_STACK_TOP, VM_BASE};
use crate::mem::pagetable::PTEFlags;
use crate::mem::palloc::UserPool;
use crate::mem::suppage::PageSource;
use crate::mem::userbuf::{
    __knrl_read_usr_byte_pc, __knrl_read_usr_exit, __knrl_write_usr_byte_pc, __knrl_write_usr_exit,
};
use crate::mem::{PhysAddr, PG_MASK, PG_SIZE};
use crate::thread;
use crate::trap::Frame;
use crate::userproc;

use riscv::register::scause::Exception::{self, *};
use riscv::register::sstatus::{self, SPP};

pub fn handler(frame: &mut Frame, fault: Exception, addr: usize) {
    let privilege = frame.sstatus.spp();

    #[cfg(feature = "debug")]
    let present = {
        let table = unsafe { crate::mem::pagetable::PageTable::effective_pagetable() };
        match table.get_pte(addr) {
            Some(entry) => entry.is_valid(),
            None => false,
        }
    };

    unsafe { sstatus::set_sie() };

    // Collect info about the faulting context
    let cur = thread::current();
    let userproc = cur.userproc.as_ref();

    #[cfg(feature = "debug")]
    kprintln!(
        "Page fault at {:#x}: {} error {} page in {} context.",
        addr,
        if present { "rights" } else { "not present" },
        match fault {
            StorePageFault => "writing",
            LoadPageFault => "reading",
            InstructionPageFault => "fetching instruction",
            _ => panic!("Unknown Page Fault"),
        },
        match privilege {
            SPP::Supervisor => "kernel",
            SPP::User => "user",
        }
    );

    match privilege {
        SPP::Supervisor => {
            if frame.sepc == __knrl_read_usr_byte_pc as _ {
                // Kernel fault reading user memory — try to fix the fault
                if addr < VM_BASE {
                    if handle_kernel_user_fault(addr) {
                        // Fault resolved (e.g. stack grew or page swapped in),
                        // return so the instruction retries.
                        return;
                    }
                }
                // Failed — signal error to caller
                frame.x[11] = 1;
                frame.sepc = __knrl_read_usr_exit as _;
            } else if frame.sepc == __knrl_write_usr_byte_pc as _ {
                if addr < VM_BASE {
                    if handle_kernel_user_fault(addr) {
                        return;
                    }
                }
                frame.x[11] = 1;
                frame.sepc = __knrl_write_usr_exit as _;
            } else {
                panic!("Kernel page fault at {:#x}", addr);
            }
        }
        SPP::User => {
            let resolved = match userproc {
                Some(_proc) => handle_user_fault(frame, fault, addr),
                None => false,
            };

            if !resolved {
                #[cfg(feature = "debug")]
                kprintln!(
                    "User thread {} dying due to page fault at {:#x}.",
                    cur.name(),
                    addr
                );
                userproc::exit(-1);
            }
        }
    }
}

/// Try to resolve a user-mode page fault. Returns true if the fault was resolved.
fn handle_user_fault(
    frame: &Frame,
    fault: Exception,
    addr: usize,
) -> bool {
    let fault_page = addr & !PG_MASK;

    // (0) If the page is already present (V=1), it's a permission fault —
    // kill the process. This also prevents reloading pages that are in a frame
    // but whose supp_page source was not mutated from InFile/MmapFile.
    let is_present = {
        let table = unsafe { crate::mem::pagetable::PageTable::effective_pagetable() };
        table.get_pte(addr).map(|e| e.is_valid()).unwrap_or(false)
    };
    if is_present {
        return false;
    }

    // (1) Check supplementary page table for known pages
    let action = {
        let cur = thread::current();
        if let Some(proc) = cur.userproc.as_ref() {
            let supp_page = proc.supp_page.lock();
            supp_page.get(addr).cloned().and_then(|entry| {
                match entry.source {
                    PageSource::InFile {
                        file,
                        offset,
                        filesz,
                        flags,
                    } => Some(FaultAction::LoadFromFile {
                        file,
                        offset,
                        filesz,
                        flags,
                    }),
                    PageSource::MmapFile {
                        file,
                        offset,
                        writable,
                        ..
                    } => {
                        let mut pte_flags = PTEFlags::R | PTEFlags::U | PTEFlags::V;
                        if writable {
                            pte_flags |= PTEFlags::W;
                        }
                        Some(FaultAction::LoadFromFile {
                            file,
                            offset,
                            filesz: PG_SIZE,
                            flags: pte_flags,
                        })
                    }
                    PageSource::InSwap { swap_index, flags } => {
                        Some(FaultAction::SwapIn { swap_index, flags })
                    }
                    PageSource::MmapSwap {
                        swap_index, flags, ..
                    } => Some(FaultAction::SwapIn { swap_index, flags }),
                    PageSource::AnonSwap { swap_index, flags } => {
                        Some(FaultAction::SwapIn { swap_index, flags })
                    }
                    PageSource::InFrame { .. } | PageSource::AnonFrame { .. } => None,
                }
            })
        } else {
            None
        }
    };

    // Execute the action outside the supp_page lock
    match action {
        Some(FaultAction::LoadFromFile {
            file,
            offset,
            filesz,
            flags,
        }) => {
            return load_from_file(fault_page, &file, offset, filesz, flags);
        }
        Some(FaultAction::SwapIn { swap_index, flags }) => {
            return swap_in(fault_page, swap_index, flags);
        }
        None => {
            // Check if PTE stores swap index from cross-process eviction.
            // When evicted cross-process, the swap slot is stored in the
            // PTE's PPN field (as swap_index + 1) and V is cleared.
            let table = unsafe { crate::mem::pagetable::PageTable::effective_pagetable() };
            if let Some(entry) = table.get_pte(addr) {
                let ppn = entry.ppn_raw();
                if ppn > 0 && !entry.is_valid() {
                    let flags = entry.flags() | PTEFlags::V;
                    return swap_in(fault_page, ppn - 1, flags);
                }
            }
        }
    }

    // (2) Stack growth heuristic — permission faults were ruled out above.
    // Only grow the stack if the user's sp is in the faulting page.
    // This distinguishes genuine stack accesses from wild pointer dereferences.
    let user_sp = frame.x[2];
    let sp_page = user_sp & !PG_MASK;

    if matches!(fault, LoadPageFault | StorePageFault)
        && fault_page == sp_page
        && fault_page >= (USER_STACK_TOP - MAX_STACK_SIZE)
        && fault_page >= 0x800
    {
        return grow_stack(fault_page);
    }

    false
}

/// Action extracted from supp page table to execute outside the lock
enum FaultAction {
    LoadFromFile {
        file: crate::fs::File,
        offset: usize,
        filesz: usize,
        flags: PTEFlags,
    },
    SwapIn {
        swap_index: usize,
        flags: PTEFlags,
    },
}

/// Handle a kernel-mode page fault on a user address (e.g., during a syscall
/// that reads/writes user memory). Tries stack growth or lazy loading.
fn handle_kernel_user_fault(addr: usize) -> bool {
    let fault_page = addr & !PG_MASK;

    // If the PTE is already valid, this is a permission fault (e.g. kernel
    // tried to write to a read-only user page). Signal an error.
    let is_present = {
        let table = unsafe { crate::mem::pagetable::PageTable::effective_pagetable() };
        table.get_pte(addr).map(|e| e.is_valid()).unwrap_or(false)
    };
    if is_present {
        return false;
    }

    let cur = thread::current();
    if let Some(proc) = cur.userproc.as_ref() {
        // Extract action from supp page table
        let action = {
            let supp = proc.supp_page.lock();
            if let Some(entry) = supp.get(addr) {
                let source_clone = entry.source.clone();
                match &source_clone {
                    PageSource::InSwap { swap_index, flags } => {
                        Some(FaultAction::SwapIn {
                            swap_index: *swap_index,
                            flags: *flags,
                        })
                    }
                    PageSource::MmapSwap {
                        swap_index, flags, ..
                    } => Some(FaultAction::SwapIn {
                        swap_index: *swap_index,
                        flags: *flags,
                    }),
                    PageSource::AnonSwap { swap_index, flags } => {
                        Some(FaultAction::SwapIn {
                            swap_index: *swap_index,
                            flags: *flags,
                        })
                    }
                    PageSource::InFile {
                        file,
                        offset,
                        filesz,
                        flags,
                    } => Some(FaultAction::LoadFromFile {
                        file: file.clone(),
                        offset: *offset,
                        filesz: *filesz,
                        flags: *flags,
                    }),
                    PageSource::MmapFile {
                        file,
                        offset,
                        writable,
                        ..
                    } => {
                        let mut pte_flags = PTEFlags::R | PTEFlags::U | PTEFlags::V;
                        if *writable {
                            pte_flags |= PTEFlags::W;
                        }
                        Some(FaultAction::LoadFromFile {
                            file: file.clone(),
                            offset: *offset,
                            filesz: PG_SIZE,
                            flags: pte_flags,
                        })
                    }
                    _ => None,
                }
            } else {
                None
            }
        };

        match action {
            Some(FaultAction::LoadFromFile {
                file,
                offset,
                filesz,
                flags,
            }) => {
                return load_from_file(fault_page, &file, offset, filesz, flags);
            }
            Some(FaultAction::SwapIn { swap_index, flags }) => {
                return swap_in(fault_page, swap_index, flags);
            }
            None => {
                // Check PTE for cross-process swap index
                let table = unsafe { crate::mem::pagetable::PageTable::effective_pagetable() };
                if let Some(entry) = table.get_pte(addr) {
                    let ppn = entry.ppn_raw();
                    if ppn > 0 && !entry.is_valid() {
                        let flags = entry.flags() | PTEFlags::V;
                        return swap_in(fault_page, ppn - 1, flags);
                    }
                }
            }
        }

        // Check stack growth: if fault is in the stack region, try growth
        if fault_page < USER_STACK_TOP && fault_page >= (USER_STACK_TOP - MAX_STACK_SIZE) {
            let in_supp = proc.supp_page.lock().contains(fault_page);
            if !in_supp {
                return grow_stack(fault_page);
            }
        }
    }

    false
}

/// Allocate a physical page, map it at `va`, read data from file at `offset`.
fn load_from_file(
    va: usize,
    file: &crate::fs::File,
    offset: usize,
    filesz: usize,
    flags: PTEFlags,
) -> bool {
    let page = allocate_or_evict();
    if page.is_null() {
        return false;
    }

    let page_slice = unsafe { core::slice::from_raw_parts_mut(page, PG_SIZE) };

    // Read data from file; zero-fill only the tail beyond file data
    let mut f = file.clone();
    if let Ok(pos) = f.pos() {
        *pos = offset;
    }
    let readsz = filesz.min(PG_SIZE);
    if readsz > 0 {
        let _ = f.read(&mut page_slice[..readsz]);
    }
    if readsz < PG_SIZE {
        page_slice[readsz..].fill(0);
    }

    #[cfg(feature = "debug")]
    kprintln!(
        "[LOAD] va={:#x} offset={:#x} readsz={} flags={:?}",
        va, offset, readsz, flags
    );

    // Map the page
    let pa = PhysAddr::from(page as usize);
    let mut pagetable = unsafe { crate::mem::pagetable::PageTable::effective_pagetable() };
    pagetable.map(pa, va, PG_SIZE, flags);

    // Register in frame table
    let tid = thread::current().id();
    FrameTable::instance().lock().register(pa.value(), tid, va);

    // Keep the original supp_page source (InFile / MmapFile) so that future
    // evictions can fall back to re-reading from the file. The PTE V bit
    // (checked in step 0 of handle_user_fault) tells us whether the page is
    // already in a frame.

    true
}

/// Grow the user stack by allocating the page at `va_start` and all unmapped
/// pages between it and the first already-mapped page above (closing the gap).
fn grow_stack(va_start: usize) -> bool {
    let stack_top = USER_STACK_TOP;
    let stack_bottom = USER_STACK_TOP - MAX_STACK_SIZE;
    let flags = PTEFlags::V | PTEFlags::R | PTEFlags::W | PTEFlags::U;
    let tid = thread::current().id();

    let cur = thread::current();
    let proc = match cur.userproc.as_ref() {
        Some(p) => p,
        None => return false,
    };

    let mut pagetable = unsafe { crate::mem::pagetable::PageTable::effective_pagetable() };
    let mut va = va_start;

    loop {
        if va >= stack_top || va < stack_bottom {
            break;
        }

        // Stop when we reach an already-mapped page (the existing stack)
        if pagetable.get_pte(va).map(|e| e.is_valid()).unwrap_or(false) {
            break;
        }

        let page = allocate_or_evict();
        if page.is_null() {
            return false;
        }

        unsafe { core::ptr::write_bytes(page, 0, PG_SIZE) };

        let pa = PhysAddr::from(page as usize);
        pagetable.map(pa, va, PG_SIZE, flags);

        FrameTable::instance().lock().register(pa.value(), tid, va);

        let mut supp = proc.supp_page.lock();
        supp.insert(
            va,
            PageSource::AnonFrame {
                phys_addr: pa.value(),
                flags,
            },
        );

        #[cfg(feature = "debug")]
        kprintln!("[STACK] Grew stack to {:#x}", va);

        va += PG_SIZE;
    }

    true
}

/// Swap in a page from the swap file.
fn swap_in(
    va: usize,
    swap_index: usize,
    flags: PTEFlags,
) -> bool {
    let page = allocate_or_evict();
    if page.is_null() {
        return false;
    }

    let page_slice = unsafe { core::slice::from_raw_parts_mut(page, PG_SIZE) };
    if !SwapManager::read_slot(swap_index, page_slice) {
        unsafe { UserPool::dealloc_pages(page, 1) };
        return false;
    }

    SwapManager::free_slot(swap_index);

    let pa = PhysAddr::from(page as usize);
    let mut pagetable = unsafe { crate::mem::pagetable::PageTable::effective_pagetable() };
    pagetable.map(pa, va, PG_SIZE, flags);

    let tid = thread::current().id();
    FrameTable::instance().lock().register(pa.value(), tid, va);

    // Update supp page table
    if let Some(proc) = thread::current().userproc.as_ref() {
        let mut supp = proc.supp_page.lock();
        if let Some(entry) = supp.get_mut(va) {
            entry.source = PageSource::InFrame {
                phys_addr: pa.value(),
                flags,
            };
        }
    }

    true
}

/// Allocate a page from UserPool. If the pool is exhausted, evict a frame
/// belonging to the current thread and retry. Returns null if allocation is
/// impossible.
pub(crate) fn allocate_or_evict() -> *mut u8 {
    unsafe {
        if let Some(page) = UserPool::try_alloc_pages(1) {
            return page;
        }
    }
    if evict_one_frame() {
        unsafe { UserPool::try_alloc_pages(1).unwrap_or(core::ptr::null_mut()) }
    } else {
        core::ptr::null_mut()
    }
}

/// Evict one frame belonging to the current thread using the clock algorithm.
/// Returns true if a frame was successfully evicted, false otherwise.
pub(crate) fn evict_one_frame() -> bool {
    evict_from_current()
}

/// Same-process eviction: evict a frame belonging to the current thread.
fn evict_from_current() -> bool {
    let cur = thread::current();
    let cur_tid = cur.id();

    let (phys, va) = {
        let victim = FrameTable::instance().lock().evict_one_for(cur_tid);
        if victim.is_none() {
            return false;
        }
        let (phys, entry) = victim.unwrap();
        #[cfg(feature = "debug")]
        kprintln!(
            "[EVICT] Evicting frame {:#x} (va={:#x}, tid={})",
            phys, entry.user_va, entry.owner_tid
        );
        (phys, entry.user_va)
    };

    let proc = match cur.userproc.as_ref() {
        Some(p) => p,
        None => {
            FrameTable::instance().lock().register(phys, cur_tid, va);
            return false;
        }
    };

    // Use a static buffer to avoid stack overflow (the 4KB page buffer
    // combined with deep call chains from page fault handling can exceed
    // the 16KB kernel stack).
    static mut PAGE_BUF: [u8; PG_SIZE] = [0u8; PG_SIZE];

    // Read dirty bit and copy page data
    let page_buf = (&raw mut PAGE_BUF).cast::<u8>();
    let dirty = {
        let pagetable = cur.pagetable.as_ref().unwrap().lock();
        let dirty = pagetable.get_pte(va).map(|e| e.is_dirty()).unwrap_or(false);
        unsafe {
            core::ptr::copy_nonoverlapping(
                (phys + crate::mem::layout::VM_OFFSET) as *const u8,
                page_buf,
                PG_SIZE,
            );
        }
        dirty
    };

    // Update supp entry and determine what I/O is needed
    let (action, _swap_slot) = {
        let mut supp = proc.supp_page.lock();
        if let Some(supp_entry) = supp.get_mut(va) {
            evict_update_supp(supp_entry, dirty)
        } else {
            (None, None)
        }
    };

    // Unmap the PTE
    {
        let mut pagetable = cur.pagetable.as_ref().unwrap().lock();
        pagetable.unmap(va);
    }

    // Perform I/O (outside all Intr locks)
    match action {
        Some(EvictAction::WriteSwap { slot }) => {
            let page_buf = (&raw const PAGE_BUF).cast::<u8>();
            if !SwapManager::write_slot(slot, unsafe { core::slice::from_raw_parts(page_buf, PG_SIZE) }) {
                SwapManager::free_slot(slot);
            }
        }
        Some(EvictAction::WriteFile { file, offset }) => {
            let page_buf = (&raw const PAGE_BUF).cast::<u8>();
            let mut f = file.clone();
            if let Ok(pos) = f.pos() {
                *pos = offset;
            }
            let _ = f.write(unsafe { core::slice::from_raw_parts(page_buf, PG_SIZE) });
        }
        None => {}
    }

    // Free the physical page
    unsafe {
        UserPool::dealloc_pages((phys + crate::mem::layout::VM_OFFSET) as *mut u8, 1);
    }

    true
}/// Update supp entry for eviction and return (action, swap_slot_to_write).
fn evict_update_supp(
    supp_entry: &mut crate::mem::suppage::SuppEntry,
    dirty: bool,
) -> (Option<EvictAction>, Option<usize>) {
    let source = supp_entry.source.clone();
    match source {
        PageSource::InFrame { flags, .. } | PageSource::AnonFrame { flags, .. } => {
            let is_anon = matches!(source, PageSource::AnonFrame { .. });
            if let Some(slot) = SwapManager::alloc_slot() {
                supp_entry.source = if is_anon {
                    PageSource::AnonSwap { swap_index: slot, flags }
                } else {
                    PageSource::InSwap { swap_index: slot, flags }
                };
                (Some(EvictAction::WriteSwap { slot }), Some(slot))
            } else {
                (None, None)
            }
        }
        PageSource::InFile { flags, .. } => {
            if dirty {
                if let Some(slot) = SwapManager::alloc_slot() {
                    supp_entry.source = PageSource::InSwap { swap_index: slot, flags };
                    (Some(EvictAction::WriteSwap { slot }), Some(slot))
                } else {
                    (None, None)
                }
            } else {
                // Clean InFile page: keep source as-is (will re-read from file)
                (None, None)
            }
        }
        PageSource::MmapFile { file, offset, writable, mapid } => {
            if dirty {
                supp_entry.source = PageSource::MmapFile {
                    file: file.clone(), offset, mapid, writable,
                };
                (Some(EvictAction::WriteFile { file: file.clone(), offset }), None)
            } else {
                // Clean MmapFile page: keep source as-is
                (None, None)
            }
        }
        _ => (None, None),
    }
}

enum EvictAction {
    WriteSwap { slot: usize },
    WriteFile { file: crate::fs::File, offset: usize },
}
