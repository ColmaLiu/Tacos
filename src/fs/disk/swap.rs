//! Swap file management.
//!
//! The `.glbswap` file on disk serves as backing store for evicted pages.
//! This module tracks free/in-use swap slots and provides read/write access.

use alloc::vec::Vec;

use super::DISKFS;
use crate::fs::{File, FileSys};
use crate::io::prelude::*;
use crate::mem::PG_SIZE;
use crate::sync::{Lazy, Mutex};

/// Global swap manager singleton
pub struct SwapManager {
    file: Mutex<File>,
    /// Stack of free slot indices for O(1) alloc/free
    free_slots: Mutex<Vec<usize>>,
}

impl SwapManager {
    fn instance() -> &'static SwapManager {
        static SWAP: Lazy<SwapManager> = Lazy::new(|| {
            let file = DISKFS
                .open(".glbswap".into())
                .expect("swap file \".glbswap\" should exist");
            let total_slots = file.len().unwrap() / PG_SIZE;
            SwapManager {
                file: Mutex::new(file),
                free_slots: Mutex::new((0..total_slots).rev().collect()),
            }
        });
        &SWAP
    }

    /// Allocate a free swap slot. Returns None if no free slots exist.
    pub fn alloc_slot() -> Option<usize> {
        Self::instance().free_slots.lock().pop()
    }

    /// Free a previously allocated swap slot.
    pub fn free_slot(index: usize) {
        Self::instance().free_slots.lock().push(index);
    }

    /// Read one page from the swap slot at `index` into `buf`.
    pub fn read_slot(index: usize, buf: &mut [u8]) -> bool {
        if buf.len() < PG_SIZE {
            return false;
        }
        let mut file = Self::instance().file.lock();
        if let Ok(pos) = file.pos() {
            *pos = index * PG_SIZE;
        }
        match file.read(&mut buf[..PG_SIZE]) {
            Ok(n) => n == PG_SIZE,
            Err(_) => false,
        }
    }

    /// Write one page to the swap slot at `index` from `buf`.
    pub fn write_slot(index: usize, buf: &[u8]) -> bool {
        if buf.len() < PG_SIZE {
            return false;
        }
        let mut file = Self::instance().file.lock();
        if let Ok(pos) = file.pos() {
            *pos = index * PG_SIZE;
        }
        match file.write(&buf[..PG_SIZE]) {
            Ok(n) => n == PG_SIZE,
            Err(_) => false,
        }
    }

    pub(crate) fn len() -> usize {
        Self::instance().file.lock().len().unwrap()
    }

    pub(crate) fn page_num() -> usize {
        Self::len() / PG_SIZE
    }

    pub(crate) fn lock() -> crate::sync::MutexGuard<'static, File, crate::sync::Primitive> {
        Self::instance().file.lock()
    }
}

/// Thin compatibility wrapper around [`SwapManager`].
pub struct Swap;

impl Swap {
    pub fn len() -> usize {
        SwapManager::len()
    }

    pub fn page_num() -> usize {
        SwapManager::page_num()
    }

    pub fn lock() -> crate::sync::MutexGuard<'static, File, crate::sync::Primitive> {
        SwapManager::lock()
    }
}
