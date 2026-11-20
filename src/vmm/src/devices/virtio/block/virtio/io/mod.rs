// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod async_io;
pub mod format;
pub mod sync_io;

use std::fmt::Debug;
use std::fs::File;

pub use self::async_io::{AsyncFileEngine, AsyncIoError};
pub use self::format::{DiskImageFormat, VmdkFileEngine, VmdkIoError};
pub use self::sync_io::{SyncFileEngine, SyncIoError};
use crate::devices::virtio::block::virtio::PendingRequest;
use crate::devices::virtio::block::virtio::device::FileEngineType;
use crate::utils::u64_to_usize;
use crate::vstate::memory::{GuestAddress, GuestMemoryExtension, GuestMemoryMmap};

/// One data segment of a request: guest address and length.
pub type DataSegment = (GuestAddress, u32);

/// The most bytes a request of several data segments may carry: what Linux transfers in one
/// read or write (`MAX_RW_COUNT` with 4 KiB pages). It also fits the signed result of an
/// io_uring completion.
pub const MAX_VECTORED_BYTES: u32 = 0x7fff_f000;

/// The most `iovec`s one vectored system call or io_uring operation takes (`UIO_MAXIOV`).
const MAX_IOVECS: usize = 1024;

/// The host ranges of `segments`, in order and without the empty ones. Fails, before anything
/// is transferred, if a segment is not inside one region of guest memory.
fn segment_iovecs(
    mem: &GuestMemoryMmap,
    segments: &[DataSegment],
) -> Result<Vec<libc::iovec>, vm_memory::GuestMemoryError> {
    use vm_memory::GuestMemoryBackend;

    segments
        .iter()
        .filter(|&&(_, len)| len != 0)
        .map(|&(addr, len)| {
            let len = u64_to_usize(u64::from(len));
            let slice = mem.get_slice(addr, len)?;
            Ok(libc::iovec {
                iov_base: slice.ptr_guard_mut().as_ptr().cast(),
                iov_len: len,
            })
        })
        .collect()
}

/// Drops the first `done` bytes from the ranges of `iovecs`.
fn advance_iovecs(iovecs: &mut Vec<libc::iovec>, mut done: usize) {
    let mut whole = 0;
    for iov in iovecs.iter_mut() {
        if done < iov.iov_len {
            iov.iov_base = iov.iov_base.cast::<u8>().wrapping_add(done).cast();
            iov.iov_len -= done;
            break;
        }
        done -= iov.iov_len;
        whole += 1;
    }
    iovecs.drain(..whole);
}

/// Marks dirty `len` bytes of the data that `segments` describe in order, starting `from` bytes
/// into it.
fn mark_dirty_range(mem: &GuestMemoryMmap, segments: &[DataSegment], from: u32, len: u32) {
    let mut skip = u64::from(from);
    let mut left = u64::from(len);
    for &(addr, segment_len) in segments {
        if left == 0 {
            break;
        }
        let segment_len = u64::from(segment_len);
        if skip >= segment_len {
            skip -= segment_len;
            continue;
        }
        let count = (segment_len - skip).min(left);
        mem.mark_dirty(GuestAddress(addr.0 + skip), u64_to_usize(count));
        left -= count;
        skip = 0;
    }
}

/// The total length of `segments`.
fn segments_len(segments: &[DataSegment]) -> u64 {
    segments.iter().map(|&(_, len)| u64::from(len)).sum()
}

#[derive(Debug)]
pub struct RequestOk {
    pub req: PendingRequest,
    pub count: u32,
}

#[derive(Debug)]
pub enum FileEngineOk {
    Submitted,
    Executed(RequestOk),
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum BlockIoError {
    /// Sync error: {0}
    Sync(SyncIoError),
    /// Async error: {0}
    Async(AsyncIoError),
    /// VMDK error: {0}
    Vmdk(VmdkIoError),
    /// A request of several data segments has a length that cannot be transferred: {0} bytes
    VectoredLength(u64),
}

impl BlockIoError {
    /// Bytes known to have been transferred before the error.
    pub fn progress(&self) -> u32 {
        match self {
            BlockIoError::Sync(SyncIoError::Vectored(_, done)) => *done,
            _ => 0,
        }
    }

    pub fn is_throttling_err(&self) -> bool {
        match self {
            BlockIoError::Async(AsyncIoError::IoUring(err)) => err.is_throttling_err(),
            _ => false,
        }
    }
}

#[derive(Debug)]
pub struct RequestError<E> {
    pub req: PendingRequest,
    pub error: E,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum FileEngine {
    #[allow(unused)]
    Async(AsyncFileEngine),
    Sync(SyncFileEngine),
    Vmdk(VmdkFileEngine),
}

impl From<&FileEngine> for DiskImageFormat {
    fn from(engine: &FileEngine) -> Self {
        match engine {
            FileEngine::Vmdk(_) => DiskImageFormat::Vmdk,
            FileEngine::Async(_) | FileEngine::Sync(_) => DiskImageFormat::Raw,
        }
    }
}

impl FileEngine {
    pub fn from_file(
        file: File,
        engine_type: FileEngineType,
        discard: bool,
    ) -> Result<FileEngine, BlockIoError> {
        match engine_type {
            FileEngineType::Async => Ok(FileEngine::Async(
                AsyncFileEngine::from_file(file, discard).map_err(BlockIoError::Async)?,
            )),
            FileEngineType::Sync => Ok(FileEngine::Sync(SyncFileEngine::from_file(file))),
        }
    }

    pub fn update_file_path(&mut self, file: File) -> Result<(), BlockIoError> {
        match self {
            FileEngine::Async(engine) => engine.update_file(file).map_err(BlockIoError::Async)?,
            FileEngine::Sync(engine) => engine.update_file(file),
            FileEngine::Vmdk(_) => {
                return Err(BlockIoError::Vmdk(VmdkIoError::UnsupportedOperation));
            }
        };

        Ok(())
    }

    pub fn file(&self) -> &File {
        match self {
            FileEngine::Async(engine) => engine.file(),
            FileEngine::Sync(engine) => engine.file(),
            FileEngine::Vmdk(_) => {
                panic!("file() is not supported for VMDK engine")
            }
        }
    }

    pub fn read(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        req: PendingRequest,
    ) -> Result<FileEngineOk, RequestError<BlockIoError>> {
        match self {
            FileEngine::Async(engine) => match engine.push_read(offset, mem, addr, count, req) {
                Ok(_) => Ok(FileEngineOk::Submitted),
                Err(err) => Err(RequestError {
                    req: err.req,
                    error: BlockIoError::Async(err.error),
                }),
            },
            FileEngine::Sync(engine) => match engine.read(offset, mem, addr, count) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk { req, count })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Sync(err),
                }),
            },
            FileEngine::Vmdk(engine) => match engine.read(offset, mem, addr, count) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk { req, count })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Vmdk(err),
                }),
            },
        }
    }

    pub fn write(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        req: PendingRequest,
    ) -> Result<FileEngineOk, RequestError<BlockIoError>> {
        match self {
            FileEngine::Async(engine) => match engine.push_write(offset, mem, addr, count, req) {
                Ok(_) => Ok(FileEngineOk::Submitted),
                Err(err) => Err(RequestError {
                    req: err.req,
                    error: BlockIoError::Async(err.error),
                }),
            },
            FileEngine::Sync(engine) => match engine.write(offset, mem, addr, count) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk { req, count })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Sync(err),
                }),
            },
            FileEngine::Vmdk(engine) => match engine.write(offset, mem, addr, count) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk { req, count })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Vmdk(err),
                }),
            },
        }
    }

    /// Reads a request of several data segments: `count` bytes at `offset` into `segments`, in
    /// order. Every segment is checked to be guest memory before anything is read.
    pub fn readv(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        segments: Vec<DataSegment>,
        count: u32,
        req: PendingRequest,
    ) -> Result<FileEngineOk, RequestError<BlockIoError>> {
        let total = segments_len(&segments);
        if total != u64::from(count) || count > MAX_VECTORED_BYTES {
            return Err(RequestError {
                req,
                error: BlockIoError::VectoredLength(total),
            });
        }
        match self {
            FileEngine::Async(engine) => match engine.push_readv(offset, mem, segments, req) {
                Ok(_) => Ok(FileEngineOk::Submitted),
                Err(err) => Err(RequestError {
                    req: err.req,
                    error: BlockIoError::Async(err.error),
                }),
            },
            FileEngine::Sync(engine) => match engine.readv(offset, mem, &segments) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk { req, count })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Sync(err),
                }),
            },
            FileEngine::Vmdk(engine) => {
                // The image is read extent by extent anyway: one segment at a time.
                let mut done: u32 = 0;
                for &(addr, len) in &segments {
                    match engine.read(offset + u64::from(done), mem, addr, len) {
                        Ok(read) => done += read,
                        Err(err) => {
                            return Err(RequestError {
                                req,
                                error: BlockIoError::Vmdk(err),
                            });
                        }
                    }
                }
                Ok(FileEngineOk::Executed(RequestOk { req, count: done }))
            }
        }
    }

    /// Writes a request of several data segments: `count` bytes from `segments`, in order, at
    /// `offset`. Every segment is checked to be guest memory before anything is written.
    pub fn writev(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        segments: Vec<DataSegment>,
        count: u32,
        req: PendingRequest,
    ) -> Result<FileEngineOk, RequestError<BlockIoError>> {
        let total = segments_len(&segments);
        if total != u64::from(count) || count > MAX_VECTORED_BYTES {
            return Err(RequestError {
                req,
                error: BlockIoError::VectoredLength(total),
            });
        }
        match self {
            FileEngine::Async(engine) => match engine.push_writev(offset, mem, segments, req) {
                Ok(_) => Ok(FileEngineOk::Submitted),
                Err(err) => Err(RequestError {
                    req: err.req,
                    error: BlockIoError::Async(err.error),
                }),
            },
            FileEngine::Sync(engine) => match engine.writev(offset, mem, &segments) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk { req, count })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Sync(err),
                }),
            },
            // A VMDK image is read-only.
            FileEngine::Vmdk(_) => Err(RequestError {
                req,
                error: BlockIoError::Vmdk(VmdkIoError::WriteNotSupported),
            }),
        }
    }

    pub fn flush(
        &mut self,
        req: PendingRequest,
    ) -> Result<FileEngineOk, RequestError<BlockIoError>> {
        match self {
            FileEngine::Async(engine) => match engine.push_flush(req) {
                Ok(_) => Ok(FileEngineOk::Submitted),
                Err(err) => Err(RequestError {
                    req: err.req,
                    error: BlockIoError::Async(err.error),
                }),
            },
            FileEngine::Sync(engine) => match engine.flush() {
                Ok(_) => Ok(FileEngineOk::Executed(RequestOk { req, count: 0 })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Sync(err),
                }),
            },
            FileEngine::Vmdk(engine) => match engine.flush() {
                Ok(_) => Ok(FileEngineOk::Executed(RequestOk { req, count: 0 })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Vmdk(err),
                }),
            },
        }
    }

    pub fn discard(
        &mut self,
        range: (u64, u32),
        req: PendingRequest,
    ) -> Result<FileEngineOk, RequestError<BlockIoError>> {
        match self {
            FileEngine::Async(engine) => match engine.push_discard(range, req) {
                Ok(_) => Ok(FileEngineOk::Submitted),
                Err(err) => Err(RequestError {
                    req: err.req,
                    error: BlockIoError::Async(err.error),
                }),
            },
            FileEngine::Sync(engine) => match engine.discard(range) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk { req, count })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Sync(err),
                }),
            },
            FileEngine::Vmdk(_) => Err(RequestError {
                req,
                error: BlockIoError::Vmdk(VmdkIoError::WriteNotSupported),
            }),
        }
    }

    pub fn drain(&mut self, discard: bool) -> Result<(), BlockIoError> {
        match self {
            FileEngine::Async(engine) => engine.drain(discard).map_err(BlockIoError::Async),
            FileEngine::Sync(_engine) => Ok(()),
            FileEngine::Vmdk(_engine) => Ok(()),
        }
    }

    pub fn drain_and_flush(&mut self, discard: bool) -> Result<(), BlockIoError> {
        match self {
            FileEngine::Async(engine) => {
                engine.drain_and_flush(discard).map_err(BlockIoError::Async)
            }
            FileEngine::Sync(engine) => engine.flush().map_err(BlockIoError::Sync),
            FileEngine::Vmdk(engine) => engine.flush().map_err(BlockIoError::Vmdk),
        }
    }
}

#[cfg(test)]
pub mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]
    use std::os::linux::fs::MetadataExt;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileExt;

    use vm_memory::{GuestMemoryBackend, GuestMemoryError, GuestMemoryRegion};
    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::devices::virtio::block::virtio::device::FileEngineType;
    use crate::devices::virtio::block::virtio::io::format::vmdk::tests::create_test_vmdk;
    use crate::utils::u64_to_usize;
    use crate::vmm_config::machine_config::HugePageConfig;
    use crate::vstate::memory;
    use crate::vstate::memory::{Bitmap, Bytes, GuestRegionMmapExt};

    const FILE_LEN: u32 = 1024;
    // 2 pages of memory should be enough to test read/write ops and also dirty tracking.
    const MEM_LEN: usize = 8192;

    macro_rules! assert_sync_execution {
        ($expression:expr, $count:expr) => {
            match $expression {
                Ok(FileEngineOk::Executed(RequestOk { req: _, count })) => {
                    assert_eq!(count, $count)
                }
                other => panic!(
                    "Expected: Ok(FileEngineOk::Executed(UserDataOk {{ user_data: _, count: {} \
                     }})), got: {:?}",
                    $count, other
                ),
            }
        };
    }

    macro_rules! assert_queued {
        ($expression:expr) => {
            assert!(matches!($expression, Ok(FileEngineOk::Submitted)))
        };
    }

    fn assert_async_execution(mem: &GuestMemoryMmap, engine: &mut FileEngine, count: u32) {
        if let FileEngine::Async(engine) = engine {
            engine.drain(false).unwrap();
            assert_eq!(engine.pop(mem).unwrap().unwrap().result().unwrap(), count);
        }
    }

    fn create_mem() -> GuestMemoryMmap {
        GuestMemoryMmap::from_regions(
            memory::anonymous(&[(GuestAddress(0), MEM_LEN)], true, HugePageConfig::None)
                .unwrap()
                .into_iter()
                .map(|region| GuestRegionMmapExt::dram_from_mmap_region(region, 0))
                .collect(),
        )
        .unwrap()
    }

    fn check_dirty_mem(mem: &GuestMemoryMmap, addr: GuestAddress, len: u32) {
        let bitmap = mem.find_region(addr).unwrap().bitmap();
        for offset in addr.0..addr.0 + u64::from(len) {
            assert!(bitmap.dirty_at(u64_to_usize(offset)));
        }
    }

    fn check_clean_mem(mem: &GuestMemoryMmap, addr: GuestAddress, len: u32) {
        let bitmap = mem.find_region(addr).unwrap().bitmap();
        for offset in addr.0..addr.0 + u64::from(len) {
            assert!(!bitmap.dirty_at(u64_to_usize(offset)));
        }
    }

    #[test]
    fn test_sync() {
        let mem = create_mem();
        // Create backing file.
        let file = TempFile::new().unwrap().into_file();
        let mut engine = FileEngine::from_file(file, FileEngineType::Sync, false).unwrap();

        let data = vmm_sys_util::rand::rand_alphanumerics(FILE_LEN as usize)
            .as_bytes()
            .to_vec();

        // Partial write
        let partial_len = 50;
        let addr = GuestAddress(MEM_LEN as u64 - u64::from(partial_len));
        mem.write(&data, addr).unwrap();
        assert_sync_execution!(
            engine.write(0, &mem, addr, partial_len, PendingRequest::default()),
            partial_len
        );
        // Partial read
        let mem = create_mem();
        assert_sync_execution!(
            engine.read(0, &mem, addr, partial_len, PendingRequest::default()),
            partial_len
        );
        // Check data
        let mut buf = vec![0u8; partial_len as usize];
        mem.read_slice(&mut buf, addr).unwrap();
        assert_eq!(buf, data[..partial_len as usize]);

        // Offset write
        let offset = 100;
        let partial_len = 50;
        let addr = GuestAddress(0);
        mem.write(&data, addr).unwrap();
        assert_sync_execution!(
            engine.write(offset, &mem, addr, partial_len, PendingRequest::default()),
            partial_len
        );
        // Offset read
        let mem = create_mem();
        assert_sync_execution!(
            engine.read(offset, &mem, addr, partial_len, PendingRequest::default()),
            partial_len
        );
        // Check data
        let mut buf = vec![0u8; partial_len as usize];
        mem.read_slice(&mut buf, addr).unwrap();
        assert_eq!(buf, data[..partial_len as usize]);

        // Full write
        mem.write(&data, GuestAddress(0)).unwrap();
        assert_sync_execution!(
            engine.write(
                0,
                &mem,
                GuestAddress(0),
                FILE_LEN,
                PendingRequest::default()
            ),
            FILE_LEN
        );
        // Full read
        let mem = create_mem();
        assert_sync_execution!(
            engine.read(
                0,
                &mem,
                GuestAddress(0),
                FILE_LEN,
                PendingRequest::default()
            ),
            FILE_LEN
        );
        // Check data
        let mut buf = vec![0u8; FILE_LEN as usize];
        mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, data.as_slice());

        // Check other ops
        engine.flush(PendingRequest::default()).unwrap();
        engine.drain(true).unwrap();
        engine.drain_and_flush(true).unwrap();
    }

    #[test]
    fn test_sync_dirty_tracking() {
        let temp_file = TempFile::new().unwrap();
        // A second, write-only handle on the same file, for the failing read below.
        let write_only = std::fs::OpenOptions::new()
            .write(true)
            .open(temp_file.as_path())
            .unwrap();
        let mut engine =
            FileEngine::from_file(temp_file.into_file(), FileEngineType::Sync, false).unwrap();
        let data = vmm_sys_util::rand::rand_alphanumerics(FILE_LEN as usize)
            .as_bytes()
            .to_vec();
        let mem = create_mem();
        mem.write(&data, GuestAddress(0)).unwrap();
        assert_sync_execution!(
            engine.write(
                0,
                &mem,
                GuestAddress(0),
                FILE_LEN,
                PendingRequest::default()
            ),
            FILE_LEN
        );

        // A read into the second page marks that page and not the first.
        let mem = create_mem();
        let addr = GuestAddress(4096);
        assert_sync_execution!(
            engine.read(0, &mem, addr, FILE_LEN, PendingRequest::default()),
            FILE_LEN
        );
        check_clean_mem(&mem, GuestAddress(0), 4096);
        check_dirty_mem(&mem, addr, FILE_LEN);

        // A read past the end of the file fails after reading what there is, and marks only
        // that: the file ends in the first page, so the second page stays clean.
        let mem = create_mem();
        let too_long = FILE_LEN + 4096;
        match engine.read(
            0,
            &mem,
            GuestAddress(0),
            too_long,
            PendingRequest::default(),
        ) {
            Err(err) => assert!(matches!(
                err.error,
                BlockIoError::Sync(SyncIoError::Transfer(GuestMemoryError::IOError(_)))
            )),
            Ok(_) => panic!("read past the end of the file succeeded"),
        }
        let mut buf = vec![0u8; FILE_LEN as usize];
        mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, data.as_slice());
        check_dirty_mem(&mem, GuestAddress(0), FILE_LEN);
        check_clean_mem(&mem, GuestAddress(4096), 4096);

        // A `read` that fails outright writes nothing, so nothing is marked: on the write-only
        // handle `read` fails with EBADF on the first call.
        let mut engine = FileEngine::from_file(write_only, FileEngineType::Sync, false).unwrap();
        let mem = create_mem();
        match engine.read(
            0,
            &mem,
            GuestAddress(0),
            2 * 4096,
            PendingRequest::default(),
        ) {
            Err(err) => assert!(matches!(
                err.error,
                BlockIoError::Sync(SyncIoError::Transfer(GuestMemoryError::IOError(_)))
            )),
            Ok(_) => panic!("read from a write-only file succeeded"),
        }
        check_clean_mem(&mem, GuestAddress(0), 2 * 4096);
    }

    #[test]
    fn test_async() {
        // Create backing file.
        let file = TempFile::new().unwrap().into_file();
        let mut engine = FileEngine::from_file(file, FileEngineType::Async, false).unwrap();

        let data = vmm_sys_util::rand::rand_alphanumerics(FILE_LEN as usize)
            .as_bytes()
            .to_vec();

        // Partial reads and writes cannot really be tested because io_uring will return an error
        // code for trying to write to unmapped memory.

        // Offset write
        let mem = create_mem();
        let offset = 100;
        let partial_len = 50;
        let addr = GuestAddress(0);
        mem.write(&data, addr).unwrap();
        assert_queued!(engine.write(offset, &mem, addr, partial_len, PendingRequest::default()));
        assert_async_execution(&mem, &mut engine, partial_len);
        // Offset read
        let mem = create_mem();
        assert_queued!(engine.read(offset, &mem, addr, partial_len, PendingRequest::default()));
        assert_async_execution(&mem, &mut engine, partial_len);
        // Check data
        let mut buf = vec![0u8; partial_len as usize];
        mem.read_slice(&mut buf, addr).unwrap();
        assert_eq!(buf, data[..partial_len as usize]);
        // check dirty mem
        check_dirty_mem(&mem, addr, partial_len);
        check_clean_mem(&mem, GuestAddress(4096), 4096);

        // Full write
        mem.write(&data, GuestAddress(0)).unwrap();
        assert_queued!(engine.write(0, &mem, addr, FILE_LEN, PendingRequest::default()));
        assert_async_execution(&mem, &mut engine, FILE_LEN);

        // Full read
        let mem = create_mem();
        assert_queued!(engine.read(0, &mem, addr, FILE_LEN, PendingRequest::default()));
        assert_async_execution(&mem, &mut engine, FILE_LEN);
        // Check data
        let mut buf = vec![0u8; FILE_LEN as usize];
        mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, data.as_slice());
        // check dirty mem
        check_dirty_mem(&mem, addr, FILE_LEN);
        check_clean_mem(&mem, GuestAddress(4096), 4096);

        // Check other ops
        assert_queued!(engine.flush(PendingRequest::default()));
        assert_async_execution(&mem, &mut engine, 0);

        engine.drain(true).unwrap();
        engine.drain_and_flush(true).unwrap();
    }

    #[test]
    fn test_async_discard() {
        const LEN: u32 = 1 << 20;

        let mem = create_mem();
        let file = TempFile::new().unwrap().into_file();
        file.write_all_at(&vec![0xab; LEN as usize], 0).unwrap();
        file.sync_all().unwrap();
        let blocks_before = file.metadata().unwrap().st_blocks();

        // Without discard enabled the engine has no discard operation.
        let mut engine =
            FileEngine::from_file(file.try_clone().unwrap(), FileEngineType::Async, false).unwrap();
        assert!(matches!(
            engine
                .discard((0, LEN), PendingRequest::default())
                .unwrap_err()
                .error,
            BlockIoError::Async(AsyncIoError::NotImplemented)
        ));

        let mut engine = FileEngine::from_file(file, FileEngineType::Async, true).unwrap();
        assert_queued!(engine.discard((0, LEN), PendingRequest::default()));
        let FileEngine::Async(async_engine) = &mut engine else {
            unreachable!()
        };
        async_engine.drain(false).unwrap();
        match async_engine.pop(&mem).unwrap().unwrap().result() {
            Ok(count) => assert_eq!(count, 0),
            // Hole punching is not supported by every filesystem.
            Err(err) if err.raw_os_error() == Some(-libc::EOPNOTSUPP) => return,
            Err(err) => panic!("discard failed: {err}"),
        }

        let metadata = engine.file().metadata().unwrap();
        assert_eq!(metadata.len(), u64::from(LEN));
        assert!(metadata.st_blocks() < blocks_before);
    }

    #[test]
    fn test_vmdk() {
        let (_dir, descriptor) = create_test_vmdk("extent", 0);
        let mut engine = FileEngine::Vmdk(VmdkFileEngine::from_path(&descriptor).unwrap());
        let mem = create_mem();

        assert_sync_execution!(
            engine.read(0, &mem, GuestAddress(0), 512, PendingRequest::default()),
            512
        );
        assert!(matches!(
            engine
                .read(
                    0,
                    &mem,
                    GuestAddress(MEM_LEN as u64),
                    512,
                    PendingRequest::default()
                )
                .unwrap_err()
                .error,
            BlockIoError::Vmdk(VmdkIoError::GuestMemory(_))
        ));
        assert!(matches!(
            engine
                .write(0, &mem, GuestAddress(0), 512, PendingRequest::default())
                .unwrap_err()
                .error,
            BlockIoError::Vmdk(VmdkIoError::WriteNotSupported)
        ));
        assert_sync_execution!(engine.flush(PendingRequest::default()), 0);
        assert!(matches!(
            engine
                .discard((0, 512), PendingRequest::default())
                .unwrap_err()
                .error,
            BlockIoError::Vmdk(VmdkIoError::WriteNotSupported)
        ));
        engine.drain(true).unwrap();
        engine.drain_and_flush(true).unwrap();
        assert!(matches!(
            engine.update_file_path(File::open(descriptor).unwrap()),
            Err(BlockIoError::Vmdk(VmdkIoError::UnsupportedOperation))
        ));
    }

    fn create_mem_pages(pages: usize) -> GuestMemoryMmap {
        GuestMemoryMmap::from_regions(
            memory::anonymous(
                &[(GuestAddress(0), pages * 4096)],
                true,
                HugePageConfig::None,
            )
            .unwrap()
            .into_iter()
            .map(|region| GuestRegionMmapExt::dram_from_mmap_region(region, 0))
            .collect(),
        )
        .unwrap()
    }

    #[test]
    fn test_advance_iovecs() {
        let mut buf = [0u8; 64];
        let base = buf.as_mut_ptr();
        let mut iovecs = vec![
            libc::iovec {
                iov_base: base.cast(),
                iov_len: 16,
            },
            libc::iovec {
                iov_base: base.wrapping_add(32).cast(),
                iov_len: 16,
            },
        ];
        // Nothing done changes nothing.
        advance_iovecs(&mut iovecs, 0);
        assert_eq!(iovecs.len(), 2);
        // The whole first range and part of the second.
        advance_iovecs(&mut iovecs, 20);
        assert_eq!(iovecs.len(), 1);
        assert_eq!(iovecs[0].iov_base, base.wrapping_add(36).cast());
        assert_eq!(iovecs[0].iov_len, 12);
        // More than is left empties it.
        advance_iovecs(&mut iovecs, 100);
        assert!(iovecs.is_empty());
    }

    #[test]
    fn test_mark_dirty_range() {
        let mem = create_mem_pages(4);
        let segments = [
            (GuestAddress(0), 512),
            (GuestAddress(4096), 512),
            (GuestAddress(2 * 4096), 512),
            (GuestAddress(3 * 4096), 512),
        ];
        // Bytes 600..1100 of the data are in the second and third segments only.
        mark_dirty_range(&mem, &segments, 600, 500);
        check_clean_mem(&mem, GuestAddress(0), 512);
        check_dirty_mem(&mem, GuestAddress(4096), 512);
        check_dirty_mem(&mem, GuestAddress(2 * 4096), 512);
        check_clean_mem(&mem, GuestAddress(3 * 4096), 512);
        // Nothing, and a range past the end of the segments, mark nothing more.
        mark_dirty_range(&mem, &segments, 0, 0);
        mark_dirty_range(&mem, &segments, 2048, 512);
        check_clean_mem(&mem, GuestAddress(0), 512);
        check_clean_mem(&mem, GuestAddress(3 * 4096), 512);
    }

    // A file that ends inside a request of several segments: both engines stop at its end,
    // report the bytes read and mark dirty the pages those went to and no other.
    #[test]
    fn test_vectored_read_past_end_of_file() {
        for engine_type in [FileEngineType::Sync, FileEngineType::Async] {
            let mem = create_mem_pages(3);
            let file = TempFile::new().unwrap().into_file();
            file.write_all_at(&[0x5a; 768], 0).unwrap();
            let mut engine = FileEngine::from_file(file, engine_type, false).unwrap();
            let segments = vec![
                (GuestAddress(0), 512),
                (GuestAddress(4096), 512),
                (GuestAddress(2 * 4096), 512),
            ];
            let res = engine.readv(0, &mem, segments, 1536, PendingRequest::default());
            match &mut engine {
                FileEngine::Async(engine) => {
                    assert_queued!(res);
                    engine.drain(false).unwrap();
                    // Fewer bytes than the request has: the device fails such a request.
                    assert_eq!(engine.pop(&mem).unwrap().unwrap().result().unwrap(), 768);
                    assert!(!engine.has_pending_ops());
                }
                _ => {
                    let err = res.unwrap_err();
                    assert!(matches!(
                        err.error,
                        BlockIoError::Sync(SyncIoError::Vectored(_, 768))
                    ));
                    assert_eq!(err.error.progress(), 768);
                }
            }
            let mut read = [0u8; 256];
            mem.read_slice(&mut read, GuestAddress(4096)).unwrap();
            assert_eq!(read, [0x5a; 256]);
            check_dirty_mem(&mem, GuestAddress(0), 512);
            check_dirty_mem(&mem, GuestAddress(4096), 256);
            check_clean_mem(&mem, GuestAddress(2 * 4096), 512);
        }
    }

    // A segment that is not guest memory, in the middle of a request: the request is refused
    // before anything is transferred, so a write leaves the file as it was and a read leaves
    // the other segments untouched and clean.
    #[test]
    fn test_vectored_refused_before_any_io() {
        for engine_type in [FileEngineType::Sync, FileEngineType::Async] {
            for bad in [
                // Outside guest memory.
                (GuestAddress(3 * 4096), 512),
                // Starts inside it and runs past its end.
                (GuestAddress(3 * 4096 - 256), 512),
            ] {
                let mem = create_mem_pages(3);
                mem.write_slice(&[0x11; 512], GuestAddress(0)).unwrap();
                mem.reset_dirty();
                let file = TempFile::new().unwrap().into_file();
                file.write_all_at(&[0x22; 1536], 0).unwrap();
                let mut engine = FileEngine::from_file(file, engine_type, false).unwrap();
                let segments = vec![(GuestAddress(0), 512), bad, (GuestAddress(4096), 512)];

                let err = engine
                    .writev(0, &mem, segments.clone(), 1536, PendingRequest::default())
                    .unwrap_err();
                assert!(matches!(
                    err.error,
                    BlockIoError::Sync(SyncIoError::Transfer(_))
                        | BlockIoError::Async(AsyncIoError::GuestMemory(_))
                ));
                let mut on_disk = [0u8; 1536];
                engine.file().read_exact_at(&mut on_disk, 0).unwrap();
                assert_eq!(on_disk, [0x22; 1536]);

                let err = engine
                    .readv(0, &mem, segments, 1536, PendingRequest::default())
                    .unwrap_err();
                assert_eq!(err.error.progress(), 0);
                let mut first = [0u8; 512];
                mem.read_slice(&mut first, GuestAddress(0)).unwrap();
                assert_eq!(first, [0x11; 512]);
                check_clean_mem(&mem, GuestAddress(0), 512);
                check_clean_mem(&mem, GuestAddress(4096), 512);

                if let FileEngine::Async(engine) = &engine {
                    assert!(!engine.has_pending_ops());
                }
            }
        }
    }

    // A request longer than one transfer moves, or whose length is not its segments', is
    // refused before its segments are looked at.
    #[test]
    fn test_vectored_length_limit() {
        for engine_type in [FileEngineType::Sync, FileEngineType::Async] {
            let mem = create_mem_pages(2);
            let file = TempFile::new().unwrap().into_file();
            let mut engine = FileEngine::from_file(file, engine_type, false).unwrap();

            let too_long = vec![(GuestAddress(0), 0x4000_0000); 2];
            let err = engine
                .writev(
                    0,
                    &mem,
                    too_long.clone(),
                    0x8000_0000,
                    PendingRequest::default(),
                )
                .unwrap_err();
            assert!(matches!(
                err.error,
                BlockIoError::VectoredLength(0x8000_0000)
            ));
            let err = engine
                .readv(0, &mem, too_long, 0x8000_0000, PendingRequest::default())
                .unwrap_err();
            assert!(matches!(
                err.error,
                BlockIoError::VectoredLength(0x8000_0000)
            ));

            let segments = vec![(GuestAddress(0), 512), (GuestAddress(4096), 512)];
            let err = engine
                .readv(0, &mem, segments, 512, PendingRequest::default())
                .unwrap_err();
            assert!(matches!(err.error, BlockIoError::VectoredLength(1024)));
        }
    }

    // A multi-segment write then read, on both engines: the file gets the segments in order,
    // the read puts them back in other scattered segments and marks exactly those dirty.
    #[test]
    fn test_vectored_read_write() {
        const MEM: usize = 16 * 4096;
        for engine_type in [FileEngineType::Sync, FileEngineType::Async] {
            let mem = create_mem_pages(16);
            let file = TempFile::new().unwrap().into_file();
            file.set_len(MEM as u64).unwrap();
            let check = file.try_clone().unwrap();
            let mut engine = FileEngine::from_file(file, engine_type, false).unwrap();

            // Three scattered source segments, odd lengths that add up to whole sectors.
            let src: Vec<DataSegment> = vec![
                (GuestAddress(0x3000), 4096),
                (GuestAddress(0x1000), 100),
                (GuestAddress(0x7010), 412),
            ];
            let mut expected = Vec::new();
            for (i, &(addr, len)) in src.iter().enumerate() {
                let bytes: Vec<u8> = (0..len)
                    .map(|b| u8::try_from(b % 256).unwrap() ^ u8::try_from(i * 0x55).unwrap())
                    .collect();
                mem.write_slice(&bytes, addr).unwrap();
                expected.extend(bytes);
            }
            let total = u32::try_from(expected.len()).unwrap();
            let res = engine.writev(512, &mem, src, total, PendingRequest::default());
            match engine_type {
                FileEngineType::Sync => assert_sync_execution!(res, total),
                FileEngineType::Async => {
                    assert_queued!(res);
                    assert_async_execution(&mem, &mut engine, total);
                }
            }
            let mut on_disk = vec![0u8; expected.len()];
            check.read_exact_at(&mut on_disk, 512).unwrap();
            assert_eq!(on_disk, expected);

            // Read back into two other segments, from a clean bitmap.
            mem.reset_dirty();
            let dst: Vec<DataSegment> = vec![
                (GuestAddress(0xa000), 2048),
                (GuestAddress(0xc000), total - 2048),
            ];
            let res = engine.readv(512, &mem, dst.clone(), total, PendingRequest::default());
            match engine_type {
                FileEngineType::Sync => assert_sync_execution!(res, total),
                FileEngineType::Async => {
                    assert_queued!(res);
                    assert_async_execution(&mem, &mut engine, total);
                }
            }
            let mut got = Vec::new();
            for &(addr, len) in &dst {
                let mut part = vec![0u8; len as usize];
                mem.read_slice(&mut part, addr).unwrap();
                got.extend(part);
                check_dirty_mem(&mem, addr, len);
            }
            assert_eq!(got, expected);
            check_clean_mem(&mem, GuestAddress(0x3000), 4096);

            // A segment outside guest memory fails the request without touching the file.
            let bad: Vec<DataSegment> =
                vec![(GuestAddress(0), 512), (GuestAddress(MEM as u64), 512)];
            engine
                .writev(0, &mem, bad, 1024, PendingRequest::default())
                .unwrap_err();
        }
    }

    #[test]
    fn test_async_full_cq_allows_continuation() {
        use crate::devices::virtio::block::virtio::IO_URING_NUM_ENTRIES;
        let mem = create_mem();
        let file = TempFile::new().unwrap().into_file();
        file.write_all_at(&[0x5a; 512], 0).unwrap();
        let mut engine = AsyncFileEngine::from_file(file, false).unwrap();
        // A read of two segments that the 512-byte file cuts short, then flushes until the
        // ring holds as many operations as its completion queue has entries.
        engine
            .push_readv(
                0,
                &mem,
                vec![(GuestAddress(0), 512), (GuestAddress(4096), 512)],
                PendingRequest::default(),
            )
            .unwrap();
        for batch in 0..2 {
            let count = if batch == 0 {
                IO_URING_NUM_ENTRIES - 1
            } else {
                IO_URING_NUM_ENTRIES
            };
            for _ in 0..count {
                engine.push_flush(PendingRequest::default()).unwrap();
            }
            engine.kick_submission_queue().unwrap();
        }
        // The ring is full: the next request is pushed back, not lost.
        let err = engine.push_flush(PendingRequest::default()).unwrap_err();
        assert!(matches!(&err.error, AsyncIoError::IoUring(err) if err.is_throttling_err()));
        let mut completed = 0;
        let mut short_reads = 0;
        while engine.has_pending_ops() {
            engine.drain(false).unwrap();
            while let Some(cqe) = engine.pop(&mem).unwrap() {
                completed += 1;
                // The read continued after its short completion, although the ring was full,
                // and ended at the end of the file with the bytes it read.
                if cqe.result().unwrap() == 512 {
                    short_reads += 1;
                }
            }
        }
        assert_eq!(completed, u32::from(IO_URING_NUM_ENTRIES) * 2);
        assert_eq!(short_reads, 1);
    }

    #[test]
    fn test_async_short_read_continues_from_known_prefix() {
        let mem = create_mem();
        let file = TempFile::new().unwrap().into_file();
        file.write_all_at(&[0x31; 512], 0).unwrap();
        let mut engine = AsyncFileEngine::from_file(file, false).unwrap();
        engine
            .push_readv(
                0,
                &mem,
                vec![(GuestAddress(0), 256), (GuestAddress(4096), 768)],
                PendingRequest::default(),
            )
            .unwrap();
        // Wait until the initial 1024-byte READV hits the old 512-byte EOF.
        engine.kick_submission_queue().unwrap();
        while engine.completion_evt().read().is_err() {
            std::thread::yield_now();
        }
        // Extend before harvesting the CQE: the next child must start at byte 512.
        engine.file().write_all_at(&[0x72; 512], 512).unwrap();
        let cqe = loop {
            if let Some(cqe) = engine.pop(&mem).unwrap() {
                break cqe;
            }
            engine.drain(false).unwrap();
        };
        assert_eq!(cqe.result().unwrap(), 1024);
        assert!(!engine.has_pending_ops());
        let mut first = [0; 256];
        let mut rest = [0; 768];
        mem.read_slice(&mut first, GuestAddress(0)).unwrap();
        mem.read_slice(&mut rest, GuestAddress(4096)).unwrap();
        assert_eq!(first, [0x31; 256]);
        assert_eq!(&rest[..256], &[0x31; 256]);
        assert_eq!(&rest[256..], &[0x72; 512]);
        check_dirty_mem(&mem, GuestAddress(0), 256);
        check_dirty_mem(&mem, GuestAddress(4096), 768);
    }

    #[test]
    fn test_async_short_read_then_eof_preserves_progress_and_dirty() {
        let mem = create_mem();
        let file = TempFile::new().unwrap().into_file();
        file.write_all_at(&[0x5a; 512], 0).unwrap();
        let mut engine = AsyncFileEngine::from_file(file, false).unwrap();
        engine
            .push_readv(
                0,
                &mem,
                vec![(GuestAddress(0), 256), (GuestAddress(4096), 768)],
                PendingRequest::default(),
            )
            .unwrap();
        let cqe = loop {
            engine.drain(false).unwrap();
            if let Some(cqe) = engine.pop(&mem).unwrap() {
                break cqe;
            }
        };
        // Fewer bytes than the request has: the device fails it and reports them.
        assert_eq!(cqe.result().unwrap(), 512);
        assert!(!engine.has_pending_ops());
        let mut first = [0; 256];
        mem.read_slice(&mut first, GuestAddress(0)).unwrap();
        assert_eq!(first, [0x5a; 256]);
        let mut second = [0; 256];
        mem.read_slice(&mut second, GuestAddress(4096)).unwrap();
        assert_eq!(second, [0x5a; 256]);
        check_dirty_mem(&mem, GuestAddress(0), 256);
        check_dirty_mem(&mem, GuestAddress(4096), 768);
    }
}
