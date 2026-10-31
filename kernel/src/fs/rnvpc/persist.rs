// SPDX-License-Identifier: MPL-2.0

//! RNVPC frame-table formatting and persistence protocol.

use core::sync::atomic::{AtomicBool, Ordering};

use super::{
    backend::NvMemoryBackend,
    descriptor::{NvDescriptor, NvRedirectExtentRecord},
    fault::{FaultPoint, inject},
    id::NvPageId,
    layout::{
        NV_IMAGE_MAGIC, NV_IMAGE_VERSION, NvEntry, NvImageHeader, commit_offset, control_len,
        descriptor_offset, redirect_extent_offset,
    },
};
use crate::prelude::*;

pub(crate) struct PersistEngine {
    backend: Arc<dyn NvMemoryBackend>,
    capacity_pages: usize,
    publication_uncertain: AtomicBool,
}

impl PersistEngine {
    pub(super) fn read_data_bytes(&self, index: usize, output: &mut [u8]) -> Result<()> {
        self.check_index(index)?;
        if output.len() > PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "persistent record exceeds one NV page");
        }
        self.backend
            .read_page_range(NvPageId::try_from(index)?, 0, output)
    }
    pub(super) fn new(backend: Arc<dyn NvMemoryBackend>, capacity_pages: usize) -> Result<Self> {
        if backend.capacity_pages() != capacity_pages
            || backend.control_len() < control_len(capacity_pages)?
        {
            return_errno_with_message!(Errno::EINVAL, "RNVPC image capacity mismatch");
        }
        Ok(Self {
            backend,
            capacity_pages,
            publication_uncertain: AtomicBool::new(false),
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

    /// Reads only the selected descriptor, retrying if publication races a read.
    /// Callers retain a lifetime pin/lease; this does not grant mutation ownership.
    pub(super) fn read_entry(&self, index: usize) -> Result<Option<NvDescriptor>> {
        self.check_index(index)?;
        for _ in 0..16 {
            let selector = self.backend.load_control_u64(commit_offset(index)?)?;
            if selector == 0 {
                return Ok(None);
            }
            let mut descriptor = NvDescriptor::default();
            self.backend.read_control(
                descriptor_offset(index, selector)?,
                descriptor.as_mut_bytes(),
            )?;
            if self.backend.load_control_u64(commit_offset(index)?)? != selector {
                continue;
            }
            if !descriptor.is_valid() {
                return_errno_with_message!(Errno::EINVAL, "invalid selected NV descriptor");
            }
            return Ok(Some(descriptor));
        }
        return_errno!(Errno::EBUSY)
    }

    pub(super) fn read_redirect_extent(
        &self,
        index: usize,
    ) -> Result<Option<NvRedirectExtentRecord>> {
        self.check_index(index)?;
        for _ in 0..16 {
            let selector = self.backend.load_control_u64(commit_offset(index)?)?;
            if selector == 0 {
                return Ok(None);
            }
            let mut record = NvRedirectExtentRecord::default();
            self.backend.read_control(
                redirect_extent_offset(index, selector)?,
                record.as_mut_bytes(),
            )?;
            if self.backend.load_control_u64(commit_offset(index)?)? != selector {
                continue;
            }
            if record.is_empty() {
                return Ok(None);
            }
            record.validate()?;
            return Ok(Some(record));
        }
        return_errno!(Errno::EBUSY)
    }

    pub(super) fn publish_redirect_extent(
        &self,
        record: &NvRedirectExtentRecord,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        self.ensure_writable()?;
        record.validate()?;
        let index =
            usize::try_from(record.nv_slot_start()).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        self.check_index(index)?;
        let old = self.backend.load_control_u64(commit_offset(index)?)?;
        if old == 0 {
            return_errno_with_message!(Errno::ESTALE, "redirect extent head is not published");
        }
        let mut descriptor = NvDescriptor::default();
        self.backend
            .read_control(descriptor_offset(index, old)?, descriptor.as_mut_bytes())?;
        let next = old
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let descriptor_offset = descriptor_offset(index, next)?;
        let record_offset = redirect_extent_offset(index, next)?;
        self.backend
            .write_control(descriptor_offset, descriptor.as_bytes())?;
        self.backend
            .persist_control(descriptor_offset, size_of::<NvDescriptor>())?;
        self.backend
            .write_control(record_offset, record.as_bytes())?;
        self.backend
            .persist_control(record_offset, size_of::<NvRedirectExtentRecord>())?;
        self.backend.fence()?;
        inject(fault, FaultPoint::AfterContentPersist)?;
        let commit = commit_offset(index)?;
        self.backend.store_control_u64(commit, next)?;
        self.backend.persist_control(commit, size_of::<u64>())?;
        self.backend.fence()
    }

    /// Persists all data pages, then publishes one selector per redirect range.
    /// Tail slots deliberately retain selector zero: the head range record is
    /// their only persistent allocation and recovery identity.
    pub(super) fn commit_redirect_extents(
        &self,
        records: &[NvRedirectExtentRecord],
        data_slots: &[usize],
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        self.commit_redirect_extents_inner(records, data_slots, |point| inject(fault, point))
    }

    #[cfg(ktest)]
    pub(super) fn commit_redirect_extents_counted(
        &self,
        records: &[NvRedirectExtentRecord],
        data_slots: &[usize],
        fault: Option<super::fault::FaultSpec>,
    ) -> Result<()> {
        let mut seen = 0;
        self.commit_redirect_extents_inner(records, data_slots, |point| {
            super::fault::inject_counted(fault, point, &mut seen)
        })
    }

    fn commit_redirect_extents_inner(
        &self,
        records: &[NvRedirectExtentRecord],
        data_slots: &[usize],
        mut inject_fault: impl FnMut(FaultPoint) -> Result<()>,
    ) -> Result<()> {
        self.ensure_writable()?;
        if records.len() > 128 || data_slots.len() > 128 {
            return_errno!(Errno::EINVAL);
        }
        let mut heads = BTreeSet::new();
        for record in records {
            record.validate()?;
            let head = usize::try_from(record.nv_slot_start())
                .map_err(|_| Error::new(Errno::EOVERFLOW))?;
            self.check_index(head)?;
            if !heads.insert(head) || self.backend.load_control_u64(commit_offset(head)?)? != 0 {
                return_errno_with_message!(Errno::ESTALE, "redirect extent head is already used");
            }
        }
        for slot in data_slots {
            self.check_index(*slot)?;
        }
        inject_fault(FaultPoint::BeforeContentPersist)?;
        for slot in data_slots {
            self.backend.persist(slot * PAGE_SIZE, PAGE_SIZE)?;
        }
        for record in records {
            let head = usize::try_from(record.nv_slot_start())
                .map_err(|_| Error::new(Errno::EOVERFLOW))?;
            let selector = 1;
            let descriptor_offset = descriptor_offset(head, selector)?;
            let descriptor = NvDescriptor::default();
            self.backend
                .write_control(descriptor_offset, descriptor.as_bytes())?;
            self.backend
                .persist_control(descriptor_offset, size_of::<NvDescriptor>())?;
            let record_offset = redirect_extent_offset(head, selector)?;
            self.backend
                .write_control(record_offset, record.as_bytes())?;
            self.backend
                .persist_control(record_offset, size_of::<NvRedirectExtentRecord>())?;
        }
        self.backend.fence()?;
        inject_fault(FaultPoint::AfterContentPersist)?;
        let publication = (|| {
            for record in records {
                let head = usize::try_from(record.nv_slot_start())
                    .map_err(|_| Error::new(Errno::EOVERFLOW))?;
                let offset = commit_offset(head)?;
                self.backend.store_control_u64(offset, 1)?;
                inject_fault(FaultPoint::AfterCommitStore)?;
                self.backend.persist_control(offset, size_of::<u64>())?;
                inject_fault(FaultPoint::AfterCommitPersist)?;
            }
            self.backend.fence()
        })();
        if let Err(error) = publication {
            self.publication_uncertain.store(true, Ordering::Release);
            return Err(error);
        }
        if let Err(error) = inject_fault(FaultPoint::AfterCommitFence) {
            self.publication_uncertain.store(true, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }

    /// Persists mutations to already-published range-owned data pages without
    /// rewriting any descriptor or range selector.
    pub(super) fn persist_data_slots(&self, slots: &[usize]) -> Result<()> {
        self.ensure_writable()?;
        for slot in slots {
            self.check_index(*slot)?;
            self.backend.persist(slot * PAGE_SIZE, PAGE_SIZE)?;
        }
        self.backend.fence()
    }

    /// Persists changed data before publishing a descriptor. Existing selected
    /// descriptor bytes remain untouched until the new copy is complete.
    pub(super) fn commit_frame(
        &self,
        index: usize,
        descriptor: &NvDescriptor,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        self.ensure_writable()?;
        self.check_descriptor(index, descriptor)?;
        inject(fault, FaultPoint::BeforeContentPersist)?;
        self.backend.persist(index * PAGE_SIZE, PAGE_SIZE)?;
        self.publish_descriptors(&[(index, *descriptor)], fault)
    }

    /// Persists a set of independent data frames before publishing their
    /// descriptors with one pair of ordering fences.
    pub(super) fn commit_frames(
        &self,
        updates: &[(usize, NvDescriptor)],
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        self.ensure_writable()?;
        if updates.len() > 128 {
            return_errno!(Errno::EINVAL);
        }
        for (index, descriptor) in updates {
            self.check_descriptor(*index, descriptor)?;
        }
        inject(fault, FaultPoint::BeforeContentPersist)?;
        for (index, _) in updates {
            self.backend.persist(index * PAGE_SIZE, PAGE_SIZE)?;
        }
        self.publish_descriptors(updates, fault)
    }

    /// Updates already committed descriptors without persisting unchanged data.
    /// The caller owns an exclusive mutation/eviction lease for every entry.
    pub(super) fn update_descriptors(
        &self,
        updates: &[(usize, NvDescriptor)],
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        for (index, descriptor) in updates {
            self.check_descriptor(*index, descriptor)?;
            let old = self
                .read_entry(*index)?
                .ok_or_else(|| Error::new(Errno::ESTALE))?;
            if old.allocation_seq() != descriptor.allocation_seq()
                || old.fs_id != descriptor.fs_id
                || old.inode != descriptor.inode
                || old.logical_index != descriptor.logical_index
                || old.kind != descriptor.kind
            {
                return_errno!(Errno::ESTALE);
            }
        }
        self.publish_descriptors(updates, fault)
    }

    fn publish_descriptors(
        &self,
        updates: &[(usize, NvDescriptor)],
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        self.ensure_writable()?;
        if updates.is_empty() {
            return Ok(());
        }
        if updates.len() > 128 {
            return_errno!(Errno::EINVAL);
        }
        let mut selected = Vec::with_capacity(updates.len());
        let mut slots = BTreeSet::new();
        // Validate the whole batch before touching any inactive copy.
        for (index, descriptor) in updates {
            self.check_descriptor(*index, descriptor)?;
            if !slots.insert(*index) {
                return_errno!(Errno::EINVAL);
            }
            let old = self.backend.load_control_u64(commit_offset(*index)?)?;
            let next = old
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            let redirect = if old == 0 {
                NvRedirectExtentRecord::default()
            } else {
                let mut redirect = NvRedirectExtentRecord::default();
                self.backend.read_control(
                    redirect_extent_offset(*index, old)?,
                    redirect.as_mut_bytes(),
                )?;
                redirect
            };
            selected.push((*index, next, redirect));
        }
        inject(fault, FaultPoint::BeforeContentPersist)?;
        for ((index, descriptor), (_, next, redirect)) in updates.iter().zip(&selected) {
            let offset = descriptor_offset(*index, *next)?;
            self.backend.write_control(offset, descriptor.as_bytes())?;
            self.backend
                .persist_control(offset, size_of::<NvDescriptor>())?;
            let redirect_offset = redirect_extent_offset(*index, *next)?;
            self.backend
                .write_control(redirect_offset, redirect.as_bytes())?;
            self.backend
                .persist_control(redirect_offset, size_of::<NvRedirectExtentRecord>())?;
        }
        self.backend.fence()?;
        inject(fault, FaultPoint::AfterContentPersist)?;
        let publication = (|| {
            for (index, next, _) in selected {
                let offset = commit_offset(index)?;
                self.backend.store_control_u64(offset, next)?;
                inject(fault, FaultPoint::AfterCommitStore)?;
                self.backend.persist_control(offset, size_of::<u64>())?;
                inject(fault, FaultPoint::AfterCommitPersist)?;
            }
            self.backend.fence()
        })();
        if let Err(error) = publication {
            // A volatile selector may be newer than its durable value. Reusing
            // its inactive copy could overwrite the last recoverable descriptor.
            // Recovery constructs a fresh engine from durable selectors.
            self.publication_uncertain.store(true, Ordering::Release);
            return Err(error);
        }
        if let Err(error) = inject(fault, FaultPoint::AfterCommitFence) {
            // The selector is already durable. The caller cannot safely treat
            // this as an unpublished object or reuse its slot before recovery.
            self.publication_uncertain.store(true, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn clear_frame(&self, index: usize, fault: Option<FaultPoint>) -> Result<()> {
        self.clear_frames(&[index], fault)
    }

    /// No slot may be reused before this method's final fence succeeds.
    pub(super) fn clear_frames(&self, indices: &[usize], fault: Option<FaultPoint>) -> Result<()> {
        self.ensure_writable()?;
        for index in indices {
            self.check_index(*index)?;
        }
        if indices.is_empty() {
            return Ok(());
        }
        inject(fault, FaultPoint::BeforeClear)?;
        let clearing = (|| {
            for index in indices {
                let offset = commit_offset(*index)?;
                self.backend.store_control_u64(offset, 0)?;
                inject(fault, FaultPoint::AfterClearStore)?;
                self.backend.persist_control(offset, size_of::<u64>())?;
                inject(fault, FaultPoint::AfterClearPersist)?;
            }
            self.backend.fence()
        })();
        if let Err(error) = clearing {
            self.publication_uncertain.store(true, Ordering::Release);
            return Err(error);
        }
        inject(fault, FaultPoint::AfterClearFence)
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.publication_uncertain.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EUCLEAN, "NV selector publication requires recovery");
        }
        Ok(())
    }

    pub(super) fn publication_uncertain(&self) -> bool {
        self.publication_uncertain.load(Ordering::Acquire)
    }

    pub(super) fn latch_publication_uncertain(&self) {
        self.publication_uncertain.store(true, Ordering::Release);
    }

    #[cfg(ktest)]
    pub(super) fn slot_is_selected_for_test(&self, index: usize) -> Result<bool> {
        self.check_index(index)?;
        Ok(self.backend.load_control_u64(commit_offset(index)?)? != 0)
    }

    fn check_descriptor(&self, index: usize, descriptor: &NvDescriptor) -> Result<()> {
        self.check_index(index)?;
        if !descriptor.is_valid() {
            return_errno_with_message!(Errno::EINVAL, "invalid NV descriptor");
        }
        Ok(())
    }

    fn check_index(&self, index: usize) -> Result<()> {
        if index >= self.capacity_pages {
            return_errno_with_message!(Errno::EINVAL, "RNVPC frame index is out of bounds");
        }
        Ok(())
    }
}
