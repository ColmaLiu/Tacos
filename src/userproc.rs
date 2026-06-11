//! User process.
//!

mod fdtable;
mod load;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::asm;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicIsize, Ordering::SeqCst};
use riscv::register::sstatus;

use crate::fs::File;
use crate::io::prelude::*;
use crate::mem::pagetable::KernelPgTable;
use crate::mem::userbuf::{write_user_buf, write_user_usize};
use crate::mem::PG_SIZE;
use crate::sbi;
use crate::sync::{Mutex as SyncMutex, Spin};
use crate::thread::{self, Mutex};
use crate::trap::{trap_exit_u, Frame};
use crate::userproc::fdtable::FdTable;

pub use self::fdtable::{FileType, OpenFile, O_CREATE, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};

pub struct UserProc {
    bin: Mutex<Option<File>>,
    pub fd_table: Mutex<FdTable>,
    pub supp_page: Mutex<crate::mem::suppage::SuppPageTable>,
    pub mmap_table: Mutex<MmapTable>,
}

/// Tracks per-process mmap regions
pub struct MmapTable {
    regions: Vec<MmapRegion>,
    next_id: usize,
}

pub struct MmapRegion {
    pub mapid: usize,
    pub file: File,
    pub addr: usize,
    pub size: usize,
    pub pages: usize,
    pub writable: bool,
}

impl MmapTable {
    pub fn new() -> Self {
        Self {
            regions: Vec::new(),
            next_id: 1,
        }
    }

    pub fn insert(
        &mut self,
        file: File,
        addr: usize,
        size: usize,
        writable: bool,
        supp_page: &crate::mem::suppage::SuppPageTable,
    ) -> Result<usize, &'static str> {
        let pages = (size + crate::mem::PG_SIZE - 1) / crate::mem::PG_SIZE;
        let new_end = addr + pages * crate::mem::PG_SIZE;
        let overlaps = |other_start, other_end| addr < other_end && new_end > other_start;

        // Check overlaps with existing mmap regions
        for region in &self.regions {
            if overlaps(
                region.addr,
                region.addr + region.pages * crate::mem::PG_SIZE,
            ) {
                return Err("overlaps existing mmap region");
            }
        }

        // Check overlaps with supp page table entries (code/data loaded pages)
        for (&va, _) in supp_page.iter() {
            if overlaps(va, va + crate::mem::PG_SIZE) {
                return Err("overlaps loaded segment");
            }
        }

        // Check overlap with stack
        let stk_start = crate::mem::layout::USER_STACK_TOP - crate::mem::layout::MAX_STACK_SIZE;
        let stk_end = crate::mem::layout::USER_STACK_TOP;
        if overlaps(stk_start, stk_end) {
            return Err("overlaps stack");
        }

        let id = self.next_id;
        self.next_id += 1;
        self.regions.push(MmapRegion {
            mapid: id,
            file,
            addr,
            size,
            pages,
            writable,
        });

        Ok(id)
    }

    pub fn find_by_addr(&self, addr: usize) -> Option<&MmapRegion> {
        let page = addr & !(crate::mem::PG_SIZE - 1);
        self.regions.iter().find(|r| {
            let start = r.addr;
            let end = start + r.pages * crate::mem::PG_SIZE;
            page >= start && page < end
        })
    }

    pub fn find_by_id(&self, mapid: usize) -> Option<&MmapRegion> {
        self.regions.iter().find(|r| r.mapid == mapid)
    }

    pub fn remove(&mut self, mapid: usize) -> Option<MmapRegion> {
        if let Some(pos) = self.regions.iter().position(|r| r.mapid == mapid) {
            Some(self.regions.remove(pos))
        } else {
            None
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &MmapRegion> {
        self.regions.iter()
    }
}

pub struct ProcInfo {
    parent_tid: AtomicIsize,
    state: SyncMutex<WaitState, Spin>,
}

struct WaitState {
    has_exited: bool,
    exit_status: isize,
    waiter: Option<Arc<thread::Thread>>,
}

const NO_PARENT: isize = -1;

impl ProcInfo {
    fn new(parent_tid: isize) -> Self {
        Self {
            parent_tid: AtomicIsize::new(parent_tid),
            state: SyncMutex::new(WaitState {
                has_exited: false,
                exit_status: 0,
                waiter: None,
            }),
        }
    }
}

impl UserProc {
    pub fn new(file: File, supp_page: crate::mem::suppage::SuppPageTable) -> Self {
        Self {
            bin: Mutex::new(Some(file)),
            fd_table: Mutex::new(FdTable::new()),
            supp_page: Mutex::new(supp_page),
            mmap_table: Mutex::new(MmapTable::new()),
        }
    }
}

/// Execute an object file with arguments.
///
/// ## Return
/// - `-1`: On error.
/// - `tid`: Tid of the newly spawned thread.
#[allow(unused_variables)]
pub fn execute(mut file: File, argv: Vec<String>) -> isize {
    #[cfg(feature = "debug")]
    kprintln!(
        "[PROCESS] Kernel thread {} prepare to execute a process with args {:?}",
        thread::current().name(),
        argv
    );

    // It only copies L2 pagetable. This approach allows the new thread
    // to access kernel code and data during syscall without the need to
    // switch pagetables.
    let mut pt = KernelPgTable::clone();

    let (exec_info, supp_page) = match load::load_executable(&mut file, &mut pt) {
        Ok(x) => x,
        Err(_) => unsafe {
            pt.destroy();
            return -1;
        },
    };

    // Impose a reasonable limit on total argument block size
    const ARG_MAX: usize = 4096;
    let strings_size: usize = argv.iter().map(|arg| arg.len() + 1).sum();
    let argv_array_size = (argv.len() + 1) * core::mem::size_of::<usize>();
    let total_size = strings_size + argv_array_size + 15; // alignment slack

    if total_size > ARG_MAX {
        unsafe {
            pt.destroy();
        }
        return -1;
    }

    // Initialize frame, pass argument to user.
    let mut frame = unsafe { MaybeUninit::<Frame>::zeroed().assume_init() };
    frame.sepc = exec_info.entry_point;

    // Here the new process will be created.
    let userproc = UserProc::new(file, supp_page);

    let old_pt = unsafe { crate::mem::pagetable::PageTable::effective_pagetable() };
    pt.activate();

    let mut sp = exec_info.init_sp;
    let mut arg_addrs: Vec<usize> = Vec::new();

    // Copy argument strings to user stack, from back to front
    for arg in argv.iter().rev() {
        let bytes = arg.as_bytes();
        let len = bytes.len() + 1; // include trailing '\0'

        sp -= len;

        if write_user_buf(sp as *const u8, bytes).is_err() {
            old_pt.activate();
            unsafe {
                pt.destroy();
            }
            return -1;
        }

        if write_user_buf((sp + bytes.len()) as *const u8, &[0]).is_err() {
            old_pt.activate();
            unsafe {
                pt.destroy();
            }
            return -1;
        }

        arg_addrs.push(sp);
    }

    // Align stack to 16 bytes
    sp &= !0xf;

    // Push argv[argc] = NULL
    sp -= core::mem::size_of::<usize>();
    if write_user_usize(sp as *const u8, 0).is_err() {
        old_pt.activate();
        unsafe {
            pt.destroy();
        }
        return -1;
    }

    for &addr in arg_addrs.iter() {
        sp -= core::mem::size_of::<usize>();
        if write_user_usize(sp as *const u8, addr).is_err() {
            old_pt.activate();
            unsafe {
                pt.destroy();
            }
            return -1;
        }
    }

    let argv_base = sp;

    // Set up user registers
    frame.x[2] = sp; // sp
    frame.x[10] = arg_addrs.len(); // a0 = argc
    frame.x[11] = argv_base; // a1 = argv

    old_pt.activate();

    // Evict pages from the current process to give the child a working set.
    // Each evicted page becomes a free page the child can use.  More pages
    // mean faster bootstrap for the child.  Capped to avoid evicting the
    // parent's own code/stack pages (which it needs to return to user mode).
    for _ in 0..16 {
        if !crate::trap::pagefault::evict_one_frame() {
            break;
        }
    }

    let child = thread::Builder::new(move || start(frame))
        .pagetable(pt)
        .userproc(userproc)
        .build();

    let child_tid = child.id();

    // Update the stack frame's owner from placeholder -1 to the actual child tid
    {
        let mut ft = crate::mem::frame::FrameTable::instance().lock();
        // Find and update all frames with owner_tid = -1
        let to_update: alloc::vec::Vec<usize> = ft
            .iter()
            .filter(|(_, e)| e.owner_tid == -1)
            .map(|(&phys, _)| phys)
            .collect();
        for phys in to_update {
            ft.update_owner(phys, child_tid);
        }
    }

    thread::Manager::get()
        .proc_table
        .lock()
        .insert(child_tid, Arc::new(ProcInfo::new(thread::current().id())));

    thread::Manager::get().register(child);
    thread::schedule();

    child_tid
}

/// Exits a process.
///
/// Panic if the current thread doesn't own a user process.
pub fn exit(_value: isize) -> ! {
    let cur = thread::current();
    let proc = cur
        .userproc
        .as_ref()
        .expect("current thread doesn't own a user process");

    // Release the executable's deny-write before the parent can observe exit.
    proc.bin.lock().take();

    // Clean up all frames before destroying the page table.
    // This ensures FrameTable entries don't dangle with raw pointers to
    // freed Mutexes after the page table and supp_page are destroyed.
    //
    // Phase 1: collect mmap writeback data (needs pagetable locked)
    let mmap_ids: Vec<usize> = {
        let mmap = proc.mmap_table.lock();
        mmap.iter().map(|r| r.mapid).collect()
    };
    let all_writebacks: alloc::vec::Vec<(File, usize, [u8; PG_SIZE])> = {
        let pagetable = cur
            .pagetable
            .as_ref()
            .map(|pt| pt.lock())
            .expect("user thread must have a page table");
        let mut wb = Vec::new();
        for &mapid in &mmap_ids {
            let supp = proc.supp_page.lock();
            wb.extend(supp.collect_dirty_writebacks(mapid, &*pagetable));
        }
        wb
    };
    // Phase 2: write back dirty pages (no locks held — may block on I/O)
    for (file, offset, data) in &all_writebacks {
        let mut f = file.clone();
        if let Ok(pos) = f.pos() {
            *pos = *offset;
        }
        let _ = f.write(data);
    }
    // Phase 3: cleanup frames and unmap
    {
        let mut pagetable = cur
            .pagetable
            .as_ref()
            .map(|pt| pt.lock())
            .expect("user thread must have a page table");

        for mapid in mmap_ids {
            proc.mmap_table.lock().remove(mapid);
            let mut supp = proc.supp_page.lock();
            let mut frame_table = crate::mem::frame::FrameTable::instance().lock();
            supp.cleanup_mapid(mapid, &mut *pagetable, &mut frame_table);
        }

        // Then unregister any remaining frames (code, data, stack, BSS)
        // that were allocated by this process
        {
            let supp = proc.supp_page.lock();
            let mut frame_table = crate::mem::frame::FrameTable::instance().lock();
            let vas: alloc::vec::Vec<usize> = supp.iter().map(|(&va, _)| va).collect();
            for va in vas {
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
            }
        }
    }

    let cur_tid = cur.id();

    if let Some(info) = {
        thread::Manager::get()
            .proc_table
            .lock()
            .get(&cur_tid)
            .cloned()
    } {
        let waiter = {
            let mut state = info.state.lock();
            state.exit_status = _value;
            state.has_exited = true;
            state.waiter.take()
        };

        if info.parent_tid.load(SeqCst) == NO_PARENT {
            thread::Manager::get().proc_table.lock().remove(&cur_tid);
        } else if let Some(waiter) = waiter {
            thread::wake_up(waiter);
        }
    }

    orphan_children(cur_tid);

    thread::exit();
}

/// Waits for a child thread, which must own a user process.
///
/// ## Return
/// - `Some(exit_value)`
/// - `None`: if tid was not created by the current thread.
pub fn wait(_tid: isize) -> Option<isize> {
    let cur_tid = thread::current().id();

    let info = {
        let table = thread::Manager::get().proc_table.lock();
        let info = table.get(&_tid)?.clone();
        if info.parent_tid.load(SeqCst) != cur_tid {
            return None;
        }
        info
    };

    loop {
        let old = sbi::interrupt::set(false);
        let current = thread::current();
        let mut state = info.state.lock();

        if state.has_exited {
            let code = state.exit_status;
            drop(state);
            sbi::interrupt::set(old);
            thread::Manager::get().proc_table.lock().remove(&_tid);
            return Some(code);
        }

        state.waiter = Some(current.clone());
        current.set_status(thread::Status::Blocked);
        drop(state);

        thread::schedule();
        sbi::interrupt::set(old);
    }
}

fn orphan_children(parent_tid: isize) {
    let mut table = thread::Manager::get().proc_table.lock();
    let mut reaped = Vec::new();

    for (&tid, info) in table.iter() {
        if info
            .parent_tid
            .compare_exchange(parent_tid, NO_PARENT, SeqCst, SeqCst)
            .is_err()
        {
            continue;
        }

        if info.state.lock().has_exited {
            reaped.push(tid);
        }
    }

    for tid in reaped {
        table.remove(&tid);
    }
}

/// Initializes a user process in current thread.
///
/// This function won't return.
pub fn start(mut frame: Frame) -> ! {
    unsafe { sstatus::set_spp(sstatus::SPP::User) };
    frame.sstatus = sstatus::read();

    // Set kernel stack pointer to intr frame and then jump to `trap_exit_u()`.
    let kernal_sp = (&frame as *const Frame) as usize;

    unsafe {
        asm!(
            "mv sp, t0",
            "jr t1",
            in("t0") kernal_sp,
            in("t1") trap_exit_u as *const u8
        );
    }

    unreachable!();
}
