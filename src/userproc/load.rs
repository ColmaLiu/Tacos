use alloc::vec;
use elf_rs::{Elf, ElfFile, ProgramHeaderEntry, ProgramHeaderFlags, ProgramType};

use crate::fs::File;
use crate::io::prelude::*;
use crate::mem::pagetable::{PTEFlags, PageTable};
use crate::mem::suppage::{PageSource, SuppPageTable};
use crate::mem::{div_round_up, PageAlign, PhysAddr, PG_MASK, PG_SIZE};
use crate::{OsError, Result};

#[derive(Debug, Clone, Copy)]
pub(super) struct ExecInfo {
    pub entry_point: usize,
    pub init_sp: usize,
}

/// Loads an executable file lazily.
///
/// Returns the supplementary page table populated with segment metadata
/// and the initial user stack page (allocated eagerly).
///
/// ## Return
/// On success, returns `Ok((ExecInfo, SuppPageTable))`.
pub(super) fn load_executable(
    file: &mut File,
    pagetable: &mut PageTable,
) -> Result<(ExecInfo, SuppPageTable)> {
    let mut supp_page = SuppPageTable::new();

    let exec_info = load_elf(file, &mut supp_page)?;

    // Initialize user stack (first page eagerly, as required).
    init_user_stack(pagetable, exec_info.init_sp, &mut supp_page);

    // Forbid modifying executable file when running
    file.deny_write();

    Ok((exec_info, supp_page))
}

/// Parses the ELF and records segment metadata in the supp page table.
/// No physical pages are allocated — all done lazily via page faults.
fn load_elf(file: &mut File, supp_page: &mut SuppPageTable) -> Result<ExecInfo> {
    file.rewind()?;

    let len = file.len()?;
    let mut buf = vec![0u8; len];
    file.read(&mut buf)?;

    let elf = match Elf::from_bytes(&buf) {
        Ok(Elf::Elf64(elf)) => elf,
        Ok(Elf::Elf32(_)) | Err(_) => return Err(OsError::UnknownFormat),
    };

    // Record metadata for each loadable segment
    elf.program_header_iter()
        .filter(|p| p.ph_type() == ProgramType::LOAD)
        .for_each(|p| record_segment(file, &p, supp_page));

    Ok(ExecInfo {
        entry_point: elf.elf_header().entry_point() as _,
        init_sp: 0x80500000,
    })
}

/// Records per-page metadata in the supplementary page table for a LOAD segment.
/// Each page is tagged with the file handle, file offset, segment size, and PTE flags.
fn record_segment(file: &File, phdr: &ProgramHeaderEntry, supp_page: &mut SuppPageTable) {
    assert_eq!(phdr.ph_type(), ProgramType::LOAD);

    let fileoff = phdr.offset() as usize;
    let mut readpos = fileoff & !PG_MASK;

    let mut leaf_flag = PTEFlags::V | PTEFlags::U | PTEFlags::R;
    if phdr.flags().contains(ProgramHeaderFlags::EXECUTE) {
        leaf_flag |= PTEFlags::X;
    }
    if phdr.flags().contains(ProgramHeaderFlags::WRITE) {
        leaf_flag |= PTEFlags::W;
    }

    let ubase = (phdr.vaddr() as usize) & !PG_MASK;
    let pageoff = (phdr.vaddr() as usize) & PG_MASK;
    assert_eq!(fileoff & PG_MASK, pageoff);

    let pages = div_round_up(pageoff + phdr.memsz() as usize, PG_SIZE);
    let filesz = phdr.filesz() as usize;
    let file_end = fileoff + filesz;

    for p in 0..pages {
        let uaddr = ubase + p * PG_SIZE;
        let file_offset = readpos;
        let page_filesz = if file_offset >= file_end {
            0
        } else {
            (file_end - file_offset).min(PG_SIZE)
        };

        supp_page.insert(
            uaddr,
            PageSource::InFile {
                file: file.clone(),
                offset: file_offset,
                filesz: page_filesz,
                flags: leaf_flag,
            },
        );

        readpos += PG_SIZE;
    }
}

/// Initializes the user stack with one eagerly allocated page.
fn init_user_stack(pagetable: &mut PageTable, init_sp: usize, supp_page: &mut SuppPageTable) {
    assert!(init_sp % PG_SIZE == 0, "initial sp address misaligns");

    // Allocate a page from UserPool as user stack, evicting if necessary.
    let stack_va = crate::trap::pagefault::allocate_or_evict();
    if stack_va.is_null() {
        panic!("Cannot allocate initial stack page");
    }
    let stack_pa = PhysAddr::from(stack_va);

    // Get the start address of stack page
    let stack_page_begin = PageAlign::floor(init_sp - 1);

    // Install mapping
    let flags = PTEFlags::V | PTEFlags::R | PTEFlags::W | PTEFlags::U;
    pagetable.map(stack_pa, stack_page_begin, PG_SIZE, flags);

    // Record in supplementary page table
    supp_page.insert(
        stack_page_begin,
        PageSource::AnonFrame {
            phys_addr: stack_pa.value(),
            flags,
        },
    );

    // Register in global frame table so this frame can be tracked for eviction
    crate::mem::frame::FrameTable::instance()
        .lock()
        .register(stack_pa.value(), -1, stack_page_begin);

    #[cfg(feature = "debug")]
    kprintln!(
        "[USERPROC] User Stack Mapping: (k){:p} -> (u) {:#x}",
        stack_va,
        stack_page_begin
    );
}
