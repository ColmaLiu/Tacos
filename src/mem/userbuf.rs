#![allow(dead_code)]

use core::arch::global_asm;

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::OsError;
use crate::mem::in_kernel_space;
use crate::Result;

/// Read a single byte from user space.
///
/// ## Return
/// - `Ok(byte)`
/// - `Err`: A page fault happened.
fn read_user_byte(user_src: *const u8) -> Result<u8> {
    if in_kernel_space(user_src as usize) {
        return Err(OsError::BadPtr);
    }

    let byte: u8 = 0;
    let ret_status: u8 = unsafe { __knrl_read_usr_byte(user_src, &byte as *const u8) };

    if ret_status == 0 {
        Ok(byte)
    } else {
        Err(OsError::BadPtr)
    }
}

/// Write a single byte to user space.
///
/// ## Return
/// - `Ok(())`
/// - `Err`: A page fault happened.
fn write_user_byte(user_src: *const u8, value: u8) -> Result<()> {
    if in_kernel_space(user_src as usize) {
        return Err(OsError::BadPtr);
    }

    let ret_status: u8 = unsafe { __knrl_write_usr_byte(user_src, value) };

    if ret_status == 0 {
        Ok(())
    } else {
        Err(OsError::BadPtr)
    }
}

pub fn read_user_cstr(user_src: *const u8, max_len: usize) -> Result<String> {
    let mut buf = Vec::new();
    for i in 0..max_len {
        let ch = read_user_byte(unsafe { user_src.add(i) })?;
        if ch == 0 {
            return String::from_utf8(buf).map_err(|_| OsError::BadPtr);
        }
        buf.push(ch);
    }
    Err(OsError::BadPtr)
}

pub fn read_user_buf(user_src: *const u8, len: usize) -> Result<Vec<u8>> {
    let mut v = Vec::with_capacity(len);
    for i in 0..len {
        v.push(read_user_byte(unsafe { user_src.add(i) })?);
    }
    Ok(v)
}

pub fn read_user_usize(user_src: *const u8) -> Result<usize> {
    let mut bytes = [0u8; core::mem::size_of::<usize>()];
    for i in 0..bytes.len() {
        bytes[i] = read_user_byte(unsafe { user_src.add(i) })?;
    }
    Ok(usize::from_ne_bytes(bytes))
}

pub fn write_user_buf(user_dst: *const u8, buf: &[u8]) -> Result<()> {
    for (i, b) in buf.iter().enumerate() {
        write_user_byte(unsafe { user_dst.add(i) }, *b)?;
    }
    Ok(())
}

pub fn write_user_usize(user_dst: *const u8, value: usize) -> Result<()> {
    write_user_buf(user_dst, &value.to_ne_bytes())
}

extern "C" {
    pub fn __knrl_read_usr_byte(user_src: *const u8, byte_ptr: *const u8) -> u8;
    pub fn __knrl_read_usr_byte_pc();
    pub fn __knrl_read_usr_exit();
    pub fn __knrl_write_usr_byte(user_src: *const u8, value: u8) -> u8;
    pub fn __knrl_write_usr_byte_pc();
    pub fn __knrl_write_usr_exit();
}

global_asm! {r#"
        .section .text
        .globl __knrl_read_usr_byte
        .globl __knrl_read_usr_exit
        .globl __knrl_read_usr_byte_pc

    __knrl_read_usr_byte:
        mv t1, a1
        li a1, 0
    __knrl_read_usr_byte_pc:
        lb t0, (a0)
    __knrl_read_usr_exit:
        # pagefault handler will set a1 if any error occurs
        sb t0, (t1)
        mv a0, a1
        ret

        .globl __knrl_write_usr_byte
        .globl __knrl_write_usr_exit
        .globl __knrl_write_usr_byte_pc

    __knrl_write_usr_byte:
        mv t1, a1
        li a1, 0
    __knrl_write_usr_byte_pc:
        sb t1, (a0)
    __knrl_write_usr_exit:
        # pagefault handler will set a1 if any error occurs
        mv a0, a1
        ret
"#}
