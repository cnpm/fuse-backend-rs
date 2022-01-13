// Copyright (C) 2020 Alibaba Cloud. All rights reserved.
//
// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

//! Fuse transport drivers to receive requests from/send reply to Fuse clients.

use std::cmp;
use std::collections::VecDeque;
#[cfg(feature = "async-io")]
use std::io::IoSlice;
#[cfg(all(feature = "async-io", feature = "virtiofs"))]
use std::io::IoSliceMut;
use std::io::{self, Read};
use std::mem::{size_of, MaybeUninit};
use std::ptr::copy_nonoverlapping;

use vm_memory::{ByteValued, VolatileSlice};

use crate::BitmapSlice;

pub mod file_traits;
pub use file_traits::{FileReadWriteVolatile, FileSetLen};

#[cfg(feature = "virtiofs")]
pub mod virtiofs;
#[cfg(feature = "virtiofs")]
pub use self::virtiofs::{Error, FsCacheReqHandler, Result, Writer};

#[cfg(all(feature = "fusedev", not(feature = "virtiofs"), not(feature = "macfuse")))]
pub mod fusedev;
#[cfg(all(feature = "fusedev", not(feature = "virtiofs"), not(feature = "macfuse")))]
pub use self::fusedev::{Error, FsCacheReqHandler, FuseBuf, FuseSession, Result, Writer};

#[cfg(all(feature = "macfuse", not(feature = "virtiofs"), not(feature = "fusedev")))]
pub mod macfuse;
#[cfg(all(feature = "macfuse", not(feature = "virtiofs"), not(feature = "fusedev")))]
pub use self::macfuse::{Error, FsCacheReqHandler, FuseBuf, FuseSession, Result, Writer};

#[derive(Clone)]
struct IoBuffers<'a, S> {
    buffers: VecDeque<VolatileSlice<'a, S>>,
    bytes_consumed: usize,
}

impl<S: BitmapSlice> Default for IoBuffers<'_, S> {
    fn default() -> Self {
        IoBuffers {
            buffers: VecDeque::new(),
            bytes_consumed: 0,
        }
    }
}

impl<S: BitmapSlice> IoBuffers<'_, S> {
    fn available_bytes(&self) -> usize {
        // This is guaranteed not to overflow because the total length of the chain
        // is checked during all creations of `IoBuffers` (see
        // `Reader::new()` and `Writer::new()`).
        self.buffers
            .iter()
            .fold(0usize, |count, buf| count + buf.len() as usize)
    }

    fn bytes_consumed(&self) -> usize {
        self.bytes_consumed
    }

    fn allocate_volatile_slice(&self, count: usize) -> Vec<VolatileSlice<'_, S>> {
        let mut rem = count;
        let mut bufs = Vec::with_capacity(self.buffers.len());

        for buf in &self.buffers {
            if rem == 0 {
                break;
            }

            // If buffer contains more data than `rem`, truncate buffer to `rem`, otherwise
            // more data is written out and causes data corruption.
            let local_buf = if buf.len() > rem {
                // Safe because we just check rem < buf.len()
                buf.subslice(0, rem).unwrap()
            } else {
                buf.clone()
            };
            bufs.push(local_buf.clone());

            // Don't need check_sub() as we just made sure rem >= local_buf.len()
            rem -= local_buf.len() as usize;
        }

        bufs
    }

    #[cfg(feature = "async-io")]
    fn allocate_io_slice(&self, count: usize) -> Vec<IoSlice> {
        let mut rem = count;
        let mut bufs = Vec::with_capacity(self.buffers.len());

        for buf in &self.buffers {
            if rem == 0 {
                break;
            }

            // If buffer contains more data than `rem`, truncate buffer to `rem`, otherwise
            // more data is written out and causes data corruption.
            let local_buf = if buf.len() > rem {
                // Safe because we just check rem < buf.len()
                buf.subslice(0, rem).unwrap()
            } else {
                buf.clone()
            };
            // Safe because we just change the interface to access underlying buffers.
            bufs.push(IoSlice::new(unsafe {
                std::slice::from_raw_parts(local_buf.as_ptr(), local_buf.len())
            }));

            // Don't need check_sub() as we just made sure rem >= local_buf.len()
            rem -= local_buf.len() as usize;
        }

        bufs
    }

    #[cfg(all(feature = "async-io", feature = "virtiofs"))]
    fn allocate_mut_io_slice(&self, count: usize) -> Vec<IoSliceMut> {
        let mut rem = count;
        let mut bufs = Vec::with_capacity(self.buffers.len());

        for buf in &self.buffers {
            if rem == 0 {
                break;
            }

            // If buffer contains more data than `rem`, truncate buffer to `rem`, otherwise
            // more data is written out and causes data corruption.
            let local_buf = if buf.len() > rem {
                // Safe because we just check rem < buf.len()
                buf.subslice(0, rem).unwrap()
            } else {
                buf.clone()
            };
            // Safe because we just change the interface to access underlying buffers.
            bufs.push(IoSliceMut::new(unsafe {
                std::slice::from_raw_parts_mut(local_buf.as_ptr(), local_buf.len())
            }));

            // Don't need check_sub() as we just made sure rem >= local_buf.len()
            rem -= local_buf.len() as usize;
        }

        bufs
    }

    #[cfg(all(feature = "async-io", feature = "virtiofs"))]
    fn mark_dirty(&self, count: usize) {
        let mut rem = count;

        for buf in &self.buffers {
            if rem == 0 {
                break;
            }

            // If buffer contains more data than `rem`, truncate buffer to `rem`, otherwise
            // more data is written out and causes data corruption.
            let local_buf = if buf.len() > rem {
                // Safe because we just check rem < buf.len()
                buf.subslice(0, rem).unwrap()
            } else {
                buf.clone()
            };
            local_buf.bitmap().mark_dirty(0, local_buf.len());

            // Don't need check_sub() as we just made sure rem >= local_buf.len()
            rem -= local_buf.len() as usize;
        }
    }

    fn mark_used(&mut self, bytes_consumed: usize) -> io::Result<()> {
        // This can happen if a driver tricks a device into reading/writing more data than
        // fits in a `usize`.
        let total_bytes_consumed =
            self.bytes_consumed
                .checked_add(bytes_consumed)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, Error::DescriptorChainOverflow)
                })?;

        let mut rem = bytes_consumed;
        while let Some(buf) = self.buffers.pop_front() {
            if rem < buf.len() {
                // Split the slice and push the remainder back into the buffer list. Safe because we
                // know that `rem` is not out of bounds due to the check and we checked the bounds
                // on `buf` when we added it to the buffer list.
                self.buffers.push_front(buf.offset(rem).unwrap());
                break;
            }

            // No need for checked math because we know that `buf.size() <= rem`.
            rem -= buf.len();
        }

        self.bytes_consumed = total_bytes_consumed;

        Ok(())
    }

    /// Consumes at most `count` bytes from the `DescriptorChain`. Callers must provide a function
    /// that takes a `&[VolatileSlice]` and returns the total number of bytes consumed. This
    /// function guarantees that the combined length of all the slices in the `&[VolatileSlice]` is
    /// less than or equal to `count`.
    ///
    /// # Errors
    ///
    /// If the provided function returns any error then no bytes are consumed from the buffer and
    /// the error is returned to the caller.
    fn consume<F>(&mut self, count: usize, f: F) -> io::Result<usize>
    where
        F: FnOnce(&[VolatileSlice<'_, S>]) -> io::Result<usize>,
    {
        let bufs = self.allocate_volatile_slice(count);
        if bufs.is_empty() {
            Ok(0)
        } else {
            let bytes_consumed = f(&*bufs)?;
            self.mark_used(bytes_consumed)?;
            Ok(bytes_consumed)
        }
    }

    fn split_at(&mut self, offset: usize) -> Result<Self> {
        let mut rem = offset;
        let pos = self.buffers.iter().position(|buf| {
            if rem < buf.len() {
                true
            } else {
                rem -= buf.len();
                false
            }
        });

        if let Some(at) = pos {
            let mut other = self.buffers.split_off(at);

            if rem > 0 {
                // There must be at least one element in `other` because we checked
                // its `size` value in the call to `position` above.
                let front = other.pop_front().expect("empty VecDeque after split");
                self.buffers
                    .push_back(front.subslice(0, rem).map_err(Error::VolatileMemoryError)?);
                other.push_front(front.offset(rem).map_err(Error::VolatileMemoryError)?);
            }

            Ok(IoBuffers {
                buffers: other,
                bytes_consumed: 0,
            })
        } else if rem == 0 {
            Ok(IoBuffers {
                buffers: VecDeque::new(),
                bytes_consumed: 0,
            })
        } else {
            Err(Error::SplitOutOfBounds(offset))
        }
    }
}

/// Provides high-level interface over the sequence of memory regions
/// defined by readable descriptors in the descriptor chain.
///
/// Note that virtio spec requires driver to place any device-writable
/// descriptors after any device-readable descriptors (2.6.4.2 in Virtio Spec v1.1).
/// Reader will skip iterating over descriptor chain when first writable
/// descriptor is encountered.
#[derive(Clone)]
pub struct Reader<'a, S = ()> {
    buffers: IoBuffers<'a, S>,
}

impl<S: BitmapSlice> Default for Reader<'_, S> {
    fn default() -> Self {
        Reader {
            buffers: IoBuffers::default(),
        }
    }
}

impl<S: BitmapSlice> Reader<'_, S> {
    /// Reads an object from the descriptor chain buffer.
    pub fn read_obj<T: ByteValued>(&mut self) -> io::Result<T> {
        let mut obj = MaybeUninit::<T>::uninit();

        // Safe because `MaybeUninit` guarantees that the pointer is valid for
        // `size_of::<T>()` bytes.
        let buf = unsafe {
            ::std::slice::from_raw_parts_mut(obj.as_mut_ptr() as *mut u8, size_of::<T>())
        };

        self.read_exact(buf)?;

        // Safe because any type that implements `ByteValued` can be considered initialized
        // even if it is filled with random data.
        Ok(unsafe { obj.assume_init() })
    }

    /// Reads data from the descriptor chain buffer into a file descriptor.
    /// Returns the number of bytes read from the descriptor chain buffer.
    /// The number of bytes read can be less than `count` if there isn't
    /// enough data in the descriptor chain buffer.
    pub fn read_to<F: FileReadWriteVolatile<S>>(
        &mut self,
        mut dst: F,
        count: usize,
    ) -> io::Result<usize> {
        self.buffers
            .consume(count, |bufs| dst.write_vectored_volatile(bufs))
    }

    /// Reads data from the descriptor chain buffer into a File at offset `off`.
    /// Returns the number of bytes read from the descriptor chain buffer.
    /// The number of bytes read can be less than `count` if there isn't
    /// enough data in the descriptor chain buffer.
    pub fn read_to_at<F: FileReadWriteVolatile<S>>(
        &mut self,
        mut dst: F,
        count: usize,
        off: u64,
    ) -> io::Result<usize> {
        self.buffers
            .consume(count, |bufs| dst.write_vectored_at_volatile(bufs, off))
    }

    /// Reads exactly size of data from the descriptor chain buffer into a file descriptor.
    pub fn read_exact_to<F: FileReadWriteVolatile<S>>(
        &mut self,
        mut dst: F,
        mut count: usize,
    ) -> io::Result<()> {
        while count > 0 {
            match self.read_to(&mut dst, count) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    ))
                }
                Ok(n) => count -= n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }

    /// Returns number of bytes available for reading.  May return an error if the combined
    /// lengths of all the buffers in the DescriptorChain would cause an integer overflow.
    pub fn available_bytes(&self) -> usize {
        self.buffers.available_bytes()
    }

    /// Returns number of bytes already read from the descriptor chain buffer.
    pub fn bytes_read(&self) -> usize {
        self.buffers.bytes_consumed()
    }

    /// Splits this `Reader` into two at the given offset in the `DescriptorChain` buffer.
    /// After the split, `self` will be able to read up to `offset` bytes while the returned
    /// `Reader` can read up to `available_bytes() - offset` bytes.  Returns an error if
    /// `offset > self.available_bytes()`.
    pub fn split_at(&mut self, offset: usize) -> Result<Self> {
        self.buffers
            .split_at(offset)
            .map(|buffers| Reader { buffers })
    }
}

impl<S: BitmapSlice> io::Read for Reader<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.buffers.consume(buf.len(), |bufs| {
            let mut rem = buf;
            let mut total = 0;
            for buf in bufs {
                let copy_len = cmp::min(rem.len(), buf.len());

                // Safe because we have already verified that `buf` points to valid memory.
                unsafe {
                    copy_nonoverlapping(buf.as_ptr() as *const u8, rem.as_mut_ptr(), copy_len);
                }
                rem = &mut rem[copy_len..];
                total += copy_len;
            }
            Ok(total)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::transport::IoBuffers;
    use std::collections::VecDeque;
    use vm_memory::VolatileSlice;

    #[test]
    fn test_io_buffers() {
        let mut buf1 = vec![0x0u8; 16];
        let mut buf2 = vec![0x0u8; 16];
        let mut bufs = VecDeque::new();
        unsafe {
            bufs.push_back(VolatileSlice::new(buf1.as_mut_ptr(), buf1.len()));
            bufs.push_back(VolatileSlice::new(buf2.as_mut_ptr(), buf2.len()));
        }
        let mut buffers = IoBuffers {
            buffers: bufs,
            bytes_consumed: 0,
        };

        assert_eq!(buffers.available_bytes(), 32);
        assert_eq!(buffers.bytes_consumed(), 0);

        assert_eq!(buffers.consume(2, |buf| Ok(buf[0].len())).unwrap(), 2);
        assert_eq!(buffers.available_bytes(), 30);
        assert_eq!(buffers.bytes_consumed(), 2);

        let mut buffers2 = buffers.split_at(10).unwrap();
        assert_eq!(buffers.available_bytes(), 10);
        assert_eq!(buffers.bytes_consumed(), 2);
        assert_eq!(buffers2.available_bytes(), 20);
        assert_eq!(buffers2.bytes_consumed(), 0);

        assert_eq!(
            buffers2
                .consume(10, |buf| Ok(buf[0].len() + buf[1].len()))
                .unwrap(),
            10
        );
        assert_eq!(buffers2.consume(20, |buf| Ok(buf[0].len())).unwrap(), 10);

        let _buffers3 = buffers2.split_at(0).unwrap();
        assert!(buffers2.split_at(1).is_err());
    }
}
