// SPDX-License-Identifier: MPL-2.0

//! RNVPC frame-table formatting and persistence protocol.

use super::{
    backend::NvMemoryBackend,
    descriptor::NvDescriptor,
    fault::{FaultPoint, inject},
    layout::{
        FRAME_COMMIT_WORD, NV_IMAGE_MAGIC, NV_IMAGE_VERSION, NvEntry, NvImageHeader, commit_offset,
        control_len, entry_offset,
    },
};
use crate::prelude::*;

pub(crate) struct PersistEngine {
    backend: Arc<dyn NvMemoryBackend>,
    capacity_pages: usize,
}

impl PersistEngine {
    pub(super) fn new(backend: Arc<dyn NvMemoryBackend>, capacity_pages: usize) -> Result<Self> {
        if backend.capacity_pages() != capacity_pages
            || backend.control_len() < control_len(capacity_pages)?
        {
            return_errno_with_message!(Errno::EINVAL, "RNVPC image capacity mismatch");
        }
        Ok(Self {
            backend,
            capacity_pages,
        })
    }

    pub(super) fn format(&self) -> Result<()> {
        let image_len = control_len(self.capacity_pages)?;
        let mut image = vec![0; image_len];
        let header = NvImageHeader {
            magic: NV_IMAGE_MAGIC,
            capacity_pages: self.capacity_pages as u64,
            version: NV_IMAGE_VERSION,
            header_size: size_of::<NvImageHeader>() as u32,
            entry_size: size_of::<NvEntry>() as u32,
            flags: 0,
            reserved: [0; 32],
        };
        image[..size_of::<NvImageHeader>()].copy_from_slice(header.as_bytes());
        self.backend.write_control(0, &image)?;
        self.backend.persist_control(0, image_len)?;
        self.backend.fence()
    }

    pub(super) fn read_header(&self) -> Result<NvImageHeader> {
        let mut header = NvImageHeader::default();
        self.backend.read_control(0, header.as_mut_bytes())?;
        if header.magic != NV_IMAGE_MAGIC
            || header.version != NV_IMAGE_VERSION
            || header.header_size as usize != size_of::<NvImageHeader>()
            || header.entry_size as usize != size_of::<NvEntry>()
            || header.capacity_pages != self.capacity_pages as u64
        {
            return_errno_with_message!(Errno::EINVAL, "invalid RNVPC image header");
        }
        Ok(header)
    }

    /// 读出一个**已提交**对象的自描述描述符；未提交（提交字不为 `FRAME_COMMIT_WORD`）
    /// 或描述符不自洽则返回 `None`/`EINVAL`。
    pub(super) fn read_entry(&self, index: usize) -> Result<Option<NvDescriptor>> {
        self.check_index(index)?;
        if self.backend.load_control_u64(commit_offset(index)?)? != FRAME_COMMIT_WORD {
            return Ok(None);
        }
        let mut entry = NvEntry::default();
        self.backend
            .read_control(entry_offset(index)?, entry.as_mut_bytes())?;
        if !entry.descriptor.is_valid() {
            return_errno_with_message!(Errno::EINVAL, "invalid RNVPC object descriptor");
        }
        Ok(Some(entry.descriptor))
    }

    /// 提交一个自描述对象（两阶段：内容先落盘，再落提交字）。
    pub(super) fn commit_frame(
        &self,
        index: usize,
        descriptor: &NvDescriptor,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        self.check_index(index)?;
        if !descriptor.is_valid() {
            return_errno_with_message!(
                Errno::EINVAL,
                "refusing to persist an invalid NV descriptor"
            );
        }
        // NV 槽位由 `index` 定位；描述符里的 `ssd_backing_block` 是独立的
        // 最终回写目标，二者不能混为一个地址空间。
        inject(fault, FaultPoint::BeforeContentPersist)?;
        let entry = NvEntry {
            descriptor: *descriptor,
            commit_word: 0,
        };
        self.backend
            .write_control(entry_offset(index)?, entry.as_bytes())?;
        self.backend.persist(index * PAGE_SIZE, PAGE_SIZE)?;
        self.backend
            .persist_control(entry_offset(index)?, size_of::<NvEntry>())?;
        self.backend.fence()?;
        inject(fault, FaultPoint::AfterContentPersist)?;

        let commit_offset = commit_offset(index)?;
        self.backend
            .store_control_u64(commit_offset, FRAME_COMMIT_WORD)?;
        inject(fault, FaultPoint::AfterCommitStore)?;
        self.backend
            .persist_control(commit_offset, size_of::<u64>())?;
        inject(fault, FaultPoint::AfterCommitPersist)?;
        self.backend.fence()?;
        inject(fault, FaultPoint::AfterCommitFence)
    }

    pub(super) fn clear_frame(&self, index: usize, fault: Option<FaultPoint>) -> Result<()> {
        self.check_index(index)?;
        inject(fault, FaultPoint::BeforeClear)?;
        let commit_offset = commit_offset(index)?;
        self.backend.store_control_u64(commit_offset, 0)?;
        inject(fault, FaultPoint::AfterClearStore)?;
        self.backend
            .persist_control(commit_offset, size_of::<u64>())?;
        inject(fault, FaultPoint::AfterClearPersist)?;
        self.backend.fence()?;
        inject(fault, FaultPoint::AfterClearFence)
    }

    fn check_index(&self, index: usize) -> Result<()> {
        if index >= self.capacity_pages {
            return_errno_with_message!(Errno::EINVAL, "RNVPC frame index is out of bounds");
        }
        Ok(())
    }
}
