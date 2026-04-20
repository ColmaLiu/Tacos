use alloc::collections::BTreeMap;

use crate::fs::File;

pub const O_RDONLY: usize = 0x000;
pub const O_WRONLY: usize = 0x001;
pub const O_RDWR: usize = 0x002;
pub const O_CREATE: usize = 0x200;
pub const O_TRUNC: usize = 0x400;

pub enum FileType {
    File(File),
    Stdin,
    Stdout,
    Stderr,
}

pub struct OpenFile {
    pub file: FileType,
    pub readable: bool,
    pub writable: bool,
}

impl OpenFile {
    pub fn new(file: FileType, flags: usize) -> Self {
        let readable;
        let writable;
        match file {
            FileType::File(_) => {
                let accmode = flags & 0x3;
                readable = accmode == O_RDONLY || accmode == O_RDWR;
                writable = accmode == O_WRONLY || accmode == O_RDWR;
            }
            FileType::Stdin => {
                readable = true;
                writable = false;
            }
            FileType::Stdout | FileType::Stderr => {
                readable = false;
                writable = true;
            }
        }
        Self {
            file,
            readable,
            writable,
        }
    }
}

pub struct FdTable {
    next_fd: usize,
    table: BTreeMap<usize, OpenFile>,
}

impl FdTable {
    pub fn new() -> Self {
        let mut table = BTreeMap::new();
        table.insert(0, OpenFile::new(FileType::Stdin, 0));
        table.insert(1, OpenFile::new(FileType::Stdout, 0));
        table.insert(2, OpenFile::new(FileType::Stderr, 0));
        Self { next_fd: 3, table }
    }

    pub fn insert(&mut self, open_file: OpenFile) -> usize {
        let fd = self.next_fd;
        self.table.insert(fd, open_file);
        self.next_fd += 1;
        fd
    }

    pub fn get(&self, fd: usize) -> Option<&OpenFile> {
        self.table.get(&fd)
    }

    pub fn get_mut(&mut self, fd: usize) -> Option<&mut OpenFile> {
        self.table.get_mut(&fd)
    }

    pub fn remove(&mut self, fd: usize) -> Option<OpenFile> {
        self.table.remove(&fd)
    }
}
