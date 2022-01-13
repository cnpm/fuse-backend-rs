use std::any::Any;
use std::ffi::CStr;
use std::mem::size_of;
use std::path::Path;
use std::sync::Arc;

use vm_memory::bitmap::BitmapSlice;
use fuse_backend_rs::abi::linux_abi::{Attr, FsOptions, InHeader, InitIn, Opcode, OpenOptions, OutHeader, SetattrValid};

#[macro_use]
extern crate log;
extern crate simple_logger;

#[cfg(feature = "macfuse")]
use fuse_backend_rs::transport::macfuse::{FuseSession};
use simple_logger::SimpleLogger;


use std::io;
use std::io::{ErrorKind, IoSlice, Write};
use std::thread::sleep;
use std::time::{Duration, SystemTime};
use libc::{stat, time_t};
use log::LevelFilter;
use vm_memory::ByteValued;
use fuse_backend_rs::abi::virtio_fs::RemovemappingOne;
use fuse_backend_rs::api::{BackendFileSystem, CreateIn, Vfs, VfsOptions};
use fuse_backend_rs::api::filesystem::{Context, DirEntry, Entry, FileSystem, GetxattrReply, ListxattrReply, ROOT_ID, ZeroCopyReader, ZeroCopyWriter};
use fuse_backend_rs::api::server::Server;
use fuse_backend_rs::async_util::AsyncDriver;
use fuse_backend_rs::transport::FsCacheReqHandler;

/// Error codes for Fuse related operations.
#[derive(Debug)]
pub enum Error {
    /// Failed to decode protocol messages.
    DecodeMessage(io::Error),
    /// Failed to encode protocol messages.
    EncodeMessage(io::Error),
    /// One or more parameters are missing.
    MissingParameter,
    /// A C string parameter is invalid.
    // InvalidCString(FromBytesWithNulError),
    /// The `len` field of the header is too small.
    InvalidHeaderLength,
    /// The `size` field of the `SetxattrIn` message does not match the length
    /// of the decoded value.
    InvalidXattrSize((u32, usize)),
}

const MAX_BUFFER_SIZE: u32 = 1 << 20;

// fn reply_no_sys_error(in_header: &InHeader, mut r: Reader, w: Writer)-> Result<usize, Error> {
//     return reply_error_explicit(io::Error::from_raw_os_error(libc::ENOSYS),in_header.unique, w);
//
// }
//
// fn handle_message(mut r: Reader, w: Writer) -> Result<usize, Error> {
//     println!("handle message!!!");
//     let in_header: &InHeader = &r.read_obj().map_err(Error::DecodeMessage)?;
//     println!(
//         "fuse: new req {:?}: {:?}",
//         Opcode::from(in_header.opcode),
//         in_header
//     );
//     if in_header.len > MAX_BUFFER_SIZE {
//         return reply_error_explicit(
//             std::io::Error::from_raw_os_error(libc::ENOMEM),
//             in_header.unique,
//             w,
//         );
//     }
//     let r = match in_header.opcode {
//         // x if x == Opcode::Lookup as u32 => reply_error(in_header, r, w),
//         // x if x == Opcode::Forget as u32 => reply_error(in_header, r), // No reply.
//         // x if x == Opcode::Getattr as u32 => reply_error(in_header, r, w),
//         // x if x == Opcode::Setattr as u32 => reply_error(in_header, r, w),
//         // x if x == Opcode::Readlink as u32 => self.readlink(in_header, w),
//         // x if x == Opcode::Symlink as u32 => self.symlink(in_header, r, w),
//         // x if x == Opcode::Mknod as u32 => self.mknod(in_header, r, w),
//         // x if x == Opcode::Mkdir as u32 => self.mkdir(in_header, r, w),
//         // x if x == Opcode::Unlink as u32 => self.unlink(in_header, r, w),
//         // x if x == Opcode::Rmdir as u32 => self.rmdir(in_header, r, w),
//         // x if x == Opcode::Rename as u32 => self.rename(in_header, r, w),
//         // x if x == Opcode::Link as u32 => self.link(in_header, r, w),
//         // x if x == Opcode::Open as u32 => self.open(in_header, r, w),
//         // x if x == Opcode::Read as u32 => self.read(in_header, r, w),
//         // x if x == Opcode::Write as u32 => self.write(in_header, r, w),
//         // x if x == Opcode::Statfs as u32 => self.statfs(in_header, w),
//         // x if x == Opcode::Release as u32 => self.release(in_header, r, w),
//         // x if x == Opcode::Fsync as u32 => self.fsync(in_header, r, w),
//         // x if x == Opcode::Setxattr as u32 => self.setxattr(in_header, r, w),
//         // x if x == Opcode::Getxattr as u32 => self.getxattr(in_header, r, w),
//         // x if x == Opcode::Listxattr as u32 => self.listxattr(in_header, r, w),
//         // x if x == Opcode::Removexattr as u32 => self.removexattr(in_header, r, w),
//         // x if x == Opcode::Flush as u32 => self.flush(in_header, r, w),
//         // x if x == Opcode::Init as u32 => init(in_header, r, w),
//         // x if x == Opcode::Opendir as u32 => self.opendir(in_header, r, w),
//         // x if x == Opcode::Readdir as u32 => self.readdir(in_header, r, w),
//         // x if x == Opcode::Releasedir as u32 => self.releasedir(in_header, r, w),
//         // x if x == Opcode::Fsyncdir as u32 => self.fsyncdir(in_header, r, w),
//         // x if x == Opcode::Getlk as u32 => self.getlk(in_header, r, w),
//         // x if x == Opcode::Setlk as u32 => self.setlk(in_header, r, w),
//         // x if x == Opcode::Setlkw as u32 => self.setlkw(in_header, r, w),
//         // x if x == Opcode::Access as u32 => self.access(in_header, r, w),
//         // x if x == Opcode::Create as u32 => self.create(in_header, r, w),
//         // x if x == Opcode::Interrupt as u32 => self.interrupt(in_header),
//         // x if x == Opcode::Bmap as u32 => self.bmap(in_header, r, w),
//         // x if x == Opcode::Destroy as u32 => self.destroy(),
//         // x if x == Opcode::Ioctl as u32 => self.ioctl(in_header, r, w),
//         // x if x == Opcode::Poll as u32 => self.poll(in_header, r, w),
//         // x if x == Opcode::NotifyReply as u32 => self.notify_reply(in_header, r, w),
//         // x if x == Opcode::BatchForget as u32 => self.batch_forget(in_header, r, w),
//         // x if x == Opcode::Fallocate as u32 => self.fallocate(in_header, r, w),
//         // x if x == Opcode::Readdirplus as u32 => self.readdirplus(in_header, r, w),
//         #[cfg(target_os = "linux")]
//         x if x == Opcode::Rename2 as u32 => self.rename2(in_header, r, w),
//         // x if x == Opcode::Lseek as u32 => self.lseek(in_header, r, w),
//         // #[cfg(feature = "virtiofs")]
//         // x if x == Opcode::SetupMapping as u32 => self.setupmapping(in_header, r, w, vu_req),
//         // #[cfg(feature = "virtiofs")]
//         // x if x == Opcode::RemoveMapping as u32 => self.removemapping(in_header, r, w, vu_req),
//         _ => reply_no_sys_error(in_header, r, w),
//     };
//     return Ok(0);
// }
//
// fn reply_error_explicit(err: io::Error, unique: u64, w: Writer) -> Result<usize, Error> {
//     do_reply_error(err, unique, w, true)
// }
//
// pub fn encode_io_error_kind(kind: ErrorKind) -> i32 {
//     match kind {
//         //ErrorKind::ConnectionRefused => libc::ECONNREFUSED,
//         //ErrorKind::ConnectionReset => libc::ECONNRESET,
//         ErrorKind::PermissionDenied => libc::EPERM | libc::EACCES,
//         //ErrorKind::BrokenPipe => libc::EPIPE,
//         //ErrorKind::NotConnected => libc::ENOTCONN,
//         //ErrorKind::ConnectionAborted => libc::ECONNABORTED,
//         //ErrorKind::AddrNotAvailable => libc::EADDRNOTAVAIL,
//         //ErrorKind::AddrInUse => libc::EADDRINUSE,
//         ErrorKind::NotFound => libc::ENOENT,
//         ErrorKind::Interrupted => libc::EINTR,
//         //ErrorKind::InvalidInput => libc::EINVAL,
//         //ErrorKind::TimedOut => libc::ETIMEDOUT,
//         ErrorKind::AlreadyExists => libc::EEXIST,
//         ErrorKind::WouldBlock => libc::EWOULDBLOCK,
//         _ => libc::EIO,
//     }
// }
//
//
// fn do_reply_error(err: io::Error, unique: u64, mut w: Writer, explicit: bool) -> Result<usize, Error> {
//     let header = OutHeader {
//         len: size_of::<OutHeader>() as u32,
//         error: -err
//             .raw_os_error()
//             .unwrap_or_else(|| encode_io_error_kind(err.kind())),
//         unique,
//     };
//
//     if explicit {
//         error!("fuse: reply error header {:?}, error {:?}", header, err);
//     } else {
//         trace!("fuse: reply error header {:?}, error {:?}", header, err);
//     }
//     let mut buf = Vec::with_capacity(16);
//     buf.push(IoSlice::new(header.as_slice()));
//     w.write_vectored(&buf).map_err(Error::EncodeMessage)?;
//     // Commit header if it is buffered otherwise kernel gets nothing back.
//     w.commit(None)
//         .map(|_| {
//             debug_assert_eq!(header.len as usize, w.bytes_written());
//             w.bytes_written()
//         })
//         .map_err(Error::EncodeMessage)
// }

pub(crate) struct FakeFileSystemOne {}
impl FileSystem for FakeFileSystemOne {
    type Inode = u64;
    type Handle = u64;
    fn lookup(&self, _: &Context, _: Self::Inode, _: &CStr) -> Result<Entry, io::Error> {
        Ok(Entry {
            inode: 0,
            generation: 0,
            attr: Attr::default().into(),
            attr_flags: 0,
            attr_timeout: Duration::new(0, 0),
            entry_timeout: Duration::new(0, 0),
        })
    }

    // fn readdir(&self, ctx: &Context, inode: Self::Inode, handle: Self::Handle, size: u32, offset: u64, add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>) -> io::Result<()> {
    //     add_entry(DirEntry)
    // }

    fn getattr(&self, ctx: &Context, inode: Self::Inode, handle: Option<Self::Handle>) -> io::Result<(libc::stat, Duration)> {
        let now = SystemTime::now();
        let time = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as time_t;
        return Ok((libc::stat {
            st_dev: 0,
            st_mode: (libc::S_IFDIR | libc::S_IRWXU | libc::S_IRWXG | libc::S_IRWXO),
            st_nlink: 1,
            st_ino: 1,
            st_uid: 0,
            st_gid: 0,
            st_rdev: 0,
            st_atime: time,
            st_atime_nsec: 0,
            st_mtime: time,
            st_mtime_nsec: 0,
            st_ctime: time,
            st_ctime_nsec:0,
            st_birthtime: 0,
            st_birthtime_nsec:0,
            st_size: 0,
            st_blocks: 0,
            st_blksize: 4096,
            st_flags: 0,
            st_gen: 0,
            st_lspare: 0,
            st_qspare: [0,0],
        }, Duration::from_secs(1)));
    }

    fn access(&self, ctx: &Context, inode: Self::Inode, mask: u32) -> io::Result<()> {
        return Ok(());
    }
}

impl BackendFileSystem<AsyncDriver, ()> for FakeFileSystemOne {
    fn mount(&self) -> Result<(Entry, u64), io::Error> {
        Ok((
            Entry {
                inode: 1,
                generation: 0,
                attr: Attr::default().into(),
                attr_flags: 0,
                attr_timeout: Duration::new(0, 0),
                entry_timeout: Duration::new(0, 0),
            },
            0,
        ))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(feature = "macfuse")]
fn main() {
    SimpleLogger::new()
        .with_level(LevelFilter::Trace)
        .with_utc_timestamps()
        .init()
        .unwrap();


    let mut options = VfsOptions::default();
    options.in_opts = FsOptions::EXPORT_SUPPORT;
    let vfs = Vfs::<AsyncDriver>::new(options);
    let fs = FakeFileSystemOne {};
    vfs.mount(Box::new(fs), "/");

    let mount_point = Path::new("/Users/killa/workspace/project/test/hello");
    info!("start session!!!");
    let mut session = FuseSession::new(mount_point, "npmfs", "").unwrap();
    info!("start mount!!!");
    session.mount().unwrap();
    info!("mount end!!!");

    let server = Server::<Vfs<AsyncDriver>, AsyncDriver>::new(vfs);
    let server = Arc::new(server);
    let mut channel = session.new_channel().unwrap();
    loop {
        match channel.get_request() {
            Ok(request) => {
                match request {
                    Some((reader, writer)) => {
                        println!("get request!!!");
                        server.handle_message(reader, writer, None, None);
                    },
                    None => {
                        println!("get no request");
                        break;

                    },
                }
            },
            Err(e) => {
                println!("get request error: {:?}", e);
                break;
            }
        }
    }

    //
    // let mut handlers = Vec::new();
    //
    // for index in 0..5 {
    //     let mut channel = session.new_channel().unwrap();
    //     let server = server.clone();
    //     let handler = std::thread::spawn(move || {
    //         loop {
    //             match channel.get_request() {
    //                 Ok(request) => {
    //                     match request {
    //                         Some((reader, writer)) => {
    //                             println!("get request!!! {:?}", index);
    //                             server.handle_message(reader, writer, None, None);
    //                         },
    //                         None => {
    //                             println!("get no request {:?}", index);
    //                             break;
    //
    //                         },
    //                     }
    //                 },
    //                 Err(e) => {
    //                     println!("get request error: {:?}", e);
    //                     break;
    //                 }
    //             }
    //         }
    //     });
    //     handlers.push(handler);
    // }
    // for handler in handlers {
    //     handler.join();
    // }
}
