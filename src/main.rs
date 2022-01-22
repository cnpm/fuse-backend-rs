use std::any::Any;
use std::ffi::CStr;
use std::mem::size_of;
use std::path::Path;
use std::sync::{Arc, Mutex};

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
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, SystemTime};
use libc::{stat, time_t};
use log::LevelFilter;
use vm_memory::ByteValued;
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


pub(crate) struct FakeFileSystemOne {
    fuse_session_end: Arc<AtomicBool>,
}

impl FileSystem for FakeFileSystemOne {
    type Inode = u64;
    type Handle = u64;
    fn lookup(&self, _: &Context, parent: Self::Inode, name: &CStr) -> Result<Entry, io::Error> {
        let content = "hello, fuse".as_bytes();
        let now = SystemTime::now();
        let time = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Ok(Entry {
            inode: 2,
            generation: 0,
            attr: Attr {
                ino: 2,
                size: content.len() as u64,
                blocks: 1,
                atime: time,
                mtime: time,
                ctime: time,
                crtime: time,
                atimensec: 0,
                mtimensec: 0,
                ctimensec: 0,
                crtimensec: 0,
                mode: (libc::S_IFREG | libc::S_IREAD | libc::S_IEXEC | libc::S_IRGRP | libc::S_IXGRP | libc::S_IROTH | libc::S_IXOTH) as u32,
                nlink: 1,
                uid: 0,
                gid: 0,
                rdev: 0,
                flags: 0,
                blksize: 4096,
                padding: 0,
            }.into(),
            attr_flags: 0,
            attr_timeout: Duration::new(0, 0),
            entry_timeout: Duration::new(0, 0),
        })
    }

    fn readdir(&self, ctx: &Context, inode: Self::Inode, handle: Self::Handle, size: u32, offset: u64, add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>) -> io::Result<()> {
        if offset != 0 {
            return Ok(());
        }
        println!("read dir: {:?} {:?}", size, offset);
        let mut offset: usize = offset as usize;
        let entry = DirEntry {
            ino: 1,
            offset: offset as u64,
            type_: libc::DT_DIR as u32,
            name: ".".as_bytes(),
        };
        offset += add_entry(entry).unwrap();

        let entry = DirEntry {
            ino: 1,
            offset: offset as u64,
            type_: libc::DT_DIR as u32,
            name: "..".as_bytes(),
        };
        offset += add_entry(entry).unwrap();

        let entry = DirEntry {
            ino: 2,
            offset: offset  as u64,
            type_: libc::DT_REG as u32,
            name: "hello".as_bytes(),
        };
        add_entry(entry);
        Ok(())
    }

    fn read(&self, ctx: &Context, inode: Self::Inode, handle: Self::Handle, w: &mut dyn ZeroCopyWriter<S=()>, size: u32, offset: u64, lock_owner: Option<u64>, flags: u32) -> io::Result<usize> {
        let offset = offset as usize;
        let content = "hello, fuse".as_bytes();
        let mut buf = Vec::<u8>::with_capacity(size as usize);
        let can_read_size = content.len() - offset;
        let read_size = if can_read_size < size as usize {
            can_read_size
        } else {
            size as usize
        };
        let read_end = (offset as usize) + read_size;
        println!("size {} offset {} read_end {}", size, offset, read_end);
        buf.extend_from_slice(&content[(offset as usize)..(read_end as usize)]);
        w.write(buf.as_slice());
        Ok(read_size)
    }

    fn destroy(&self) {
        self.fuse_session_end.store(true, Ordering::SeqCst);
    }

    fn getattr(&self, ctx: &Context, inode: Self::Inode, handle: Option<Self::Handle>) -> io::Result<(libc::stat, Duration)> {
        println!("get attr: {}", inode);
        if inode == 1 {
            let now = SystemTime::now();
            let time = now
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs() as time_t;
            return Ok((libc::stat {
                st_dev: 0,
                st_mode: (libc::S_IFDIR | libc::S_IREAD | libc::S_IEXEC | libc::S_IRGRP | libc::S_IXGRP | libc::S_IROTH | libc::S_IXOTH),
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
        } else {
            let content = "hello, fuse".as_bytes();
            let now = SystemTime::now();
            let time = now
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs() as time_t;
            return Ok((libc::stat {
                st_dev: 0,
                st_mode: (libc::S_IFREG | libc::S_IREAD | libc::S_IEXEC | libc::S_IRGRP | libc::S_IXGRP | libc::S_IROTH | libc::S_IXOTH),
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
                st_size: content.len() as libc::off_t,
                st_blocks: 1,
                st_blksize: 4096,
                st_flags: 0,
                st_gen: 0,
                st_lspare: 0,
                st_qspare: [0,0],
            }, Duration::from_secs(1)));
        }
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
    let fuse_session_end: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let fs = FakeFileSystemOne {
        fuse_session_end: fuse_session_end.clone(),
    };
    vfs.mount(Box::new(fs), "/");

    let mount_point = Path::new("/Users/killa/workspace/project/test2/hello");
    info!("start session!!!");
    let mut session = FuseSession::new(mount_point, "npmfs", "").unwrap();
    info!("start mount!!!");
    session.mount().unwrap();
    info!("mount end!!!");

    let server = Server::<Vfs<AsyncDriver>, AsyncDriver>::new(vfs);
    let server = Arc::new(server);
    let mut channel = session.new_channel(fuse_session_end.clone()).unwrap();
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
