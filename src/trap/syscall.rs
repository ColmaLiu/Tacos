//! Syscall handlers
//!

#![allow(dead_code)]

use alloc::{string::String, vec::Vec};
use core::slice::from_raw_parts;

use crate::fs::{disk::DISKFS, FileSys};
use crate::io::{Read, Seek, Write};
use crate::mem::layout::{MAX_STACK_SIZE, USER_STACK_TOP};
use crate::mem::suppage::PageSource;
use crate::mem::userbuf::{read_user_buf, read_user_cstr, read_user_usize, write_user_buf};
use crate::mem::{PageTable, PG_SIZE};
use crate::sbi::shutdown;
use crate::thread;
use crate::userproc::{
    execute, exit, wait, FileType, OpenFile, O_CREATE, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY,
};

/* -------------------------------------------------------------------------- */
/*                               SYSCALL NUMBER                               */
/* -------------------------------------------------------------------------- */

const SYS_HALT: usize = 1;
const SYS_EXIT: usize = 2;
const SYS_EXEC: usize = 3;
const SYS_WAIT: usize = 4;
const SYS_REMOVE: usize = 5;
const SYS_OPEN: usize = 6;
const SYS_READ: usize = 7;
const SYS_WRITE: usize = 8;
const SYS_SEEK: usize = 9;
const SYS_TELL: usize = 10;
const SYS_CLOSE: usize = 11;
const SYS_FSTAT: usize = 12;
const SYS_MMAP: usize = 13;
const SYS_MUNMAP: usize = 14;

pub fn syscall_handler(_id: usize, _args: [usize; 3]) -> isize {
    match _id {
        // void halt(void);
        SYS_HALT => shutdown(),
        // void exit(int status);
        SYS_EXIT => exit(_args[0] as isize),
        // int exec(const char* pathname, const char* argv[]);
        SYS_EXEC => {
            let path_ptr = _args[0] as *const u8;
            let argv_ptr = _args[1] as *const usize;

            if !is_valid_area(path_ptr, 1) || !is_valid_area(argv_ptr as *const u8, 1) {
                return -1;
            }

            let pathname = match read_user_cstr(path_ptr, 4096) {
                Ok(s) => s,
                Err(_) => return -1,
            };

            let mut argv = Vec::new();

            for i in 0..0x7fffffff {
                let p = match read_user_usize(unsafe {
                    (argv_ptr as *const u8).add(i * core::mem::size_of::<usize>())
                }) {
                    Ok(p) => p,
                    Err(_) => return -1,
                };
                if p == 0 {
                    break;
                }
                let s = match read_user_cstr(p as *const u8, 4096) {
                    Ok(s) => s,
                    Err(_) => return -1,
                };
                argv.push(s);
            }

            let file = match DISKFS.open(pathname.as_str().into()) {
                Ok(f) => f,
                Err(_) => return -1,
            };

            execute(file, argv)
        }
        // int wait(int pid);
        SYS_WAIT => wait(_args[0] as isize).unwrap_or(-1),
        // int remove(const char* pathname);
        SYS_REMOVE => {
            let path_ptr = _args[0] as *const u8;

            if !is_valid_area(path_ptr, 1) {
                return -1;
            }

            let pathname = match read_user_cstr(path_ptr, 256) {
                Ok(s) => s,
                Err(_) => return -1,
            };

            match DISKFS.remove(pathname.as_str().into()) {
                Ok(_) => 0,
                Err(_) => -1,
            }
        }
        // int open(const char* pathname, int flags);
        SYS_OPEN => {
            let path_ptr = _args[0] as *const u8;
            let flags = _args[1] as usize;

            if !is_valid_area(path_ptr, 1) {
                return -1;
            }

            let pathname = match read_user_cstr(path_ptr, 4096) {
                Ok(s) => {
                    if s.is_empty() {
                        return -1;
                    } else {
                        s
                    }
                }
                Err(_) => return -1,
            };

            let accmode = flags & 0x3;
            if accmode != O_RDONLY && accmode != O_WRONLY && accmode != O_RDWR {
                return -1;
            }

            let file = if flags & (O_TRUNC | O_CREATE) != 0 {
                match DISKFS.create(pathname.as_str().into()) {
                    Ok(f) => f,
                    Err(_) => return -1,
                }
            } else {
                match DISKFS.open(pathname.as_str().into()) {
                    Ok(f) => f,
                    Err(_) => return -1,
                }
            };

            let cur = thread::current();
            let proc = match cur.userproc.as_ref() {
                Some(p) => p,
                None => return -1,
            };

            let fd = proc
                .fd_table
                .lock()
                .insert(OpenFile::new(FileType::File(file), flags));
            fd as isize
        }
        // int read(int fd, void* buffer, unsigned size);
        SYS_READ => {
            let fd = _args[0] as usize;
            let ptr = _args[1] as *const u8;
            let len = _args[2];

            if !is_valid_area(ptr, len) {
                return -1;
            }

            let cur = thread::current();
            let proc = match cur.userproc.as_ref() {
                Some(p) => p,
                None => return -1,
            };

            let mut table = proc.fd_table.lock();
            let of = match table.get_mut(fd) {
                Some(f) => f,
                None => return -1,
            };

            if !of.readable {
                return -1;
            }

            let mut kbuf = alloc::vec![0u8; len];
            let n = match &mut of.file {
                FileType::File(file) => match file.read(&mut kbuf) {
                    Ok(n) => n,
                    Err(_) => return -1,
                },
                FileType::Stdin => unimplemented!(),
                FileType::Stdout | FileType::Stderr => unreachable!(),
            };

            if write_user_buf(ptr, &kbuf[..n]).is_err() {
                return -1;
            }

            n as isize
        }
        // int write(int fd, const void* buffer, unsigned size);
        SYS_WRITE => {
            let fd = _args[0] as usize;
            let ptr = _args[1] as *const u8;
            let len = _args[2] as usize;

            if !is_valid_area(ptr, len) {
                return -1;
            }

            let buf = match read_user_buf(ptr, len) {
                Ok(b) => b,
                Err(_) => return -1,
            };

            match fd {
                0 => -1, // stdin
                1 | 2 => {
                    kprint!("{}", String::from_utf8_lossy(&buf));
                    len as isize
                } // stdout or stderr
                _ => {
                    let cur = thread::current();
                    let proc = match cur.userproc.as_ref() {
                        Some(p) => p,
                        None => return -1,
                    };

                    let mut table = proc.fd_table.lock();
                    let of = match table.get_mut(fd) {
                        Some(f) => f,
                        None => return -1,
                    };

                    if !of.writable {
                        return -1;
                    }

                    if let FileType::File(file) = &mut of.file {
                        match file.write(&buf) {
                            Ok(n) => return n as isize,
                            Err(_) => return -1,
                        };
                    }
                    -1
                }
            }
        }
        // void seek(int fd, unsigned position);
        SYS_SEEK => {
            let fd = _args[0] as usize;
            let position = _args[1] as usize;

            let cur = thread::current();
            if let Some(proc) = cur.userproc.as_ref() {
                let mut fdt = proc.fd_table.lock();
                if let Some(open_file) = fdt.get_mut(fd) {
                    if let FileType::File(file) = &mut open_file.file {
                        if let Ok(pos) = file.pos() {
                            *pos = position;
                        }
                    }
                }
            }
            0
        }
        // int tell(int fd);
        SYS_TELL => {
            let fd = _args[0] as usize;

            let cur = thread::current();
            if let Some(proc) = cur.userproc.as_ref() {
                let mut fdt = proc.fd_table.lock();
                if let Some(open_file) = fdt.get_mut(fd) {
                    if let FileType::File(file) = &mut open_file.file {
                        if let Ok(pos) = file.pos() {
                            return *pos as isize;
                        }
                    }
                }
            }
            -1
        }
        // int close (int fd);
        SYS_CLOSE => {
            let fd = _args[0] as usize;

            let cur = thread::current();
            let proc = match cur.userproc.as_ref() {
                Some(p) => p,
                None => return -1,
            };

            let mut fdt = proc.fd_table.lock();
            match fdt.remove(fd) {
                Some(_) => 0,
                None => -1,
            }
        }
        // typedef struct {
        //     uint64 ino;     // Inode number
        //     uint64 size;  // Size of file in bytes
        // } stat;
        // int fstat(int fd, stat* buf);
        SYS_FSTAT => {
            #[repr(C)]
            struct Stat {
                ino: u64,
                size: u64,
            }

            let fd = _args[0] as usize;
            let user_stat = _args[1] as *const u8;

            if !is_valid_area(user_stat, core::mem::size_of::<Stat>()) {
                return -1;
            }

            let cur = thread::current();
            let proc = match cur.userproc.as_ref() {
                Some(p) => p,
                None => return -1,
            };

            let mut fdt = proc.fd_table.lock();
            let file = match fdt.get_mut(fd) {
                Some(f) => {
                    if let FileType::File(file) = &f.file {
                        file
                    } else {
                        return -1;
                    }
                }
                None => return -1,
            };

            let size = match file.len() {
                Ok(s) => s,
                Err(_) => return -1,
            };
            let stat = Stat {
                ino: file.inum() as u64,
                size: size as u64,
            };

            if write_user_buf(user_stat, unsafe {
                from_raw_parts(
                    &stat as *const Stat as *const u8,
                    core::mem::size_of::<Stat>(),
                )
            })
            .is_err()
            {
                return -1;
            }

            0
        }
        // mapid_t mmap(int fd, void* addr);
        SYS_MMAP => {
            let fd = _args[0] as usize;
            let addr = _args[1] as usize;

            // addr must not be null
            if addr == 0 {
                return -1;
            }
            // addr must be page-aligned
            if addr % PG_SIZE != 0 {
                return -1;
            }
            // fd must be > 2 (not stdin/stdout/stderr)
            if fd <= 2 {
                return -1;
            }

            let cur = thread::current();
            let proc = match cur.userproc.as_ref() {
                Some(p) => p,
                None => return -1,
            };

            // Get file and writability from fd table in one lock
            let (file, writable) = {
                let table = proc.fd_table.lock();
                match table.get(fd) {
                    Some(of) => match &of.file {
                        FileType::File(f) => (f.clone(), of.writable),
                        _ => return -1,
                    },
                    None => return -1,
                }
            };

            // Check file length > 0
            let file_len = match file.len() {
                Ok(l) => l,
                Err(_) => return -1,
            };
            if file_len == 0 {
                return -1;
            }

            let pages = (file_len + PG_SIZE - 1) / PG_SIZE;

            // Check overlap with stack
            {
                let stk_start = USER_STACK_TOP - MAX_STACK_SIZE;
                let stk_end = USER_STACK_TOP;
                let mmap_end = addr + pages * PG_SIZE;
                if addr < stk_end && mmap_end > stk_start {
                    return -1;
                }
            }

            // Insert into mmap table (checks overlap with existing regions + supp page)
            let mapid = {
                let mut mmap_table = proc.mmap_table.lock();
                let supp_page = proc.supp_page.lock();
                match mmap_table.insert(
                    file.clone(),
                    addr,
                    file_len,
                    writable,
                    &supp_page,
                ) {
                    Ok(id) => id,
                    Err(_) => return -1,
                }
            };

            // Insert supp page entries for lazy loading
            {
                let mut supp = proc.supp_page.lock();
                for p in 0..pages {
                    let page_addr = addr + p * PG_SIZE;
                    let offset = p * PG_SIZE;
                    supp.insert(
                        page_addr,
                        PageSource::MmapFile {
                            file: file.clone(),
                            offset,
                            mapid,
                            writable,
                        },
                    );
                }
            }

            mapid as isize
        }
        // void munmap(mapid_t mapping);
        SYS_MUNMAP => {
            let mapid = _args[0] as usize;

            let cur = thread::current();
            let proc = match cur.userproc.as_ref() {
                Some(p) => p,
                None => return -1,
            };

            // Remove from mmap table first (validates mapid)
            let _region = {
                let mut mmap_table = proc.mmap_table.lock();
                match mmap_table.remove(mapid) {
                    Some(r) => r,
                    None => return -1,
                }
            };

            // Step 1: collect dirty pages to write back
            let writebacks = {
                let supp = proc.supp_page.lock();
                let pagetable = unsafe { PageTable::effective_pagetable() };
                supp.collect_dirty_writebacks(mapid, &pagetable)
            };

            // Step 2: write back dirty pages (no locks held)
            for (file, offset, data) in &writebacks {
                let mut f = file.clone();
                use crate::io::Seek;
                if let Ok(pos) = f.pos() {
                    *pos = *offset;
                }
                use crate::io::Write;
                let _ = f.write(data);
            }

            // Step 3: clean up frames, unmap PTEs, remove supp entries
            {
                let mut supp = proc.supp_page.lock();
                let mut pagetable = unsafe { PageTable::effective_pagetable() };
                let mut frame_table = crate::mem::frame::FrameTable::instance().lock();
                supp.cleanup_mapid(mapid, &mut pagetable, &mut frame_table);
            }

            0
        }
        _ => unreachable!(),
    }
}

fn is_valid_area(ptr: *const u8, len: usize) -> bool {
    let start = ptr as usize;
    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return false,
    };
    let table = unsafe { PageTable::effective_pagetable() };

    // Acquire supp_page lock once to check all non-present pages
    let cur = thread::current();
    let supp = cur.userproc.as_ref().map(|p| p.supp_page.lock());

    for addr in (start..end).step_by(4096) {
        match table.get_pte(addr) {
            Some(entry) if entry.is_valid() => (),
            _ => {
                if supp.as_ref().map_or(true, |s| !s.contains(addr)) {
                    return false;
                }
            }
        };
    }
    if len > 1 {
        match table.get_pte(end - 1) {
            Some(entry) if entry.is_valid() => (),
            _ => {
                if supp.as_ref().map_or(true, |s| !s.contains(end - 1)) {
                    return false;
                }
            }
        };
    }
    true
}
