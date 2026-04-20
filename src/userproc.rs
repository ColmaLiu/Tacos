//! User process.
//!

mod fdtable;
mod load;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::asm;
use core::mem::MaybeUninit;
use riscv::register::sstatus;

use crate::fs::File;
use crate::mem::pagetable::KernelPgTable;
use crate::mem::userbuf::{write_user_buf, write_user_usize};
use crate::thread::{self, Mutex};
use crate::trap::{trap_exit_u, Frame};
use crate::userproc::fdtable::FdTable;

pub use self::fdtable::{FileType, OpenFile, O_CREATE, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};

pub struct UserProc {
    #[allow(dead_code)]
    bin: File,
    exit_status: Arc<Mutex<Option<isize>>>,
    pub fd_table: Mutex<FdTable>,
}

pub struct ProcInfo {
    parent_tid: isize,
    exit_status: Arc<Mutex<Option<isize>>>,
}

impl UserProc {
    pub fn new(file: File) -> Self {
        Self {
            bin: file,
            exit_status: Arc::new(Mutex::new(None)),
            fd_table: Mutex::new(FdTable::new()),
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

    let exec_info = match load::load_executable(&mut file, &mut pt) {
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
    let userproc = UserProc::new(file);

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

    let child = thread::Builder::new(move || start(frame))
        .pagetable(pt)
        .userproc(userproc)
        .spawn();

    thread::Manager::get().proc_table.lock().insert(
        child.id(),
        ProcInfo {
            parent_tid: thread::current().id(),
            exit_status: child.userproc.as_ref().unwrap().exit_status.clone(),
        },
    );

    child.id()
}

/// Exits a process.
///
/// Panic if the current thread doesn't own a user process.
pub fn exit(_value: isize) -> ! {
    {
        let cur = thread::current();

        let proc = cur
            .userproc
            .as_ref()
            .expect("current thread doesn't own a user process");
        *proc.exit_status.lock() = Some(_value);
    }

    thread::exit();
}

/// Waits for a child thread, which must own a user process.
///
/// ## Return
/// - `Some(exit_value)`
/// - `None`: if tid was not created by the current thread.
pub fn wait(_tid: isize) -> Option<isize> {
    let cur_tid = thread::current().id();

    let status = {
        let table = thread::Manager::get().proc_table.lock();
        let info = table.get(&_tid)?;
        if info.parent_tid != cur_tid {
            return None;
        }
        let status = info.exit_status.clone();
        status
    };

    loop {
        let st = status.lock();
        if let Some(code) = *st {
            thread::Manager::get().proc_table.lock().remove(&_tid);
            return Some(code);
        }
        drop(st);
        thread::schedule();
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
