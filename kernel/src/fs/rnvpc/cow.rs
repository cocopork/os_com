// SPDX-License-Identifier: MPL-2.0

//! Persistent foreground copy-on-write state for one rNVPC inode.

use core::mem;
#[cfg(ktest)]
use core::sync::atomic::AtomicBool;
#[cfg(ktest)]
use core::sync::atomic::{AtomicUsize, Ordering};

#[cfg(ktest)]
use super::pcache_store::PcFaultPoint;
use super::{
    data_page::DataPageState,
    frame::Rnvpc,
    id::NvPageId,
    index::PageKey,
    pcache_layout::{PcObjectType, PersistentPageRef},
    pcache_store::PersistentMetadataStore,
    persistent_btree::PersistentBtree,
    wal::{PersistentInodeWal, WalUpsert},
};
use crate::prelude::*;
#[cfg(ktest)]
use crate::thread::Thread;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CowCounters {
    pub(crate) updates: u64,
    pub(crate) cow_alloc_copy_ns: u64,
    pub(crate) wal_backpressure_waits: u64,
    pub(crate) foreground_checkpoints: u64,
    pub(crate) records_replayed: u64,
    pub(crate) old_pages_freed: u64,
    pub(crate) checkpoint_blocked_writes: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ForegroundCheckpointCounters {
    pub(crate) wal_appends: u64,
    pub(crate) checkpoint_count: u64,
    pub(crate) checkpoint_records: u64,
    pub(crate) btree_updates: u64,
    pub(crate) btree_splits: u64,
    pub(crate) persist_fences: u64,
    pub(crate) checkpoint_blocked_writes: u64,
    pub(crate) wal_max_used: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UpdateOutcome {
    pub(crate) page_index: u64,
    pub(crate) new_page: NvPageId,
    pub(crate) replaced: Option<NvPageId>,
    pub(crate) waited_for_wal: bool,
}

#[derive(Clone, Copy)]
struct CowEntry {
    page: PersistentPageRef,
    lsn: u64,
    ssd_target: u64,
    wal_pinned: bool,
}

/// The lock is deliberately per context: it serializes one inode without
/// coupling independent `(fs_id, inode)` foreground checkpoints.
pub(crate) struct InodeCowContext {
    fs_id: u64,
    inode: u64,
    pool: Arc<Rnvpc>,
    wal: Arc<PersistentInodeWal>,
    tree: Arc<PersistentBtree>,
    mappings: Mutex<BTreeMap<u64, CowEntry>>,
    replaced_pages: Mutex<Vec<PersistentPageRef>>,
    mutation_lock: ostd::sync::Mutex<()>,
    counters: Mutex<CowCounters>,
    #[cfg(ktest)]
    checkpoint_fault: Mutex<Option<PcFaultPoint>>,
    #[cfg(ktest)]
    append_fault: Mutex<Option<PcFaultPoint>>,
    #[cfg(ktest)]
    mutation_active: AtomicUsize,
    #[cfg(ktest)]
    mutation_peak: AtomicUsize,
    #[cfg(ktest)]
    checkpoint_hook: Mutex<Option<Arc<CheckpointTestHook>>>,
}

impl InodeCowContext {
    pub(crate) fn create(pool: Arc<Rnvpc>, fs_id: u64, inode: u64) -> Result<Arc<Self>> {
        let tree = PersistentBtree::create(pool.clone(), fs_id, inode)?;
        let wal =
            PersistentInodeWal::create(pool.clone(), fs_id, inode, tree.root_ref(), tree.height())?;
        Ok(Arc::new(Self::with_parts(
            pool.clone(),
            fs_id,
            inode,
            wal,
            tree,
        )))
    }

    pub(crate) fn recover_from_catalog(
        pool: Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
    ) -> Result<Arc<Self>> {
        let result = Self::recover_from_catalog_inner(pool.clone(), fs_id, inode);
        if result.is_err() {
            pool.invalidate_recovered_pcache();
        }
        result
    }

    fn recover_from_catalog_inner(pool: Arc<Rnvpc>, fs_id: u64, inode: u64) -> Result<Arc<Self>> {
        let recovered = pool
            .peek_recovered_pcache(fs_id, inode)?
            .ok_or_else(|| Error::new(Errno::ENOENT))?;
        let store = PersistentMetadataStore::new(pool.clone());
        store.validate_typed_ref(recovered.anchor_ref, fs_id, inode, PcObjectType::Anchor, 0)?;
        for (node_id, reference) in &recovered.node_frames {
            store.validate_typed_ref(
                *reference,
                fs_id,
                inode,
                PcObjectType::BtreeNode,
                *node_id,
            )?;
        }
        for (index, reference) in recovered.wal_frames.iter().enumerate() {
            store.validate_typed_ref(
                *reference,
                fs_id,
                inode,
                PcObjectType::WalFrame,
                index as u64,
            )?;
        }
        let anchor_ref = recovered.anchor_ref;
        let anchor = recovered.anchor;
        let selected = anchor.images[0];
        let tree =
            PersistentBtree::recover(pool.clone(), fs_id, inode, selected.root, selected.height)?;
        let wal = PersistentInodeWal::recover(pool.clone(), anchor_ref, anchor)?;
        let context = Arc::new(Self::with_parts(pool.clone(), fs_id, inode, wal, tree));
        let records = context.wal.records_after(context.wal.checkpointed_lsn())?;
        let wal_frames = recovered.wal_frames;
        for record in &records {
            if let Some(root_update) = context.tree.replay_upsert(record, None)? {
                context.wal.publish_root(root_update, None)?;
                context.tree.install_published_root(root_update)?;
            }
        }
        if let Some(last) = records.last() {
            context.wal.publish_checkpoint(last.lsn, None)?;
        }
        context.reclaim_recovered_orphans(anchor_ref, &wal_frames, &[])?;
        context.rebuild_mapping_cache()?;
        let _ = pool.take_recovered_pcache(fs_id, inode);
        Ok(context)
    }

    pub(crate) fn recover(
        pool: Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
        wal: Arc<PersistentInodeWal>,
        tree: Arc<PersistentBtree>,
    ) -> Result<Arc<Self>> {
        let context = Arc::new(Self::with_parts(pool.clone(), fs_id, inode, wal, tree));
        context.rebuild_mapping_cache()?;
        Ok(context)
    }

    fn with_parts(
        pool: Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
        wal: Arc<PersistentInodeWal>,
        tree: Arc<PersistentBtree>,
    ) -> Self {
        Self {
            fs_id,
            inode,
            pool,
            wal,
            tree,
            mappings: Mutex::new(BTreeMap::new()),
            replaced_pages: Mutex::new(Vec::new()),
            mutation_lock: ostd::sync::Mutex::new(()),
            counters: Mutex::new(CowCounters::default()),
            #[cfg(ktest)]
            checkpoint_fault: Mutex::new(None),
            #[cfg(ktest)]
            append_fault: Mutex::new(None),
            #[cfg(ktest)]
            mutation_active: AtomicUsize::new(0),
            #[cfg(ktest)]
            mutation_peak: AtomicUsize::new(0),
            #[cfg(ktest)]
            checkpoint_hook: Mutex::new(None),
        }
    }

    pub(crate) fn wal(&self) -> &Arc<PersistentInodeWal> {
        &self.wal
    }
    pub(crate) fn counters(&self) -> CowCounters {
        *self.counters.lock()
    }
    pub(crate) fn wal_counters(&self) -> super::wal::WalCounters {
        Default::default()
    }

    pub(crate) fn foreground_checkpoint_counters(&self) -> ForegroundCheckpointCounters {
        let cow = self.counters();
        let wal = self.wal.counters();
        let tree = self.tree.stats();
        ForegroundCheckpointCounters {
            wal_appends: wal.wal_appends,
            checkpoint_count: cow.foreground_checkpoints,
            checkpoint_records: cow.records_replayed,
            btree_updates: tree.node_updates,
            btree_splits: tree.btree_splits,
            persist_fences: wal.persist_fences + tree.persist_fences,
            checkpoint_blocked_writes: cow.checkpoint_blocked_writes,
            wal_max_used: wal.wal_max_used,
        }
    }

    #[cfg(ktest)]
    pub(crate) fn set_checkpoint_fault_for_test(&self, fault: Option<PcFaultPoint>) {
        *self.checkpoint_fault.lock() = fault;
    }

    #[cfg(ktest)]
    pub(crate) fn set_append_fault_for_test(&self, fault: Option<PcFaultPoint>) {
        *self.append_fault.lock() = fault;
    }

    #[cfg(ktest)]
    pub(crate) fn tree_mapping_for_test(
        &self,
        page_index: u64,
    ) -> Result<super::persistent_btree::PersistentMapping> {
        self.tree
            .lookup(page_index)?
            .ok_or_else(|| Error::new(Errno::ENOENT))
    }

    #[cfg(ktest)]
    pub(crate) fn context_key_for_test(&self) -> (u64, u64) {
        (self.fs_id, self.inode)
    }

    #[cfg(ktest)]
    pub(crate) fn max_mutation_concurrency_for_test(&self) -> usize {
        self.mutation_peak.load(Ordering::Relaxed)
    }

    #[cfg(ktest)]
    pub(crate) fn set_checkpoint_hook_for_test(&self, hook: Option<Arc<CheckpointTestHook>>) {
        *self.checkpoint_hook.lock() = hook;
    }

    #[cfg(ktest)]
    pub(crate) fn force_checkpoint_for_test(&self) -> Result<()> {
        let _serial = self.mutation_lock.lock();
        if !self.wal.is_full() {
            return_errno_with_message!(Errno::EINVAL, "test checkpoint requires a full WAL");
        }
        self.checkpoint_if_full_locked()?;
        Ok(())
    }

    /// Replays exactly a full WAL before accepting the caller's update.
    pub(crate) fn checkpoint_if_full(&self) -> Result<bool> {
        let _serial = self.mutation_lock.lock();
        self.checkpoint_if_full_locked()
    }

    fn checkpoint_if_full_locked(&self) -> Result<bool> {
        if !self.wal.is_full() {
            return Ok(false);
        }
        self.counters.lock().checkpoint_blocked_writes += 1;
        #[cfg(ktest)]
        let _hook = self.checkpoint_hook.lock().clone().map(|hook| hook.enter());
        let records = self.wal.records_after(self.wal.checkpointed_lsn())?;
        if records.len() != super::wal::WAL_CAPACITY_PER_INODE {
            return_errno_with_message!(Errno::EIO, "full WAL has an incomplete replay set");
        }
        for record in &records {
            let mut replay = *record;
            if let Some(entry) = self.mappings.lock().get(&record.page_index).copied()
                && entry.lsn == record.lsn
            {
                replay.ssd_backing_block = entry.ssd_target;
            }
            if let Some(root_update) = self.tree.replay_upsert(&replay, None)? {
                self.wal.publish_root(root_update, None)?;
                self.tree.install_published_root(root_update)?;
            }
        }
        #[cfg(ktest)]
        let fault = *self.checkpoint_fault.lock();
        #[cfg(not(ktest))]
        let fault = None;
        let checkpointed_lsn = records.last().unwrap().lsn;
        self.wal.publish_checkpoint(checkpointed_lsn, fault)?;
        {
            let mut mappings = self.mappings.lock();
            for entry in mappings.values_mut() {
                if entry.lsn <= checkpointed_lsn {
                    entry.wal_pinned = false;
                }
            }
        }
        self.reclaim_replaced_pages(checkpointed_lsn);
        let mut counters = self.counters.lock();
        counters.foreground_checkpoints += 1;
        counters.records_replayed += records.len() as u64;
        Ok(true)
    }

    pub(crate) fn update(
        &self,
        page_index: u64,
        ssd_backing_block: u64,
        apply: impl FnOnce(&mut [u8]),
    ) -> Result<UpdateOutcome> {
        let _serial = self.mutation_lock.lock();
        #[cfg(ktest)]
        let _mutation = MutationGuard::new(self);
        self.checkpoint_if_full_locked()?;
        let old = self.current_mapping(page_index)?;
        let mut bytes = [0u8; PAGE_SIZE];
        if let Some(mapping) = old {
            self.pool
                .pin_committed_data_ref(
                    mapping.page,
                    PageKey::new(self.fs_id, self.inode, page_index),
                )?
                .read(&mut bytes)?;
        }
        apply(&mut bytes);
        let new = self.persist_new_nvpage(page_index, &bytes)?;
        let appended = match self.wal.append(
            WalUpsert {
                page_index,
                old_page: old.map_or(PersistentPageRef::NONE, |mapping| mapping.page),
                new_page: new,
                ssd_backing_block,
            },
            #[cfg(ktest)]
            *self.append_fault.lock(),
            #[cfg(not(ktest))]
            None,
        ) {
            Ok(appended) => appended,
            Err(error) => {
                if !self.wal.is_poisoned() {
                    self.discard_unreferenced_page(new);
                }
                return Err(error);
            }
        };
        let new_id = NvPageId::try_from(new.slot as usize).map_err(|_| Error::new(Errno::EIO))?;
        if let Some(old) = old {
            self.replaced_pages.lock().push(old.page);
        }
        self.mappings.lock().insert(
            page_index,
            CowEntry {
                page: new,
                lsn: appended.lsn,
                ssd_target: ssd_backing_block,
                wal_pinned: true,
            },
        );
        self.counters.lock().updates += 1;
        Ok(UpdateOutcome {
            page_index,
            new_page: new_id,
            replaced: old.and_then(|m| NvPageId::try_from(m.page.slot as usize).ok()),
            waited_for_wal: false,
        })
    }

    pub(crate) fn mark_writeback_target(&self, page_index: u64, pblock: u64) -> Result<bool> {
        let _serial = self.mutation_lock.lock();
        let existing = self.current_mapping(page_index)?;
        let Some(mut entry) = existing else {
            return Ok(false);
        };
        if entry.lsn <= self.wal.checkpointed_lsn() {
            self.tree.update_ssd_backing_block(page_index, pblock)?;
        }
        entry.ssd_target = pblock;
        self.mappings.lock().insert(page_index, entry);
        Ok(true)
    }

    pub(crate) fn flush_candidates(&self, budget: usize) -> Vec<(u64, NvPageId, u64, u64, u64)> {
        self.mappings
            .lock()
            .iter()
            .filter(|(_, entry)| entry.ssd_target != u64::MAX && !entry.wal_pinned)
            .take(budget)
            .filter_map(|(index, entry)| {
                let id = NvPageId::try_from(entry.page.slot as usize).ok()?;
                Some((
                    *index,
                    id,
                    entry.ssd_target,
                    entry.page.generation,
                    entry.page.generation,
                ))
            })
            .collect()
    }

    pub(crate) fn has_flushable_page(&self) -> bool {
        self.mappings
            .lock()
            .values()
            .any(|entry| entry.ssd_target != u64::MAX && !entry.wal_pinned)
    }

    pub(crate) fn prepare_flush_completion(
        &self,
        page_index: u64,
        page: NvPageId,
        generation: u64,
    ) -> Result<bool> {
        let _serial = self.mutation_lock.lock();
        let expected = PersistentPageRef::new(page.index() as u32, generation);
        let mut mappings = self.mappings.lock();
        let Some(entry) = mappings.get(&page_index).copied() else {
            return Ok(false);
        };
        if entry.page != expected || entry.wal_pinned {
            return Ok(false);
        }
        if !self.tree.remove_mapping(page_index, expected)? {
            return Ok(false);
        }
        mappings.remove(&page_index);
        Ok(true)
    }

    pub(crate) fn finalize_flush_completion(&self, page: NvPageId, generation: u64) -> Result<()> {
        if !self.pool.free_data_page_if_seq(page, generation)?
            && self.pool.data_page_seq(page) == Some(generation)
        {
            return_errno_with_message!(
                Errno::EBUSY,
                "SSD-completed page remains pinned or committed"
            );
        }
        Ok(())
    }

    pub(crate) fn read_page(&self, page_index: u64, out: &mut [u8]) -> Result<bool> {
        let _serial = self.mutation_lock.lock();
        let Some(mapping) = self.current_mapping(page_index)? else {
            return Ok(false);
        };
        self.pool
            .pin_committed_data_ref(
                mapping.page,
                PageKey::new(self.fs_id, self.inode, page_index),
            )?
            .read(out)?;
        Ok(true)
    }

    pub(crate) fn resident_page(&self, page_index: u64) -> Option<NvPageId> {
        self.current_mapping(page_index)
            .ok()
            .flatten()
            .and_then(|mapping| NvPageId::try_from(mapping.page.slot as usize).ok())
    }

    pub(crate) fn read_resident(&self, page: NvPageId, out: &mut [u8]) -> Result<bool> {
        let _serial = self.mutation_lock.lock();
        let entry = self
            .mappings
            .lock()
            .iter()
            .find(|(_, entry)| entry.page.slot as usize == page.index())
            .map(|(index, entry)| (*index, *entry));
        let Some((index, entry)) = entry else {
            return Ok(false);
        };
        self.pool
            .pin_committed_data_ref(entry.page, PageKey::new(self.fs_id, self.inode, index))?
            .read(out)?;
        Ok(true)
    }

    pub(crate) fn pending_old_pages(&self) -> usize {
        self.replaced_pages.lock().len()
    }
    pub(crate) fn complete_page(&self, _page_index: u64) -> Result<usize> {
        Ok(0)
    }

    fn current_mapping(&self, page_index: u64) -> Result<Option<CowEntry>> {
        if let Some(entry) = self.mappings.lock().get(&page_index).copied() {
            return Ok(Some(entry));
        }
        Ok(self.tree.lookup(page_index)?.map(|mapping| CowEntry {
            page: mapping.page,
            lsn: mapping.lsn,
            ssd_target: mapping.ssd_backing_block,
            wal_pinned: mapping.lsn > self.wal.checkpointed_lsn(),
        }))
    }

    fn persist_new_nvpage(&self, page_index: u64, bytes: &[u8]) -> Result<PersistentPageRef> {
        let page = self.pool.stage_data_page(bytes)?;
        let generation = self
            .pool
            .data_page_seq(page)
            .ok_or_else(|| Error::new(Errno::EIO))?;
        if let Err(error) =
            self.pool
                .transition_data_page(page, DataPageState::Staged, DataPageState::NvDirty)
        {
            let _ = self.pool.free_data_page(page);
            return Err(error);
        }
        if let Err(error) = self.pool.commit_data_page_in_owner(
            page,
            PageKey::new(self.fs_id, self.inode, page_index),
            None,
        ) {
            let _ = self.pool.free_data_page(page);
            return Err(error);
        }
        Ok(PersistentPageRef::new(page.index() as u32, generation))
    }

    fn discard_unreferenced_page(&self, page: PersistentPageRef) {
        let _ = self.reclaim_page_exact(page);
    }

    fn reclaim_replaced_pages(&self, checkpointed_lsn: u64) {
        let pages = {
            let mut pages = self.replaced_pages.lock();
            mem::take(&mut *pages)
        };
        let mut freed = 0;
        for page in pages {
            freed += self.reclaim_page_exact(page) as u64;
        }
        self.counters.lock().old_pages_freed += freed;
        let _ = checkpointed_lsn;
    }

    fn reclaim_page_exact(&self, page: PersistentPageRef) -> bool {
        let Ok(id) = NvPageId::try_from(page.slot as usize) else {
            return false;
        };
        let Ok(eviction) = self
            .pool
            .begin_data_page_eviction_if_seq(id, page.generation)
        else {
            return false;
        };
        eviction.finish(None).is_ok()
            && self
                .pool
                .free_data_page_if_seq(id, page.generation)
                .unwrap_or(false)
    }

    fn reclaim_recovered_orphans(
        &self,
        anchor: PersistentPageRef,
        wal_frames: &[PersistentPageRef],
        pending: &[super::pcache_layout::PersistentWalRecord],
    ) -> Result<()> {
        let (mappings, nodes) = self.tree.reachable_mappings_and_nodes()?;
        let mut metadata = BTreeSet::new();
        metadata.insert((anchor.slot, anchor.generation));
        metadata.extend(wal_frames.iter().map(|page| (page.slot, page.generation)));
        metadata.extend(nodes.iter().map(|page| (page.slot, page.generation)));
        let mut data = BTreeSet::new();
        data.extend(
            mappings
                .iter()
                .map(|mapping| (mapping.page.slot, mapping.page.generation)),
        );
        for record in pending {
            data.insert((record.new_page.slot, record.new_page.generation));
            if record.old_page != PersistentPageRef::NONE {
                data.insert((record.old_page.slot, record.old_page.generation));
            }
        }
        for (_, page) in self
            .pool
            .committed_typed_metadata_for(self.fs_id, self.inode)
        {
            if !metadata.contains(&(page.slot, page.generation)) {
                PersistentMetadataStore::new(self.pool.clone()).remove(page)?;
            }
        }
        for (page_index, page) in self.pool.committed_data_pages_of(self.fs_id, self.inode) {
            let generation = self
                .pool
                .data_page_seq(page)
                .ok_or_else(|| Error::new(Errno::EIO))?;
            if !data.contains(&(page.index() as u32, generation)) {
                self.pool.reclaim_committed_data_page_exact(
                    page,
                    generation,
                    PageKey::new(self.fs_id, self.inode, page_index),
                    None,
                )?;
            }
        }
        Ok(())
    }
    fn rebuild_mapping_cache(&self) -> Result<()> {
        let (mappings, _) = self.tree.reachable_mappings_and_nodes()?;
        let mut cache = self.mappings.lock();
        cache.clear();
        for mapping in mappings {
            cache.insert(
                mapping.page_index,
                CowEntry {
                    page: mapping.page,
                    lsn: mapping.lsn,
                    ssd_target: mapping.ssd_backing_block,
                    wal_pinned: false,
                },
            );
        }
        Ok(())
    }
}

#[cfg(ktest)]
pub(crate) struct CheckpointTestHook {
    entered: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
    released: AtomicBool,
}

#[cfg(ktest)]
impl CheckpointTestHook {
    pub(crate) fn new() -> Self {
        Self {
            entered: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            released: AtomicBool::new(false),
        }
    }

    fn enter(self: &Arc<Self>) -> CheckpointHookGuard {
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(active, Ordering::AcqRel);
        self.entered.fetch_add(1, Ordering::Release);
        while !self.released.load(Ordering::Acquire) {
            Thread::yield_now();
        }
        CheckpointHookGuard { hook: self.clone() }
    }

    pub(crate) fn wait_for_entered(&self, count: usize) {
        while self.entered.load(Ordering::Acquire) < count {
            Thread::yield_now();
        }
    }
    pub(crate) fn entered(&self) -> usize {
        self.entered.load(Ordering::Acquire)
    }
    pub(crate) fn max_active(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }
    pub(crate) fn release(&self) {
        self.released.store(true, Ordering::Release);
    }
}

#[cfg(ktest)]
struct CheckpointHookGuard {
    hook: Arc<CheckpointTestHook>,
}
#[cfg(ktest)]
impl Drop for CheckpointHookGuard {
    fn drop(&mut self) {
        self.hook.active.fetch_sub(1, Ordering::AcqRel);
    }
}
#[cfg(ktest)]
struct MutationGuard<'a> {
    context: &'a InodeCowContext,
}
#[cfg(ktest)]
impl<'a> MutationGuard<'a> {
    fn new(context: &'a InodeCowContext) -> Self {
        let active = context.mutation_active.fetch_add(1, Ordering::AcqRel) + 1;
        context.mutation_peak.fetch_max(active, Ordering::AcqRel);
        Self { context }
    }
}
#[cfg(ktest)]
impl Drop for MutationGuard<'_> {
    fn drop(&mut self) {
        self.context.mutation_active.fetch_sub(1, Ordering::AcqRel);
    }
}
