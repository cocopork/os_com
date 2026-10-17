// SPDX-License-Identifier: MPL-2.0

//! Storage-independent access to RNVPC memory.

use ostd::mm::USegment;

use crate::{fs::rnvpc::id::NvPageId, prelude::*};

/// Byte access does not serialize unrelated objects. Callers must retain object
/// lifetime and coordinate overlapping accesses using pins/mutation leases;
/// bulk reads/writes are not atomic snapshots. User copies may fault or sleep.
pub(crate) trait NvMemoryBackend: Send + Sync {
    fn capacity_pages(&self) -> usize;

    fn control_len(&self) -> usize;

    fn read_page(&self, id: NvPageId, output: &mut [u8]) -> Result<()>;

    /// Copies one NV-page range directly into the caller's destination.
    fn copy_to_writer(
        &self,
        id: NvPageId,
        offset: usize,
        writer: &mut VmWriter,
        len: usize,
    ) -> Result<usize>;

    /// Returns a DMA-capable handle to exactly one NV data page.
    fn data_page_segment(&self, id: NvPageId) -> Result<USegment>;

    fn write_page(&self, id: NvPageId, input: &[u8]) -> Result<()>;

    fn write_page_range(&self, id: NvPageId, offset: usize, input: &[u8]) -> Result<()>;

    /// Copies at most `len` bytes directly from the caller's address space into
    /// one NV page and returns the copied prefix length.
    fn copy_from_reader(
        &self,
        id: NvPageId,
        offset: usize,
        reader: &mut VmReader,
        len: usize,
    ) -> Result<usize>;

    fn zero_page_range(&self, id: NvPageId, offset: usize, len: usize) -> Result<()>;

    /// Stores one naturally aligned machine word atomically within a data page.
    /// Persistence still requires the caller to flush the containing range and fence.
    fn store_page_u64(&self, id: NvPageId, offset: usize, value: u64) -> Result<()>;

    fn read_control(&self, offset: usize, output: &mut [u8]) -> Result<()>;

    fn write_control(&self, offset: usize, input: &[u8]) -> Result<()>;

    fn load_control_u64(&self, offset: usize) -> Result<u64>;

    fn store_control_u64(&self, offset: usize, value: u64) -> Result<()>;

    fn persist(&self, offset: usize, len: usize) -> Result<()>;

    fn persist_control(&self, offset: usize, len: usize) -> Result<()>;

    fn fence(&self) -> Result<()>;
}
