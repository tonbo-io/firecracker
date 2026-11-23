// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::fmt::Debug;
use std::fs::File;
use std::os::fd::RawFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::AsRawFd;

use vm_memory::{GuestMemoryBackend, GuestMemoryError};
use vmm_sys_util::eventfd::EventFd;

use crate::arch::host_kernel_version;
use crate::devices::virtio::block::virtio::io::{
    DataSegment, MAX_IOVECS, RequestError, advance_iovecs, mark_dirty_range, segment_iovecs,
};
use crate::devices::virtio::block::virtio::{IO_URING_NUM_ENTRIES, PendingRequest};
use crate::io_uring::operation::{Cqe, OpCode, Operation};
use crate::io_uring::restriction::Restriction;
use crate::io_uring::{IoUring, IoUringError};
use crate::logger::log_dev_preview_warning;
use crate::utils::{Version, u64_to_usize};
use crate::vstate::memory::{GuestAddress, GuestMemoryExtension, GuestMemoryMmap};

/// `BLOCK_URING_CMD_DISCARD` was added in Linux 6.12. Probing `IORING_OP_URING_CMD` is not
/// enough, because older kernels support the opcode without implementing it for block devices.
const BLOCK_URING_DISCARD_MIN_KERNEL: Version = Version::new(6, 12, 0);
const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum AsyncIoError {
    /// Not implemented
    NotImplemented,
    /// Discard on a block device with the Async engine requires host kernel 6.12 or newer
    BlockDiscardUnsupported,
    /// Could not read the metadata of the backing file: {0}
    Metadata(std::io::Error),
    /// IO: {0}
    IO(std::io::Error),
    /// IoUring: {0}
    IoUring(IoUringError),
    /// Submit: {0}
    Submit(std::io::Error),
    /// SyncAll: {0}
    SyncAll(std::io::Error),
    /// EventFd: {0}
    EventFd(std::io::Error),
    /// GuestMemory: {0}
    GuestMemory(GuestMemoryError),
}

#[derive(Debug)]
pub struct AsyncFileEngine {
    file: File,
    ring: IoUring<WrappedRequest>,
    completion_evt: EventFd,
    discard: bool,
    discard_op: Option<DiscardOp>,
    /// Multi-segment transfers that came back short and wait to be issued again for the rest.
    continuations: VecDeque<WrappedRequest>,
    /// Completions taken off the ring and not handed to the device yet.
    ready: VecDeque<Cqe<WrappedRequest>>,
}

/// How discard is executed for the backing file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscardOp {
    /// `IORING_OP_FALLOCATE` punching a hole in a regular file.
    Fallocate,
    /// `BLOCK_URING_CMD_DISCARD` through `IORING_OP_URING_CMD` on a block device.
    BlockDiscard,
}

impl DiscardOp {
    fn select(is_block_device: bool, host_kernel: Version) -> Result<Self, AsyncIoError> {
        if !is_block_device {
            Ok(DiscardOp::Fallocate)
        } else if host_kernel >= BLOCK_URING_DISCARD_MIN_KERNEL {
            Ok(DiscardOp::BlockDiscard)
        } else {
            Err(AsyncIoError::BlockDiscardUnsupported)
        }
    }

    fn for_file(file: &File) -> Result<Self, AsyncIoError> {
        let is_block_device = file
            .metadata()
            .map_err(AsyncIoError::Metadata)?
            .file_type()
            .is_block_device();
        // The host kernel version only matters for block devices.
        let host_kernel = if is_block_device {
            host_kernel_version()
        } else {
            Version::default()
        };
        Self::select(is_block_device, host_kernel)
    }
}

#[derive(Debug)]
pub struct WrappedRequest {
    addr: Option<GuestAddress>,
    req: PendingRequest,
    vectored: Option<Vectored>,
}

/// What is left of the transfer of a request of several data segments. One operation is issued
/// for all of it; if that completes short, another is issued for the rest, until an operation
/// transfers nothing or fails.
#[derive(Debug)]
struct Vectored {
    write: bool,
    /// The file offset of the request's first byte.
    offset: u64,
    /// The guest memory the request's segments are in, kept mapped until the last completion.
    mem: GuestMemoryMmap,
    /// The request's guest ranges, in order, for marking what a read wrote; empty for a write.
    segments: Vec<DataSegment>,
    /// The host ranges not transferred yet. The operation in flight was given this array, so
    /// it changes only between a completion and the next submission.
    iovecs: Vec<libc::iovec>,
    /// Bytes transferred so far.
    done: u32,
}

// SAFETY: The pointers in `iovecs` point into the guest memory that `mem` keeps mapped, which
// may be accessed from any thread, as the single-segment operations' buffers are.
unsafe impl Send for Vectored {}

impl WrappedRequest {
    fn new(req: PendingRequest) -> Self {
        WrappedRequest {
            addr: None,
            req,
            vectored: None,
        }
    }

    fn new_with_dirty_tracking(addr: GuestAddress, req: PendingRequest) -> Self {
        WrappedRequest {
            addr: Some(addr),
            req,
            vectored: None,
        }
    }

    /// The operation that transfers what is left of a multi-segment request.
    fn vectored_operation(self) -> Operation<Self> {
        let (write, iovecs, count, offset) = match &self.vectored {
            Some(vectored) => (
                vectored.write,
                vectored.iovecs.as_ptr() as usize,
                u32::try_from(vectored.iovecs.len().min(MAX_IOVECS))
                    .expect("MAX_IOVECS fits a u32"),
                vectored.offset.saturating_add(u64::from(vectored.done)),
            ),
            None => (false, 0, 0, 0),
        };
        if write {
            Operation::writev(0, iovecs, count, offset, self)
        } else {
            Operation::readv(0, iovecs, count, offset, self)
        }
    }

    fn mark_dirty_mem_and_unwrap(self, mem: &GuestMemoryMmap, count: u32) -> PendingRequest {
        if let Some(addr) = self.addr {
            mem.mark_dirty(addr, count as usize)
        }

        self.req
    }
}

impl AsyncFileEngine {
    fn new_ring(
        file: &File,
        completion_fd: RawFd,
        discard_op: Option<DiscardOp>,
    ) -> Result<IoUring<WrappedRequest>, IoUringError> {
        let mut restrictions = vec![
            // Make sure we only allow operations on pre-registered fds.
            Restriction::RequireFixedFds,
            // Allowlist of opcodes.
            Restriction::AllowOpCode(OpCode::Read),
            Restriction::AllowOpCode(OpCode::Write),
            Restriction::AllowOpCode(OpCode::Fsync),
            // A request of several data segments.
            Restriction::AllowOpCode(OpCode::Readv),
            Restriction::AllowOpCode(OpCode::Writev),
        ];
        let mut extra_ops = vec![OpCode::Readv, OpCode::Writev];
        match discard_op {
            Some(DiscardOp::Fallocate) => {
                restrictions.push(Restriction::AllowOpCode(OpCode::Fallocate));
                extra_ops.push(OpCode::Fallocate);
            }
            Some(DiscardOp::BlockDiscard) => {
                restrictions.push(Restriction::AllowOpCode(OpCode::UringCmd));
                // The block discard command is forced into io-wq.
                restrictions.push(Restriction::AllowAsync);
                extra_ops.push(OpCode::UringCmd);
            }
            None => (),
        }

        IoUring::new_with_required_ops(
            u32::from(IO_URING_NUM_ENTRIES),
            vec![file],
            restrictions,
            Some(completion_fd),
            &extra_ops,
        )
    }

    pub fn from_file(file: File, discard: bool) -> Result<AsyncFileEngine, AsyncIoError> {
        log_dev_preview_warning("Async file IO", Option::None);

        let completion_evt = EventFd::new(libc::EFD_NONBLOCK).map_err(AsyncIoError::EventFd)?;
        let discard_op = discard.then(|| DiscardOp::for_file(&file)).transpose()?;
        let ring = Self::new_ring(&file, completion_evt.as_raw_fd(), discard_op)
            .map_err(AsyncIoError::IoUring)?;

        Ok(AsyncFileEngine {
            file,
            ring,
            completion_evt,
            discard,
            discard_op,
            continuations: VecDeque::new(),
            ready: VecDeque::new(),
        })
    }

    pub fn update_file(&mut self, file: File) -> Result<(), AsyncIoError> {
        // The new backing file may be of a different type than the old one.
        let discard_op = self
            .discard
            .then(|| DiscardOp::for_file(&file))
            .transpose()?;
        let ring = Self::new_ring(&file, self.completion_evt.as_raw_fd(), discard_op)
            .map_err(AsyncIoError::IoUring)?;

        self.file = file;
        self.ring = ring;
        self.discard_op = discard_op;
        Ok(())
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn completion_evt(&self) -> &EventFd {
        &self.completion_evt
    }

    pub fn push_read(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        let buf = match mem.get_slice(addr, count as usize) {
            Ok(slice) => slice.ptr_guard_mut().as_ptr(),
            Err(err) => {
                return Err(RequestError {
                    req,
                    error: AsyncIoError::GuestMemory(err),
                });
            }
        };

        let wrapped_user_data = WrappedRequest::new_with_dirty_tracking(addr, req);

        self.ring
            .push(Operation::read(
                0,
                buf as usize,
                count,
                offset,
                wrapped_user_data,
            ))
            .map_err(|(io_uring_error, data)| RequestError {
                req: data.req,
                error: AsyncIoError::IoUring(io_uring_error),
            })
    }

    pub fn push_write(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        let buf = match mem.get_slice(addr, count as usize) {
            Ok(slice) => slice.ptr_guard_mut().as_ptr(),
            Err(err) => {
                return Err(RequestError {
                    req,
                    error: AsyncIoError::GuestMemory(err),
                });
            }
        };

        let wrapped_user_data = WrappedRequest::new(req);

        self.ring
            .push(Operation::write(
                0,
                buf as usize,
                count,
                offset,
                wrapped_user_data,
            ))
            .map_err(|(io_uring_error, data)| RequestError {
                req: data.req,
                error: AsyncIoError::IoUring(io_uring_error),
            })
    }

    /// Queues the read of a request of several data segments, as one `IORING_OP_READV`.
    pub fn push_readv(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        segments: Vec<DataSegment>,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        self.push_vectored(offset, mem, segments, false, req)
    }

    /// Queues the write of a request of several data segments, as one `IORING_OP_WRITEV`.
    pub fn push_writev(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        segments: Vec<DataSegment>,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        self.push_vectored(offset, mem, segments, true, req)
    }

    fn push_vectored(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        segments: Vec<DataSegment>,
        write: bool,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        // Every segment is resolved before anything is queued.
        let iovecs = match segment_iovecs(mem, &segments) {
            Ok(iovecs) => iovecs,
            Err(err) => {
                return Err(RequestError {
                    req,
                    error: AsyncIoError::GuestMemory(err),
                });
            }
        };
        let wrapped = WrappedRequest {
            addr: None,
            req,
            vectored: Some(Vectored {
                write,
                offset,
                mem: mem.clone(),
                segments: if write { Vec::new() } else { segments },
                iovecs,
                done: 0,
            }),
        };

        self.ring
            .push(wrapped.vectored_operation())
            .map_err(|(io_uring_error, data)| RequestError {
                req: data.req,
                error: AsyncIoError::IoUring(io_uring_error),
            })
    }

    pub fn push_flush(&mut self, req: PendingRequest) -> Result<(), RequestError<AsyncIoError>> {
        let wrapped_user_data = WrappedRequest::new(req);

        self.ring
            .push(Operation::fsync(0, wrapped_user_data))
            .map_err(|(io_uring_error, data)| RequestError {
                req: data.req,
                error: AsyncIoError::IoUring(io_uring_error),
            })
    }

    pub fn push_discard(
        &mut self,
        (offset, len): (u64, u32),
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        let wrapped_user_data = WrappedRequest::new(req);
        let len = u64::from(len);
        let operation = match self.discard_op {
            Some(DiscardOp::Fallocate) => Operation::fallocate(
                0,
                FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE,
                offset,
                len,
                wrapped_user_data,
            ),
            Some(DiscardOp::BlockDiscard) => {
                Operation::block_discard(0, offset, len, wrapped_user_data)
            }
            None => {
                return Err(RequestError {
                    req: wrapped_user_data.req,
                    error: AsyncIoError::NotImplemented,
                });
            }
        };

        self.ring
            .push(operation)
            .map_err(|(io_uring_error, data)| RequestError {
                req: data.req,
                error: AsyncIoError::IoUring(io_uring_error),
            })
    }

    pub fn kick_submission_queue(&mut self) -> Result<(), AsyncIoError> {
        self.ring
            .submit()
            .map(|_| ())
            .map_err(AsyncIoError::IoUring)
    }

    /// Whether a request is queued, in flight, or completed and not handed to the device yet.
    pub(crate) fn has_pending_ops(&self) -> bool {
        self.ring.num_ops() != 0 || !self.continuations.is_empty() || !self.ready.is_empty()
    }

    /// Waits until every request has completed. A multi-segment request whose operation came
    /// back short is issued again for the rest, and waited for, unless `discard_cqes`: then the
    /// completions are dropped, and with them every request, as nobody will read them.
    pub fn drain(&mut self, discard_cqes: bool) -> Result<(), AsyncIoError> {
        if discard_cqes {
            // Nothing in flight uses a queued continuation's ranges, so it can be dropped.
            self.continuations.clear();
            self.ring
                .submit_and_wait_all()
                .map_err(AsyncIoError::IoUring)?;
            // Drain the completion queue so that we may deallocate the user_data fields.
            while self.do_pop()?.is_some() {}
            self.ready.clear();
            return Ok(());
        }

        loop {
            self.push_continuations()?;
            self.ring
                .submit_and_wait_all()
                .map_err(AsyncIoError::IoUring)?;
            // Every operation has completed; one that was short leaves a continuation.
            self.harvest()?;
            if self.ring.num_ops() == 0 && self.continuations.is_empty() {
                return Ok(());
            }
        }
    }

    pub fn drain_and_flush(&mut self, discard_cqes: bool) -> Result<(), AsyncIoError> {
        self.drain(discard_cqes)?;

        // Sync data out to physical media on host.
        // We don't need to call flush first since all the ops are performed through io_uring
        // and Rust shouldn't manage any data in its internal buffers.
        self.file.sync_all().map_err(AsyncIoError::SyncAll)?;

        Ok(())
    }

    fn do_pop(&mut self) -> Result<Option<Cqe<WrappedRequest>>, AsyncIoError> {
        self.ring.pop().map_err(AsyncIoError::IoUring)
    }

    /// Issues the queued continuations of short multi-segment transfers, and submits them.
    ///
    /// A continuation takes the place of the operation whose completion left it, so the ring
    /// has room for it; should the ring be full all the same, it stays queued until the next
    /// completion is taken off the ring.
    fn push_continuations(&mut self) -> Result<(), AsyncIoError> {
        if self.continuations.is_empty() {
            return Ok(());
        }
        while let Some(wrapped) = self.continuations.pop_front() {
            if let Err((err, wrapped)) = self.ring.push(wrapped.vectored_operation()) {
                self.continuations.push_front(wrapped);
                if err.is_throttling_err() {
                    break;
                }
                return Err(AsyncIoError::IoUring(err));
            }
        }
        self.kick_submission_queue()
    }

    /// Takes every completion off the ring. A multi-segment transfer that came back short is
    /// issued again for the rest; every other completion is kept for [`Self::pop`].
    fn harvest(&mut self) -> Result<(), AsyncIoError> {
        while let Some(cqe) = self.do_pop()? {
            let (res, mut wrapped) = cqe.into_parts();
            let Some(vectored) = wrapped.vectored.as_mut() else {
                self.ready.push_back(Cqe::new(res, wrapped));
                continue;
            };
            // A failed operation transferred nothing: what the earlier ones of this request
            // read is already marked dirty, and the request fails as a whole.
            let Ok(moved) = u32::try_from(res) else {
                self.ready.push_back(Cqe::new(res, wrapped));
                continue;
            };
            if !vectored.write {
                // Exactly what this operation read, which starts where the last one stopped.
                mark_dirty_range(&vectored.mem, &vectored.segments, vectored.done, moved);
            }
            vectored.done = vectored.done.saturating_add(moved);
            advance_iovecs(&mut vectored.iovecs, u64_to_usize(u64::from(moved)));
            if moved == 0 || vectored.iovecs.is_empty() {
                // All of it, or as much as there is (an operation that transfers nothing has
                // reached the end of the file): the request completes with the bytes
                // transferred, and the device fails it if they are not all of them.
                let done = i32::try_from(vectored.done).unwrap_or(i32::MAX);
                self.ready.push_back(Cqe::new(done, wrapped));
            } else {
                self.continuations.push_back(wrapped);
            }
        }
        self.push_continuations()
    }

    pub fn pop(
        &mut self,
        mem: &GuestMemoryMmap,
    ) -> Result<Option<Cqe<PendingRequest>>, AsyncIoError> {
        self.harvest()?;
        let cqe = self.ready.pop_front().map(|cqe| {
            let count = cqe.count();
            cqe.map_user_data(|wrapped_user_data| {
                wrapped_user_data.mark_dirty_mem_and_unwrap(mem, count)
            })
        });

        Ok(cqe)
    }
}

#[cfg(test)]
mod tests {
    use vmm_sys_util::tempfile::TempFile;

    use super::*;

    #[test]
    fn test_discard_op_select() {
        assert_eq!(
            DiscardOp::select(false, Version::new(5, 10, 0)).unwrap(),
            DiscardOp::Fallocate
        );
        assert_eq!(
            DiscardOp::select(true, Version::new(6, 12, 0)).unwrap(),
            DiscardOp::BlockDiscard
        );
        assert_eq!(
            DiscardOp::select(true, Version::new(6, 18, 0)).unwrap(),
            DiscardOp::BlockDiscard
        );
        for kernel in [
            Version::new(5, 10, 0),
            Version::new(6, 1, 0),
            Version::new(6, 11, 255),
            // An unreadable host kernel version is reported as 0.0.0.
            Version::default(),
        ] {
            assert!(matches!(
                DiscardOp::select(true, kernel),
                Err(AsyncIoError::BlockDiscardUnsupported)
            ));
        }
    }

    #[test]
    fn test_discard_op_for_regular_file() {
        let file = TempFile::new().unwrap().into_file();
        let engine = AsyncFileEngine::from_file(file, true).unwrap();
        assert_eq!(engine.discard_op, Some(DiscardOp::Fallocate));

        let file = TempFile::new().unwrap().into_file();
        let mut engine = AsyncFileEngine::from_file(file, false).unwrap();
        assert_eq!(engine.discard_op, None);
        // Without discard, nothing is selected for a new backing file either.
        engine
            .update_file(TempFile::new().unwrap().into_file())
            .unwrap();
        assert_eq!(engine.discard_op, None);
    }
}
