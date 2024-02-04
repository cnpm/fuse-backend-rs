use crate::{
    api::{filesystem::DirEntry, CURRENT_DIR_CSTR, PARENT_DIR_CSTR},
    bytes_to_cstr,
    passthrough::util::einval,
};
use std::ffi::CStr;
use std::ptr;
use std::{
    io,
    mem::{self, size_of},
    os::fd::{AsRawFd, RawFd},
};
use vm_memory::bitmap::BitmapSlice;
use vm_memory::ByteValued;

use super::{Handle, Inode, OffT, PassthroughFs};

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MacosDirent64 {
    pub d_ino: u64,
    pub d_seekoff: u64,
    pub d_reclen: u16,
    pub d_namlen: u16,
    pub d_type: u8,
}
unsafe impl ByteValued for MacosDirent64 {}

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

        let mut buf = Vec::<u8>::with_capacity(size as usize);
        let data = self.get_dirdata(handle, inode, libc::O_RDONLY)?;

        {
            // Since we are going to work with the kernel offset, we have to acquire the file lock
            // for both the `lseek64` and `getdents64` syscalls to ensure that no other thread
            // changes the kernel offset while we are using it.
            let (guard, dir) = data.get_file_mut();

            // Safe because this doesn't modify any memory and we check the return value.
            let res = unsafe { libc::lseek(dir.as_raw_fd(), offset as OffT, libc::SEEK_SET) };
            if res < 0 {
                return Err(io::Error::last_os_error());
            }

            // Safe because the kernel guarantees that it will only write to `buf` and we check the
            // return value.
            let res = unsafe {
                libc::read(
                    dir.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    size as libc::size_t,
                )
            };
            if res < 0 {
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EISDIR) => {
                        let dir = unsafe { libc::fdopendir(dir.as_raw_fd()) };
                        loop {
                            let entry_ptr = unsafe { libc::readdir(dir) };

                            if entry_ptr.is_null() {
                                break;
                            }

                            let entry: libc::dirent = unsafe { ptr::read(entry_ptr) };

                            let cstr = unsafe { CStr::from_ptr(entry.d_name.as_ptr()) };
                            let name_str = cstr.to_str().expect("Failed to convert CStr to str");
                            let res = if name_str == "." || name_str == ".." {
                                Ok(1)
                            } else {
                                add_entry(
                                    DirEntry {
                                        ino: entry.d_ino,
                                        offset: entry.d_seekoff,
                                        type_: entry.d_type as u32,
                                        name: cstr.to_bytes(),
                                    },
                                    data.borrow_fd().as_raw_fd(),
                                )
                            };
                            match res {
                                Ok(0) => break,
                                Ok(_) => continue,
                                Err(_) => return Ok(()),
                            }
                        }
                        unsafe { libc::closedir(dir) };
                        return Ok(());
                    }
                    _ => return Err(err),
                }
            }

            // Safe because we trust the value returned by kernel.
            unsafe { buf.set_len(res as usize) };

            // Explicitly drop the lock so that it's not held while we fill in the fuse buffer.
            mem::drop(guard);
        }

        let mut rem = &buf[..];
        let orig_rem_len = rem.len();

        while !rem.is_empty() {
            debug_assert!(
                rem.len() >= mem::size_of::<libc::dirent>(),
                "fuse: not enough space left in `rem`"
            );

            let (front, back) = rem.split_at(mem::size_of::<libc::dirent>());

            let dirent = unsafe { *(front.as_ptr() as *const libc::dirent) };

            let namelen = dirent.d_namlen as usize;
            debug_assert!(
                namelen <= back.len(),
                "fuse: back is smaller than `namelen`"
            );

            let name = &back[..namelen];
            let res = if name.starts_with(CURRENT_DIR_CSTR) || name.starts_with(PARENT_DIR_CSTR) {
                Ok(1)
            } else {
                let name = bytes_to_cstr(name)
                    .map_err(|e| {
                        error!("fuse: do_readdir: {:?}", e);
                        einval()
                    })?
                    .to_bytes();

                add_entry(
                    DirEntry {
                        ino: dirent.d_ino,
                        offset: dirent.d_seekoff,
                        type_: dirent.d_type as u32,
                        name,
                    },
                    data.borrow_fd().as_raw_fd(),
                )
            };

            debug_assert!(
                rem.len() >= dirent.d_reclen as usize,
                "fuse: rem is smaller than `d_reclen`"
            );

            match res {
                Ok(0) => break,
                Ok(_) => rem = &rem[dirent.d_reclen as usize..],
                Err(e) if rem.len() == orig_rem_len => return Err(e),
                Err(_) => return Ok(()),
            }
        }

        Ok(())
    }
}
