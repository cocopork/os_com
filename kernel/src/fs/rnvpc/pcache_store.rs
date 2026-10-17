// SPDX-License-Identifier: MPL-2.0

//! Typed, self-describing persistent page-cache metadata objects.

use core::ops::Range;

use super::{
    fault::FaultPoint,
    frame::{PersistentDataRef, PersistentFrameUpdate, Rnvpc},
    id::NvFrameId,
    index::PageKey,
    pcache_layout::{PcObjectType, PcPageCodec, PersistentPageRef},
};
use crate::prelude::*;

const TYPE_BITS: u32 = 16;
const INDEX_BITS: u32 = 64 - TYPE_BITS;
const INDEX_MASK: u64 = (1u64 << INDEX_BITS) - 1;

/// A collision-free `PageKey::page_index` for typed metadata.
///
/// Bits `63..48` contain the `PcObjectType` discriminant and bits `47..0`
/// contain its object-local index. The fixed 16/48 split is injective for
/// the supported wire types and rejects indices that would overlap the type.
pub(crate) fn typed_object_index(object_type: PcObjectType, object_index: u64) -> Result<u64> {
    if object_index > INDEX_MASK {
        return_errno_with_message!(Errno::EOVERFLOW, "persistent object index does not fit");
    }
    Ok(((object_type as u64) << INDEX_BITS) | object_index)
}

pub(crate) fn typed_object_type(index: u64) -> Option<PcObjectType> {
    match (index >> INDEX_BITS) as u16 {
        1 => Some(PcObjectType::Anchor),
        2 => Some(PcObjectType::WalFrame),
        3 => Some(PcObjectType::BtreeNode),
        _ => None,
    }
}

pub(crate) const fn typed_object_local_index(index: u64) -> u64 {
    index & INDEX_MASK
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PcFaultPoint {
    BeforeDescriptorCommit,
    AfterDescriptorCommitPersist,
    BeforeImagePersist,
    AfterImagePersist,
    BeforeSelectorPersist,
    AfterSelectorPersist,
    AfterWalRecordPersist,
    AfterWalCommitPersist,
    AfterWalAnchorImagePersist,
    AfterWalAnchorSelectorPersist,
    AfterRightNodePersist,
    AfterInternalRightNodePersist,
    AfterLeftSiblingLinkPersist,
    AfterParentSeparatorPersist,
    AfterNewRootPersist,
    AfterRootAnchorImagePersist,
    AfterRootAnchorPersist,
    BeforeCheckpointAnchorAdvance,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistPlan {
    pub(crate) image_range: Range<usize>,
    pub(crate) selector_range: Range<usize>,
}

pub(crate) struct PersistentMetadataStore {
    pool: Arc<Rnvpc>,
}

impl PersistentMetadataStore {
    pub(crate) fn new(pool: Arc<Rnvpc>) -> Self {
        Self { pool }
    }

    pub(crate) fn allocate<T: PcPageCodec>(
        &self,
        fs_id: u64,
        inode: u64,
        object_type: PcObjectType,
        object_index: u64,
        value: &T,
        fault: Option<PcFaultPoint>,
    ) -> Result<PersistentPageRef> {
        if object_type != T::OBJECT_TYPE {
            return_errno_with_message!(
                Errno::EINVAL,
                "persistent object type does not match payload"
            );
        }
        let key = PageKey::new(fs_id, inode, typed_object_index(object_type, object_index)?);
        let mut page = [0; PAGE_SIZE];
        value.encode_page(&mut page)?;
        let (id, generation) = self.pool.alloc_frame_seq()?;
        let mut allocation = UncommittedFrame::new(self.pool.clone(), id);
        let pin = self.pool.pin_frame(id)?;
        pin.write(&page)?;
        drop(pin);
        inject(fault, PcFaultPoint::BeforeDescriptorCommit)?;

        self.pool
            .commit_metadata_typed_frame(id, key, commit_fault(fault), || allocation.disarm())?;
        Ok(PersistentPageRef::new(id.index() as u32, generation))
    }

    pub(crate) fn read<T: PcPageCodec>(&self, page: PersistentPageRef) -> Result<T> {
        self.validate_ref(page)?;
        let update = self.pool.begin_metadata_update(page)?;
        let mut bytes = [0; PAGE_SIZE];
        update.read(&mut bytes)?;
        let value = T::decode_page(&bytes)?;
        if let Some(selector_range) = T::repair_selector(&mut bytes, &value) {
            update.write_page(&bytes)?;
            update.persist_range(selector_range.start, selector_range.len())?;
            update.fence()?;
        }
        update.finish();
        Ok(value)
    }

    pub(crate) fn update<T: PcPageCodec>(
        &self,
        page: PersistentPageRef,
        fault: Option<PcFaultPoint>,
        mutate: impl FnOnce(&mut T) -> Result<PersistPlan>,
    ) -> Result<()> {
        let _ = self.validate_ref(page)?;
        let update = self.pool.begin_metadata_update(page)?;
        let mut current = [0; PAGE_SIZE];
        update.read(&mut current)?;
        let mut value = T::decode_page(&current)?;
        let plan = mutate(&mut value)?;
        validate_plan(&plan)?;

        let mut desired = [0; PAGE_SIZE];
        value.encode_page(&mut desired)?;
        let mut image_copy = current;
        image_copy[plan.image_range.clone()].copy_from_slice(&desired[plan.image_range.clone()]);
        update.write_page(&image_copy)?;
        inject(fault, PcFaultPoint::BeforeImagePersist)?;
        update.persist_range(plan.image_range.start, plan.image_range.len())?;
        update.fence()?;
        inject(fault, PcFaultPoint::AfterImagePersist)?;

        let mut selector_copy = image_copy;
        selector_copy[plan.selector_range.clone()]
            .copy_from_slice(&desired[plan.selector_range.clone()]);
        update.write_page(&selector_copy)?;
        inject(fault, PcFaultPoint::BeforeSelectorPersist)?;
        update.persist_range(plan.selector_range.start, plan.selector_range.len())?;
        update.fence()?;
        inject(fault, PcFaultPoint::AfterSelectorPersist)?;
        update.finish();
        Ok(())
    }

    pub(crate) fn validate_ref(&self, page: PersistentPageRef) -> Result<NvFrameId> {
        let id = NvFrameId::try_from(page.slot as usize).map_err(|_| Error::new(Errno::EINVAL))?;
        if self.pool.frame_seq(id) != Some(page.generation) {
            return_errno_with_message!(Errno::EINVAL, "stale persistent page reference");
        }
        Ok(id)
    }

    /// Validates a live data-page identity carried by a B-tree leaf mapping.
    pub(crate) fn pin_data_ref(
        &self,
        page: PersistentPageRef,
        key: PageKey,
    ) -> Result<PersistentDataRef> {
        self.pool.pin_committed_data_ref(page, key)
    }

    /// Validates a committed data-page identity carried by a B-tree leaf mapping.
    pub(crate) fn validate_data_ref(&self, page: PersistentPageRef, key: PageKey) -> Result<()> {
        drop(self.pin_data_ref(page, key)?);
        Ok(())
    }

    /// Validates a metadata reference against its exact typed descriptor key.
    pub(crate) fn validate_typed_ref(
        &self,
        page: PersistentPageRef,
        fs_id: u64,
        inode: u64,
        object_type: PcObjectType,
        object_index: u64,
    ) -> Result<()> {
        let id = self.validate_ref(page)?;
        let key = PageKey::new(fs_id, inode, typed_object_index(object_type, object_index)?);
        if self.pool.committed_metadata_slot(key) != Some(id) {
            return_errno_with_message!(Errno::EINVAL, "persistent object descriptor mismatch");
        }
        Ok(())
    }

    /// Runs a generation-checked in-place metadata update without imposing a
    /// dual-image protocol. WAL frames have their own record/commit protocol.
    pub(crate) fn raw_update(
        &self,
        page: PersistentPageRef,
        mutate: impl FnOnce(&PersistentFrameUpdate) -> Result<()>,
    ) -> Result<()> {
        self.validate_ref(page)?;
        let update = self.pool.begin_metadata_update(page)?;
        let result = mutate(&update);
        if result.is_ok() {
            update.finish();
        }
        result
    }

    /// Removes a committed typed object without leaving its descriptor key
    /// reachable. A clear failure leaves the slot committed for recovery.
    pub(crate) fn remove(&self, page: PersistentPageRef) -> Result<()> {
        self.pool.clear_and_free_metadata(page)
    }

    pub(crate) fn typed_objects(
        &self,
        fs_id: u64,
        inode: u64,
        object_type: PcObjectType,
    ) -> Result<Vec<(u64, PersistentPageRef)>> {
        self.pool
            .committed_metadata_of(fs_id, inode)
            .into_iter()
            .filter(|(index, _)| typed_object_type(*index) == Some(object_type))
            .map(|(index, id)| {
                let generation = self
                    .pool
                    .frame_seq(id)
                    .ok_or_else(|| Error::new(Errno::EIO))?;
                Ok((
                    typed_object_local_index(index),
                    PersistentPageRef::new(id.index() as u32, generation),
                ))
            })
            .collect()
    }
}

struct UncommittedFrame {
    pool: Arc<Rnvpc>,
    id: NvFrameId,
    active: bool,
}

impl UncommittedFrame {
    fn new(pool: Arc<Rnvpc>, id: NvFrameId) -> Self {
        Self {
            pool,
            id,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for UncommittedFrame {
    fn drop(&mut self) {
        if self.active {
            if self.pool.free_frame(self.id).is_err() {
                error!(
                    "RNVPC could not return uncommitted metadata frame {:?}",
                    self.id
                );
            }
        }
    }
}

fn commit_fault(fault: Option<PcFaultPoint>) -> Option<FaultPoint> {
    match fault {
        Some(PcFaultPoint::AfterDescriptorCommitPersist) => Some(FaultPoint::AfterCommitPersist),
        _ => None,
    }
}

fn inject(selected: Option<PcFaultPoint>, current: PcFaultPoint) -> Result<()> {
    if selected == Some(current) {
        return_errno_with_message!(Errno::EIO, "injected page-cache persistence fault");
    }
    Ok(())
}

pub(crate) fn inject_fault(selected: Option<PcFaultPoint>, current: PcFaultPoint) -> Result<()> {
    inject(selected, current)
}

fn validate_plan(plan: &PersistPlan) -> Result<()> {
    if plan.image_range.start >= plan.image_range.end
        || plan.selector_range.start >= plan.selector_range.end
        || plan.image_range.end > PAGE_SIZE
        || plan.selector_range.end > PAGE_SIZE
        || plan.image_range.start < plan.selector_range.end
            && plan.selector_range.start < plan.image_range.end
    {
        return_errno_with_message!(Errno::EINVAL, "persistent update range is out of bounds");
    }
    Ok(())
}
