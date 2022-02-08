// Copyright 2020-2021 Ant Group. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! FUSE session management.
//!
//! A FUSE channel is a FUSE request handling context that takes care of handling FUSE requests
//! sequentially. A FUSE session is a connection from a FUSE mountpoint to a FUSE server daemon.
//! A FUSE session can have multiple FUSE channels so that FUSE requests are handled in parallel.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::ops::Deref;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use core_foundation_sys::base::CFRelease;
use core_foundation_sys::string::{CFStringCreateWithBytes, kCFStringEncodingUTF8};
use core_foundation_sys::url::{CFURLCreateWithFileSystemPath, CFURLPathStyle, kCFURLPOSIXPathStyle};
use diskarbitration_sys::base::{DADiskUnmount, kDADiskUnmountOptionForce};
use diskarbitration_sys::disk::{DADiskCreateFromVolumePath, DADiskRef};
use diskarbitration_sys::session::{DASessionCreate, DASessionRef};

use libc::{sysconf, _SC_PAGESIZE};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::NixPath;
use nix::poll::{poll, PollFd, PollFlags};
use nix::unistd::{getgid, getuid, read};

use super::{Error::SessionFailure, FuseBuf, Reader, Result, Writer, macfuse};

// These follows definition from libfuse.
const FUSE_KERN_BUF_SIZE: usize = 256;
const FUSE_HEADER_SIZE: usize = 0x1000;

const FUSE_DEV_EVENT: u32 = 0;
const EXIT_FUSE_EVENT: u32 = 1;

mod ioctl {
    use nix::ioctl_write_ptr;

    // #define FUSEDEVIOCSETDAEMONDEAD _IOW('F', 3,  u_int32_t)
    const FUSE_FD_DEAD_MAGIC: u8 = 'F' as u8;
    const FUSE_FD_DEAD:u8 = 3;
    ioctl_write_ptr!(set_fuse_fd_dead, FUSE_FD_DEAD_MAGIC, FUSE_FD_DEAD, u32);
}



/// A fuse session manager to manage the connection with the in kernel fuse driver.
pub struct FuseSession {
    mountpoint: PathBuf,
    fsname: String,
    subtype: String,
    file: Option<File>,
    bufsize: usize,
    disk: Arc<Mutex<Option<DADiskRef>>>,
    dasession: Arc<Mutex<DASessionRef>>,
}

unsafe impl Send for FuseSession {

}

impl FuseSession {
    /// Create a new fuse session, without mounting/connecting to the in kernel fuse driver.
    pub fn new(mountpoint: &Path, fsname: &str, subtype: &str) -> Result<FuseSession> {
        let dest = mountpoint
            .canonicalize()
            .map_err(|_| SessionFailure(format!("invalid mountpoint {:?}", mountpoint)))?;
        if !dest.is_dir() {
            return Err(SessionFailure(format!("{:?} is not a directory", dest)));
        }

        Ok(FuseSession {
            mountpoint: dest,
            fsname: fsname.to_owned(),
            subtype: subtype.to_owned(),
            file: None,
            bufsize: FUSE_KERN_BUF_SIZE * pagesize() + FUSE_HEADER_SIZE,
            disk: Arc::new(Mutex::new(None)),
            dasession: Arc::new(Mutex::new(unsafe { DASessionCreate(std::ptr::null()) })),
        })
    }

    /// Mount the fuse mountpoint, building connection with the in kernel fuse driver.
    pub fn mount(&mut self) -> Result<()> {
        let mut disk = self.disk.lock().expect("lock disk failed");
        let file = fuse_kern_mount(&self.mountpoint, &self.fsname, &self.subtype)?;
        let session = self.dasession.lock().expect("lock da session failed");
        let mount_disk = create_disk(&self.mountpoint, *session.deref());

        self.file = Some(file);
        *disk = Some(mount_disk);

        Ok(())
    }

    /// Expose the associated FUSE session file.
    pub fn get_fuse_file(&mut self) -> Option<&File> {
        self.file.as_ref()
    }

    /// Force setting the associated FUSE session file.
    pub fn set_fuse_file(&mut self, file: File) {
        self.file = Some(file);
    }

    /// Destroy a fuse session.
    pub fn umount(&mut self) -> Result<()> {
        if let Some(file) = self.file.take() {
            if let Some(mountpoint) = self.mountpoint.to_str() {
                let mut disk = self.disk.lock().expect("lock disk failed");
                fuse_kern_umount(file, disk.take())
            } else {
                Err(SessionFailure("invalid mountpoint".to_string()))
            }
        } else {
            Ok(())
        }
    }

    /// Get the mountpoint of the session.
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Get the file system name of the session.
    pub fn fsname(&self) -> &str {
        &self.fsname
    }

    /// Get the subtype of the session.
    pub fn subtype(&self) -> &str {
        &self.subtype
    }

    /// Get the default buffer size of the session.
    pub fn bufsize(&self) -> usize {
        self.bufsize
    }

    /// Create a new fuse message channel.
    pub fn new_channel(&self, fuse_session_end: RawFd) -> Result<FuseChannel> {
        if let Some(file) = &self.file {
            let file = file
                .try_clone()
                .map_err(|e| SessionFailure(format!("dup fd: {}", e)))?;
            FuseChannel::new(file, fuse_session_end, self.bufsize)
        } else {
            Err(SessionFailure("invalid fuse session".to_string()))
        }
    }
}

impl Drop for FuseSession {
    fn drop(&mut self) {
        let _ = self.umount();
    }
}

/// A fuse channel abstruction. Each session can hold multiple channels.
pub struct FuseChannel {
    file: File,
    pipe_fd: RawFd,
    buf: Vec<u8>,
}

impl FuseChannel {
    fn new(file: File, pipe_fd: RawFd, bufsize: usize) -> Result<Self> {
        Ok(FuseChannel {
            file,
            pipe_fd,
            buf: vec![0x0u8; bufsize],
        })
    }

    /// Get next available FUSE request from the underlying fuse device file.
    ///
    /// Returns:
    /// - Ok(None): signal has pending on the exiting event channel
    /// - Ok(Some((reader, writer))): reader to receive request and writer to send reply
    /// - Err(e): error message
    pub fn get_request(&mut self) -> Result<Option<(Reader, Writer)>> {
        let fd = self.file.as_raw_fd();
        loop {
            match read(fd, &mut self.buf) {
                Ok(len) => {
                    // ###############################################
                    // Note: it's a heavy hack to reuse the same underlying data
                    // buffer for both Reader and Writer, in order to reduce memory
                    // consumption. Here we assume Reader won't be used anymore once
                    // we start to write to the Writer. To get rid of this hack,
                    // just allocate a dedicated data buffer for Writer.
                    let buf = unsafe {
                        std::slice::from_raw_parts_mut(
                            self.buf.as_mut_ptr(),
                            self.buf.len(),
                        )
                    };
                    // Reader::new() and Writer::new() should always return success.
                    let reader =
                        Reader::new(FuseBuf::new(&mut self.buf[..len])).unwrap();
                    let writer = Writer::new(fd, buf).unwrap();
                    return Ok(Some((reader, writer)));
                }
                Err(e) => match e {
                    Errno::ENOENT => {
                        // ENOENT means the operation was interrupted, it's safe
                        // to restart
                        trace!("restart reading");
                        continue;
                    }
                    Errno::EAGAIN | Errno::EINTR => {
                        continue;
                    }
                    Errno::ENODEV => {
                        info!("fuse filesystem umounted");
                        return Ok(None);
                    }
                    e => {
                        warn! {"read fuse dev failed on fd {}: {}", fd, e};
                        return Err(SessionFailure(format!(
                            "read new request: {:?}",
                            e
                        )));
                    }
                },
            }
        }
    }
}

/// Safe wrapper for `sysconf(_SC_PAGESIZE)`.
#[inline(always)]
fn pagesize() -> usize {
    // Trivially safe
    unsafe { sysconf(_SC_PAGESIZE) as usize }
}

/// Mount a fuse file system
fn fuse_kern_mount(mountpoint: &Path, fsname: &str, subtype: &str) -> Result<File> {
    unsafe {
        let mut mountpoint_argv = String::from(mountpoint.to_str().unwrap());
        let mut mountpoint_argv = CString::new(mountpoint_argv).unwrap();

        let mut argv = vec![
            mountpoint_argv.into_raw(),
        ];
        // let mut argv = ManuallyDrop::new(argv);
        let mut args = macfuse::fuse_args {
            argc: argv.len() as i32,
            argv: argv.as_mut_ptr(),
            allocated: 0,
        };

        let mut mountpoint_argv = String::from(mountpoint.to_str().unwrap());
        let mut mountpoint_argv = CString::new(mountpoint_argv).unwrap();
        let fd = macfuse::fuse_mount_compat25(
            mountpoint_argv.as_ptr(),
            (&mut args),
            // flags.bits as libc::c_int,
            // opts.as_str().as_ptr() as *mut libc::c_void,
        );
        if fd < 0 {
            return Err(SessionFailure(String::from("failed to mount")))
        }
        let file: File = File::from_raw_fd(fd);
        return Ok(file);
    }
}

fn create_disk(mountpoint: &Path, dasession: DASessionRef) -> DADiskRef {
    unsafe {
        let mut mountpoint_argv = String::from(mountpoint.to_str().unwrap());
        let mut mountpoint_argv = CString::new(mountpoint_argv).unwrap();
        let mut mountpoint_argv = mountpoint_argv.as_ptr();
        let url_str = CFStringCreateWithBytes(
            std::ptr::null(),
            std::mem::transmute(mountpoint_argv),
            mountpoint.len() as i64,
            kCFStringEncodingUTF8,
            1u8,
            std::ptr::null());
        let url = CFURLCreateWithFileSystemPath(std::ptr::null(), url_str, kCFURLPOSIXPathStyle, 1u8);
        let disk = DADiskCreateFromVolumePath(std::ptr::null(), dasession, url);
        CFRelease(std::mem::transmute(url_str));
        CFRelease(std::mem::transmute(url));
        disk
    }

}

/// Umount a fuse file system
fn fuse_kern_umount(file: File, disk: Option<DADiskRef>) -> Result<()> {
    let fd = file.as_raw_fd();
    if let Err(e) = set_fuse_fd_dead(file.as_raw_fd()) {
        return Err(SessionFailure(String::from("ioctl set fuse deamon dead failed")));
    }
    drop(file);

    if let Some(disk) = disk {
        unsafe {
            DADiskUnmount(disk, kDADiskUnmountOptionForce, None, std::ptr::null_mut());
            CFRelease(std::mem::transmute(disk));
        }
    }
    return Ok(())
}

fn set_fuse_fd_dead(fd: RawFd) -> std::io::Result<()> {
    unsafe {
        match ioctl::set_fuse_fd_dead(fd, std::mem::transmute(&fd)) {
            Ok(i) => {
                return Ok(());
            },
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use super::*;
    use std::os::unix::io::{FromRawFd, RawFd};
    use std::path::Path;
    use vmm_sys_util::tempdir::TempDir;

    #[test]
    fn test_new_session() {
        let se = FuseSession::new(Path::new("haha"), "foo", "bar");
        assert!(se.is_err());

        let dir = TempDir::new().unwrap();
        let se = FuseSession::new(dir.as_path(), "foo", "bar");
        assert!(se.is_ok());
    }

    #[test]
    fn test_new_channel() {
        let ch = FuseChannel::new(
            unsafe { File::from_raw_fd(RawFd::new(0).unwrap().as_raw_fd()) },
            RawFd::new(0).unwrap(),
            3,
        );
        assert!(ch.is_ok());
    }
}

#[cfg(feature = "async-io")]
pub use asyncio::FuseDevTask;

#[cfg(feature = "async-io")]
/// Task context to handle fuse request in asynchronous mode.
mod asyncio {
    use std::os::unix::io::RawFd;
    use std::sync::Arc;

    use crate::api::filesystem::AsyncFileSystem;
    use crate::api::server::Server;
    use crate::async_util::{AsyncDriver, AsyncExecutorState, AsyncUtil};
    use crate::transport::{FuseBuf, Reader, Writer};

    /// Task context to handle fuse request in asynchronous mode.
    ///
    /// This structure provides a context to handle fuse request in asynchronous mode, including
    /// the fuse fd, a internal buffer and a `Server` instance to serve requests.
    ///
    /// ## Examples
    /// ```ignore
    /// let buf_size = 0x1_0000;
    /// let state = AsyncExecutorState::new();
    /// let mut task = FuseDevTask::new(buf_size, fuse_dev_fd, fs_server, state.clone());
    ///
    /// // Run the task
    /// executor.spawn(async move { task.poll_handler().await });
    ///
    /// // Stop the task
    /// state.quiesce();
    /// ```
    pub struct FuseDevTask<F: AsyncFileSystem + Sync> {
        fd: RawFd,
        buf: Vec<u8>,
        state: AsyncExecutorState,
        server: Arc<Server<F>>,
    }

    impl<F: AsyncFileSystem + Sync> FuseDevTask<F> {
        /// Create a new fuse task context for asynchronous IO.
        ///
        /// # Parameters
        /// - buf_size: size of buffer to receive requests from/send reply to the fuse fd
        /// - fd: fuse device file descriptor
        /// - server: `Server` instance to serve requests from the fuse fd
        /// - state: shared state object to control the task object
        ///
        /// # Safety
        /// The caller must ensure `fd` is valid during the lifetime of the returned task object.
        pub fn new(
            buf_size: usize,
            fd: RawFd,
            server: Arc<Server<F>>,
            state: AsyncExecutorState,
        ) -> Self {
            FuseDevTask {
                fd,
                server,
                state,
                buf: vec![0x0u8; buf_size],
            }
        }

        /// Handler to process fuse requests in asynchronous mode.
        ///
        /// An async fn to handle requests from the fuse fd. It works in asynchronous IO mode when:
        /// - receiving request from fuse fd
        /// - handling requests by calling Server::async_handle_requests()
        /// - sending reply to fuse fd
        ///
        /// The async fn repeatedly return Poll::Pending when polled until the state has been set
        /// to quiesce mode.
        pub async fn poll_handler(&mut self) {
            // TODO: register self.buf as io uring buffers.
            let drive = AsyncDriver::default();

            while !self.state.quiescing() {
                let result = AsyncUtil::read(drive.clone(), self.fd, &mut self.buf, 0).await;
                match result {
                    Ok(len) => {
                        // ###############################################
                        // Note: it's a heavy hack to reuse the same underlying data
                        // buffer for both Reader and Writer, in order to reduce memory
                        // consumption. Here we assume Reader won't be used anymore once
                        // we start to write to the Writer. To get rid of this hack,
                        // just allocate a dedicated data buffer for Writer.
                        let buf = unsafe {
                            std::slice::from_raw_parts_mut(self.buf.as_mut_ptr(), self.buf.len())
                        };
                        // Reader::new() and Writer::new() should always return success.
                        let reader = Reader::new(FuseBuf::new(&mut self.buf[0..len])).unwrap();
                        let writer = Writer::new(self.fd, buf).unwrap();
                        let result = unsafe {
                            self.server
                                .async_handle_message(drive.clone(), reader, writer, None, None)
                                .await
                        };

                        if let Err(e) = result {
                            // TODO: error handling
                            error!("failed to handle fuse request, {}", e);
                        }
                    }
                    Err(e) => {
                        // TODO: error handling
                        error!("failed to read request from fuse device fd, {}", e);
                    }
                }
            }

            // TODO: unregister self.buf as io uring buffers.

            // Report that the task has been quiesced.
            self.state.report();
        }
    }

    impl<F: AsyncFileSystem + Sync> Clone for FuseDevTask<F> {
        fn clone(&self) -> Self {
            FuseDevTask {
                fd: self.fd,
                server: self.server.clone(),
                state: self.state.clone(),
                buf: vec![0x0u8; self.buf.capacity()],
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::os::unix::io::AsRawFd;
        use std::sync::Arc;

        use super::*;
        use crate::api::{Vfs, VfsOptions};
        use crate::api::server::Server;
        use crate::async_util::{AsyncDriver, AsyncExecutor, AsyncExecutorState};

        #[test]
        fn test_fuse_task() {
            let state = AsyncExecutorState::new();
            let fs = Vfs::<AsyncDriver, ()>::new(VfsOptions::default());
            let _server = Arc::new(Server::<Vfs<AsyncDriver, ()>, AsyncDriver, ()>::new(fs));
            let file = vmm_sys_util::tempfile::TempFile::new().unwrap();
            let _fd = file.as_file().as_raw_fd();

            let mut executor = AsyncExecutor::new(32);
            executor.setup().unwrap();

            /*
            // Create three tasks, which could handle three concurrent fuse requests.
            let mut task = FuseDevTask::new(0x1000, fd, server.clone(), state.clone());
            executor
                .spawn(async move { task.poll_handler().await })
                .unwrap();
            let mut task = FuseDevTask::new(0x1000, fd, server.clone(), state.clone());
            executor
                .spawn(async move { task.poll_handler().await })
                .unwrap();
            let mut task = FuseDevTask::new(0x1000, fd, server.clone(), state.clone());
            executor
                .spawn(async move { task.poll_handler().await })
                .unwrap();
             */

            for _i in 0..10 {
                executor.run_once(false).unwrap();
            }

            // Set existing flag
            state.quiesce();
            // Close the fusedev fd, so all pending async io requests will be aborted.
            drop(file);

            for _i in 0..10 {
                executor.run_once(false).unwrap();
            }
        }
    }
}
