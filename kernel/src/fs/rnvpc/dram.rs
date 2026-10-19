// SPDX-License-Identifier: MPL-2.0

//! DRAM-backed implementation of RNVPC memory.

use core::sync::atomic::{AtomicU64, Ordering, compiler_fence};

use ostd::mm::{
    FrameAllocOptions, HasSize, Segment, USegment, VmIo, VmIoFill, VmIoOnce,
    io::util::HasVmReaderWriter,
};

use super::{
    backend::NvMemoryBackend,
    config::{NvSimMode, RnvpcConfig},
    id::NvPageId,
    layout::control_len,
};
use crate::prelude::*;

const DEFAULT_CHUNK_PAGES: usize = 512;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DramNvBackendStats {
    pub(crate) persist_calls: u64,
    pub(crate) fence_calls: u64,
}

struct ImageBytes {
    chunks: Vec<Segment<()>>,
    control_chunks: Vec<Segment<()>>,
}

impl ImageBytes {
    fn allocate(capacity_pages: usize, chunk_pages: usize, control_bytes: usize) -> Result<Self> {
        let chunk_count = capacity_pages.div_ceil(chunk_pages);
        let mut chunks = Vec::new();
        chunks.try_reserve_exact(chunk_count).map_err(|_| {
            Error::with_message(Errno::ENOMEM, "RNVPC chunk table allocation failed")
        })?;
        let mut remaining = capacity_pages;
        while remaining > 0 {
            let pages = remaining.min(chunk_pages);
            chunks.push(FrameAllocOptions::new().alloc_segment(pages)?);
            remaining -= pages;
        }
        let mut control_chunks = Vec::new();
        let mut remaining = control_bytes.div_ceil(PAGE_SIZE);
        while remaining > 0 {
            let pages = remaining.min(chunk_pages);
            control_chunks.push(FrameAllocOptions::new().alloc_segment(pages)?);
            remaining -= pages;
        }
        Ok(Self {
            chunks,
            control_chunks,
        })
    }

    fn read_data(&self, chunk_pages: usize, offset: usize, output: &mut [u8]) -> Result<()> {
        let mut copied = 0;
        while copied < output.len() {
            let absolute = offset + copied;
            let chunk_index = absolute / (chunk_pages * PAGE_SIZE);
            let chunk_offset = absolute % (chunk_pages * PAGE_SIZE);
            let amount =
                (output.len() - copied).min(self.chunks[chunk_index].size() - chunk_offset);
            self.chunks[chunk_index]
                .read_bytes(chunk_offset, &mut output[copied..copied + amount])?;
            copied += amount;
        }
        Ok(())
    }

    fn write_data(&self, chunk_pages: usize, offset: usize, input: &[u8]) -> Result<()> {
        let mut copied = 0;
        while copied < input.len() {
            let absolute = offset + copied;
            let chunk_index = absolute / (chunk_pages * PAGE_SIZE);
            let chunk_offset = absolute % (chunk_pages * PAGE_SIZE);
            let amount = (input.len() - copied).min(self.chunks[chunk_index].size() - chunk_offset);
            self.chunks[chunk_index].write_bytes(chunk_offset, &input[copied..copied + amount])?;
            copied += amount;
        }
        Ok(())
    }

    fn write_data_once(&self, chunk_pages: usize, offset: usize, value: u64) -> Result<()> {
        let chunk_bytes = chunk_pages * PAGE_SIZE;
        self.chunks[offset / chunk_bytes]
            .write_once(offset % chunk_bytes, &value)
            .map_err(Into::into)
    }

    fn read_control(&self, chunk_pages: usize, offset: usize, output: &mut [u8]) -> Result<()> {
        read_chunks(&self.control_chunks, chunk_pages, offset, output)
    }

    fn write_control(&self, chunk_pages: usize, offset: usize, input: &[u8]) -> Result<()> {
        write_chunks(&self.control_chunks, chunk_pages, offset, input)
    }

    fn read_control_once(&self, chunk_pages: usize, offset: usize) -> Result<u64> {
        let chunk_bytes = chunk_pages * PAGE_SIZE;
        self.control_chunks[offset / chunk_bytes]
            .read_once(offset % chunk_bytes)
            .map_err(Into::into)
    }

    fn write_control_once(&self, chunk_pages: usize, offset: usize, value: u64) -> Result<()> {
        let chunk_bytes = chunk_pages * PAGE_SIZE;
        self.control_chunks[offset / chunk_bytes]
            .write_once(offset % chunk_bytes, &value)
            .map_err(Into::into)
    }
}

fn read_chunks(
    chunks: &[Segment<()>],
    chunk_pages: usize,
    offset: usize,
    output: &mut [u8],
) -> Result<()> {
    let mut copied = 0;
    while copied < output.len() {
        let absolute = offset + copied;
        let chunk_index = absolute / (chunk_pages * PAGE_SIZE);
        let chunk_offset = absolute % (chunk_pages * PAGE_SIZE);
        let amount = (output.len() - copied).min(chunks[chunk_index].size() - chunk_offset);
        chunks[chunk_index].read_bytes(chunk_offset, &mut output[copied..copied + amount])?;
        copied += amount;
    }
    Ok(())
}

fn write_chunks(
    chunks: &[Segment<()>],
    chunk_pages: usize,
    offset: usize,
    input: &[u8],
) -> Result<()> {
    let mut copied = 0;
    while copied < input.len() {
        let absolute = offset + copied;
        let chunk_index = absolute / (chunk_pages * PAGE_SIZE);
        let chunk_offset = absolute % (chunk_pages * PAGE_SIZE);
        let amount = (input.len() - copied).min(chunks[chunk_index].size() - chunk_offset);
        chunks[chunk_index].write_bytes(chunk_offset, &input[copied..copied + amount])?;
        copied += amount;
    }
    Ok(())
}

struct DramNvImageInner {
    working: ImageBytes,
    #[cfg(ktest)]
    durable: Option<ImageBytes>,
    skip_persistence: bool,
    chunk_pages: usize,
    capacity_pages: usize,
    control_bytes: usize,
    epoch: AtomicU64,
    persist_calls: AtomicU64,
    fence_calls: AtomicU64,
    // Only crash-model fixtures serialize snapshots; ordinary access uses caller leases.
    #[cfg(ktest)]
    io_lock: Mutex<()>,
}

#[derive(Clone)]
pub(crate) struct DramNvImage {
    inner: Arc<DramNvImageInner>,
}

impl DramNvImage {
    fn allocate_inner(config: RnvpcConfig, chunk_pages: usize, model: bool) -> Result<Self> {
        #[cfg(not(ktest))]
        if model || config.mode() == NvSimMode::Model {
            return_errno_with_message!(Errno::EOPNOTSUPP, "RNVPC crash model is test-only");
        }
        if chunk_pages == 0 {
            return_errno_with_message!(Errno::EINVAL, "RNVPC chunk size must be nonzero");
        }
        let capacity_pages = config.total_pages();
        let control_bytes = control_len(capacity_pages)?;
        let working = ImageBytes::allocate(capacity_pages, chunk_pages, control_bytes)?;
        #[cfg(ktest)]
        let durable = if model || config.mode() == NvSimMode::Model {
            Some(ImageBytes::allocate(
                capacity_pages,
                chunk_pages,
                control_bytes,
            )?)
        } else {
            None
        };
        Ok(Self {
            inner: Arc::new(DramNvImageInner {
                working,
                #[cfg(ktest)]
                durable,
                skip_persistence: !model && config.mode() == NvSimMode::FastNoPersist,
                chunk_pages,
                capacity_pages,
                control_bytes,
                epoch: AtomicU64::new(0),
                persist_calls: AtomicU64::new(0),
                fence_calls: AtomicU64::new(0),
                #[cfg(ktest)]
                io_lock: Mutex::new(()),
            }),
        })
    }

    #[cfg(ktest)]
    pub(super) fn allocate_model(config: RnvpcConfig) -> Result<Self> {
        if config.mode() != NvSimMode::Model {
            return_errno_with_message!(Errno::EINVAL, "model image requires nv_sim_mode=model");
        }
        Self::allocate_inner(config, DEFAULT_CHUNK_PAGES, true)
    }

    pub(crate) fn attach(&self) -> DramNvBackend {
        DramNvBackend {
            image: self.clone(),
            epoch: self.inner.epoch.load(Ordering::Acquire),
        }
    }

    #[cfg(ktest)]
    pub(super) fn simulate_crash(&self) {
        let _guard = self.inner.io_lock.lock();
        if let Some(durable) = &self.inner.durable {
            let mut page = [0; PAGE_SIZE];
            for index in 0..self.inner.capacity_pages {
                let offset = index * PAGE_SIZE;
                durable
                    .read_data(self.inner.chunk_pages, offset, &mut page)
                    .unwrap();
                self.inner
                    .working
                    .write_data(self.inner.chunk_pages, offset, &page)
                    .unwrap();
            }
            let mut control = vec![0; self.inner.control_bytes];
            durable
                .read_control(self.inner.chunk_pages, 0, &mut control)
                .unwrap();
            self.inner
                .working
                .write_control(self.inner.chunk_pages, 0, &control)
                .unwrap();
        }
        self.inner.epoch.fetch_add(1, Ordering::Release);
    }
}

pub(crate) struct DramNvBackend {
    image: DramNvImage,
    epoch: u64,
}

impl DramNvBackend {
    pub(crate) fn allocate(config: RnvpcConfig) -> Result<Self> {
        if config.mode() == NvSimMode::Model {
            return_errno_with_message!(
                Errno::EOPNOTSUPP,
                "RNVPC model is disabled for normal use; select fast or fast-no-persist"
            );
        }
        DramNvImage::allocate_inner(config, DEFAULT_CHUNK_PAGES, false).map(|image| image.attach())
    }

    #[cfg(ktest)]
    pub(super) fn allocate_with_chunk_pages(
        config: RnvpcConfig,
        chunk_pages: usize,
    ) -> Result<Self> {
        DramNvImage::allocate_inner(config, chunk_pages, false).map(|image| image.attach())
    }

    pub(crate) fn stats(&self) -> DramNvBackendStats {
        DramNvBackendStats {
            persist_calls: self.image.inner.persist_calls.load(Ordering::Relaxed),
            fence_calls: self.image.inner.fence_calls.load(Ordering::Relaxed),
        }
    }

    fn check_epoch(&self) -> Result<()> {
        if self.epoch != self.image.inner.epoch.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EIO, "stale RNVPC backend after simulated crash");
        }
        Ok(())
    }

    fn validate_data_range(&self, offset: usize, len: usize) -> Result<()> {
        self.check_epoch()?;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if end > self.image.inner.capacity_pages * PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "RNVPC data range is out of bounds");
        }
        Ok(())
    }

    fn validate_control_range(&self, offset: usize, len: usize) -> Result<()> {
        self.check_epoch()?;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if end > self.image.inner.control_bytes {
            return_errno_with_message!(Errno::EINVAL, "RNVPC control range is out of bounds");
        }
        Ok(())
    }

    fn has_durable_image(&self) -> bool {
        #[cfg(ktest)]
        {
            self.image.inner.durable.is_some()
        }
        #[cfg(not(ktest))]
        {
            false
        }
    }

    #[cfg(ktest)]
    fn copy_data_to_durable(&self, offset: usize, len: usize) -> Result<()> {
        let Some(durable) = &self.image.inner.durable else {
            return Ok(());
        };
        let mut page = [0; PAGE_SIZE];
        let mut copied = 0;
        while copied < len {
            let amount = (len - copied).min(PAGE_SIZE);
            self.image.inner.working.read_data(
                self.image.inner.chunk_pages,
                offset + copied,
                &mut page[..amount],
            )?;
            durable.write_data(
                self.image.inner.chunk_pages,
                offset + copied,
                &page[..amount],
            )?;
            copied += amount;
        }
        Ok(())
    }
}

impl NvMemoryBackend for DramNvBackend {
    fn capacity_pages(&self) -> usize {
        self.image.inner.capacity_pages
    }

    fn control_len(&self) -> usize {
        self.image.inner.control_bytes
    }

    fn read_page(&self, id: NvPageId, output: &mut [u8]) -> Result<()> {
        if output.len() != PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "RNVPC reads must cover one page");
        }
        let offset = id
            .index()
            .checked_mul(PAGE_SIZE)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.validate_data_range(offset, output.len())?;
        #[cfg(ktest)]
        let _guard = self
            .image
            .inner
            .durable
            .as_ref()
            .map(|_| self.image.inner.io_lock.lock());
        self.image
            .inner
            .working
            .read_data(self.image.inner.chunk_pages, offset, output)
    }

    fn write_page(&self, id: NvPageId, input: &[u8]) -> Result<()> {
        if input.len() != PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "RNVPC writes must cover one page");
        }
        let offset = id
            .index()
            .checked_mul(PAGE_SIZE)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.validate_data_range(offset, input.len())?;
        #[cfg(ktest)]
        let _guard = self
            .image
            .inner
            .durable
            .as_ref()
            .map(|_| self.image.inner.io_lock.lock());
        self.image
            .inner
            .working
            .write_data(self.image.inner.chunk_pages, offset, input)?;
        if !self.has_durable_image() {
            compiler_fence(Ordering::Release);
        }
        Ok(())
    }

    fn copy_to_writer(
        &self,
        id: NvPageId,
        page_offset: usize,
        writer: &mut VmWriter,
        len: usize,
    ) -> Result<usize> {
        let end = page_offset
            .checked_add(len)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if end > PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "RNVPC page range is out of bounds");
        }
        let offset = id
            .index()
            .checked_mul(PAGE_SIZE)
            .and_then(|base| base.checked_add(page_offset))
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.validate_data_range(offset, len)?;
        if len == 0 {
            return Ok(0);
        }

        let chunk_bytes = self.image.inner.chunk_pages * PAGE_SIZE;
        let chunk_index = offset / chunk_bytes;
        let chunk_offset = offset % chunk_bytes;
        #[cfg(ktest)]
        let _guard = self
            .image
            .inner
            .durable
            .as_ref()
            .map(|_| self.image.inner.io_lock.lock());
        let mut reader = self.image.inner.working.chunks[chunk_index].reader();
        reader.skip(chunk_offset).limit(len);
        match reader.read_fallible(writer) {
            Ok(copied) => Ok(copied),
            Err((_error, copied)) if copied != 0 => Ok(copied),
            Err((error, 0)) => Err(error.into()),
            Err((_error, _)) => unreachable!(),
        }
    }

    fn data_page_segment(&self, id: NvPageId) -> Result<USegment> {
        let offset = id
            .index()
            .checked_mul(PAGE_SIZE)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.validate_data_range(offset, PAGE_SIZE)?;
        let chunk_bytes = self.image.inner.chunk_pages * PAGE_SIZE;
        let chunk_index = offset / chunk_bytes;
        let chunk_offset = offset % chunk_bytes;
        Ok(self.image.inner.working.chunks[chunk_index]
            .slice(&(chunk_offset..chunk_offset + PAGE_SIZE))
            .into())
    }

    fn copy_from_reader(
        &self,
        id: NvPageId,
        page_offset: usize,
        reader: &mut VmReader,
        len: usize,
    ) -> Result<usize> {
        let end = page_offset
            .checked_add(len)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if end > PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "RNVPC page range is out of bounds");
        }
        let offset = id
            .index()
            .checked_mul(PAGE_SIZE)
            .and_then(|base| base.checked_add(page_offset))
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.validate_data_range(offset, len)?;
        if len == 0 {
            return Ok(0);
        }

        let chunk_bytes = self.image.inner.chunk_pages * PAGE_SIZE;
        let chunk_index = offset / chunk_bytes;
        let chunk_offset = offset % chunk_bytes;
        #[cfg(ktest)]
        let _guard = self
            .image
            .inner
            .durable
            .as_ref()
            .map(|_| self.image.inner.io_lock.lock());
        let mut writer = self.image.inner.working.chunks[chunk_index].writer();
        writer.skip(chunk_offset).limit(len);
        let copied = match reader.read_fallible(&mut writer) {
            Ok(copied) => copied,
            Err((_error, copied)) if copied != 0 => copied,
            Err((error, 0)) => return Err(error.into()),
            Err((_error, _)) => unreachable!(),
        };
        if !self.has_durable_image() {
            compiler_fence(Ordering::Release);
        }
        Ok(copied)
    }

    fn zero_page_range(&self, id: NvPageId, page_offset: usize, len: usize) -> Result<()> {
        let end = page_offset
            .checked_add(len)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if end > PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "RNVPC page range is out of bounds");
        }
        let offset = id
            .index()
            .checked_mul(PAGE_SIZE)
            .and_then(|base| base.checked_add(page_offset))
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.validate_data_range(offset, len)?;
        if len == 0 {
            return Ok(());
        }
        let chunk_bytes = self.image.inner.chunk_pages * PAGE_SIZE;
        let chunk_index = offset / chunk_bytes;
        let chunk_offset = offset % chunk_bytes;
        #[cfg(ktest)]
        let _guard = self
            .image
            .inner
            .durable
            .as_ref()
            .map(|_| self.image.inner.io_lock.lock());
        self.image.inner.working.chunks[chunk_index]
            .fill_zeros(chunk_offset, len)
            .map_err(|(error, _)| Error::from(error))?;
        if !self.has_durable_image() {
            compiler_fence(Ordering::Release);
        }
        Ok(())
    }

    fn store_page_u64(&self, id: NvPageId, page_offset: usize, value: u64) -> Result<()> {
        if !page_offset.is_multiple_of(size_of::<u64>())
            || page_offset > PAGE_SIZE - size_of::<u64>()
        {
            return_errno_with_message!(Errno::EINVAL, "RNVPC atomic page store is misaligned");
        }
        let offset = id
            .index()
            .checked_mul(PAGE_SIZE)
            .and_then(|base| base.checked_add(page_offset))
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.validate_data_range(offset, size_of::<u64>())?;
        #[cfg(ktest)]
        let _guard = self
            .image
            .inner
            .durable
            .as_ref()
            .map(|_| self.image.inner.io_lock.lock());
        self.image
            .inner
            .working
            .write_data_once(self.image.inner.chunk_pages, offset, value)?;
        if !self.has_durable_image() {
            compiler_fence(Ordering::Release);
        }
        Ok(())
    }

    fn write_page_range(&self, id: NvPageId, page_offset: usize, input: &[u8]) -> Result<()> {
        let end = page_offset
            .checked_add(input.len())
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if end > PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "RNVPC page range is out of bounds");
        }
        let offset = id
            .index()
            .checked_mul(PAGE_SIZE)
            .and_then(|base| base.checked_add(page_offset))
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.validate_data_range(offset, input.len())?;
        #[cfg(ktest)]
        let _guard = self
            .image
            .inner
            .durable
            .as_ref()
            .map(|_| self.image.inner.io_lock.lock());
        self.image
            .inner
            .working
            .write_data(self.image.inner.chunk_pages, offset, input)?;
        if !self.has_durable_image() {
            compiler_fence(Ordering::Release);
        }
        Ok(())
    }

    fn read_control(&self, offset: usize, output: &mut [u8]) -> Result<()> {
        self.validate_control_range(offset, output.len())?;
        #[cfg(ktest)]
        let _guard = self
            .image
            .inner
            .durable
            .as_ref()
            .map(|_| self.image.inner.io_lock.lock());
        self.image
            .inner
            .working
            .read_control(self.image.inner.chunk_pages, offset, output)?;
        Ok(())
    }

    fn write_control(&self, offset: usize, input: &[u8]) -> Result<()> {
        self.validate_control_range(offset, input.len())?;
        #[cfg(ktest)]
        let _guard = self
            .image
            .inner
            .durable
            .as_ref()
            .map(|_| self.image.inner.io_lock.lock());
        self.image
            .inner
            .working
            .write_control(self.image.inner.chunk_pages, offset, input)?;
        if !self.has_durable_image() {
            compiler_fence(Ordering::Release);
        }
        Ok(())
    }

    fn load_control_u64(&self, offset: usize) -> Result<u64> {
        if !offset.is_multiple_of(align_of::<u64>()) {
            return_errno_with_message!(Errno::EINVAL, "unaligned RNVPC atomic load");
        }
        self.validate_control_range(offset, size_of::<u64>())?;
        #[cfg(ktest)]
        let _guard = self
            .image
            .inner
            .durable
            .as_ref()
            .map(|_| self.image.inner.io_lock.lock());
        self.image
            .inner
            .working
            .read_control_once(self.image.inner.chunk_pages, offset)
    }

    fn store_control_u64(&self, offset: usize, value: u64) -> Result<()> {
        if !offset.is_multiple_of(align_of::<u64>()) {
            return_errno_with_message!(Errno::EINVAL, "unaligned RNVPC atomic store");
        }
        self.validate_control_range(offset, size_of::<u64>())?;
        #[cfg(ktest)]
        let _guard = self
            .image
            .inner
            .durable
            .as_ref()
            .map(|_| self.image.inner.io_lock.lock());
        self.image
            .inner
            .working
            .write_control_once(self.image.inner.chunk_pages, offset, value)
    }

    fn persist(&self, offset: usize, len: usize) -> Result<()> {
        self.validate_data_range(offset, len)?;
        super::batch_metrics::count(super::batch_metrics::Event::DataPersist, 1);
        if self.image.inner.skip_persistence {
            return Ok(());
        }
        #[cfg(ktest)]
        let _guard = self
            .image
            .inner
            .durable
            .as_ref()
            .map(|_| self.image.inner.io_lock.lock());
        #[cfg(ktest)]
        self.copy_data_to_durable(offset, len)?;
        compiler_fence(Ordering::Release);
        self.image
            .inner
            .persist_calls
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn persist_control(&self, offset: usize, len: usize) -> Result<()> {
        self.validate_control_range(offset, len)?;
        super::batch_metrics::count(super::batch_metrics::Event::ControlPersist, 1);
        if self.image.inner.skip_persistence {
            return Ok(());
        }
        #[cfg(ktest)]
        let _guard = self
            .image
            .inner
            .durable
            .as_ref()
            .map(|_| self.image.inner.io_lock.lock());
        #[cfg(ktest)]
        if let Some(durable) = &self.image.inner.durable {
            let mut bytes = vec![0; len];
            self.image.inner.working.read_control(
                self.image.inner.chunk_pages,
                offset,
                &mut bytes,
            )?;
            durable.write_control(self.image.inner.chunk_pages, offset, &bytes)?;
        }
        compiler_fence(Ordering::Release);
        self.image
            .inner
            .persist_calls
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn fence(&self) -> Result<()> {
        self.check_epoch()?;
        super::batch_metrics::count(super::batch_metrics::Event::Fence, 1);
        if self.image.inner.skip_persistence {
            return Ok(());
        }
        compiler_fence(Ordering::SeqCst);
        self.image.inner.fence_calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(ktest)]
mod recovery_tests {
    use ostd::prelude::ktest;

    use super::*;

    fn config(mode: NvSimMode) -> RnvpcConfig {
        RnvpcConfig::from_bytes(
            Some(5 * PAGE_SIZE),
            Some(PAGE_SIZE),
            Some(4 * PAGE_SIZE),
            Some(mode),
        )
        .unwrap()
        .unwrap()
    }

    #[ktest]
    fn recovered_backend_direct_copy_and_ranges() {
        let backend = DramNvBackend::allocate_with_chunk_pages(config(NvSimMode::Fast), 2).unwrap();
        let id = NvPageId::try_from(4usize).unwrap();
        backend.write_page(id, &[0x55; PAGE_SIZE]).unwrap();
        let input = [1, 2, 3];
        let mut reader = VmReader::from(input.as_slice()).to_fallible();
        assert_eq!(backend.copy_from_reader(id, 17, &mut reader, 8).unwrap(), 3);
        let mut output = [0; 3];
        let mut writer = VmWriter::from(output.as_mut_slice()).to_fallible();
        assert_eq!(backend.copy_to_writer(id, 17, &mut writer, 8).unwrap(), 3);
        assert_eq!(output, input);
        backend.zero_page_range(id, 20, 4).unwrap();
        let segment = backend.data_page_segment(id).unwrap();
        assert_eq!(segment.size(), PAGE_SIZE);
        let mut page = [0; PAGE_SIZE];
        backend.read_page(id, &mut page).unwrap();
        assert_eq!(&page[16..25], &[0x55, 1, 2, 3, 0, 0, 0, 0, 0x55]);
        assert!(backend.write_page_range(id, PAGE_SIZE, &[1]).is_err());
        assert!(backend.store_page_u64(id, 1, 1).is_err());
        assert!(
            backend
                .read_page(NvPageId::try_from(5usize).unwrap(), &mut page)
                .is_err()
        );
    }

    #[ktest]
    fn recovered_backend_model_preserves_only_persisted_prefix() {
        let config = config(NvSimMode::Model);
        assert!(DramNvBackend::allocate(config).is_err());
        let image = DramNvImage::allocate_model(config).unwrap();
        let backend = image.attach();
        let id = NvPageId::try_from(0usize).unwrap();
        backend.write_page(id, &[0x77; PAGE_SIZE]).unwrap();
        backend.persist(0, 3).unwrap();
        backend.store_control_u64(0, 23).unwrap();
        backend.persist_control(0, 8).unwrap();
        backend.fence().unwrap();
        image.simulate_crash();
        assert!(backend.fence().is_err());
        let backend = image.attach();
        let mut page = [0; PAGE_SIZE];
        backend.read_page(id, &mut page).unwrap();
        assert_eq!(&page[..3], &[0x77; 3]);
        assert!(page[3..].iter().all(|byte| *byte == 0));
        assert_eq!(backend.load_control_u64(0).unwrap(), 23);
    }

    #[ktest]
    fn recovered_backend_ablation_validates_but_skips_persistence() {
        let backend = DramNvBackend::allocate(config(NvSimMode::FastNoPersist)).unwrap();
        backend.persist(0, PAGE_SIZE).unwrap();
        backend.persist_control(0, 8).unwrap();
        backend.fence().unwrap();
        assert_eq!(backend.stats(), DramNvBackendStats::default());
        assert!(backend.persist(5 * PAGE_SIZE, 1).is_err());
        assert!(backend.persist_control(backend.control_len(), 1).is_err());
    }

    #[ktest]
    fn recovered_backend_parallel_disjoint_page_access() {
        let backend = Arc::new(DramNvBackend::allocate(config(NvSimMode::Fast)).unwrap());
        let mut threads = Vec::new();
        for index in 0..4usize {
            let backend = backend.clone();
            threads.push(
                crate::thread::kernel_thread::ThreadOptions::new(move || {
                    let page = NvPageId::try_from(index).unwrap();
                    for step in 0..32u64 {
                        let pattern = [((index as u64 * 32) + step) as u8; PAGE_SIZE];
                        backend.write_page(page, &pattern).unwrap();
                        backend.store_control_u64(index * 8, step).unwrap();
                        crate::thread::Thread::yield_now();
                        let mut readback = [0; PAGE_SIZE];
                        backend.read_page(page, &mut readback).unwrap();
                        assert_eq!(readback, pattern);
                        assert_eq!(backend.load_control_u64(index * 8).unwrap(), step);
                    }
                })
                .spawn(),
            );
        }
        for thread in threads {
            thread.join();
        }
    }
}
