// SPDX-License-Identifier: MPL-2.0

//! Persistent rNVPC frame leases adapted from the recovered WAL branch.
//! Object identity and pin admission use the shared per-slot object table.

use super::{ObjectKind, Rnvpc, note_metadata_pin_change};
use crate::{
    fs::rnvpc::{
        capacity::PoolClass,
        fault::FaultPoint,
        id::{NvFrameId, NvPageId},
        index::{PageKey, RuntimeHandle},
        pcache_layout::PersistentPageRef,
        pcache_store::typed_object_type,
        recover,
    },
    prelude::*,
};

pub(crate) struct PersistentDataRef {
    pool: Arc<Rnvpc>,
    id: NvPageId,
}

impl PersistentDataRef {
    pub(crate) fn read(&self, output: &mut [u8]) -> Result<()> {
        self.pool.backend.read_page(self.id, output)
    }
}

impl Drop for PersistentDataRef {
    fn drop(&mut self) {
        self.pool.unpin_data_page(self.id);
    }
}

pub(crate) struct PersistentFrameUpdate {
    pool: Arc<Rnvpc>,
    id: NvFrameId,
    active: bool,
}

impl Rnvpc {
    pub(crate) fn pin_committed_data_ref(
        self: &Arc<Self>,
        page: PersistentPageRef,
        key: PageKey,
    ) -> Result<PersistentDataRef> {
        if page == PersistentPageRef::NONE {
            return_errno_with_message!(Errno::EINVAL, "persistent data reference cannot be NONE");
        }
        let id = NvPageId::try_from(page.slot as usize).map_err(|_| Error::new(Errno::EINVAL))?;
        let mut objects = self.objects.lock_checked(id.index())?;
        let record = &mut objects.records[id.index()];
        if !matches!(record.kind, ObjectKind::Data(_))
            || !record.committed
            || record.alloc_seq != page.generation
            || record.key != Some(key)
        {
            return_errno_with_message!(Errno::EINVAL, "persistent data reference is not live");
        }
        if record.persisting {
            return_errno_with_message!(
                Errno::EBUSY,
                "persistent page has an active mutation or migration"
            );
        }
        let pins = record
            .pins
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.account_pin()?;
        record.pins = pins;
        Ok(PersistentDataRef {
            pool: self.clone(),
            id,
        })
    }

    pub(crate) fn begin_metadata_update(
        self: &Arc<Self>,
        page: PersistentPageRef,
    ) -> Result<PersistentFrameUpdate> {
        let id = NvFrameId::try_from(page.slot as usize).map_err(|_| Error::new(Errno::EINVAL))?;
        // Unlike the historical shared-lock implementation, close admission
        // and drain existing readers before changing a full working image.
        self.begin_exclusive_metadata_access(id, page.generation, false)?;
        Ok(PersistentFrameUpdate {
            pool: self.clone(),
            id,
            active: true,
        })
    }

    /// Retains the allocation on any ambiguous persistence failure.
    pub(crate) fn commit_metadata_typed_frame(
        &self,
        id: NvFrameId,
        key: PageKey,
        fault: Option<FaultPoint>,
        on_persistence_start: impl FnOnce(),
    ) -> Result<()> {
        if typed_object_type(key.page_index).is_none() {
            return_errno_with_message!(Errno::EINVAL, "unknown persistent metadata type");
        }
        let (version, _) = self.begin_commit(id.index(), ObjectKind::Metadata, key, false, None)?;
        let descriptor =
            match self.descriptor_for(PoolClass::Metadata, key, version, id.index(), None) {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    self.release_pcache_update(id);
                    return Err(error);
                }
            };
        on_persistence_start();
        let result = self.persist.commit_frame(id.index(), &descriptor, fault);
        {
            let mut state = self.state.lock();
            let mut objects = self.objects.lock(id.index());
            let record = &mut objects.records[id.index()];
            record.pins -= 1;
            record.persisting = false;
            self.account_unpin();
            if result.is_ok() {
                record.committed = true;
                record.key = Some(key);
                record.version = version;
                state.metadata_index.insert(key, id.index());
            }
        }
        note_metadata_pin_change();
        result
    }

    fn release_pcache_update(&self, id: NvFrameId) {
        {
            let mut objects = self.objects.lock(id.index());
            let record = &mut objects.records[id.index()];
            debug_assert_eq!(record.kind, ObjectKind::Metadata);
            debug_assert!(record.persisting && record.pins > 0);
            record.pins -= 1;
            record.persisting = false;
            self.account_unpin();
        }
        note_metadata_pin_change();
    }

    /// Clears the descriptor before returning its exact generation to the pool.
    pub(crate) fn clear_and_free_metadata(&self, page: PersistentPageRef) -> Result<()> {
        let id = NvFrameId::try_from(page.slot as usize).map_err(|_| Error::new(Errno::EINVAL))?;
        {
            let mut objects = self.objects.lock_checked(id.index())?;
            let record = &objects.records[id.index()];
            if record.kind != ObjectKind::Metadata
                || !record.committed
                || record.alloc_seq != page.generation
                || record
                    .key
                    .is_none_or(|key| typed_object_type(key.page_index).is_none())
            {
                return_errno_with_message!(Errno::EINVAL, "stale persistent metadata reference");
            }
            if record.pins != 0 || record.persisting || objects.metadata_mutation.is_some() {
                return_errno_with_message!(Errno::EBUSY, "persistent metadata frame is busy");
            }
            self.account_pin()?;
            objects.records[id.index()].pins = 1;
            objects.records[id.index()].persisting = true;
        }
        let result = self.persist.clear_frame(id.index(), None);
        {
            let mut state = self.state.lock();
            let mut allocation = self.allocation.lock();
            let mut objects = self.objects.lock(id.index());
            let record = &mut objects.records[id.index()];
            debug_assert_eq!(record.alloc_seq, page.generation);
            debug_assert_eq!(record.pins, 1);
            record.pins = 0;
            record.persisting = false;
            self.account_unpin();
            if result.is_ok() {
                if let Some(key) = record.key
                    && state.metadata_index.get(&key) == Some(&id.index())
                {
                    state.metadata_index.remove(&key);
                }
                record.committed = false;
                record.key = None;
                record.version = 0;
                record.kind = ObjectKind::Free;
                allocation.free_ids.push(id.index());
                allocation.capacity.account_free(PoolClass::Metadata);
            }
        }
        note_metadata_pin_change();
        result
    }

    pub(crate) fn committed_typed_metadata(&self) -> Vec<(PageKey, PersistentPageRef)> {
        self.objects.collect(|index, record| {
            let key = record.key?;
            (record.committed
                && record.kind == ObjectKind::Metadata
                && typed_object_type(key.page_index).is_some())
            .then_some((key, PersistentPageRef::new(index as u32, record.alloc_seq)))
        })
    }

    pub(crate) fn committed_typed_metadata_for(
        &self,
        fs_id: u64,
        inode: u64,
    ) -> Vec<(u64, PersistentPageRef)> {
        self.committed_typed_metadata()
            .into_iter()
            .filter(|(key, _)| key.fs_id == fs_id && key.inode == inode)
            .map(|(key, reference)| (key.page_index, reference))
            .collect()
    }

    pub(crate) fn peek_recovered_pcache(
        self: &Arc<Self>,
        fs_id: u64,
        inode: u64,
    ) -> Result<Option<recover::RecoveredInodePcache>> {
        if self.recovered_pcache.lock().is_none() {
            let catalog = recover::pcache_catalog(self.clone())?;
            let mut recovered = self.recovered_pcache.lock();
            if recovered.is_none() {
                *recovered = Some(catalog);
            }
        }
        Ok(self
            .recovered_pcache
            .lock()
            .as_ref()
            .and_then(|catalog| catalog.get(&(fs_id, inode)).cloned()))
    }

    pub(crate) fn take_recovered_pcache(
        &self,
        fs_id: u64,
        inode: u64,
    ) -> Option<recover::RecoveredInodePcache> {
        self.recovered_pcache
            .lock()
            .as_mut()
            .and_then(|catalog| catalog.remove(&(fs_id, inode)))
    }

    pub(crate) fn invalidate_recovered_pcache(&self) {
        *self.recovered_pcache.lock() = None;
    }

    /// Reclaims an unselected rNVPC page; the caller has traced tree/WAL roots.
    pub(crate) fn reclaim_committed_data_page_exact(
        &self,
        id: NvPageId,
        generation: u64,
        key: PageKey,
        fault: Option<FaultPoint>,
    ) -> Result<bool> {
        {
            let state = self.state.lock();
            let mut objects = self.objects.lock_checked(id.index())?;
            let record = &objects.records[id.index()];
            if !matches!(record.kind, ObjectKind::Data(_))
                || !record.committed
                || record.key != Some(key)
                || record.alloc_seq != generation
                || record.pins != 0
                || record.persisting
                || state.references.values().any(|entry| entry.page == id)
            {
                return Ok(false);
            }
            self.account_pin()?;
            objects.records[id.index()].pins = 1;
            objects.records[id.index()].persisting = true;
        }
        let result = self.persist.clear_frame(id.index(), fault);
        {
            let mut state = self.state.lock();
            let mut allocation = self.allocation.lock();
            let mut objects = self.objects.lock(id.index());
            let record = &mut objects.records[id.index()];
            debug_assert_eq!(record.alloc_seq, generation);
            debug_assert_eq!(record.key, Some(key));
            debug_assert_eq!(record.pins, 1);
            record.pins = 0;
            record.persisting = false;
            self.account_unpin();
            if result.is_ok() {
                if state
                    .index
                    .lookup(key)
                    .is_some_and(|entry| entry.handle == Some(RuntimeHandle::Nv(id)))
                {
                    state.index.remove(key);
                }
                record.committed = false;
                record.key = None;
                record.version = 0;
                record.kind = ObjectKind::Free;
                allocation.free_ids.push(id.index());
                allocation.capacity.account_free(PoolClass::Data);
            }
        }
        result.map(|_| true)
    }
}

impl PersistentFrameUpdate {
    pub(crate) fn read(&self, output: &mut [u8; PAGE_SIZE]) -> Result<()> {
        self.pool
            .backend
            .read_page(self.pool.page_id(self.id), output)
    }

    pub(crate) fn write_page(&self, input: &[u8; PAGE_SIZE]) -> Result<()> {
        self.pool
            .backend
            .write_page(self.pool.page_id(self.id), input)
    }

    pub(crate) fn persist_range(&self, offset: usize, len: usize) -> Result<()> {
        if offset.checked_add(len).is_none_or(|end| end > PAGE_SIZE) {
            return_errno_with_message!(
                Errno::EINVAL,
                "metadata persistence range is out of bounds"
            );
        }
        let absolute = self
            .id
            .index()
            .checked_mul(PAGE_SIZE)
            .and_then(|base| base.checked_add(offset))
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.pool.backend.persist(absolute, len)
    }

    pub(crate) fn fence(&self) -> Result<()> {
        self.pool.backend.fence()
    }

    pub(crate) fn finish(mut self) {
        self.release();
    }

    fn release(&mut self) {
        if self.active {
            self.pool.release_pcache_update(self.id);
            self.active = false;
        }
    }
}

impl Drop for PersistentFrameUpdate {
    fn drop(&mut self) {
        self.release();
    }
}
