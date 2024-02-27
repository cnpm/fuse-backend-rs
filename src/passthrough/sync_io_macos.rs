// Copyright (C) 2020-2022 Alibaba Cloud. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Fuse passthrough file system, mirroring an existing FS hierarchy.
//!
//! This file system mirrors the existing file system hierarchy of the system, starting at the
//! root file system. This is implemented by just "passing through" all requests to the
//! corresponding underlying file system.
//!
//! The code is derived from the
//! [CrosVM](https://chromium.googlesource.com/chromiumos/platform/crosvm/) project,
//! with heavy modification/enhancements from Alibaba Cloud OS team.
use std::{
    ffi::CStr,
    io,
    os::fd::{AsRawFd, RawFd},
    ptr,
};

use vm_memory::bitmap::BitmapSlice;

use crate::api::filesystem::DirEntry;

use super::{Handle, Inode, OffT, PassthroughFs};

impl<S: BitmapSlice + Send + Sync> PassthroughFs<S> {
    pub fn do_readdir(
        &self,
        inode: Inode,
        handle: Handle,
        size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry, RawFd) -> io::Result<usize>,
    ) -> io::Result<()> {
        if size == 0 {
            return Ok(());
        }

        let data = self.get_dirdata(handle, inode, libc::O_RDONLY)?;

        let (_guard, dir) = data.get_file_mut();

        if !dir.metadata()?.is_dir() {
            return Ok(());
        }

        if let Some(u_ptr) = data.get_dir_ptr() {
            let dir = u_ptr as *mut libc::DIR;

            unsafe { libc::seekdir(dir, offset as libc::c_long) };

            let mut loc = 0;

            loop {
                let entry_ptr = unsafe { libc::readdir(dir) };

                if entry_ptr.is_null() {
                    break;
                }

                let entry: libc::dirent = unsafe { ptr::read(entry_ptr) };

                let cstr = unsafe { CStr::from_ptr(entry.d_name.as_ptr()) };
                let name_str = cstr.to_str().expect("Failed to convert CStr to str");
                if name_str == "." || name_str == ".." {
                    loc = unsafe { libc::telldir(dir) };
                    continue;
                } else {
                    loc = unsafe { libc::telldir(dir) };
                    if loc as u64 > offset {
                        let _ = add_entry(
                            DirEntry {
                                ino: entry.d_ino,
                                offset: loc as u64,
                                type_: entry.d_type as u32,
                                name: cstr.to_bytes(),
                            },
                            data.borrow_fd().as_raw_fd(),
                        );

                        break;
                    }
                };
            }
        }

        return Ok(());
    }
}
