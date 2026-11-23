// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::convert::From;
use std::mem::size_of;

use vm_memory::GuestMemoryError;

use super::io::DataSegment;
use super::{MAX_DISCARD_SECTORS, SECTOR_SHIFT, SECTOR_SIZE, VirtioBlockError, io as block_io};
use crate::devices::virtio::block::virtio::device::DiskProperties;
use crate::devices::virtio::block::virtio::metrics::BlockDeviceMetrics;
pub use crate::devices::virtio::generated::virtio_blk::{
    VIRTIO_BLK_ID_BYTES, VIRTIO_BLK_S_IOERR, VIRTIO_BLK_S_OK, VIRTIO_BLK_S_UNSUPP,
    VIRTIO_BLK_T_DISCARD, VIRTIO_BLK_T_FLUSH, VIRTIO_BLK_T_GET_ID, VIRTIO_BLK_T_IN,
    VIRTIO_BLK_T_OUT,
};
use crate::devices::virtio::queue::{
    DescriptorChain, FIRECRACKER_MAX_QUEUE_SIZE, VIRTQ_DESC_F_NEXT,
};
use crate::logger::{IncMetric, error};
use crate::rate_limiter::{RateLimiter, TokenType};
use crate::vstate::memory::{
    Address, ByteValued, Bytes, GuestAddress, GuestMemoryExtension, GuestMemoryMmap,
    GuestMemoryRegion,
};

#[derive(Debug)]
pub enum IoErr {
    GetId(GuestMemoryError),
    UnsupportedRequest(u32),
    PartialTransfer { completed: u32, expected: u32 },
    FileEngine(block_io::BlockIoError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestType {
    In,
    Out,
    Discard,
    Flush,
    GetDeviceID,
    Unsupported(u32),
}

impl From<u32> for RequestType {
    fn from(value: u32) -> Self {
        match value {
            VIRTIO_BLK_T_IN => RequestType::In,
            VIRTIO_BLK_T_OUT => RequestType::Out,
            VIRTIO_BLK_T_DISCARD => RequestType::Discard,
            VIRTIO_BLK_T_FLUSH => RequestType::Flush,
            VIRTIO_BLK_T_GET_ID => RequestType::GetDeviceID,
            t => RequestType::Unsupported(t),
        }
    }
}

#[derive(Debug)]
pub enum ProcessingResult {
    Submitted,
    Throttled,
    Executed(FinishedRequest),
}

#[derive(Debug)]
pub struct FinishedRequest {
    pub num_bytes_to_mem: u32,
    pub desc_idx: u16,
}

#[derive(Debug)]
enum Status {
    Ok { num_bytes_to_mem: u32 },
    IoErr { num_bytes_to_mem: u32, err: IoErr },
    Unsupported { op: u32 },
}

impl Status {
    fn from_data(data_len: u32, transferred_data_len: u32, data_to_mem: bool) -> Status {
        let num_bytes_to_mem = match data_to_mem {
            true => transferred_data_len,
            false => 0,
        };

        match transferred_data_len == data_len {
            true => Status::Ok { num_bytes_to_mem },
            false => Status::IoErr {
                num_bytes_to_mem,
                err: IoErr::PartialTransfer {
                    completed: transferred_data_len,
                    expected: data_len,
                },
            },
        }
    }
}

#[derive(Debug)]
pub struct PendingRequest {
    r#type: RequestType,
    data_len: u32,
    status_addr: GuestAddress,
    desc_idx: u16,
}

impl PendingRequest {
    fn write_status_and_finish(
        self,
        status: &Status,
        mem: &GuestMemoryMmap,
        block_metrics: &BlockDeviceMetrics,
    ) -> FinishedRequest {
        let (num_bytes_to_mem, status_code) = match status {
            Status::Ok { num_bytes_to_mem } => {
                (*num_bytes_to_mem, u8::try_from(VIRTIO_BLK_S_OK).unwrap())
            }
            Status::IoErr {
                num_bytes_to_mem,
                err,
            } => {
                block_metrics.invalid_reqs_count.inc();
                error!(
                    "Failed to execute {:?} virtio block request: {:?}",
                    self.r#type, err
                );
                (*num_bytes_to_mem, u8::try_from(VIRTIO_BLK_S_IOERR).unwrap())
            }
            Status::Unsupported { op } => {
                block_metrics.invalid_reqs_count.inc();
                error!("Received unsupported virtio block request: {}", op);
                (0, u8::try_from(VIRTIO_BLK_S_UNSUPP).unwrap())
            }
        };

        let num_bytes_to_mem = mem
            .write_obj(status_code, self.status_addr)
            .map(|_| {
                // Account for the status byte
                num_bytes_to_mem + 1
            })
            .unwrap_or_else(|err| {
                error!("Failed to write virtio block status: {:?}", err);
                // If we can't write the status, discard the virtio descriptor
                0
            });

        FinishedRequest {
            num_bytes_to_mem,
            desc_idx: self.desc_idx,
        }
    }

    pub fn finish(
        self,
        mem: &GuestMemoryMmap,
        res: Result<u32, IoErr>,
        block_metrics: &BlockDeviceMetrics,
    ) -> FinishedRequest {
        self.finish_with_progress(mem, res, 0, block_metrics)
    }

    /// Complete a request after a terminal error that may have transferred a known prefix.
    pub fn finish_with_progress(
        self,
        mem: &GuestMemoryMmap,
        res: Result<u32, IoErr>,
        known_prefix: u32,
        block_metrics: &BlockDeviceMetrics,
    ) -> FinishedRequest {
        let known_prefix = known_prefix.min(self.data_len);
        let status = match (res, self.r#type) {
            (Ok(transferred_data_len), RequestType::In) => {
                let status = Status::from_data(self.data_len, transferred_data_len, true);
                block_metrics.read_bytes.add(transferred_data_len.into());
                if let Status::Ok { .. } = status {
                    block_metrics.read_count.inc();
                }
                status
            }
            (Ok(transferred_data_len), RequestType::Out) => {
                let status = Status::from_data(self.data_len, transferred_data_len, false);
                block_metrics.write_bytes.add(transferred_data_len.into());
                if let Status::Ok { .. } = status {
                    block_metrics.write_count.inc();
                }
                status
            }
            (Ok(_), RequestType::Discard) => Status::Ok {
                num_bytes_to_mem: 0,
            },
            (Ok(_), RequestType::Flush) => {
                block_metrics.flush_count.inc();
                Status::Ok {
                    num_bytes_to_mem: 0,
                }
            }
            (Ok(transferred_data_len), RequestType::GetDeviceID) => {
                Status::from_data(self.data_len, transferred_data_len, true)
            }
            (Err(IoErr::UnsupportedRequest(op)), _) | (_, RequestType::Unsupported(op)) => {
                Status::Unsupported { op }
            }
            (Err(err), ty) => {
                let num_bytes_to_mem = match ty {
                    RequestType::In => {
                        block_metrics.read_bytes.add(known_prefix.into());
                        known_prefix
                    }
                    RequestType::Out => {
                        block_metrics.write_bytes.add(known_prefix.into());
                        0
                    }
                    _ => 0,
                };
                Status::IoErr {
                    num_bytes_to_mem,
                    err,
                }
            }
        };

        self.write_status_and_finish(&status, mem, block_metrics)
    }
}

/// The request header represents the mandatory fields of each block device request.
///
/// A request header contains the following fields:
///   * request_type: an u32 value mapping to a read, write or flush operation.
///   * reserved: 32 bits are reserved for future extensions of the Virtio Spec.
///   * sector: an u64 value representing the offset where a read/write is to occur.
///
/// The header simplifies reading the request from memory as all request follow
/// the same memory layout.
#[derive(Debug, Copy, Clone, Default)]
#[repr(C)]
pub struct RequestHeader {
    request_type: u32,
    _reserved: u32,
    sector: u64,
}

// SAFETY: Safe because RequestHeader only contains plain data.
unsafe impl ByteValued for RequestHeader {}

#[derive(Debug, Copy, Clone, Default)]
#[repr(C)]
pub struct DiscardWriteZeroes {
    sector: u64,
    num_sectors: u32,
    flags: u32,
}

// SAFETY: Safe because DiscardWriteZeroes only contains plain data and has no padding.
unsafe impl ByteValued for DiscardWriteZeroes {}

/// Size of a single discard range descriptor, as fixed by the virtio specification.
const DISCARD_RANGE_SIZE: u32 = 16;
const _: () = assert!(size_of::<DiscardWriteZeroes>() == DISCARD_RANGE_SIZE as usize);

impl RequestHeader {
    pub fn new(request_type: u32, sector: u64) -> RequestHeader {
        RequestHeader {
            request_type,
            _reserved: 0,
            sector,
        }
    }
    /// Reads the request header from GuestMemoryMmap starting at `addr`.
    ///
    /// Virtio 1.0 specifies that the data is transmitted by the driver in little-endian
    /// format. Firecracker currently runs only on little endian platforms so we don't
    /// need to do an explicit little endian read as all reads are little endian by default.
    /// When running on a big endian platform, this code should not compile, and support
    /// for explicit little endian reads is required.
    #[cfg(target_endian = "little")]
    #[cfg(test)]
    fn read_from(memory: &GuestMemoryMmap, addr: GuestAddress) -> Result<Self, VirtioBlockError> {
        let request_header: RequestHeader = memory
            .read_obj(addr)
            .map_err(VirtioBlockError::GuestMemory)?;
        Ok(request_header)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub r#type: RequestType,
    /// Total length of the data, over all segments.
    pub data_len: u32,
    pub status_addr: GuestAddress,
    sector: u64,
    /// The first (for a single-segment request: the only) data segment's address.
    data_addr: GuestAddress,
    /// Every data segment when the request has more than one (`VIRTIO_BLK_F_SEG_MAX`); empty
    /// for a single-segment request, which is described by `data_addr` and `data_len` alone.
    segments: Vec<DataSegment>,
}

// Descriptor boundaries are not message boundaries for a modern virtio device.
// A stack-backed stream snapshots all descriptor metadata exactly once.
struct DescriptorStream {
    spans: [DataSegment; FIRECRACKER_MAX_QUEUE_SIZE as usize],
    count: usize,
    index: usize,
    offset: u32,
    remaining: u64,
}

impl DescriptorStream {
    fn new() -> Self {
        Self {
            spans: [(GuestAddress(0), 0); FIRECRACKER_MAX_QUEUE_SIZE as usize],
            count: 0,
            index: 0,
            offset: 0,
            remaining: 0,
        }
    }

    fn push(&mut self, addr: GuestAddress, len: u32) -> Result<(), VirtioBlockError> {
        if len != 0 {
            if self.count == self.spans.len() {
                return Err(VirtioBlockError::DescriptorChainTooShort);
            }
            self.spans[self.count] = (addr, len);
            self.count += 1;
            self.remaining += u64::from(len);
        }
        Ok(())
    }

    fn take<F>(&mut self, mut len: u64, mut f: F) -> Result<(), VirtioBlockError>
    where
        F: FnMut(GuestAddress, u32) -> Result<(), VirtioBlockError>,
    {
        if len > self.remaining {
            return Err(VirtioBlockError::DescriptorChainTooShort);
        }
        self.remaining -= len;
        while len != 0 {
            let (addr, size) = self.spans[self.index];
            let count = u32::try_from(len.min(u64::from(size - self.offset))).unwrap();
            let start =
                addr.checked_add(u64::from(self.offset))
                    .ok_or(VirtioBlockError::GuestMemory(
                        GuestMemoryError::GuestAddressOverflow,
                    ))?;
            f(start, count)?;
            self.offset += count;
            len -= u64::from(count);
            if self.offset == size {
                self.index += 1;
                self.offset = 0;
            }
        }
        Ok(())
    }

    /// The address of the last byte of the stream, if it has any.
    fn last_byte(&self) -> Option<GuestAddress> {
        let (addr, len) = *self.spans[..self.count].last()?;
        addr.checked_add(u64::from(len) - 1)
    }

    fn read(&mut self, mem: &GuestMemoryMmap, buf: &mut [u8]) -> Result<(), VirtioBlockError> {
        let mut offset = 0;
        self.take(buf.len() as u64, |addr, len| {
            let end = offset + len as usize;
            mem.check_range_plugged(addr, len as usize)
                .map_err(VirtioBlockError::GuestMemory)?;
            mem.read_slice(&mut buf[offset..end], addr)
                .map_err(VirtioBlockError::GuestMemory)?;
            offset = end;
            Ok(())
        })
    }
}

impl Request {
    pub fn parse(
        avail_desc: &DescriptorChain,
        mem: &GuestMemoryMmap,
        num_disk_sectors: u64,
    ) -> Result<Request, VirtioBlockError> {
        Self::parse_with_status(avail_desc, mem, num_disk_sectors).map_err(|(err, _)| err)
    }

    /// `parse`, and on a refusal the address of the request's status byte, when the chain was
    /// walked to its end (no indirect table, no cycle, no readable descriptor after a writable
    /// one) and its last device-writable byte (where virtio puts the status) is mapped guest
    /// memory. The device writes `VIRTIO_BLK_S_IOERR` there: without a status byte a refused
    /// request (out of range, a buffer outside guest memory, a bad length) could look like a
    /// success to the guest, which would keep the status byte of an earlier request.
    pub fn parse_with_status(
        avail_desc: &DescriptorChain,
        mem: &GuestMemoryMmap,
        num_disk_sectors: u64,
    ) -> Result<Request, (VirtioBlockError, Option<GuestAddress>)> {
        let mut readable = DescriptorStream::new();
        let mut writable = DescriptorStream::new();
        Self::walk(avail_desc, &mut readable, &mut writable).map_err(|err| (err, None))?;
        let status = writable
            .last_byte()
            .filter(|&addr| mem.check_range_plugged(addr, 1).is_ok());
        Self::parse_streams(&mut readable, &mut writable, mem, num_disk_sectors)
            .map_err(|err| (err, status))
    }

    /// Copy the chain's descriptors into a readable and a writable stream.
    fn walk(
        avail_desc: &DescriptorChain,
        readable: &mut DescriptorStream,
        writable: &mut DescriptorStream,
    ) -> Result<(), VirtioBlockError> {
        let mut desc = *avail_desc;
        let mut seen_writable = false;
        let mut chain_len = 0u64;
        loop {
            // This device does not negotiate VIRTIO_RING_F_INDIRECT_DESC.
            // Never interpret an indirect table as an ordinary data buffer.
            if desc.flags & 0x4 != 0 {
                return Err(VirtioBlockError::DescriptorChainTooShort);
            }
            // A copied descriptor is the only metadata subsequently consulted. This
            // avoids a guest changing an already validated descriptor later in parsing.
            chain_len = chain_len
                .checked_add(u64::from(desc.len))
                .ok_or(VirtioBlockError::InvalidDataLength)?;
            // Virtio limits the total length of a descriptor chain to 2^32 bytes.
            if chain_len > (1u64 << 32) {
                return Err(VirtioBlockError::InvalidDataLength);
            }
            if desc.is_write_only() {
                seen_writable = true;
                writable.push(desc.addr, desc.len)?;
            } else {
                if seen_writable {
                    return Err(VirtioBlockError::UnexpectedReadOnlyDescriptor);
                }
                readable.push(desc.addr, desc.len)?;
            }
            if desc.flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            desc = desc
                .next_descriptor()
                .ok_or(VirtioBlockError::DescriptorChainTooShort)?;
        }
        Ok(())
    }

    fn parse_streams(
        readable: &mut DescriptorStream,
        writable: &mut DescriptorStream,
        mem: &GuestMemoryMmap,
        num_disk_sectors: u64,
    ) -> Result<Request, VirtioBlockError> {
        let mut header = [0u8; 16];
        readable.read(mem, &mut header)?;
        let request_type = RequestType::from(u32::from_le_bytes(header[0..4].try_into().unwrap()));
        let sector = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let mut req = Request {
            r#type: request_type,
            sector,
            data_addr: GuestAddress(0),
            data_len: 0,
            status_addr: GuestAddress(0),
            segments: Vec::new(),
        };

        // Construct region-contained payload spans and validate them all before the
        // first host I/O. Keep the common one-span path allocation-free.
        let mut push_span = |addr: GuestAddress, len: u32| -> Result<(), VirtioBlockError> {
            mem.check_range_plugged(addr, len as usize)
                .map_err(VirtioBlockError::GuestMemory)?;
            mem.try_for_each_region_in_range(addr, len as usize, |region, offset, n| {
                let base = region
                    .start_addr()
                    .checked_add(offset.raw_value())
                    .ok_or(GuestMemoryError::GuestAddressOverflow)?;
                let n = u32::try_from(n).map_err(|_| GuestMemoryError::GuestAddressOverflow)?;
                if req.data_len == 0 {
                    req.data_addr = base;
                    req.data_len = n;
                } else {
                    if req.segments.is_empty() {
                        req.segments.push((req.data_addr, req.data_len));
                    }
                    req.segments.push((base, n));
                    req.data_len = req
                        .data_len
                        .checked_add(n)
                        .ok_or(GuestMemoryError::GuestAddressOverflow)?;
                }
                Ok(())
            })
            .map_err(VirtioBlockError::GuestMemory)
        };

        match request_type {
            RequestType::Out => {
                let len = readable.remaining;
                readable.take(len, &mut push_span)?;
            }
            RequestType::In => {
                if readable.remaining != 0 {
                    return Err(VirtioBlockError::UnexpectedReadOnlyDescriptor);
                }
                let len = writable
                    .remaining
                    .checked_sub(1)
                    .ok_or(VirtioBlockError::DescriptorLengthTooSmall)?;
                writable.take(len, &mut push_span)?;
            }
            RequestType::GetDeviceID => {
                if readable.remaining != 0 {
                    return Err(VirtioBlockError::UnexpectedReadOnlyDescriptor);
                }
                if writable.remaining < u64::from(VIRTIO_BLK_ID_BYTES) + 1 {
                    return Err(VirtioBlockError::InvalidDataLength);
                }
                writable.take(u64::from(VIRTIO_BLK_ID_BYTES), &mut push_span)?;
            }
            RequestType::Discard => {
                if readable.remaining != u64::from(DISCARD_RANGE_SIZE) {
                    return Err(VirtioBlockError::InvalidDataLength);
                }
                // Copy guest-controlled range metadata once and use only this copy.
                let mut bytes = [0u8; DISCARD_RANGE_SIZE as usize];
                readable.read(mem, &mut bytes)?;
                let range = DiscardWriteZeroes {
                    sector: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
                    num_sectors: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
                    flags: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
                };
                if range.flags != 0 {
                    // Virtio requires UNSUPP for unknown or unmap flags on discard.
                    req.r#type = RequestType::Unsupported(VIRTIO_BLK_T_DISCARD);
                } else {
                    req.validate_discard_range(range, num_disk_sectors)?;
                }
            }
            // The driver SHOULD NOT include data on FLUSH, but extra readable
            // bytes do not prevent us from issuing the flush and its status.
            RequestType::Flush => {}
            RequestType::Unsupported(_) => {
                // Unknown commands can have writable response data; the last
                // writable byte is the generic status field.
                let response = writable
                    .remaining
                    .checked_sub(1)
                    .ok_or(VirtioBlockError::DescriptorLengthTooSmall)?;
                writable.take(response, |_, _| Ok(()))?;
            }
        }
        // The status is the last device-writable byte of the chain. Whatever a driver put
        // between the data and it is neither read nor written.
        let before_status = writable.remaining.saturating_sub(1);
        writable.take(before_status, |_, _| Ok(()))?;
        // Capture the status byte from the writable stream, even if it shares a
        // descriptor with the preceding payload.
        writable.take(1, |addr, _| {
            mem.check_range_plugged(addr, 1)
                .map_err(VirtioBlockError::GuestMemory)?;
            req.status_addr = addr;
            Ok(())
        })?;

        if matches!(request_type, RequestType::In | RequestType::Out) {
            if !req.data_len.is_multiple_of(SECTOR_SIZE) {
                return Err(VirtioBlockError::InvalidDataLength);
            }
            let top = req
                .sector
                .checked_add(u64::from(req.data_len) >> SECTOR_SHIFT)
                .ok_or(VirtioBlockError::InvalidOffset)?;
            if top > num_disk_sectors {
                return Err(VirtioBlockError::InvalidOffset);
            }
        }
        Ok(req)
    }

    fn validate_discard_range(
        &mut self,
        range: DiscardWriteZeroes,
        num_disk_sectors: u64,
    ) -> Result<(), VirtioBlockError> {
        if range.num_sectors == 0 || range.num_sectors > MAX_DISCARD_SECTORS || range.flags != 0 {
            return Err(VirtioBlockError::InvalidDataLength);
        }

        let top_sector = range
            .sector
            .checked_add(u64::from(range.num_sectors))
            .ok_or(VirtioBlockError::InvalidOffset)?;

        if top_sector > num_disk_sectors {
            return Err(VirtioBlockError::InvalidOffset);
        }

        let byte_offset = range
            .sector
            .checked_mul(u64::from(SECTOR_SIZE))
            .ok_or(VirtioBlockError::InvalidOffset)?;
        let byte_len = u64::from(range.num_sectors)
            .checked_mul(u64::from(SECTOR_SIZE))
            .ok_or(VirtioBlockError::InvalidDataLength)?;
        let byte_len = u32::try_from(byte_len).map_err(|_| VirtioBlockError::InvalidDataLength)?;

        self.sector = byte_offset;
        self.data_len = byte_len;

        Ok(())
    }

    pub(crate) fn rate_limit(&self, rate_limiter: &mut RateLimiter) -> bool {
        // If limiter.consume() fails it means there is no more TokenType::Ops
        // budget and rate limiting is in effect.
        if !rate_limiter.consume(1, TokenType::Ops) {
            return true;
        }
        // Exercise the rate limiter only if this request is of data transfer type.
        if self.r#type == RequestType::In || self.r#type == RequestType::Out {
            // If limiter.consume() fails it means there is no more TokenType::Bytes
            // budget and rate limiting is in effect.
            if !rate_limiter.consume(u64::from(self.data_len), TokenType::Bytes) {
                // Revert the OPS consume().
                rate_limiter.manual_replenish(1, TokenType::Ops);
                return true;
            }
        }

        false
    }

    fn offset(&self) -> u64 {
        self.sector << SECTOR_SHIFT
    }

    /// The data segments of the request (one for a single-segment request).
    #[cfg(test)]
    pub(crate) fn segments(&self) -> Vec<DataSegment> {
        if self.segments.is_empty() {
            vec![(self.data_addr, self.data_len)]
        } else {
            self.segments.clone()
        }
    }

    fn to_pending_request(&self, desc_idx: u16) -> PendingRequest {
        PendingRequest {
            r#type: self.r#type,
            data_len: self.data_len,
            status_addr: self.status_addr,
            desc_idx,
        }
    }

    pub(crate) fn process(
        self,
        disk: &mut DiskProperties,
        desc_idx: u16,
        mem: &GuestMemoryMmap,
        block_metrics: &BlockDeviceMetrics,
        discard_supported: bool,
    ) -> ProcessingResult {
        let pending = self.to_pending_request(desc_idx);
        let res = match self.r#type {
            RequestType::In if !self.segments.is_empty() => {
                let _metric = block_metrics.read_agg.record_latency_metrics();
                disk.file_engine
                    .readv(self.offset(), mem, self.segments, self.data_len, pending)
            }
            RequestType::Out if !self.segments.is_empty() => {
                let _metric = block_metrics.write_agg.record_latency_metrics();
                disk.file_engine
                    .writev(self.offset(), mem, self.segments, self.data_len, pending)
            }
            RequestType::In => {
                let _metric = block_metrics.read_agg.record_latency_metrics();
                disk.file_engine
                    .read(self.offset(), mem, self.data_addr, self.data_len, pending)
            }
            RequestType::Out => {
                let _metric = block_metrics.write_agg.record_latency_metrics();
                disk.file_engine
                    .write(self.offset(), mem, self.data_addr, self.data_len, pending)
            }
            RequestType::Discard => {
                if !discard_supported {
                    return ProcessingResult::Executed(pending.finish(
                        mem,
                        Err(IoErr::UnsupportedRequest(VIRTIO_BLK_T_DISCARD)),
                        block_metrics,
                    ));
                }

                disk.file_engine
                    .discard((self.sector, self.data_len), pending)
            }
            RequestType::Flush => disk.file_engine.flush(pending),
            RequestType::GetDeviceID => {
                let res = if self.segments.is_empty() {
                    mem.write_slice(&disk.image_id, self.data_addr)
                } else {
                    let mut offset = 0usize;
                    self.segments.iter().try_for_each(|&(addr, len)| {
                        let end = offset + len as usize;
                        let res = mem.write_slice(&disk.image_id[offset..end], addr);
                        offset = end;
                        res
                    })
                }
                .map(|_| VIRTIO_BLK_ID_BYTES)
                .map_err(IoErr::GetId);
                return ProcessingResult::Executed(pending.finish(mem, res, block_metrics));
            }
            RequestType::Unsupported(_) => {
                return ProcessingResult::Executed(pending.finish(mem, Ok(0), block_metrics));
            }
        };

        match res {
            Ok(block_io::FileEngineOk::Submitted) => ProcessingResult::Submitted,
            Ok(block_io::FileEngineOk::Executed(res)) => {
                ProcessingResult::Executed(res.req.finish(mem, Ok(res.count), block_metrics))
            }
            Err(err) => {
                if err.error.is_throttling_err() {
                    ProcessingResult::Throttled
                } else {
                    let progress = err.error.progress();
                    ProcessingResult::Executed(err.req.finish_with_progress(
                        mem,
                        Err(IoErr::FileEngine(err.error)),
                        progress,
                        block_metrics,
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]

    use vm_memory::GuestMemoryBackend;

    use super::*;
    use crate::devices::virtio::queue::{VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
    use crate::devices::virtio::test_utils::{VirtQueue, default_mem};
    use crate::vstate::memory::{Address, GuestAddress};

    const NUM_DISK_SECTORS: u64 = 1024;

    impl Default for PendingRequest {
        fn default() -> Self {
            PendingRequest {
                r#type: RequestType::In,
                data_len: 0,
                status_addr: Default::default(),
                desc_idx: 0,
            }
        }
    }

    #[test]
    fn test_failed_read_reports_confirmed_prefix() {
        let mem = single_region_mem(0x1000);
        let metrics = BlockDeviceMetrics::default();
        let req = PendingRequest {
            r#type: RequestType::In,
            data_len: 512,
            status_addr: GuestAddress(0x100),
            desc_idx: 7,
        };
        let finished = req.finish_with_progress(
            &mem,
            Err(IoErr::PartialTransfer {
                completed: 100,
                expected: 512,
            }),
            100,
            &metrics,
        );
        assert_eq!(finished.num_bytes_to_mem, 101);
        assert_eq!(finished.desc_idx, 7);
        assert_eq!(
            mem.read_obj::<u8>(GuestAddress(0x100)).unwrap(),
            u8::try_from(VIRTIO_BLK_S_IOERR).unwrap()
        );
    }

    // SG-R4: a refused request names its status byte when its chain was walked to its end and
    // the byte is guest memory: the device completes it with IOERR there instead of leaving the
    // guest the status byte of an earlier request.
    #[test]
    fn test_refused_request_status_location() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 8);
        let parse = |vq: &VirtQueue<'_>| {
            let mut q = vq.create_queue();
            Request::parse_with_status(&q.pop().unwrap().unwrap(), vq.memory(), NUM_DISK_SECTORS)
        };
        // Out of range, and a length that is not whole sectors: the status descriptor.
        multi_chain(&vq, VIRTIO_BLK_T_IN, NUM_DISK_SECTORS, &[(512, true)]);
        let (err, status) = parse(&vq).unwrap_err();
        assert!(matches!(err, VirtioBlockError::InvalidOffset), "{err:?}");
        assert_eq!(status, Some(GuestAddress(0x20000)));
        multi_chain(&vq, VIRTIO_BLK_T_OUT, 0, &[(500, false), (13, false)]);
        let (err, status) = parse(&vq).unwrap_err();
        assert!(
            matches!(err, VirtioBlockError::InvalidDataLength),
            "{err:?}"
        );
        assert_eq!(status, Some(GuestAddress(0x20000)));
        // A data segment outside guest memory: refused before I/O, status named.
        multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &[(512, true), (512, true)]);
        vq.dtable[2].set(0x80000, 512, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 3);
        let (_, status) = parse(&vq).unwrap_err();
        assert_eq!(status, Some(GuestAddress(0x20000)));
        // The last writable byte is the status even inside a longer descriptor.
        multi_chain(&vq, VIRTIO_BLK_T_IN, NUM_DISK_SECTORS, &[(512, true)]);
        vq.dtable[2].set(0x20000, 4, VIRTQ_DESC_F_WRITE, 0);
        assert_eq!(parse(&vq).unwrap_err().1, Some(GuestAddress(0x20003)));
        // Status byte outside guest memory: none.
        multi_chain(&vq, VIRTIO_BLK_T_IN, NUM_DISK_SECTORS, &[(512, true)]);
        vq.dtable[2].set(0x80000, 1, VIRTQ_DESC_F_WRITE, 0);
        assert_eq!(parse(&vq).unwrap_err().1, None);
        // A chain that does not end (a cycle), or an indirect table: none.
        multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &[(256, true), (256, true)]);
        vq.dtable[2].set(0x4000, 256, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 2);
        assert_eq!(parse(&vq).unwrap_err().1, None);
        multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &[(512, true)]);
        vq.dtable[1].set(0x3000, 512, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE | 0x4, 2);
        assert_eq!(parse(&vq).unwrap_err().1, None);
        // No writable byte at all: none.
        multi_chain(&vq, VIRTIO_BLK_T_OUT, 0, &[(512, false)]);
        vq.dtable[2].set(0x20000, 1, 0, 0);
        assert_eq!(parse(&vq).unwrap_err().1, None);
    }

    #[test]
    fn test_read_request_header() {
        let mem = single_region_mem(0x1000);
        let addr = GuestAddress(0);
        let sector = 123_454_321;

        // Test that all supported request types are read correctly from memory.
        let supported_request_types = vec![
            VIRTIO_BLK_T_IN,
            VIRTIO_BLK_T_OUT,
            VIRTIO_BLK_T_DISCARD,
            VIRTIO_BLK_T_FLUSH,
            VIRTIO_BLK_T_GET_ID,
        ];

        for request_type in supported_request_types {
            let expected_header = RequestHeader::new(request_type, sector);
            mem.write_obj::<RequestHeader>(expected_header, addr)
                .unwrap();

            let actual_header = RequestHeader::read_from(&mem, addr).unwrap();
            assert_eq!(actual_header.request_type, expected_header.request_type);
            assert_eq!(actual_header.sector, expected_header.sector);
        }

        // Test that trying to read a request header that goes outside of the
        // memory boundary fails.
        RequestHeader::read_from(&mem, GuestAddress(0x1000)).unwrap_err();
    }

    /// Builds one chain: header (read-only), the data descriptors `(len, write_only)`, status.
    fn multi_chain(
        vq: &VirtQueue<'_>,
        request_type: u32,
        sector: u64,
        data: &[(u32, bool)],
    ) -> Vec<DataSegment> {
        let mem = vq.memory();
        let header_addr = 0x1000u64;
        mem.write_obj(
            RequestHeader::new(request_type, sector),
            GuestAddress(header_addr),
        )
        .unwrap();
        vq.dtable[0].set(header_addr, 16, VIRTQ_DESC_F_NEXT, 1);
        let mut segments = Vec::new();
        for (i, &(len, write_only)) in data.iter().enumerate() {
            let idx = u16::try_from(i + 1).unwrap();
            let addr = 0x2000 + 0x1000 * u64::from(idx);
            let flags = VIRTQ_DESC_F_NEXT | if write_only { VIRTQ_DESC_F_WRITE } else { 0 };
            vq.dtable[usize::from(idx)].set(addr, len, flags, idx + 1);
            segments.push((GuestAddress(addr), len));
        }
        let status = u16::try_from(data.len() + 1).unwrap();
        vq.dtable[usize::from(status)].set(0x20000, 1, VIRTQ_DESC_F_WRITE, 0);
        vq.avail.ring[0].set(0);
        vq.avail.idx.set(1);
        segments
    }

    fn parse_multi(vq: &VirtQueue<'_>, num_sectors: u64) -> Result<Request, VirtioBlockError> {
        let mut q = vq.create_queue();
        Request::parse(&q.pop().unwrap().unwrap(), vq.memory(), num_sectors)
    }

    // Exercise the real parse -> Sync engine -> completion path. The device
    // records a two-sector capacity before the backing file is truncated, so a
    // valid request reads one sector and then encounters EOF.
    fn sync_read_prefix_after_eof(data: &[(u32, bool)], reported: u32) {
        use std::os::unix::fs::FileExt;

        use vmm_sys_util::tempfile::TempFile;

        use crate::devices::virtio::block::virtio::device::{DiskImageFormat, FileEngineType};
        use crate::logger::IncMetric;

        let backing = TempFile::new().unwrap();
        backing.as_file().set_len(1024).unwrap();
        let prefix = [0x3cu8; 512];
        backing.as_file().write_all_at(&prefix, 0).unwrap();
        let mut disk = DiskProperties::new(
            backing.as_path().to_str().unwrap(),
            false,
            FileEngineType::Sync,
            DiskImageFormat::Raw,
            false,
        )
        .unwrap();
        backing.as_file().set_len(512).unwrap();

        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 8);
        let segments = multi_chain(&vq, VIRTIO_BLK_T_IN, 0, data);
        let sentinel = [0xa5u8; 1024];
        for &(addr, len) in &segments {
            mem.write_slice(&sentinel[..usize::try_from(len).unwrap()], addr)
                .unwrap();
        }
        mem.write_obj::<u8>(0x7f, GuestAddress(0x20000)).unwrap();
        let request = parse_multi(&vq, disk.nsectors).unwrap();
        assert_eq!(request.segments.is_empty(), data.len() == 1);
        let metrics = BlockDeviceMetrics::default();
        let ProcessingResult::Executed(finished) =
            request.process(&mut disk, 7, &mem, &metrics, false)
        else {
            panic!("Sync request did not finish inline");
        };
        assert_eq!(
            mem.read_obj::<u8>(GuestAddress(0x20000)).unwrap(),
            u8::try_from(VIRTIO_BLK_S_IOERR).unwrap()
        );
        let mut observed = Vec::new();
        for &(addr, len) in &segments {
            let mut bytes = vec![0; usize::try_from(len).unwrap()];
            mem.read_slice(&mut bytes, addr).unwrap();
            observed.extend(bytes);
        }
        assert_eq!(&observed[..512], &prefix);
        assert_eq!(&observed[512..], &sentinel[512..]);
        assert_eq!(finished.desc_idx, 7);
        assert_eq!(finished.num_bytes_to_mem, reported + 1);
        assert_eq!(metrics.read_bytes.count(), u64::from(reported));
    }

    // A read of one segment keeps the engine's path, which reports no prefix on an error.
    #[test]
    fn test_sync_read_prefix_scalar_after_eof() {
        sync_read_prefix_after_eof(&[(1024, true)], 0);
    }

    #[test]
    fn test_sync_read_prefix_vectored_after_eof() {
        sync_read_prefix_after_eof(&[(512, true), (512, true)], 512);
    }

    #[test]
    fn test_cyclic_data_chain_is_not_a_status_descriptor() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 4);
        multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &[(256, true), (256, true)]);
        // d2 loops to itself. At the traversal limit, it must not be reinterpreted
        // as a status descriptor that overlaps the second data segment.
        vq.dtable[2].set(0x4000, 256, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 2);
        assert!(matches!(
            parse_multi(&vq, NUM_DISK_SECTORS),
            Err(VirtioBlockError::DescriptorChainTooShort)
        ));
    }

    #[test]
    fn test_unnegotiated_indirect_descriptor_rejected() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 4);
        multi_chain(&vq, VIRTIO_BLK_T_OUT, 0, &[(512, false)]);
        vq.dtable[1].flags.set(VIRTQ_DESC_F_NEXT | 0x4);
        assert!(matches!(
            parse_multi(&vq, NUM_DISK_SECTORS),
            Err(VirtioBlockError::DescriptorChainTooShort)
        ));
    }

    #[test]
    fn test_unsupported_command_status_follows_writable_payload() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 4);
        multi_chain(&vq, 10, 0, &[(12, true)]);
        mem.write_obj(0xffu8, GuestAddress(0x20000)).unwrap();
        let req = parse_multi(&vq, NUM_DISK_SECTORS).unwrap();
        assert_eq!(req.status_addr, GuestAddress(0x20000));
        let mut block = crate::devices::virtio::block::virtio::test_utils::default_block(
            crate::devices::virtio::block::virtio::device::FileEngineType::Sync,
        );
        let metrics = block.metrics.clone();
        let disk = &mut block.resources_mut()[0].disk;
        let result = req.process(disk, 0, &mem, &metrics, false);
        assert!(matches!(
            result,
            ProcessingResult::Executed(FinishedRequest {
                num_bytes_to_mem: 1,
                ..
            })
        ));
        assert_eq!(
            mem.read_obj::<u8>(GuestAddress(0x20000)).unwrap(),
            u8::try_from(VIRTIO_BLK_S_UNSUPP).unwrap()
        );
    }

    #[test]
    fn test_modern_out_header_and_payload_share_descriptor() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 4);
        multi_chain(&vq, VIRTIO_BLK_T_OUT, 0, &[(512, false)]);
        vq.dtable[0].set(0x1000, 528, VIRTQ_DESC_F_NEXT, 2);
        let req = parse_multi(&vq, NUM_DISK_SECTORS).unwrap();
        assert_eq!(req.data_len, 512);
        assert_eq!(req.data_addr, GuestAddress(0x1010));
        assert_eq!(req.status_addr, GuestAddress(0x20000));
    }

    #[test]
    fn test_modern_in_data_and_status_share_descriptor() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 4);
        multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &[(512, true)]);
        vq.dtable[1].set(0x3000, 513, VIRTQ_DESC_F_WRITE, 0);
        let req = parse_multi(&vq, NUM_DISK_SECTORS).unwrap();
        assert_eq!(req.data_len, 512);
        assert_eq!(req.data_addr, GuestAddress(0x3000));
        assert_eq!(req.status_addr, GuestAddress(0x3200));
    }

    #[test]
    fn test_modern_split_header() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 8);
        multi_chain(&vq, VIRTIO_BLK_T_OUT, 7, &[(512, false)]);
        // The second half is non-contiguous; the bytes beyond the first 8 in
        // memory are deliberately wrong, so reading past the first descriptor
        // cannot accidentally pass.
        mem.write_slice(&7u64.to_le_bytes(), GuestAddress(0x5000))
            .unwrap();
        mem.write_slice(&0u64.to_le_bytes(), GuestAddress(0x1008))
            .unwrap();
        vq.dtable[0].set(0x1000, 8, VIRTQ_DESC_F_NEXT, 3);
        vq.dtable[3].set(0x5000, 8, VIRTQ_DESC_F_NEXT, 1);
        let req = parse_multi(&vq, NUM_DISK_SECTORS).unwrap();
        assert_eq!(req.sector, 7);
        assert_eq!(req.data_len, 512);
    }

    #[test]
    fn test_split_discard_metadata_is_copied_once() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 8);
        multi_chain(&vq, VIRTIO_BLK_T_DISCARD, 0, &[(8, false), (8, false)]);
        mem.write_slice(&7u64.to_le_bytes(), GuestAddress(0x3000))
            .unwrap();
        mem.write_slice(&1u32.to_le_bytes(), GuestAddress(0x4000))
            .unwrap();
        mem.write_slice(&0u32.to_le_bytes(), GuestAddress(0x4004))
            .unwrap();
        let req = parse_multi(&vq, NUM_DISK_SECTORS).unwrap();
        mem.write_slice(&9u64.to_le_bytes(), GuestAddress(0x3000))
            .unwrap();
        assert_eq!(req.sector, 7 * 512);
        assert_eq!(req.data_len, 512);
    }

    #[test]
    fn test_get_id_and_status_can_share_and_split_writable_stream() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 8);
        multi_chain(&vq, VIRTIO_BLK_T_GET_ID, 0, &[(8, true), (13, true)]);
        vq.dtable[2].flags.set(VIRTQ_DESC_F_WRITE);
        let req = parse_multi(&vq, NUM_DISK_SECTORS).unwrap();
        assert_eq!(
            req.segments(),
            vec![(GuestAddress(0x3000), 8), (GuestAddress(0x4000), 12)]
        );
        assert_eq!(req.status_addr, GuestAddress(0x400c));
        assert_eq!(req.data_len, VIRTIO_BLK_ID_BYTES);

        let mut block = crate::devices::virtio::block::virtio::test_utils::default_block(
            crate::devices::virtio::block::virtio::device::FileEngineType::Sync,
        );
        mem.write_obj(0xffu8, GuestAddress(0x400c)).unwrap();
        let expected = block.disk().image_id;
        let metrics = block.metrics.clone();
        let disk = &mut block.resources_mut()[0].disk;
        let finished = req.process(disk, 0, &mem, &metrics, false);
        assert!(matches!(
            finished,
            ProcessingResult::Executed(FinishedRequest {
                num_bytes_to_mem: 21,
                ..
            })
        ));
        let mut first = [0; 8];
        let mut second = [0; 12];
        mem.read_slice(&mut first, GuestAddress(0x3000)).unwrap();
        mem.read_slice(&mut second, GuestAddress(0x4000)).unwrap();
        assert_eq!(&first, &expected[..8]);
        assert_eq!(&second, &expected[8..]);
        assert_eq!(
            mem.read_obj::<u8>(GuestAddress(0x400c)).unwrap(),
            u8::try_from(VIRTIO_BLK_S_OK).unwrap()
        );
    }

    #[test]
    fn test_payload_spanning_adjacent_guest_regions() {
        let mem = multi_region_mem(&[(GuestAddress(0), 0x4000), (GuestAddress(0x4000), 0x4000)]);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 8);
        multi_chain(&vq, VIRTIO_BLK_T_OUT, 0, &[(512, false)]);
        vq.dtable[1].addr.set(0x3f00);
        vq.dtable[2].addr.set(0x6000);
        let req = parse_multi(&vq, NUM_DISK_SECTORS).unwrap();
        assert_eq!(
            req.segments(),
            vec![(GuestAddress(0x3f00), 256), (GuestAddress(0x4000), 256)]
        );
    }

    #[test]
    fn test_payload_and_status_must_be_mapped_before_io() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 8);
        multi_chain(&vq, VIRTIO_BLK_T_OUT, 0, &[(512, false)]);
        vq.dtable[1].addr.set(0x3ff00);
        assert!(matches!(
            parse_multi(&vq, NUM_DISK_SECTORS),
            Err(VirtioBlockError::GuestMemory(_))
        ));
        vq.dtable[1].addr.set(0x3000);
        vq.dtable[2].addr.set(0x40000);
        assert!(matches!(
            parse_multi(&vq, NUM_DISK_SECTORS),
            Err(VirtioBlockError::GuestMemory(_))
        ));
    }

    #[test]
    fn test_parse_multiple_data_descriptors() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 16);

        // A read of three write-only segments; lengths need not be whole sectors one by one.
        let segs = multi_chain(
            &vq,
            VIRTIO_BLK_T_IN,
            8,
            &[(4096, true), (100, true), (412, true)],
        );
        let req = parse_multi(&vq, NUM_DISK_SECTORS).unwrap();
        assert_eq!(req.r#type, RequestType::In);
        assert_eq!(req.data_len, 4608);
        assert_eq!(req.segments(), segs);
        assert_eq!(req.status_addr, GuestAddress(0x20000));

        // A write of two read-only segments.
        let segs = multi_chain(&vq, VIRTIO_BLK_T_OUT, 0, &[(512, false), (1024, false)]);
        let req = parse_multi(&vq, NUM_DISK_SECTORS).unwrap();
        assert_eq!((req.r#type, req.data_len), (RequestType::Out, 1536));
        assert_eq!(req.segments(), segs);

        // One data descriptor: the single-segment form, exactly as before.
        multi_chain(&vq, VIRTIO_BLK_T_OUT, 0, &[(512, false)]);
        let req = parse_multi(&vq, NUM_DISK_SECTORS).unwrap();
        assert!(req.segments.is_empty());
        assert_eq!((req.data_addr, req.data_len), (GuestAddress(0x3000), 512));

        // The most a 16-entry ring can carry: 14 segments.
        let many: Vec<(u32, bool)> = (0..14).map(|_| (512, true)).collect();
        multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &many);
        assert_eq!(
            parse_multi(&vq, NUM_DISK_SECTORS).unwrap().segments.len(),
            14
        );
    }

    #[test]
    fn test_parse_multiple_data_descriptors_refused() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
        let expect = |vq: &VirtQueue<'_>, f: fn(&VirtioBlockError) -> bool| {
            let err = parse_multi(vq, NUM_DISK_SECTORS).unwrap_err();
            assert!(f(&err), "{err:?}");
        };

        // Readable descriptors cannot follow writable descriptors.
        multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &[(512, true), (512, false)]);
        expect(&vq, |e| {
            matches!(e, VirtioBlockError::UnexpectedReadOnlyDescriptor)
        });

        // A total that is not a whole number of sectors.
        multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &[(512, true), (100, true)]);
        expect(&vq, |e| matches!(e, VirtioBlockError::InvalidDataLength));

        // Lengths that overflow u32.
        multi_chain(
            &vq,
            VIRTIO_BLK_T_OUT,
            0,
            &[(0xFFFF_FE00, false), (0x400, false)],
        );
        expect(&vq, |e| matches!(e, VirtioBlockError::InvalidDataLength));

        // Beyond the end of the disk, counted over all segments.
        multi_chain(
            &vq,
            VIRTIO_BLK_T_OUT,
            NUM_DISK_SECTORS - 2,
            &[(512, false), (1024, false)],
        );
        expect(&vq, |e| matches!(e, VirtioBlockError::InvalidOffset));
        multi_chain(
            &vq,
            VIRTIO_BLK_T_OUT,
            NUM_DISK_SECTORS - 3,
            &[(512, false), (1024, false)],
        );
        parse_multi(&vq, NUM_DISK_SECTORS).unwrap();

        // A discard contains one 16-byte range, not two range records.
        multi_chain(&vq, VIRTIO_BLK_T_DISCARD, 0, &[(16, false), (16, false)]);
        expect(&vq, |e| matches!(e, VirtioBlockError::InvalidDataLength));
        // Status is the last byte of this writable stream; 1023 payload bytes
        // are not sector aligned.
        multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &[(512, true), (512, true)]);
        vq.dtable[2].flags.set(VIRTQ_DESC_F_WRITE);
        expect(&vq, |e| matches!(e, VirtioBlockError::InvalidDataLength));
    }

    #[test]
    fn test_request_type_from() {
        assert_eq!(RequestType::from(VIRTIO_BLK_T_IN), RequestType::In);
        assert_eq!(RequestType::from(VIRTIO_BLK_T_OUT), RequestType::Out);
        assert_eq!(
            RequestType::from(VIRTIO_BLK_T_DISCARD),
            RequestType::Discard
        );
        assert_eq!(RequestType::from(VIRTIO_BLK_T_FLUSH), RequestType::Flush);
        assert_eq!(
            RequestType::from(VIRTIO_BLK_T_GET_ID),
            RequestType::GetDeviceID
        );
        assert_eq!(RequestType::from(42), RequestType::Unsupported(42));
    }

    impl RequestDescriptorChain<'_, '_> {
        fn check_parse_err(&self, _e: VirtioBlockError) {
            let mut q = self.driver_queue.create_queue();
            let memory = self.driver_queue.memory();

            assert!(matches!(
                Request::parse(&q.pop().unwrap().unwrap(), memory, NUM_DISK_SECTORS),
                Err(_e)
            ));
        }

        fn check_parse(&self, check_data: bool) {
            let mut q = self.driver_queue.create_queue();
            let memory = self.driver_queue.memory();
            let request =
                Request::parse(&q.pop().unwrap().unwrap(), memory, NUM_DISK_SECTORS).unwrap();
            let expected_header = self.header();

            assert_eq!(
                request.r#type,
                RequestType::from(expected_header.request_type)
            );
            if request.r#type == RequestType::Discard {
                let range: DiscardWriteZeroes = memory
                    .read_obj(GuestAddress(self.data_desc.addr.get()))
                    .unwrap();
                assert_eq!(request.sector, range.sector * u64::from(SECTOR_SIZE));
                assert_eq!(request.data_len, range.num_sectors * SECTOR_SIZE);
            } else {
                assert_eq!(request.sector, expected_header.sector);
            }

            if check_data && request.r#type != RequestType::Discard {
                assert_eq!(request.data_addr.raw_value(), self.data_desc.addr.get());
                assert_eq!(request.data_len, self.data_desc.len.get());
            }

            assert_eq!(request.status_addr.raw_value(), self.status_desc.addr.get());
        }
    }

    #[test]
    fn test_parse_generic() {
        let mem = &default_mem();
        let queue = VirtQueue::new(GuestAddress(0), mem, 16);
        let chain = RequestDescriptorChain::new(&queue);
        chain.header_desc.len.set(16);
        chain.status_desc.len.set(1);
        let request_header = RequestHeader::new(100, 114);
        chain.set_header(request_header);

        // Write only request type descriptor.
        chain.header_desc.flags.set(VIRTQ_DESC_F_WRITE);
        chain.check_parse_err(VirtioBlockError::UnexpectedWriteOnlyDescriptor);

        // Chain too short: no DATA_DESCRIPTOR.
        chain.header_desc.flags.set(0);
        chain.check_parse_err(VirtioBlockError::DescriptorChainTooShort);

        // Chain too short: no status descriptor.
        chain.header_desc.flags.set(VIRTQ_DESC_F_NEXT);
        chain.data_desc.flags.set(0);
        chain.check_parse_err(VirtioBlockError::DescriptorChainTooShort);

        // Status descriptor not writable.
        chain.data_desc.flags.set(VIRTQ_DESC_F_NEXT);
        chain.status_desc.flags.set(0);
        chain.check_parse_err(VirtioBlockError::UnexpectedReadOnlyDescriptor);

        // Status descriptor too small.
        chain.status_desc.flags.set(VIRTQ_DESC_F_WRITE);
        chain.status_desc.len.set(0);
        chain.check_parse_err(VirtioBlockError::DescriptorLengthTooSmall);

        // The status byte must be mapped before processing.
        chain.status_desc.len.set(1);
        chain.status_desc.addr.set(mem.last_addr().raw_value() + 1);
        chain.check_parse_err(VirtioBlockError::DescriptorLengthTooSmall);
        chain.status_desc.addr.set(0x3000);
        chain.check_parse(false);
    }

    #[test]
    fn test_parse_in() {
        let mem = &default_mem();
        let queue = VirtQueue::new(GuestAddress(0), mem, 16);
        let chain = RequestDescriptorChain::new(&queue);
        chain.header_desc.len.set(16);
        chain.status_desc.len.set(1);

        let mut request_header = RequestHeader::new(VIRTIO_BLK_T_IN, 99);
        chain.set_header(request_header);

        // Read only data descriptor for IN.
        chain.data_desc.flags.set(VIRTQ_DESC_F_NEXT);
        chain.check_parse_err(VirtioBlockError::UnexpectedReadOnlyDescriptor);

        // data_len is not multiple of 512 for IN.
        chain
            .data_desc
            .flags
            .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);
        chain.data_desc.len.set(513);
        chain.check_parse_err(VirtioBlockError::InvalidDataLength);

        // sector is to big.
        request_header.sector = NUM_DISK_SECTORS;
        chain.data_desc.len.set(512);
        chain.set_header(request_header);
        chain.check_parse_err(VirtioBlockError::InvalidOffset);

        // Fix data descriptor.
        request_header.sector = NUM_DISK_SECTORS - 1;
        chain.set_header(request_header);
        chain.check_parse(true);
    }

    #[test]
    fn test_parse_out() {
        let mem = &default_mem();
        let queue = VirtQueue::new(GuestAddress(0), mem, 16);
        let chain = RequestDescriptorChain::new(&queue);
        chain.header_desc.len.set(16);
        chain.status_desc.len.set(1);

        let mut request_header = RequestHeader::new(VIRTIO_BLK_T_OUT, 100);
        chain.set_header(request_header);

        // A readable descriptor after the writable stream is invalid.
        chain
            .data_desc
            .flags
            .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);
        chain.status_desc.flags.set(0);
        chain.check_parse_err(VirtioBlockError::UnexpectedReadOnlyDescriptor);
        chain.status_desc.flags.set(VIRTQ_DESC_F_WRITE);

        // data_len is not multiple of 512 for IN.
        chain.data_desc.flags.set(VIRTQ_DESC_F_NEXT);
        chain.data_desc.len.set(1000);
        chain.check_parse_err(VirtioBlockError::InvalidDataLength);

        // sector is to big.
        request_header.sector = NUM_DISK_SECTORS - 1;
        chain.data_desc.len.set(1024);
        chain.set_header(request_header);
        chain.check_parse_err(VirtioBlockError::InvalidOffset);

        // Fix header descriptor.
        request_header.sector = NUM_DISK_SECTORS - 2;
        chain.set_header(request_header);
        chain.check_parse(true);
    }

    #[test]
    fn test_parse_flush() {
        let mem = &default_mem();
        let queue = VirtQueue::new(GuestAddress(0), mem, 16);
        let chain = RequestDescriptorChain::new(&queue);
        chain.header_desc.len.set(16);
        chain.status_desc.len.set(1);

        // Flush has no data: status is the first byte of the writable stream.
        let request_header = RequestHeader::new(VIRTIO_BLK_T_FLUSH, 0);
        chain.set_header(request_header);
        chain.header_desc.next.set(2);
        chain.check_parse(false);

        // An extra device-readable buffer is discouraged by the driver rules,
        // but it does not make the flush command unsupported.
        chain.header_desc.next.set(1);
        chain.data_desc.flags.set(VIRTQ_DESC_F_NEXT);
        chain.check_parse(false);
    }

    #[test]
    fn test_parse_discard() {
        let mem = &default_mem();
        let queue = VirtQueue::new(GuestAddress(0), mem, 16);
        let chain = RequestDescriptorChain::new(&queue);
        chain.header_desc.len.set(16);
        chain.status_desc.len.set(1);

        let request_header = RequestHeader::new(VIRTIO_BLK_T_DISCARD, 0);
        chain.set_header(request_header);

        // Write only data descriptor for Discard.
        chain
            .data_desc
            .flags
            .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);
        chain.check_parse_err(VirtioBlockError::UnexpectedWriteOnlyDescriptor);

        chain.data_desc.flags.set(VIRTQ_DESC_F_NEXT);
        chain.data_desc.len.set(15);
        chain.check_parse_err(VirtioBlockError::InvalidDataLength);

        let range = DiscardWriteZeroes {
            sector: 0,
            num_sectors: 0,
            flags: 0,
        };
        chain.data_desc.len.set(DISCARD_RANGE_SIZE);
        mem.write_obj(range, GuestAddress(chain.data_desc.addr.get()))
            .unwrap();
        chain.check_parse_err(VirtioBlockError::InvalidDataLength);

        let range = DiscardWriteZeroes {
            sector: 0,
            num_sectors: 1,
            flags: 1,
        };
        mem.write_obj(range, GuestAddress(chain.data_desc.addr.get()))
            .unwrap();
        let mut queue = chain.driver_queue.create_queue();
        let request =
            Request::parse(&queue.pop().unwrap().unwrap(), mem, NUM_DISK_SECTORS).unwrap();
        assert_eq!(
            request.r#type,
            RequestType::Unsupported(VIRTIO_BLK_T_DISCARD)
        );
        mem.write_obj(0xffu8, GuestAddress(0x3000)).unwrap();
        let mut block = crate::devices::virtio::block::virtio::test_utils::default_block(
            crate::devices::virtio::block::virtio::device::FileEngineType::Sync,
        );
        let metrics = block.metrics.clone();
        let disk = &mut block.resources_mut()[0].disk;
        let result = request.process(disk, 0, mem, &metrics, true);
        assert!(matches!(
            result,
            ProcessingResult::Executed(FinishedRequest {
                num_bytes_to_mem: 1,
                ..
            })
        ));
        assert_eq!(
            mem.read_obj::<u8>(GuestAddress(0x3000)).unwrap(),
            u8::try_from(VIRTIO_BLK_S_UNSUPP).unwrap()
        );

        let range = DiscardWriteZeroes {
            sector: 0,
            num_sectors: MAX_DISCARD_SECTORS + 1,
            flags: 0,
        };
        mem.write_obj(range, GuestAddress(chain.data_desc.addr.get()))
            .unwrap();
        chain.check_parse_err(VirtioBlockError::InvalidDataLength);

        let range = DiscardWriteZeroes {
            sector: NUM_DISK_SECTORS - 1,
            num_sectors: 2,
            flags: 0,
        };
        chain.data_desc.len.set(DISCARD_RANGE_SIZE);
        mem.write_obj(range, GuestAddress(chain.data_desc.addr.get()))
            .unwrap();
        chain.check_parse_err(VirtioBlockError::InvalidOffset);

        let range = DiscardWriteZeroes {
            sector: NUM_DISK_SECTORS - 1,
            num_sectors: 1,
            flags: 0,
        };
        mem.write_obj(range, GuestAddress(chain.data_desc.addr.get()))
            .unwrap();
        chain.check_parse(true);
    }

    #[test]
    fn test_parse_get_id() {
        let mem = &default_mem();
        let queue = VirtQueue::new(GuestAddress(0), mem, 16);
        let chain = RequestDescriptorChain::new(&queue);
        chain.header_desc.len.set(16);
        chain.status_desc.len.set(1);

        let request_header = RequestHeader::new(VIRTIO_BLK_T_GET_ID, 15);
        chain.set_header(request_header);

        // Read only data descriptor for GetDeviceId.
        chain.data_desc.flags.set(VIRTQ_DESC_F_NEXT);
        chain.check_parse_err(VirtioBlockError::UnexpectedReadOnlyDescriptor);

        // data_len is < VIRTIO_BLK_ID_BYTES for GetDeviceID.
        chain
            .data_desc
            .flags
            .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);
        chain.data_desc.len.set(VIRTIO_BLK_ID_BYTES - 1);
        chain.check_parse_err(VirtioBlockError::InvalidDataLength);

        chain.data_desc.len.set(VIRTIO_BLK_ID_BYTES);
        chain.check_parse(true);
    }

    use proptest::prelude::*;

    use crate::devices::virtio::block::virtio::test_utils::RequestDescriptorChain;
    use crate::test_utils::{multi_region_mem, single_region_mem};

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1000))]
        #[test]
        fn parse_random_rw_streams(is_read in any::<bool>(), first in 1u32..1024,
                                   sector in 0u64..NUM_DISK_SECTORS) {
            let mem = single_region_mem(0x40000);
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            let (kind, writable) = if is_read { (VIRTIO_BLK_T_IN, true) }
                                   else { (VIRTIO_BLK_T_OUT, false) };
            let segments = multi_chain(&vq, kind, sector, &[(first, writable), (1024-first, writable)]);
            let parsed = parse_multi(&vq, NUM_DISK_SECTORS);
            if sector + 2 > NUM_DISK_SECTORS {
                prop_assert!(matches!(parsed, Err(VirtioBlockError::InvalidOffset)));
            } else {
                let req = parsed.unwrap();
                prop_assert_eq!(req.data_len, 1024);
                prop_assert_eq!(req.segments(), segments);
                prop_assert_eq!(req.status_addr, GuestAddress(0x20000));
            }
        }

        #[test]
        fn parse_random_other_commands(kind in 0u8..4, split in 1usize..16,
                                       discard_sector in 0u64..NUM_DISK_SECTORS,
                                       split_header in any::<bool>(), unknown_writable in any::<bool>()) {
            let mem = single_region_mem(0x40000);
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            let operation = match kind {
                0 => VIRTIO_BLK_T_FLUSH,
                1 => VIRTIO_BLK_T_DISCARD,
                2 => VIRTIO_BLK_T_GET_ID,
                _ => 0x1000,
            };
            match kind {
                0 => { multi_chain(&vq, operation, 0, &[]); }
                1 => {
                    multi_chain(&vq, operation, 0, &[(u32::try_from(split).unwrap(), false), (u32::try_from(16-split).unwrap(), false)]);
                    let mut range = [0u8; 16];
                    range[..8].copy_from_slice(&discard_sector.to_le_bytes());
                    range[8..12].copy_from_slice(&1u32.to_le_bytes());
                    mem.write_slice(&range[..split], GuestAddress(0x3000)).unwrap();
                    mem.write_slice(&range[split..], GuestAddress(0x4000)).unwrap();
                }
                2 => { multi_chain(&vq, operation, 0, &[(u32::try_from(split).unwrap(), true), (u32::try_from(20-split).unwrap(), true)]); }
                _ => { multi_chain(&vq, operation, 0, &[(7, unknown_writable)]); }
            }
            if split_header {
                let next = vq.dtable[0].next.get();
                let mut second = [0u8; 8];
                mem.read_slice(&mut second, GuestAddress(0x1008)).unwrap();
                mem.write_slice(&second, GuestAddress(0x6000)).unwrap();
                vq.dtable[0].set(0x1000, 8, VIRTQ_DESC_F_NEXT, 4);
                vq.dtable[4].set(0x6000, 8, VIRTQ_DESC_F_NEXT, next);
            }
            let req = parse_multi(&vq, NUM_DISK_SECTORS).unwrap();
            match kind {
                0 => prop_assert_eq!(req.r#type, RequestType::Flush),
                1 => {
                    prop_assert_eq!(req.r#type, RequestType::Discard);
                    prop_assert_eq!(req.sector, discard_sector * 512);
                    prop_assert_eq!(req.data_len, 512);
                }
                2 => {
                    prop_assert_eq!(req.r#type, RequestType::GetDeviceID);
                    prop_assert_eq!(req.data_len, VIRTIO_BLK_ID_BYTES);
                }
                _ => prop_assert_eq!(req.r#type, RequestType::Unsupported(operation)),
            }
            prop_assert_eq!(req.status_addr, GuestAddress(0x20000));
        }

        #[test]
        fn reject_random_malformed_chains(first in 1u32..512, use_cycle in any::<bool>()) {
            let mem = single_region_mem(0x40000);
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &[(first, true), (512-first, true)]);
            if use_cycle {
                vq.dtable[2].next.set(2);
            } else {
                vq.dtable[2].flags.set(VIRTQ_DESC_F_NEXT);
            }
            prop_assert!(parse_multi(&vq, NUM_DISK_SECTORS).is_err());
        }
    }

    // Requests of several data segments from a hostile driver. Each chain is built in guest
    // memory as a driver would build it, and goes through the parser and, when the parser
    // accepts it, through the Sync engine of a disk backed by a real file.

    const HOSTILE_DISK_SECTORS: u64 = 512;

    /// A disk of `HOSTILE_DISK_SECTORS` sectors of 0xdd on the Sync engine, with its file.
    fn hostile_disk() -> (vmm_sys_util::tempfile::TempFile, DiskProperties) {
        use std::os::unix::fs::FileExt;

        use crate::devices::virtio::block::virtio::device::{DiskImageFormat, FileEngineType};

        let backing = vmm_sys_util::tempfile::TempFile::new().unwrap();
        let len = usize::try_from(HOSTILE_DISK_SECTORS).unwrap() * 512;
        backing.as_file().write_all_at(&vec![0xdd; len], 0).unwrap();
        let disk = DiskProperties::new(
            backing.as_path().to_str().unwrap(),
            false,
            FileEngineType::Sync,
            DiskImageFormat::Raw,
            false,
        )
        .unwrap();
        assert_eq!(disk.nsectors, HOSTILE_DISK_SECTORS);
        (backing, disk)
    }

    /// Whether the disk's file still holds what `hostile_disk` wrote, and nothing else.
    fn disk_untouched(backing: &vmm_sys_util::tempfile::TempFile) -> bool {
        use std::os::unix::fs::FileExt;

        let len = usize::try_from(HOSTILE_DISK_SECTORS).unwrap() * 512;
        let mut bytes = vec![0u8; len];
        backing.as_file().read_exact_at(&mut bytes, 0).unwrap();
        backing.as_file().metadata().unwrap().len() == len as u64 && bytes == vec![0xdd; len]
    }

    /// Parses the chain in `vq` and, if the parser accepts it, runs it on `disk`. Returns the
    /// length the device would put in the used ring.
    fn run_chain(vq: &VirtQueue<'_>, disk: &mut DiskProperties) -> Result<u32, VirtioBlockError> {
        let request = parse_multi(vq, disk.nsectors)?;
        let metrics = BlockDeviceMetrics::default();
        match request.process(disk, 0, vq.memory(), &metrics, false) {
            ProcessingResult::Executed(finished) => Ok(finished.num_bytes_to_mem),
            other => panic!("the Sync engine finishes a request inline: {other:?}"),
        }
    }

    /// The status byte the device wrote for the chain of `multi_chain` or `long_chain`.
    fn status_at(mem: &GuestMemoryMmap, addr: u64) -> u32 {
        u32::from(mem.read_obj::<u8>(GuestAddress(addr)).unwrap())
    }

    /// A chain of `segments` data descriptors of one sector each, far from the queue's own
    /// memory so that the queue may have 256 entries: the header at 0x10000, segment `i`
    /// (from 1) at `0x20000 + i * 0x1000`, the status at 0x1ff000.
    fn long_chain(
        vq: &VirtQueue<'_>,
        request_type: u32,
        segments: u16,
        write_only: bool,
    ) -> Vec<DataSegment> {
        let mem = vq.memory();
        mem.write_obj(RequestHeader::new(request_type, 0), GuestAddress(0x10000))
            .unwrap();
        vq.dtable[0].set(0x10000, 16, VIRTQ_DESC_F_NEXT, 1);
        let mut data = Vec::new();
        for idx in 1..=segments {
            let addr = 0x20000 + 0x1000 * u64::from(idx);
            let flags = VIRTQ_DESC_F_NEXT | if write_only { VIRTQ_DESC_F_WRITE } else { 0 };
            vq.dtable[usize::from(idx)].set(addr, 512, flags, idx + 1);
            data.push((GuestAddress(addr), 512));
        }
        vq.dtable[usize::from(segments) + 1].set(0x1ff000, 1, VIRTQ_DESC_F_WRITE, 0);
        vq.avail.ring[0].set(0);
        vq.avail.idx.set(1);
        data
    }

    // Readable and writable descriptors in the wrong order: refused, the disk untouched.
    #[test]
    fn test_hostile_descriptor_order() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
        let (backing, mut disk) = hostile_disk();

        // A write with a device-writable descriptor between its data descriptors.
        multi_chain(
            &vq,
            VIRTIO_BLK_T_OUT,
            0,
            &[(512, false), (512, true), (512, false)],
        );
        assert!(matches!(
            run_chain(&vq, &mut disk),
            Err(VirtioBlockError::UnexpectedReadOnlyDescriptor)
        ));
        // A read with a device-readable descriptor after or before its data descriptors.
        for data in [
            [(512, true), (512, false), (512, true)],
            [(512, false), (512, true), (512, true)],
        ] {
            multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &data);
            assert!(matches!(
                run_chain(&vq, &mut disk),
                Err(VirtioBlockError::UnexpectedReadOnlyDescriptor)
            ));
        }
        // A read without a single writable byte, not even for the status.
        multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &[(512, false)]);
        vq.dtable[2].flags.set(0);
        run_chain(&vq, &mut disk).unwrap_err();
        assert!(disk_untouched(&backing));

        // A write whose data descriptors are all device-writable carries no data. It is a
        // write of nothing, and its status goes to the last writable byte of the chain.
        multi_chain(&vq, VIRTIO_BLK_T_OUT, 0, &[(512, true), (512, true)]);
        mem.write_obj(0xffu8, GuestAddress(0x20000)).unwrap();
        assert_eq!(run_chain(&vq, &mut disk).unwrap(), 1);
        assert_eq!(status_at(&mem, 0x20000), VIRTIO_BLK_S_OK);
        assert!(disk_untouched(&backing));
    }

    // Descriptors of no bytes are not segments.
    #[test]
    fn test_hostile_zero_length_segments() {
        use std::os::unix::fs::FileExt;

        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
        let (backing, mut disk) = hostile_disk();

        let segments = multi_chain(
            &vq,
            VIRTIO_BLK_T_OUT,
            1,
            &[
                (0, false),
                (512, false),
                (0, false),
                (512, false),
                (0, false),
            ],
        );
        mem.write_slice(&[0x11; 512], segments[1].0).unwrap();
        mem.write_slice(&[0x22; 512], segments[3].0).unwrap();
        let request = parse_multi(&vq, disk.nsectors).unwrap();
        assert_eq!(request.segments(), vec![segments[1], segments[3]]);
        assert_eq!(run_chain(&vq, &mut disk).unwrap(), 1);
        assert_eq!(status_at(&mem, 0x20000), VIRTIO_BLK_S_OK);
        let mut written = [0u8; 1024];
        backing.as_file().read_exact_at(&mut written, 512).unwrap();
        assert_eq!(&written[..512], &[0x11; 512]);
        assert_eq!(&written[512..], &[0x22; 512]);

        // Only empty data descriptors: a read of nothing.
        multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &[(0, true), (0, true)]);
        let request = parse_multi(&vq, disk.nsectors).unwrap();
        assert_eq!(request.data_len, 0);
        assert_eq!(run_chain(&vq, &mut disk).unwrap(), 1);
    }

    // A data descriptor that crosses from one guest memory region into the next is read as
    // two segments; one that runs into a hole between regions is refused with nothing read.
    #[test]
    fn test_hostile_segment_across_regions() {
        let (_backing, mut disk) = hostile_disk();

        let mem = multi_region_mem(&[(GuestAddress(0), 0x4000), (GuestAddress(0x4000), 0x3c000)]);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
        multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &[(512, true), (512, true)]);
        vq.dtable[1].addr.set(0x3f00);
        vq.dtable[2].addr.set(0x6000);
        mem.write_slice(&[0xa5; 512], GuestAddress(0x3f00)).unwrap();
        let request = parse_multi(&vq, disk.nsectors).unwrap();
        assert_eq!(
            request.segments(),
            vec![
                (GuestAddress(0x3f00), 256),
                (GuestAddress(0x4000), 256),
                (GuestAddress(0x6000), 512),
            ]
        );
        assert_eq!(run_chain(&vq, &mut disk).unwrap(), 1025);
        let mut read = [0u8; 512];
        mem.read_slice(&mut read, GuestAddress(0x3f00)).unwrap();
        assert_eq!(read, [0xdd; 512]);

        let mem = multi_region_mem(&[(GuestAddress(0), 0x4000), (GuestAddress(0x8000), 0x38000)]);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
        multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &[(512, true), (512, true)]);
        vq.dtable[1].addr.set(0x3f00);
        vq.dtable[2].addr.set(0x9000);
        mem.write_slice(&[0xa5; 256], GuestAddress(0x3f00)).unwrap();
        mem.write_slice(&[0xa5; 512], GuestAddress(0x9000)).unwrap();
        assert!(matches!(
            run_chain(&vq, &mut disk),
            Err(VirtioBlockError::GuestMemory(_))
        ));
        let mut before_hole = [0u8; 256];
        mem.read_slice(&mut before_hole, GuestAddress(0x3f00))
            .unwrap();
        assert_eq!(before_hole, [0xa5; 256]);
        let mut other = [0u8; 512];
        mem.read_slice(&mut other, GuestAddress(0x9000)).unwrap();
        assert_eq!(other, [0xa5; 512]);
    }

    // A data descriptor outside guest memory in the middle of a chain: the request is refused
    // by the parser, before any host I/O. A write has written nothing of the segments before
    // it to the disk, and a read has put nothing in them.
    #[test]
    fn test_hostile_segment_outside_guest_memory() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
        let (backing, mut disk) = hostile_disk();

        for bad in [
            // Past the end of guest memory.
            0x40000,
            // Starting inside it and running past its end.
            0x3ff00,
            // Wrapping around the address space.
            u64::MAX - 255,
        ] {
            let segments = multi_chain(
                &vq,
                VIRTIO_BLK_T_OUT,
                0,
                &[(512, false), (512, false), (512, false)],
            );
            mem.write_slice(&[0x33; 512], segments[0].0).unwrap();
            vq.dtable[2].addr.set(bad);
            assert!(matches!(
                run_chain(&vq, &mut disk),
                Err(VirtioBlockError::GuestMemory(_))
            ));
            assert!(disk_untouched(&backing));

            let segments = multi_chain(
                &vq,
                VIRTIO_BLK_T_IN,
                0,
                &[(512, true), (512, true), (512, true)],
            );
            mem.write_slice(&[0xa5; 512], segments[0].0).unwrap();
            mem.write_slice(&[0xa5; 512], segments[2].0).unwrap();
            vq.dtable[2].addr.set(bad);
            assert!(matches!(
                run_chain(&vq, &mut disk),
                Err(VirtioBlockError::GuestMemory(_))
            ));
            for segment in [segments[0], segments[2]] {
                let mut bytes = [0u8; 512];
                mem.read_slice(&mut bytes, segment.0).unwrap();
                assert_eq!(bytes, [0xa5; 512]);
            }
        }
    }

    // Lengths that overflow, and totals that are not whole sectors or do not fit the disk:
    // refused from the lengths alone, whatever the addresses are.
    #[test]
    fn test_hostile_lengths() {
        let mem = single_region_mem(0x40000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
        let (backing, mut disk) = hostile_disk();

        // More than 2^32 bytes in the chain.
        multi_chain(
            &vq,
            VIRTIO_BLK_T_OUT,
            0,
            &[
                (0x8000_0000, false),
                (0x8000_0000, false),
                (0x8000_0000, false),
            ],
        );
        assert!(matches!(
            run_chain(&vq, &mut disk),
            Err(VirtioBlockError::InvalidDataLength)
        ));
        multi_chain(
            &vq,
            VIRTIO_BLK_T_IN,
            0,
            &[(u32::MAX, true), (u32::MAX, true), (u32::MAX, true)],
        );
        assert!(matches!(
            run_chain(&vq, &mut disk),
            Err(VirtioBlockError::InvalidDataLength)
        ));
        // A total that is not a whole number of sectors, from segments of whole sectors or not.
        for data in [[(512, false), (488, false)], [(100, false), (300, false)]] {
            multi_chain(&vq, VIRTIO_BLK_T_OUT, 0, &data);
            assert!(matches!(
                run_chain(&vq, &mut disk),
                Err(VirtioBlockError::InvalidDataLength)
            ));
        }
        // Whole sectors from segments that are not: accepted.
        multi_chain(&vq, VIRTIO_BLK_T_IN, 0, &[(100, true), (412, true)]);
        assert_eq!(run_chain(&vq, &mut disk).unwrap(), 513);
        // One sector past the end of the disk, counted over the segments.
        multi_chain(
            &vq,
            VIRTIO_BLK_T_OUT,
            HOSTILE_DISK_SECTORS - 2,
            &[(512, false), (512, false), (512, false)],
        );
        assert!(matches!(
            run_chain(&vq, &mut disk),
            Err(VirtioBlockError::InvalidOffset)
        ));
        // A sector that overflows with the length.
        multi_chain(
            &vq,
            VIRTIO_BLK_T_OUT,
            u64::MAX,
            &[(512, false), (512, false)],
        );
        assert!(matches!(
            run_chain(&vq, &mut disk),
            Err(VirtioBlockError::InvalidOffset)
        ));
        assert!(disk_untouched(&backing));
    }

    // More data segments than the 32 the device advertises, up to the longest chain a queue
    // of 256 holds (254 data descriptors): the device does not hold a driver to `seg_max`,
    // and such a request is written and read back whole, in order.
    #[test]
    fn test_hostile_more_segments_than_advertised() {
        use std::os::unix::fs::FileExt;

        for count in [33u16, 253, 254] {
            let mem = single_region_mem(0x200000);
            let vq = VirtQueue::new(GuestAddress(0), &mem, 256);
            let (backing, mut disk) = hostile_disk();

            let segments = long_chain(&vq, VIRTIO_BLK_T_OUT, count, false);
            let mut expected = Vec::new();
            for (i, &(addr, len)) in segments.iter().enumerate() {
                let bytes = vec![u8::try_from(i % 251).unwrap(); usize::try_from(len).unwrap()];
                mem.write_slice(&bytes, addr).unwrap();
                expected.extend(bytes);
            }
            let request = parse_multi(&vq, disk.nsectors).unwrap();
            assert_eq!(request.segments(), segments);
            assert_eq!(request.data_len, u32::from(count) * 512);
            assert_eq!(run_chain(&vq, &mut disk).unwrap(), 1);
            assert_eq!(status_at(&mem, 0x1ff000), VIRTIO_BLK_S_OK);
            let mut written = vec![0u8; expected.len()];
            backing.as_file().read_exact_at(&mut written, 0).unwrap();
            assert_eq!(written, expected);

            let segments = long_chain(&vq, VIRTIO_BLK_T_IN, count, true);
            for &(addr, len) in &segments {
                mem.write_slice(&vec![0u8; usize::try_from(len).unwrap()], addr)
                    .unwrap();
            }
            assert_eq!(
                run_chain(&vq, &mut disk).unwrap(),
                u32::from(count) * 512 + 1
            );
            let mut read = Vec::new();
            for &(addr, len) in &segments {
                let mut bytes = vec![0u8; usize::try_from(len).unwrap()];
                mem.read_slice(&mut bytes, addr).unwrap();
                read.extend(bytes);
            }
            assert_eq!(read, expected);
        }
    }

    // A chain that loops back on itself never reaches a status descriptor.
    #[test]
    fn test_hostile_looping_chain() {
        let mem = single_region_mem(0x200000);
        let vq = VirtQueue::new(GuestAddress(0), &mem, 256);
        let (backing, mut disk) = hostile_disk();

        long_chain(&vq, VIRTIO_BLK_T_OUT, 8, false);
        // The last data descriptor points back at the first.
        vq.dtable[8].next.set(1);
        run_chain(&vq, &mut disk).unwrap_err();
        assert!(disk_untouched(&backing));
    }
}
