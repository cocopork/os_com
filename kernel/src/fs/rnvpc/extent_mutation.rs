// SPDX-License-Identifier: MPL-2.0

//! Ordering primitives for local soft-update extent mutations.
//!
//! A parent reference can be published only with an [`InitializedExtentNode`]
//! minted after the referenced node is durably committed. Split operations keep
//! a durable mutation mark from parent publication through old-child repair, so
//! recovery can finish the local mutation without rebuilding the whole tree.

use core::cell::RefCell;

#[cfg(ktest)]
use super::index::ReverseExtentRef;
use super::{
    fault::{FaultPoint, inject},
    frame::{
        BoxedMetadataRecord, FIXED_INODE_LOGICAL_INDEX, MetadataMutationGuard, PublicationGuard,
        Rnvpc,
    },
    id::NvFrameId,
    index::PageKey,
    metadata::{
        AuthoritativeMetadataRecord, EXTENT_NODE_ENTRY_CAPACITY, ExtentDataRef,
        ExtentMutationOperation, ExtentMutationPhase, ExtentNodeRecord, ExtentRecordEntry,
        InodeRecord, PENDING_EXTENT_MUTATION_LOGICAL_INDEX, PendingExtentMutationRecord,
        PersistentObjectRef,
    },
};
#[cfg(ktest)]
use crate::fs::fs_impls::ext4::pipeline_profile as pp;
use crate::{
    fs::fs_impls::ext4::{LATENCY_STATS, read_ns},
    prelude::*,
};

const EXTENT_TRANSIENT_LOGICAL_INDEX_TAG: u64 = 1 << 62;

#[derive(Clone, Copy)]
#[cfg(ktest)]
pub(crate) struct SsdExtentUpdate {
    pub(crate) locator: ReverseExtentRef,
    pub(crate) expected: PersistentObjectRef,
    pub(crate) logical_block: u64,
    pub(crate) ssd_block: u64,
}

/// Proof that an extent node is fully initialized and committed in NVPool.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InitializedExtentNode {
    reference: PersistentObjectRef,
    fs_id: u64,
    inode: u64,
    node_id: u64,
    level: u16,
}

impl InitializedExtentNode {
    pub(crate) const fn reference(&self) -> PersistentObjectRef {
        self.reference
    }
}

/// Per-inode coordinator for publishing local extent-tree references.
pub(crate) struct ExtentMutation {
    pool: Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    node_guards: RefCell<Vec<(PersistentObjectRef, MetadataMutationGuard)>>,
    publication: PublicationGuard,
}

impl ExtentMutation {
    pub(crate) fn with_publication(
        pool: Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
        publication: PublicationGuard,
    ) -> Result<Self> {
        if !publication.matches(&pool, fs_id, inode) {
            return_errno_with_message!(Errno::EPERM, "publication owner mismatch");
        }
        Ok(Self {
            pool,
            fs_id,
            inode,
            node_guards: RefCell::new(Vec::new()),
            publication,
        })
    }

    pub(crate) fn new(pool: Arc<Rnvpc>, fs_id: u64, inode: u64) -> Result<Self> {
        let publication = pool.begin_publication_for(fs_id, inode)?;
        Ok(Self {
            pool,
            fs_id,
            inode,
            node_guards: RefCell::new(Vec::new()),
            publication,
        })
    }

    /// Creates an inode-owned control record or updates its existing NVBH.
    pub(crate) fn persist_or_update_control<T>(
        &self,
        key: PageKey,
        record: &T,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef>
    where
        T: AuthoritativeMetadataRecord,
    {
        if key.fs_id != self.fs_id || key.inode != self.inode {
            return_errno_with_message!(Errno::EINVAL, "control record belongs to another inode");
        }
        match self.pool.current_metadata_ref(key) {
            Some(current) => self.pool.update_authoritative_metadata_in_place(
                &self.publication,
                current,
                record,
                fault,
            ),
            None => self.pool.persist_authoritative_metadata(key, record, fault),
        }
    }

    /// Validates a leaf's completed pages before persisting one combined image.
    /// A failure preserves every NV reference in this leaf for retry.
    #[cfg(ktest)]
    pub(crate) fn publish_ssd_extents_by_reverse(
        &self,
        updates: &[SsdExtentUpdate],
        fault: Option<FaultPoint>,
    ) -> Result<bool> {
        Self::publish_ssd_extents_locked(&self.pool, self.fs_id, self.inode, updates, fault)
    }

    #[cfg(ktest)]
    pub(crate) fn publish_ssd_extents_locked(
        pool: &Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
        updates: &[SsdExtentUpdate],
        fault: Option<FaultPoint>,
    ) -> Result<bool> {
        let started = read_ns();
        let result = Self::publish_ssd_extents_profiled(pool, fs_id, inode, updates, fault);
        LATENCY_STATS.leaf_detail_total.record_since(started);
        result
    }

    #[cfg(ktest)]
    fn publish_ssd_extents_profiled(
        pool: &Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
        updates: &[SsdExtentUpdate],
        fault: Option<FaultPoint>,
    ) -> Result<bool> {
        let started = read_ns();
        let first = updates.first().ok_or_else(|| Error::new(Errno::EINVAL))?;
        let locator = first.locator;
        let frame = NvFrameId::try_from(
            usize::try_from(locator.leaf_slot()).map_err(|_| Error::new(Errno::EOVERFLOW))?,
        )?;
        let leaf_ref = pool.persistent_metadata_ref(frame)?;
        LATENCY_STATS.leaf_detail_identity.record_since(started);
        let started = read_ns();
        let lock_started = pp::now();
        let guard = pool.lock_metadata_mutation(leaf_ref)?;
        pp::NV_EXTENT_LEAF_LOCK.record_since(lock_started, updates.len());
        LATENCY_STATS.migration_leaf_lock_wait.record_since(started);
        let started = read_ns();
        let pending = Self::check_leaf_repair_pending(pool, fs_id, inode, leaf_ref);
        LATENCY_STATS.leaf_detail_pending.record_since(started);
        pending?;
        let started = read_ns();
        let read_started = pp::now();
        let leaf = pool.read_authoritative_metadata_boxed::<ExtentNodeRecord>(leaf_ref)?;
        pp::NV_EXTENT_LEAF_READ.record_since(read_started, updates.len());
        LATENCY_STATS.leaf_detail_read.record_since(started);
        let started = read_ns();
        let validate_started = pp::now();
        if leaf.header().fs_id() != fs_id || leaf.header().inode() != inode {
            return_errno_with_message!(Errno::ESTALE, "reverse leaf owner changed");
        }
        if leaf.level() != 0 {
            return_errno_with_message!(Errno::ESTALE, "reverse locator is not a leaf");
        }
        let entries = Vec::from(leaf.entries()?);
        LATENCY_STATS.leaf_detail_entries.record_since(started);
        let started = read_ns();
        let index = locator.entry_index();
        if index >= EXTENT_NODE_ENTRY_CAPACITY
            || updates
                .iter()
                .any(|update| update.locator != locator || update.ssd_block == 0)
        {
            return_errno_with_message!(Errno::EINVAL, "invalid extent migration group");
        }
        let entry = entries
            .get(index)
            .copied()
            .ok_or_else(|| Error::new(Errno::ESTALE))?;
        if updates.len() != entry.length() as usize {
            warn!(
                "incomplete NV extent migration: leaf={} entry={} logical={} length={} updates={}",
                locator.leaf_slot(),
                index,
                entry.logical_block(),
                entry.length(),
                updates.len()
            );
            return_errno_with_message!(Errno::EAGAIN, "incomplete NV extent migration");
        }
        let mut ordered = Vec::from(updates);
        ordered.sort_unstable_by_key(|update| update.logical_block);
        let first_ssd = ordered[0].ssd_block;
        for (offset, update) in ordered.iter().enumerate() {
            if pool.reverse_lookup(update.expected)? != Some(update.locator) {
                return_errno_with_message!(Errno::ESTALE, "reverse locator moved");
            }
            let offset = u64::try_from(offset).map_err(|_| Error::new(Errno::EOVERFLOW))?;
            let logical_block = entry
                .logical_block()
                .checked_add(offset)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            let ssd_block = first_ssd
                .checked_add(offset)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            if update.logical_block != logical_block || update.ssd_block != ssd_block {
                return_errno_with_message!(Errno::ESTALE, "reverse locator forward edge changed");
            }
            match entry.data_ref()? {
                ExtentDataRef::Ssd(current) if current == first_ssd => continue,
                ExtentDataRef::Nv(_)
                    if entry.nv_slot_at(logical_block)? == update.expected.slot_plus_one() => {}
                _ => return_errno_with_message!(
                    Errno::ESTALE,
                    "reverse locator forward edge changed"
                ),
            }
        }
        pp::NV_EXTENT_LEAF_VALIDATE.record_since(validate_started, updates.len());
        LATENCY_STATS.leaf_detail_validate.record_since(started);
        if matches!(entry.data_ref()?, ExtentDataRef::Ssd(current) if current == first_ssd) {
            return Ok(false);
        }
        let replacement = ExtentRecordEntry::leaf(
            entry.logical_block(),
            entry.length(),
            first_ssd,
            entry.is_unwritten(),
        )?;
        let started = read_ns();
        let persist_started = pp::now();
        pool.persist_extent_target_with_guard(
            &guard,
            leaf_ref,
            index,
            entry.target(),
            replacement.target(),
            fault,
        )?;
        pp::NV_EXTENT_TARGET_PERSIST.record_since(persist_started, updates.len());
        LATENCY_STATS.leaf_detail_update.record_since(started);
        LATENCY_STATS.rnvpce_completion_leaf_updates.record_event();
        Ok(true)
    }

    fn check_leaf_repair_pending(
        pool: &Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
        leaf: PersistentObjectRef,
    ) -> Result<()> {
        let key = PageKey::new(fs_id, inode, PENDING_EXTENT_MUTATION_LOGICAL_INDEX);
        for _ in 0..8 {
            let Some(reference) = pool.current_metadata_ref_for_completion(key) else {
                return Ok(());
            };
            let started = read_ns();
            let result = pool.read_authoritative_metadata::<PendingExtentMutationRecord>(reference);
            LATENCY_STATS.leaf_pending_read.record_since(started);
            match result {
                Ok(mark) => {
                    if [
                        mark.old_child(),
                        mark.old_parent(),
                        mark.new_child(),
                        mark.new_parent(),
                    ]
                    .contains(&leaf)
                    {
                        return_errno_with_message!(
                            Errno::EBUSY,
                            "extent leaf has unfinished repair"
                        );
                    }
                    return Ok(());
                }
                Err(error)
                    if matches!(error.error(), Errno::ESTALE | Errno::EINVAL | Errno::EBUSY) =>
                {
                    // Another leaf's foreground mutation may be clearing its mark.
                    LATENCY_STATS.leaf_pending_retry.record_event();
                    crate::thread::Thread::yield_now();
                }
                Err(error) => return Err(error),
            }
        }
        Err(Error::with_message(
            Errno::EAGAIN,
            "extent repair mark is changing",
        ))
    }

    fn read_locked_node(
        &self,
        reference: PersistentObjectRef,
    ) -> Result<BoxedMetadataRecord<ExtentNodeRecord>> {
        let held = self
            .node_guards
            .borrow()
            .iter()
            .any(|(current, _)| *current == reference);
        if !held {
            let guard = self.pool.lock_metadata_mutation(reference)?;
            if !guard.matches(&self.pool, reference, self.fs_id, self.inode) {
                return_errno_with_message!(Errno::ESTALE, "extent node owner changed");
            }
            self.node_guards.borrow_mut().push((reference, guard));
        }
        self.pool
            .read_authoritative_metadata_boxed::<ExtentNodeRecord>(reference)
    }

    /// Persists a fresh node before any parent can reference it.
    pub(crate) fn persist_initialized_child(
        &self,
        key: PageKey,
        child: &ExtentNodeRecord,
        fault: Option<FaultPoint>,
    ) -> Result<InitializedExtentNode> {
        self.validate_owner(child)?;
        if key.fs_id != self.fs_id || key.inode != self.inode {
            return_errno_with_message!(Errno::EINVAL, "extent child key belongs to another inode");
        }
        let reference = self
            .pool
            .persist_authoritative_metadata(key, child, fault)?;
        Ok(InitializedExtentNode {
            reference,
            fs_id: self.fs_id,
            inode: self.inode,
            node_id: child.node_id(),
            level: child.level(),
        })
    }

    /// Publishes a parent version only after `child` is durably initialized.
    pub(crate) fn publish_parent_reference(
        &self,
        current_parent: PersistentObjectRef,
        replacement_parent: &ExtentNodeRecord,
        child: &InitializedExtentNode,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef> {
        self.publish_parent_reference_inner(current_parent, replacement_parent, child, fault)
    }

    fn publish_parent_reference_inner(
        &self,
        current_parent: PersistentObjectRef,
        replacement_parent: &ExtentNodeRecord,
        child: &InitializedExtentNode,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef> {
        if child.fs_id != self.fs_id || child.inode != self.inode {
            return_errno_with_message!(Errno::EINVAL, "initialized child belongs to another inode");
        }
        self.validate_owner(replacement_parent)?;

        let old_parent = self.read_locked_node(current_parent)?;
        self.validate_owner(&old_parent)?;
        if replacement_parent.node_id() != old_parent.node_id()
            || replacement_parent.level() != old_parent.level()
            || replacement_parent.header().generation() <= old_parent.header().generation()
        {
            return_errno_with_message!(Errno::EINVAL, "invalid replacement extent parent");
        }
        if child
            .level
            .checked_add(1)
            .is_none_or(|parent_level| parent_level != replacement_parent.level())
        {
            return_errno_with_message!(Errno::EINVAL, "extent child level does not match parent");
        }

        let committed_child = self.read_locked_node(child.reference)?;
        self.validate_owner(&committed_child)?;
        if committed_child.node_id() != child.node_id || committed_child.level() != child.level {
            return_errno_with_message!(Errno::ESTALE, "initialized extent child was replaced");
        }
        if !replacement_parent
            .entries()?
            .iter()
            .any(|entry| entry.child_slot() == Some(child.reference.slot_plus_one()))
        {
            return_errno_with_message!(
                Errno::EINVAL,
                "replacement parent does not reference child"
            );
        }

        let boundary = replacement_parent
            .entries()?
            .first()
            .map(ExtentRecordEntry::logical_block)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        self.publish_repairable_node_rewrite(
            current_parent,
            replacement_parent,
            None,
            boundary,
            fault,
        )?;
        Ok(current_parent)
    }

    fn validate_owner(&self, node: &ExtentNodeRecord) -> Result<()> {
        if node.header().fs_id() != self.fs_id || node.header().inode() != self.inode {
            return_errno_with_message!(Errno::EINVAL, "extent node belongs to another inode");
        }
        Ok(())
    }

    fn right_sibling_ref(&self, node: &ExtentNodeRecord) -> Result<PersistentObjectRef> {
        node.right_sibling_slot()
            .map_or(Ok(PersistentObjectRef::NONE), |slot| {
                self.pool.persistent_metadata_ref_from_slot(slot)
            })
    }

    pub(crate) fn publish_fixed_inode_root(
        &self,
        inode_ref: PersistentObjectRef,
        old_root: PersistentObjectRef,
        new_root: PersistentObjectRef,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef> {
        self.pool
            .publish_fixed_inode_root(&self.publication, inode_ref, old_root, new_root, fault)
    }

    fn validate_split_parent_topology(
        &self,
        parent: &ExtentNodeRecord,
        old_child_ref: PersistentObjectRef,
        new_child_ref: PersistentObjectRef,
        split_logical_block: u64,
    ) -> Result<()> {
        self.validate_owner(parent)?;
        let old_child = self.read_locked_node(old_child_ref)?;
        let new_child = self.read_locked_node(new_child_ref)?;
        self.validate_owner(&old_child)?;
        self.validate_owner(&new_child)?;

        let old_boundary = old_child
            .entries()?
            .first()
            .map(ExtentRecordEntry::logical_block)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let new_boundary = new_child
            .entries()?
            .first()
            .map(ExtentRecordEntry::logical_block)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let parent_entries = parent.entries()?;
        // Lookup falls back to slot zero below its stored key, so that key is
        // not a semantic lower bound and may lag a first-child boundary change.
        let old_is_routed = parent_entries.iter().enumerate().any(|(index, entry)| {
            entry.child_slot() == Some(old_child_ref.slot_plus_one())
                && (index == 0 || entry.logical_block() == old_boundary)
        });
        let new_is_routed = parent_entries.iter().any(|entry| {
            entry.logical_block() == split_logical_block
                && entry.child_slot() == Some(new_child_ref.slot_plus_one())
        });
        if old_child.level() != new_child.level()
            || parent.level() != old_child.level().saturating_add(1)
            || new_boundary != split_logical_block
            || !old_is_routed
            || !new_is_routed
        {
            return_errno_with_message!(Errno::EINVAL, "invalid extent split parent topology");
        }
        Ok(())
    }

    fn transient_metadata_key(&self, current: PersistentObjectRef) -> Result<PageKey> {
        let mut sequence = current.allocation_seq();
        loop {
            if sequence >= EXTENT_TRANSIENT_LOGICAL_INDEX_TAG {
                return_errno_with_message!(
                    Errno::EOVERFLOW,
                    "transient extent key space exhausted"
                );
            }
            let key = PageKey::new(
                self.fs_id,
                self.inode,
                EXTENT_TRANSIENT_LOGICAL_INDEX_TAG | sequence,
            );
            if self.pool.current_metadata_ref(key).is_none() {
                return Ok(key);
            }
            sequence = sequence
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingExtentMutation {
    operation: ExtentMutationOperation,
    reference: PersistentObjectRef,
    parent_key: PageKey,
    old_parent: PersistentObjectRef,
    old_child: PersistentObjectRef,
    new_child: PersistentObjectRef,
    split_logical_block: u64,
    parent_reference: PersistentObjectRef,
    repair_parent: PersistentObjectRef,
    repair_child: PersistentObjectRef,
}

impl PendingExtentMutation {
    pub(crate) const fn reference(&self) -> PersistentObjectRef {
        self.reference
    }

    pub(crate) const fn parent_reference(&self) -> PersistentObjectRef {
        self.parent_reference
    }
}

impl ExtentMutation {
    pub(crate) fn begin_split(
        &self,
        parent_key: PageKey,
        old_parent: PersistentObjectRef,
        old_child: PersistentObjectRef,
        new_child: &InitializedExtentNode,
        split_logical_block: u64,
        fault: Option<FaultPoint>,
    ) -> Result<PendingExtentMutation> {
        if parent_key.fs_id != self.fs_id
            || parent_key.inode != self.inode
            || parent_key.page_index == PENDING_EXTENT_MUTATION_LOGICAL_INDEX
            || new_child.fs_id != self.fs_id
            || new_child.inode != self.inode
        {
            return_errno_with_message!(Errno::EINVAL, "invalid extent split identity");
        }

        let parent = self.read_locked_node(old_parent)?;
        let old = self.read_locked_node(old_child)?;
        let new = self.read_locked_node(new_child.reference)?;
        self.validate_owner(&parent)?;
        self.validate_owner(&old)?;
        self.validate_owner(&new)?;
        if new.node_id() != new_child.node_id
            || new.level() != new_child.level
            || parent.level() != old.level().saturating_add(1)
            || old.level() != new.level()
            || !parent
                .entries()?
                .iter()
                .any(|entry| entry.child_slot() == Some(old_child.slot_plus_one()))
            || new
                .entries()?
                .first()
                .is_none_or(|entry| entry.logical_block() != split_logical_block)
        {
            return_errno_with_message!(Errno::EINVAL, "invalid extent split topology");
        }
        let old_entries = old.entries()?;
        let old_start = old_entries
            .first()
            .map(|entry| entry.logical_block())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let old_end = old_entries
            .last()
            .map(|entry| {
                entry.logical_block().saturating_add(if old.level() == 0 {
                    entry.length() as u64
                } else {
                    1
                })
            })
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if old_start >= split_logical_block || old_end <= split_logical_block {
            return_errno_with_message!(Errno::EINVAL, "split point is outside the old child");
        }

        let persisted_parent_key = self.pool.persistent_metadata_key(old_parent)?;
        let old_child_key = self.pool.persistent_metadata_key(old_child)?;
        if persisted_parent_key != parent_key
            || old_child_key.fs_id != self.fs_id
            || old_child_key.inode != self.inode
        {
            return_errno_with_message!(Errno::EINVAL, "extent split keys do not match objects");
        }

        let record = PendingExtentMutationRecord::prepared_split(
            self.fs_id,
            self.inode,
            1,
            parent_key.page_index,
            old_child_key.page_index,
            old_parent,
            old_child,
            new_child.reference,
            split_logical_block,
        )?;
        let reference = self.pool.persist_authoritative_metadata(
            PageKey::new(
                self.fs_id,
                self.inode,
                PENDING_EXTENT_MUTATION_LOGICAL_INDEX,
            ),
            &record,
            fault,
        )?;
        Ok(PendingExtentMutation {
            operation: ExtentMutationOperation::NodeSplit,
            reference,
            parent_key,
            old_parent,
            old_child,
            new_child: new_child.reference,
            split_logical_block,
            parent_reference: old_parent,
            repair_parent: PersistentObjectRef::NONE,
            repair_child: PersistentObjectRef::NONE,
        })
    }

    pub(crate) fn publish_parent_reference_for_split(
        &self,
        current_parent: PersistentObjectRef,
        replacement_parent: &ExtentNodeRecord,
        child: &InitializedExtentNode,
        pending: PendingExtentMutation,
        fault: Option<FaultPoint>,
    ) -> Result<(PersistentObjectRef, PendingExtentMutation)> {
        if current_parent != pending.old_parent
            || child.reference != pending.new_child
            || pending.parent_key.fs_id != self.fs_id
            || pending.parent_key.inode != self.inode
        {
            return_errno_with_message!(Errno::EINVAL, "extent split token does not match");
        }
        let record = self
            .pool
            .read_authoritative_metadata::<PendingExtentMutationRecord>(pending.reference)?;
        if record.phase()? != ExtentMutationPhase::Prepared
            || record.parent_logical_index() != pending.parent_key.page_index
            || record.old_parent() != pending.old_parent
            || record.old_child() != pending.old_child
            || record.new_child() != pending.new_child
            || record.split_logical_block() != pending.split_logical_block
        {
            return_errno_with_message!(Errno::ESTALE, "pending extent split was replaced");
        }

        self.validate_split_parent_topology(
            replacement_parent,
            pending.old_child,
            pending.new_child,
            pending.split_logical_block,
        )?;
        // Legacy split staging keeps its own durable mark. The production
        // top-down path below uses repair images; this compatibility entry
        // point only advances the stable parent and then atomically advances
        // the already-persisted split mark.
        let parent_reference = self.pool.update_authoritative_metadata_in_place(
            &self.publication,
            current_parent,
            replacement_parent,
            fault,
        )?;
        let reference = self.pool.mark_pending_extent_mutation_phase(
            &self.publication,
            pending.reference,
            ExtentMutationPhase::Prepared,
            ExtentMutationPhase::ParentPublished,
        )?;
        Ok((
            parent_reference,
            PendingExtentMutation {
                reference,
                parent_reference,
                ..pending
            },
        ))
    }
}

impl ExtentMutation {
    pub(crate) fn pending_split(&self) -> Result<Option<PendingExtentMutation>> {
        let key = PageKey::new(
            self.fs_id,
            self.inode,
            PENDING_EXTENT_MUTATION_LOGICAL_INDEX,
        );
        let Some(mut reference) = self.pool.current_metadata_ref_for_completion(key) else {
            return Ok(None);
        };
        let mut record = self
            .pool
            .read_authoritative_metadata::<PendingExtentMutationRecord>(reference)?;
        let operation = record.operation()?;
        let parent_key = PageKey::new(self.fs_id, self.inode, record.parent_logical_index());
        let has_repair_images = !record.repair_child().is_none();
        let parent_reference = if has_repair_images {
            match operation {
                ExtentMutationOperation::RootSplit => record.new_parent(),
                ExtentMutationOperation::NodeSplit => record.old_parent(),
                ExtentMutationOperation::NodeRewrite => record.old_parent(),
            }
        } else {
            match operation {
                ExtentMutationOperation::NodeSplit => match record.phase()? {
                    ExtentMutationPhase::Prepared => {
                        let current_parent = self
                            .pool
                            .current_metadata_ref(parent_key)
                            .ok_or_else(|| Error::new(Errno::ESTALE))?;
                        let parent = self.read_locked_node(current_parent)?;
                        let parent_has_new_child = parent.entries()?.iter().any(|entry| {
                            entry.child_slot() == Some(record.new_child().slot_plus_one())
                        });
                        if !parent_has_new_child {
                            current_parent
                        } else {
                            self.validate_split_parent_topology(
                                &parent,
                                record.old_child(),
                                record.new_child(),
                                record.split_logical_block(),
                            )?;
                            let published = record.with_parent_published(
                                record
                                    .header()
                                    .generation()
                                    .checked_add(1)
                                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
                                current_parent,
                            )?;
                            reference = self.pool.mark_pending_extent_mutation_phase(
                                &self.publication,
                                reference,
                                ExtentMutationPhase::Prepared,
                                ExtentMutationPhase::ParentPublished,
                            )?;
                            record = published;
                            current_parent
                        }
                    }
                    ExtentMutationPhase::ParentPublished => record.new_parent(),
                },
                ExtentMutationOperation::RootSplit => {
                    if parent_key.page_index != FIXED_INODE_LOGICAL_INDEX
                        || record.new_parent().is_none()
                    {
                        return_errno_with_message!(Errno::EUCLEAN, "invalid root-split mark");
                    }
                    let inode_ref = self
                        .pool
                        .fixed_inode_ref(self.fs_id, self.inode)?
                        .ok_or_else(|| Error::new(Errno::ESTALE))?;
                    if inode_ref != record.old_parent() {
                        return_errno_with_message!(Errno::ESTALE, "root-split inode was replaced");
                    }
                    let root = self.read_locked_node(record.new_parent())?;
                    self.validate_owner(&root)?;
                    let routes_right = root.entries()?.iter().any(|entry| {
                        entry.logical_block() == record.split_logical_block()
                            && entry.child_slot() == Some(record.new_child().slot_plus_one())
                    });
                    if !routes_right {
                        return_errno_with_message!(Errno::EUCLEAN, "new root lost split child");
                    }
                    self.pool.repair_fixed_inode_root(
                        &self.publication,
                        inode_ref,
                        record.old_child(),
                        record.new_parent(),
                    )?;
                    record.new_parent()
                }
                ExtentMutationOperation::NodeRewrite => {
                    return_errno_with_message!(Errno::EUCLEAN, "node rewrite lacks repair images")
                }
            }
        };
        Ok(Some(PendingExtentMutation {
            operation,
            reference,
            parent_key,
            old_parent: record.old_parent(),
            old_child: record.old_child(),
            new_child: record.new_child(),
            split_logical_block: record.split_logical_block(),
            parent_reference,
            repair_parent: record.repair_parent(),
            repair_child: record.repair_child(),
        }))
    }

    pub(crate) fn complete_split(
        &self,
        pending: PendingExtentMutation,
        old_child_fault: Option<FaultPoint>,
        parent_fault: Option<FaultPoint>,
        clear_fault: Option<FaultPoint>,
    ) -> Result<(PersistentObjectRef, PersistentObjectRef)> {
        let record = self
            .pool
            .read_authoritative_metadata::<PendingExtentMutationRecord>(pending.reference)?;
        if !pending.repair_child.is_none() {
            return self.complete_repairable_mutation(pending, record, clear_fault);
        }
        if pending.operation == ExtentMutationOperation::RootSplit {
            return self.complete_root_split(pending, record, old_child_fault, clear_fault);
        }
        if record.phase()? != ExtentMutationPhase::ParentPublished
            || record.old_parent() != pending.old_parent
            || record.old_child() != pending.old_child
            || record.new_child() != pending.new_child
            || record.new_parent() != pending.parent_reference
            || record.split_logical_block() != pending.split_logical_block
        {
            return_errno_with_message!(Errno::ESTALE, "pending extent split cannot be completed");
        }

        let old_child_key = PageKey::new(self.fs_id, self.inode, record.old_child_logical_index());
        let current_old_child_ref = self
            .pool
            .current_metadata_ref(old_child_key)
            .ok_or_else(|| Error::new(Errno::ESTALE))?;
        let current_old_child = self.read_locked_node(current_old_child_ref)?;
        self.validate_owner(&current_old_child)?;
        let split = record.split_logical_block();
        let already_repaired = current_old_child.entries()?.iter().all(|entry| {
            if current_old_child.level() == 0 {
                entry.logical_block().saturating_add(entry.length() as u64) <= split
            } else {
                entry.logical_block() < split
            }
        });
        let repaired_child_ref = if already_repaired {
            current_old_child_ref
        } else {
            let mut repaired_entries = Vec::new();
            for entry in current_old_child.entries()? {
                if entry.logical_block() >= split {
                    break;
                }
                if current_old_child.level() == 0
                    && entry.logical_block().saturating_add(entry.length() as u64) > split
                {
                    let length = u32::try_from(split - entry.logical_block())
                        .map_err(|_| Error::new(Errno::EOVERFLOW))?;
                    repaired_entries.push(ExtentRecordEntry::leaf(
                        entry.logical_block(),
                        length,
                        entry.ssd_block()?,
                        entry.is_unwritten(),
                    )?);
                    break;
                }
                repaired_entries.push(*entry);
            }
            if repaired_entries.is_empty() {
                return_errno_with_message!(Errno::EUCLEAN, "extent split empties the old child");
            }
            let repaired = ExtentNodeRecord::new(
                self.fs_id,
                self.inode,
                current_old_child
                    .header()
                    .generation()
                    .checked_add(1)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
                current_old_child.node_id(),
                current_old_child.level(),
                current_old_child.parent(),
                record.new_child(),
                &repaired_entries,
            )?;
            self.pool.update_authoritative_metadata_in_place(
                &self.publication,
                current_old_child_ref,
                &repaired,
                old_child_fault,
            )?
        };

        let current_parent_ref = self
            .pool
            .current_metadata_ref(pending.parent_key)
            .ok_or_else(|| Error::new(Errno::ESTALE))?;
        let current_parent = self.read_locked_node(current_parent_ref)?;
        self.validate_owner(&current_parent)?;
        let has_repaired = current_parent
            .entries()?
            .iter()
            .any(|entry| entry.child_slot() == Some(repaired_child_ref.slot_plus_one()));
        let has_new = current_parent
            .entries()?
            .iter()
            .any(|entry| entry.child_slot() == Some(record.new_child().slot_plus_one()));
        let final_parent_ref = if has_repaired && has_new {
            current_parent_ref
        } else {
            let mut entries = Vec::from(current_parent.entries()?);
            let mut replaced = false;
            for entry in &mut entries {
                if entry.child_slot() == Some(record.old_child().slot_plus_one()) {
                    *entry = ExtentRecordEntry::child(entry.logical_block(), repaired_child_ref)?;
                    replaced = true;
                    break;
                }
            }
            if !replaced {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "published extent parent lost the old child"
                );
            }
            let final_parent = ExtentNodeRecord::new(
                self.fs_id,
                self.inode,
                current_parent
                    .header()
                    .generation()
                    .checked_add(1)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
                current_parent.node_id(),
                current_parent.level(),
                current_parent.parent(),
                self.right_sibling_ref(&current_parent)?,
                &entries,
            )?;
            self.pool.update_authoritative_metadata_in_place(
                &self.publication,
                current_parent_ref,
                &final_parent,
                parent_fault,
            )?
        };

        let mark_key = PageKey::new(
            self.fs_id,
            self.inode,
            PENDING_EXTENT_MUTATION_LOGICAL_INDEX,
        );
        while let Some(mark) = self.pool.current_metadata_ref(mark_key) {
            self.clear_metadata_object(mark, clear_fault)?;
        }
        Ok((final_parent_ref, repaired_child_ref))
    }

    fn complete_repairable_mutation(
        &self,
        pending: PendingExtentMutation,
        record: PendingExtentMutationRecord,
        clear_fault: Option<FaultPoint>,
    ) -> Result<(PersistentObjectRef, PersistentObjectRef)> {
        if record.operation()? != pending.operation
            || record.old_parent() != pending.old_parent
            || record.old_child() != pending.old_child
            || record.new_child() != pending.new_child
            || record.repair_parent() != pending.repair_parent
            || record.repair_child() != pending.repair_child
            || record.split_logical_block() != pending.split_logical_block
        {
            return_errno_with_message!(Errno::ESTALE, "repairable extent mutation changed");
        }

        match pending.operation {
            ExtentMutationOperation::NodeRewrite => {
                self.pool.repair_metadata_from_image::<ExtentNodeRecord>(
                    &self.publication,
                    pending.repair_child,
                    pending.old_child,
                )?;
                if !pending.repair_parent.is_none() {
                    self.pool.repair_metadata_from_image::<ExtentNodeRecord>(
                        &self.publication,
                        pending.repair_parent,
                        pending.old_parent,
                    )?;
                }
            }
            ExtentMutationOperation::NodeSplit => {
                self.pool.repair_metadata_from_image::<ExtentNodeRecord>(
                    &self.publication,
                    pending.repair_parent,
                    pending.old_parent,
                )?;
            }
            ExtentMutationOperation::RootSplit => {
                self.pool.repair_fixed_inode_root(
                    &self.publication,
                    pending.old_parent,
                    pending.old_child,
                    pending.parent_reference,
                )?;
            }
        }

        if record.phase()? == ExtentMutationPhase::Prepared {
            self.pool.mark_pending_extent_mutation_phase(
                &self.publication,
                pending.reference,
                ExtentMutationPhase::Prepared,
                ExtentMutationPhase::ParentPublished,
            )?;
        }

        match pending.operation {
            ExtentMutationOperation::NodeSplit | ExtentMutationOperation::RootSplit => {
                self.pool.repair_metadata_from_image::<ExtentNodeRecord>(
                    &self.publication,
                    pending.repair_child,
                    pending.old_child,
                )?;
                self.verify_split_completion(
                    pending.parent_reference,
                    pending.old_child,
                    pending.new_child,
                    pending.split_logical_block,
                )?;
            }
            ExtentMutationOperation::NodeRewrite => {
                let repaired_child = self.read_locked_node(pending.old_child)?;
                if repaired_child
                    .entries()?
                    .first()
                    .map(ExtentRecordEntry::logical_block)
                    != Some(pending.split_logical_block)
                {
                    return_errno_with_message!(Errno::EUCLEAN, "node rewrite boundary is corrupt");
                }
            }
        }

        self.clear_pending_mark(clear_fault)?;
        self.pool.note_extent_mutation();
        self.clear_temporary_metadata(pending.repair_child)?;
        if !pending.repair_parent.is_none() {
            self.clear_temporary_metadata(pending.repair_parent)?;
        }
        let parent = if pending.parent_reference.is_none() {
            pending.old_child
        } else {
            pending.parent_reference
        };
        Ok((parent, pending.old_child))
    }

    fn complete_root_split(
        &self,
        pending: PendingExtentMutation,
        record: PendingExtentMutationRecord,
        old_root_fault: Option<FaultPoint>,
        clear_fault: Option<FaultPoint>,
    ) -> Result<(PersistentObjectRef, PersistentObjectRef)> {
        if record.operation()? != ExtentMutationOperation::RootSplit
            || record.old_parent() != pending.old_parent
            || record.old_child() != pending.old_child
            || record.new_child() != pending.new_child
            || record.new_parent() != pending.parent_reference
            || record.split_logical_block() != pending.split_logical_block
        {
            return_errno_with_message!(Errno::ESTALE, "pending root split cannot be completed");
        }

        let new_root = self.read_locked_node(record.new_parent())?;
        self.validate_owner(&new_root)?;
        let root_reuses_old = new_root
            .entries()?
            .iter()
            .any(|entry| entry.child_slot() == Some(record.old_child().slot_plus_one()));
        if root_reuses_old {
            let old_root = self.read_locked_node(record.old_child())?;
            self.validate_owner(&old_root)?;
            let split = record.split_logical_block();
            let already_repaired = old_root.right_sibling_slot()
                == Some(record.new_child().slot_plus_one())
                && old_root.entries()?.iter().all(|entry| {
                    if old_root.level() == 0 {
                        entry.logical_block().saturating_add(entry.length() as u64) <= split
                    } else {
                        entry.logical_block() < split
                    }
                });
            if !already_repaired {
                let retained = retained_split_entries(&old_root, split)?;
                let repaired = ExtentNodeRecord::new(
                    self.fs_id,
                    self.inode,
                    old_root
                        .header()
                        .generation()
                        .checked_add(1)
                        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
                    old_root.node_id(),
                    old_root.level(),
                    old_root.parent(),
                    record.new_child(),
                    &retained,
                )?;
                self.pool.update_authoritative_metadata_in_place(
                    &self.publication,
                    record.old_child(),
                    &repaired,
                    old_root_fault,
                )?;
            }
        }

        self.clear_pending_mark(clear_fault)?;
        Ok((record.new_parent(), record.old_child()))
    }

    fn clear_pending_mark(&self, fault: Option<FaultPoint>) -> Result<()> {
        let mark_key = PageKey::new(
            self.fs_id,
            self.inode,
            PENDING_EXTENT_MUTATION_LOGICAL_INDEX,
        );
        while let Some(mark) = self.pool.current_metadata_ref(mark_key) {
            self.clear_metadata_object(mark, fault)?;
        }
        Ok(())
    }
}

enum TreeExtentUpdate {
    Replaced(InitializedExtentNode),
    Split(LeafSplit),
    Cascaded(MaterializedNodeSplit),
}

struct LeafSplit {
    old_child: PersistentObjectRef,
    split_logical_block: u64,
    right_entries: Vec<ExtentRecordEntry>,
}

struct MaterializedNodeSplit {
    left: InitializedExtentNode,
    right: InitializedExtentNode,
    split_logical_block: u64,
}

struct SplitRightChild {
    boundary: u64,
    reference: PersistentObjectRef,
    parent_index: usize,
}

struct RoutedLeaf {
    reference: PersistentObjectRef,
    record: BoxedMetadataRecord<ExtentNodeRecord>,
    parent: Option<(PersistentObjectRef, usize)>,
    upper_bound: Option<u64>,
}

impl ExtentMutation {
    /// Inserts one mapping into a persistent leaf and coalesces compatible SSD runs.
    pub(crate) fn insert_leaf_extent(
        &self,
        current_leaf: PersistentObjectRef,
        extent: ExtentRecordEntry,
        fault: Option<FaultPoint>,
    ) -> Result<InitializedExtentNode> {
        extent.data_ref()?;
        self.validate_data_extent(&extent)?;
        let leaf = self.read_leaf(current_leaf)?;
        let mut entries = Vec::from(leaf.entries()?);
        let position =
            entries.partition_point(|entry| entry.logical_block() < extent.logical_block());
        let extent_end = entry_end(&extent)?;
        if position > 0 && entry_end(&entries[position - 1])? > extent.logical_block() {
            return_errno_with_message!(
                Errno::EINVAL,
                "persistent extent insertion overlaps left neighbor"
            );
        }
        if position < entries.len() && extent_end > entries[position].logical_block() {
            return_errno_with_message!(
                Errno::EINVAL,
                "persistent extent insertion overlaps right neighbor"
            );
        }
        entries.insert(position, extent);
        merge_leaf_entries(&mut entries)?;
        self.replace_leaf(current_leaf, &leaf, &entries, fault)
    }

    /// Replaces logical blocks in one leaf while preserving unaffected mappings.
    pub(crate) fn replace_leaf_extents(
        &self,
        current_leaf: PersistentObjectRef,
        replacements: &[ExtentRecordEntry],
        fault: Option<FaultPoint>,
    ) -> Result<InitializedExtentNode> {
        if replacements.is_empty() {
            return_errno_with_message!(Errno::EINVAL, "extent replacement set is empty");
        }
        let leaf = self.read_leaf(current_leaf)?;
        let mut entries = Vec::from(leaf.entries()?);
        for replacement in replacements {
            self.validate_data_extent(replacement)?;
            replace_leaf_entry(&mut entries, *replacement)?;
        }
        merge_leaf_entries(&mut entries)?;
        self.replace_leaf(current_leaf, &leaf, &entries, fault)
    }

    /// Replaces mappings through top-down local in-place mutations.
    ///
    /// Full nodes are split before descent. Stable left nodes retain their
    /// identity, while a per-inode mutation mark makes every structural splice
    /// recoverable and idempotent.
    pub(crate) fn replace_tree_extents(
        &self,
        current_root: PersistentObjectRef,
        replacements: &[ExtentRecordEntry],
        fault: Option<FaultPoint>,
    ) -> Result<InitializedExtentNode> {
        if replacements.is_empty() {
            return_errno_with_message!(Errno::EINVAL, "extent replacement set is empty");
        }
        for replacement in replacements {
            self.validate_data_extent(replacement)?;
        }
        let mut root = current_root;
        let mut cursor = 0usize;
        while cursor < replacements.len() {
            let routed = self.route_leaf(root, replacements[cursor].logical_block())?;
            if self.try_replace_leaf_extent_in_place(&routed, replacements[cursor], fault)? {
                cursor += 1;
                continue;
            }
            let mut entries = Vec::from(routed.record.entries()?);
            let mut accepted = 0usize;
            let mut previous_logical = None;
            for replacement in &replacements[cursor..] {
                if previous_logical.is_some_and(|previous| replacement.logical_block() < previous)
                    || routed
                        .upper_bound
                        .is_some_and(|upper| replacement.logical_block() >= upper)
                {
                    break;
                }
                let mut candidate = entries.clone();
                replace_leaf_entry(&mut candidate, *replacement)?;
                merge_leaf_entries(&mut candidate)?;
                if candidate.len() > EXTENT_NODE_ENTRY_CAPACITY {
                    break;
                }
                entries = candidate;
                previous_logical = Some(replacement.logical_block());
                accepted += 1;
            }
            if accepted == 0 {
                root = self.prepare_root_for_local_update(root, fault)?;
                self.replace_one_tree_extent_top_down(root, replacements[cursor], fault)?;
                cursor += 1;
                continue;
            }
            self.rewrite_leaf_and_route(
                routed.reference,
                &routed.record,
                &entries,
                routed.parent,
                fault,
            )?;
            cursor += accepted;
        }
        self.committed_node(root)
    }

    fn try_replace_leaf_extent_in_place(
        &self,
        routed: &RoutedLeaf,
        replacement: ExtentRecordEntry,
        fault: Option<FaultPoint>,
    ) -> Result<bool> {
        if matches!(replacement.data_ref()?, ExtentDataRef::Ssd(_)) {
            return self.try_append_or_extend_ssd_extent(routed, replacement, fault);
        }
        let logical = replacement.logical_block();
        let replacement_end = entry_end(&replacement)?;
        let Some((old_index, old)) =
            routed
                .record
                .entries()?
                .iter()
                .copied()
                .enumerate()
                .find(|(_, entry)| {
                    matches!(entry.data_ref(), Ok(ExtentDataRef::Ssd(_)))
                        && entry.logical_block() <= logical
                        && logical < entry.logical_block().saturating_add(entry.length() as u64)
                })
        else {
            let entries = routed.record.entries()?;
            if routed.record.entry_count() >= EXTENT_NODE_ENTRY_CAPACITY
                || entries
                    .last()
                    .is_some_and(|last| entry_end(last).map_or(true, |end| logical < end))
            {
                return Ok(false);
            }
            let guards = self.node_guards.borrow();
            let guard = guards
                .iter()
                .find(|(reference, _)| *reference == routed.reference)
                .map(|(_, guard)| guard)
                .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
            let target_index = routed.record.entry_count();
            self.pool
                .persist_prevalidated_extent_leaf_update_with_guard(
                    guard,
                    routed.reference,
                    routed.record.entry_count(),
                    routed.record.level_count_flags_word(
                        u16::try_from(routed.record.entry_count())
                            .map_err(|_| Error::new(Errno::EOVERFLOW))?,
                    ),
                    [Some(replacement), None],
                    None,
                    None,
                    fault,
                )?;
            drop(guards);
            self.install_extent_reverse(routed.reference, target_index, replacement)?;
            self.pool.note_extent_mutation();
            return Ok(true);
        };
        let ExtentDataRef::Ssd(ssd_start) = old.data_ref()? else {
            return Ok(false);
        };
        let old_end = entry_end(&old)?;
        if replacement_end > old_end {
            return Ok(false);
        }
        let required_slots =
            usize::from(logical > old.logical_block()) + usize::from(replacement_end < old_end);
        if routed.record.entry_count() + required_slots > EXTENT_NODE_ENTRY_CAPACITY {
            return Ok(false);
        }

        let guards = self.node_guards.borrow();
        let guard = guards
            .iter()
            .find(|(reference, _)| *reference == routed.reference)
            .map(|(_, guard)| guard)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        let page_ssd = ssd_start
            .checked_add(logical - old.logical_block())
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let mut appends = [None, None];
        let (target_index, range_update) = if logical == old.logical_block() {
            if replacement_end < old_end {
                appends[0] = Some(ExtentRecordEntry::leaf(
                    replacement_end,
                    u32::try_from(old_end - replacement_end)
                        .map_err(|_| Error::new(Errno::EOVERFLOW))?,
                    page_ssd + u64::from(replacement.length()),
                    old.is_unwritten(),
                )?);
            }
            (
                old_index,
                (replacement_end < old_end).then_some((old_index, u64::from(replacement.length()))),
            )
        } else {
            let page_index = routed.record.entry_count();
            appends[0] = Some(ExtentRecordEntry::leaf(
                logical,
                replacement.length(),
                page_ssd,
                old.is_unwritten(),
            )?);
            if replacement_end < old_end {
                appends[1] = Some(ExtentRecordEntry::leaf(
                    replacement_end,
                    u32::try_from(old_end - replacement_end)
                        .map_err(|_| Error::new(Errno::EOVERFLOW))?,
                    page_ssd + u64::from(replacement.length()),
                    old.is_unwritten(),
                )?);
            }
            (page_index, Some((old_index, logical - old.logical_block())))
        };
        self.pool
            .persist_prevalidated_extent_leaf_update_with_guard(
                guard,
                routed.reference,
                routed.record.entry_count(),
                routed.record.level_count_flags_word(
                    u16::try_from(routed.record.entry_count())
                        .map_err(|_| Error::new(Errno::EOVERFLOW))?,
                ),
                appends,
                range_update,
                Some((target_index, replacement.target())),
                fault,
            )?;
        drop(guards);
        self.install_extent_reverse(routed.reference, target_index, replacement)?;
        self.pool.note_extent_mutation();
        Ok(true)
    }

    fn try_append_or_extend_ssd_extent(
        &self,
        routed: &RoutedLeaf,
        replacement: ExtentRecordEntry,
        fault: Option<FaultPoint>,
    ) -> Result<bool> {
        let ExtentDataRef::Ssd(physical_start) = replacement.data_ref()? else {
            return Ok(false);
        };
        let logical_start = replacement.logical_block();
        let replacement_end = entry_end(&replacement)?;
        let entries = routed.record.entries()?;
        if entries.iter().any(|entry| {
            entry_end(entry).map_or(true, |end| {
                entry.logical_block() < replacement_end && logical_start < end
            })
        }) || routed
            .upper_bound
            .is_some_and(|upper| replacement_end > upper)
        {
            return Ok(false);
        }

        let tail = entries
            .iter()
            .copied()
            .enumerate()
            .max_by_key(|(_, entry)| entry.logical_block());
        let guards = self.node_guards.borrow();
        let guard = guards
            .iter()
            .find(|(reference, _)| *reference == routed.reference)
            .map(|(_, guard)| guard)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        let level_count_flags = routed.record.level_count_flags_word(
            u16::try_from(routed.record.entry_count()).map_err(|_| Error::new(Errno::EOVERFLOW))?,
        );

        if let Some((tail_index, tail_entry)) = tail {
            let tail_end = entry_end(&tail_entry)?;
            let mergeable = match tail_entry.data_ref()? {
                ExtentDataRef::Ssd(tail_physical) => {
                    tail_end == logical_start
                        && tail_physical.checked_add(u64::from(tail_entry.length()))
                            == Some(physical_start)
                        && tail_entry.is_unwritten() == replacement.is_unwritten()
                }
                ExtentDataRef::Nv(_) => false,
            };
            if mergeable {
                let length = tail_entry
                    .length()
                    .checked_add(replacement.length())
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                self.pool
                    .persist_prevalidated_extent_leaf_update_with_guard(
                        guard,
                        routed.reference,
                        routed.record.entry_count(),
                        level_count_flags,
                        [None, None],
                        Some((tail_index, u64::from(length))),
                        None,
                        fault,
                    )?;
                drop(guards);
                self.pool.note_extent_mutation();
                return Ok(true);
            }
            if logical_start < tail_end {
                return Ok(false);
            }
        }
        if routed.record.entry_count() == EXTENT_NODE_ENTRY_CAPACITY {
            return Ok(false);
        }
        self.pool
            .persist_prevalidated_extent_leaf_update_with_guard(
                guard,
                routed.reference,
                routed.record.entry_count(),
                level_count_flags,
                [Some(replacement), None],
                None,
                None,
                fault,
            )?;
        drop(guards);
        self.pool.note_extent_mutation();
        Ok(true)
    }

    fn route_leaf(&self, root: PersistentObjectRef, logical_block: u64) -> Result<RoutedLeaf> {
        #[cfg(ktest)]
        self.pool.note_extent_tree_route();
        let mut current = root;
        let mut parent = None;
        let mut upper_bound: Option<u64> = None;
        loop {
            let mut node = self.read_locked_node(current)?;
            self.validate_owner(&node)?;
            while let Some(sibling_slot) = node.right_sibling_slot() {
                let sibling_ref = self.pool.persistent_metadata_ref_from_slot(sibling_slot)?;
                let sibling = self.read_locked_node(sibling_ref)?;
                self.validate_owner(&sibling)?;
                let boundary = sibling
                    .entries()?
                    .first()
                    .map(ExtentRecordEntry::logical_block)
                    .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
                if logical_block < boundary {
                    break;
                }
                current = sibling_ref;
                node = sibling;
            }
            if node.level() == 0 {
                return Ok(RoutedLeaf {
                    reference: current,
                    record: node,
                    parent,
                    upper_bound,
                });
            }
            let entries = node.entries()?;
            let child_index = entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.logical_block() <= logical_block)
                .max_by_key(|(_, entry)| entry.logical_block())
                .map(|(index, _)| index)
                .unwrap_or(0);
            if let Some(next) = entries.get(child_index + 1) {
                upper_bound = Some(upper_bound.map_or(next.logical_block(), |bound| {
                    bound.min(next.logical_block())
                }));
            }
            let child = entries[child_index]
                .child_slot()
                .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
            parent = Some((current, child_index));
            current = self.pool.persistent_metadata_ref_from_slot(child)?;
        }
    }

    /// Keeps two free slots on every node along the insertion path. Replacing
    /// one block in the middle of an SSD run can create a left fragment, the
    /// NV mapping, and a right fragment, so the final leaf rewrite may grow by
    /// two entries. Splitting before descent prevents a bottom-up cascade from
    /// requiring one mutation mark to describe several unstable levels.
    fn prepare_root_for_local_update(
        &self,
        current_root: PersistentObjectRef,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef> {
        let mut root = self.read_locked_node(current_root)?;
        self.validate_owner(&root)?;
        if root.entries()?.len() <= EXTENT_NODE_ENTRY_CAPACITY - 2 {
            return Ok(current_root);
        }
        if self.normalize_node_for_split(current_root, &root, None, fault)? {
            root = self.read_locked_node(current_root)?;
        }
        self.split_root_in_place(current_root, &root, fault)
    }

    /// FAST inserts publish into the next dense slot, so slot order is not a
    /// logical ordering. FAIR needs a logical midpoint and a dense left prefix;
    /// normalize once, through the recoverable rewrite protocol, before split.
    fn normalize_node_for_split(
        &self,
        node_ref: PersistentObjectRef,
        node: &ExtentNodeRecord,
        parent: Option<(PersistentObjectRef, usize)>,
        fault: Option<FaultPoint>,
    ) -> Result<bool> {
        let entries = node.entries()?;
        if entries
            .windows(2)
            .all(|pair| pair[0].logical_block() <= pair[1].logical_block())
        {
            return Ok(false);
        }
        let mut normalized = Vec::from(entries);
        normalized.sort_unstable_by_key(ExtentRecordEntry::logical_block);
        self.rewrite_leaf_and_route(node_ref, node, &normalized, parent, fault)?;
        Ok(true)
    }

    fn replace_one_tree_extent_top_down(
        &self,
        root: PersistentObjectRef,
        replacement: ExtentRecordEntry,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        let mut current = root;
        let mut parent: Option<(PersistentObjectRef, usize)> = None;
        loop {
            let node = self.read_locked_node(current)?;
            self.validate_owner(&node)?;
            if node.level() == 0 {
                let mut entries = Vec::from(node.entries()?);
                replace_leaf_entry(&mut entries, replacement)?;
                merge_leaf_entries(&mut entries)?;
                if entries.len() > EXTENT_NODE_ENTRY_CAPACITY {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "top-down extent insertion reached an overfull leaf"
                    );
                }
                return self.rewrite_leaf_and_route(current, &node, &entries, parent, fault);
            }

            let entries = node.entries()?;
            let mut child_index = entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.logical_block() <= replacement.logical_block())
                .max_by_key(|(_, entry)| entry.logical_block())
                .map(|(index, _)| index)
                .unwrap_or(0);
            let child_ref = entries[child_index]
                .child_slot()
                .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
            let mut child_ref = self.pool.persistent_metadata_ref_from_slot(child_ref)?;
            let mut child = self.read_locked_node(child_ref)?;
            self.validate_owner(&child)?;

            if child.entries()?.len() > EXTENT_NODE_ENTRY_CAPACITY - 2 {
                if self.normalize_node_for_split(
                    child_ref,
                    &child,
                    Some((current, child_index)),
                    fault,
                )? {
                    child = self.read_locked_node(child_ref)?;
                }
                let right = self.split_child_in_place(
                    current,
                    &node,
                    child_index,
                    child_ref,
                    &child,
                    fault,
                )?;
                if replacement.logical_block() >= right.boundary {
                    child_index = right.parent_index;
                    child_ref = right.reference;
                }
            }

            parent = Some((current, child_index));
            current = child_ref;
        }
    }

    fn split_root_in_place(
        &self,
        old_root_ref: PersistentObjectRef,
        old_root: &ExtentNodeRecord,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef> {
        let entries = old_root.entries()?;
        if entries.len() < 2 || !old_root.parent().is_none() {
            return_errno_with_message!(Errno::EUCLEAN, "invalid root pre-split");
        }
        let split_index = entries.len() / 2;
        let split = entries[split_index].logical_block();
        let generation = old_root
            .header()
            .generation()
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let _migration_freeze = if old_root.level() == 0 {
            let old_end = entry_end(entries.last().ok_or_else(|| Error::new(Errno::EUCLEAN))?)?;
            Some(
                self.pool
                    .freeze_extent_range(self.fs_id, self.inode, split..old_end)?,
            )
        } else {
            None
        };

        let right_key = self.transient_metadata_key(old_root_ref)?;
        let right_record = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            generation,
            right_key.page_index,
            old_root.level(),
            PersistentObjectRef::NONE,
            self.right_sibling_ref(old_root)?,
            &entries[split_index..],
        )?;
        let right = self.persist_initialized_child(right_key, &right_record, fault)?;
        let left_boundary = entries[0].logical_block();
        let new_root_key = self.transient_metadata_key(right.reference)?;
        let new_root_record = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            generation,
            new_root_key.page_index,
            old_root
                .level()
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            PersistentObjectRef::NONE,
            PersistentObjectRef::NONE,
            &[
                ExtentRecordEntry::child(left_boundary, old_root_ref)?,
                ExtentRecordEntry::child(split, right.reference)?,
            ],
        )?;
        let new_root = self.persist_initialized_child(new_root_key, &new_root_record, fault)?;
        let guards = self.node_guards.borrow();
        let old_guard = guards
            .iter()
            .find(|(reference, _)| *reference == old_root_ref)
            .map(|(_, guard)| guard)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        self.pool.persist_extent_sibling_with_guard(
            old_guard,
            old_root_ref,
            old_root.right_sibling_slot(),
            right.reference,
            fault,
        )?;
        self.pool.shrink_extent_entry_count_with_guard(
            old_guard,
            old_root_ref,
            entries.len(),
            split_index,
            fault,
        )?;
        drop(guards);
        self.publish_repairable_root_split(old_root_ref, &new_root, &right, split, fault)?;
        self.refresh_leaf_reverse(old_root_ref)?;
        self.refresh_leaf_reverse(right.reference)?;
        Ok(new_root.reference)
    }

    fn split_child_in_place(
        &self,
        parent_ref: PersistentObjectRef,
        parent: &ExtentNodeRecord,
        _child_index: usize,
        child_ref: PersistentObjectRef,
        child: &ExtentNodeRecord,
        fault: Option<FaultPoint>,
    ) -> Result<SplitRightChild> {
        let child_entries = child.entries()?;
        if child_entries.len() < 2 || parent.entries()?.len() >= EXTENT_NODE_ENTRY_CAPACITY {
            return_errno_with_message!(Errno::EUCLEAN, "invalid top-down child split");
        }
        let split_index = child_entries.len() / 2;
        let split = child_entries[split_index].logical_block();
        let generation = child
            .header()
            .generation()
            .max(parent.header().generation())
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let _migration_freeze = if child.level() == 0 {
            let old_end = entry_end(
                child_entries
                    .last()
                    .ok_or_else(|| Error::new(Errno::EUCLEAN))?,
            )?;
            Some(
                self.pool
                    .freeze_extent_range(self.fs_id, self.inode, split..old_end)?,
            )
        } else {
            None
        };
        let right_key = self.transient_metadata_key(child_ref)?;
        let right_record = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            generation,
            right_key.page_index,
            child.level(),
            parent_ref,
            self.right_sibling_ref(child)?,
            &child_entries[split_index..],
        )?;
        let right = self.persist_initialized_child(right_key, &right_record, fault)?;

        let guards = self.node_guards.borrow();
        let child_guard = guards
            .iter()
            .find(|(reference, _)| *reference == child_ref)
            .map(|(_, guard)| guard)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        self.pool.persist_extent_sibling_with_guard(
            child_guard,
            child_ref,
            child.right_sibling_slot(),
            right.reference,
            fault,
        )?;
        self.pool.shrink_extent_entry_count_with_guard(
            child_guard,
            child_ref,
            child_entries.len(),
            split_index,
            fault,
        )?;
        let parent_guard = guards
            .iter()
            .find(|(reference, _)| *reference == parent_ref)
            .map(|(_, guard)| guard)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        let parent_index = self.pool.append_extent_entry_with_guard(
            parent_guard,
            parent_ref,
            ExtentRecordEntry::child(split, right.reference)?,
            fault,
        )?;
        drop(guards);
        self.pool.note_extent_mutation();
        self.refresh_leaf_reverse(child_ref)?;
        self.refresh_leaf_reverse(right.reference)?;
        Ok(SplitRightChild {
            boundary: split,
            reference: right.reference,
            parent_index,
        })
    }

    fn rewrite_leaf_and_route(
        &self,
        leaf_ref: PersistentObjectRef,
        leaf: &ExtentNodeRecord,
        entries: &[ExtentRecordEntry],
        parent: Option<(PersistentObjectRef, usize)>,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        let generation = leaf
            .header()
            .generation()
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let repaired_leaf = ExtentNodeRecord::new_boxed(
            self.fs_id,
            self.inode,
            generation,
            leaf.node_id(),
            leaf.level(),
            leaf.parent(),
            self.right_sibling_ref(leaf)?,
            entries,
        )?;
        let new_boundary = entries
            .first()
            .map(ExtentRecordEntry::logical_block)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        let repaired_parent = if let Some((parent_ref, child_index)) = parent {
            let parent_record = self.read_locked_node(parent_ref)?;
            self.validate_owner(&parent_record)?;
            let mut parent_entries = Vec::from(parent_record.entries()?);
            let routed = parent_entries
                .get_mut(child_index)
                .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
            if routed.child_slot() != Some(leaf_ref.slot_plus_one()) {
                return_errno_with_message!(Errno::EUCLEAN, "extent parent route changed");
            }
            if routed.logical_block() == new_boundary {
                None
            } else {
                *routed = ExtentRecordEntry::child(new_boundary, leaf_ref)?;
                Some((
                    parent_ref,
                    ExtentNodeRecord::new_boxed(
                        self.fs_id,
                        self.inode,
                        parent_record
                            .header()
                            .generation()
                            .max(generation)
                            .checked_add(1)
                            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
                        parent_record.node_id(),
                        parent_record.level(),
                        parent_record.parent(),
                        self.right_sibling_ref(&parent_record)?,
                        &parent_entries,
                    )?,
                ))
            }
        } else {
            None
        };
        self.publish_repairable_node_rewrite(
            leaf_ref,
            &repaired_leaf,
            repaired_parent
                .as_ref()
                .map(|(reference, record)| (*reference, &**record)),
            new_boundary,
            fault,
        )?;
        let started = read_ns();
        let result = self.refresh_leaf_reverse(leaf_ref);
        LATENCY_STATS.fg_reverse_refresh.record_since(started);
        result
    }

    fn persist_repair_image(
        &self,
        target: PersistentObjectRef,
        record: &ExtentNodeRecord,
        fault: Option<FaultPoint>,
    ) -> Result<InitializedExtentNode> {
        let key = self.transient_metadata_key(target)?;
        self.persist_initialized_child(key, record, fault)
    }

    fn publish_repairable_node_rewrite(
        &self,
        child_ref: PersistentObjectRef,
        repaired_child: &ExtentNodeRecord,
        repaired_parent: Option<(PersistentObjectRef, &ExtentNodeRecord)>,
        boundary: u64,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        let started = read_ns();
        let child_image = self.persist_reusable_repair_image(repaired_child, fault)?;
        let parent_image = match repaired_parent {
            Some((parent_ref, record)) => Some((
                parent_ref,
                self.persist_reusable_repair_image(record, fault)?,
            )),
            None => None,
        };
        LATENCY_STATS.fg_repair_image.record_since(started);
        let started = read_ns();
        let child_key = self.pool.persistent_metadata_key(child_ref)?;
        let mark = PendingExtentMutationRecord::prepared_node_rewrite(
            self.fs_id,
            self.inode,
            1,
            parent_image
                .as_ref()
                .map(|(reference, _)| self.pool.persistent_metadata_key(*reference))
                .transpose()?
                .map_or(child_key.page_index, |key| key.page_index),
            child_key.page_index,
            parent_image
                .as_ref()
                .map_or(PersistentObjectRef::NONE, |(reference, _)| *reference),
            child_ref,
            parent_image
                .as_ref()
                .map_or(PersistentObjectRef::NONE, |(_, image)| image.reference),
            child_image.reference,
            boundary,
        )?;
        let mark_ref = self.pool.persist_repair_record(
            PageKey::new(
                self.fs_id,
                self.inode,
                PENDING_EXTENT_MUTATION_LOGICAL_INDEX,
            ),
            &mark,
            true,
            fault,
        )?;
        inject(fault, FaultPoint::AfterExtentPreparedPersist)?;

        LATENCY_STATS.fg_repair_mark.record_since(started);
        let started = read_ns();
        self.pool.repair_metadata_from_image::<ExtentNodeRecord>(
            &self.publication,
            child_image.reference,
            child_ref,
        )?;
        inject(fault, FaultPoint::AfterExtentChildPersist)?;
        if let Some((parent_ref, parent_image)) = parent_image.as_ref() {
            self.pool.repair_metadata_from_image::<ExtentNodeRecord>(
                &self.publication,
                parent_image.reference,
                *parent_ref,
            )?;
            inject(fault, FaultPoint::AfterExtentParentPersist)?;
        }
        self.pool.mark_pending_extent_mutation_phase(
            &self.publication,
            mark_ref,
            ExtentMutationPhase::Prepared,
            ExtentMutationPhase::ParentPublished,
        )?;
        inject(fault, FaultPoint::AfterExtentPhasePersist)?;
        inject(fault, FaultPoint::AfterVisibilityPersist)?;
        LATENCY_STATS.fg_repair_apply.record_since(started);
        let started = read_ns();
        self.verify_repair_image(child_ref, repaired_child)?;
        if let Some((parent_ref, record)) = repaired_parent.as_ref() {
            self.verify_repair_image(*parent_ref, *record)?;
        }
        LATENCY_STATS.fg_repair_verify.record_since(started);
        let started = read_ns();
        // The mark must be durably invalid before either referenced image is reused.
        self.pool.reclaim_repair_record(mark_ref, true, fault)?;
        self.pool.note_extent_mutation();
        self.pool
            .reclaim_repair_record(child_image.reference, false, None)?;
        if let Some((_, parent_image)) = parent_image {
            self.pool
                .reclaim_repair_record(parent_image.reference, false, None)?;
        }
        LATENCY_STATS.fg_repair_cleanup.record_since(started);
        Ok(())
    }

    fn persist_reusable_repair_image(
        &self,
        record: &ExtentNodeRecord,
        fault: Option<FaultPoint>,
    ) -> Result<InitializedExtentNode> {
        self.validate_owner(record)?;
        let reference = self.pool.persist_repair_record(
            PageKey::new(self.fs_id, self.inode, EXTENT_TRANSIENT_LOGICAL_INDEX_TAG),
            record,
            false,
            fault,
        )?;
        Ok(InitializedExtentNode {
            reference,
            fs_id: self.fs_id,
            inode: self.inode,
            node_id: record.node_id(),
            level: record.level(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_repairable_node_split(
        &self,
        parent_ref: PersistentObjectRef,
        repaired_parent: &ExtentNodeRecord,
        child_ref: PersistentObjectRef,
        repaired_child: &ExtentNodeRecord,
        right: &InitializedExtentNode,
        split: u64,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        let parent_image = self.persist_repair_image(parent_ref, repaired_parent, fault)?;
        let child_image = self.persist_repair_image(child_ref, repaired_child, fault)?;
        let parent_key = self.pool.persistent_metadata_key(parent_ref)?;
        let child_key = self.pool.persistent_metadata_key(child_ref)?;
        let mark = PendingExtentMutationRecord::prepared_split(
            self.fs_id,
            self.inode,
            1,
            parent_key.page_index,
            child_key.page_index,
            parent_ref,
            child_ref,
            right.reference,
            split,
        )?
        .with_repair_refs(parent_image.reference, child_image.reference)?;
        let mark_ref = self.pool.persist_authoritative_metadata(
            PageKey::new(
                self.fs_id,
                self.inode,
                PENDING_EXTENT_MUTATION_LOGICAL_INDEX,
            ),
            &mark,
            fault,
        )?;
        inject(fault, FaultPoint::AfterExtentPreparedPersist)?;

        self.pool.repair_metadata_from_image::<ExtentNodeRecord>(
            &self.publication,
            parent_image.reference,
            parent_ref,
        )?;
        inject(fault, FaultPoint::AfterExtentParentPersist)?;
        inject(fault, FaultPoint::AfterVisibilityPersist)?;
        self.pool.mark_pending_extent_mutation_phase(
            &self.publication,
            mark_ref,
            ExtentMutationPhase::Prepared,
            ExtentMutationPhase::ParentPublished,
        )?;
        inject(fault, FaultPoint::AfterExtentPhasePersist)?;
        self.pool.repair_metadata_from_image::<ExtentNodeRecord>(
            &self.publication,
            child_image.reference,
            child_ref,
        )?;
        inject(fault, FaultPoint::AfterExtentChildPersist)?;
        inject(fault, FaultPoint::AfterContentPersist)?;
        self.verify_split_completion(parent_ref, child_ref, right.reference, split)?;
        self.clear_pending_mark(fault)?;
        self.pool.note_extent_mutation();
        self.clear_temporary_metadata(parent_image.reference)?;
        self.clear_temporary_metadata(child_image.reference)?;
        Ok(())
    }

    fn publish_repairable_root_split(
        &self,
        old_root_ref: PersistentObjectRef,
        new_root: &InitializedExtentNode,
        right: &InitializedExtentNode,
        split: u64,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        let inode_ref = self
            .pool
            .fixed_inode_ref(self.fs_id, self.inode)?
            .ok_or_else(|| Error::new(Errno::ENOENT))?;
        let old_root_key = self.pool.persistent_metadata_key(old_root_ref)?;
        let mark = PendingExtentMutationRecord::prepared_root_split(
            self.fs_id,
            self.inode,
            1,
            FIXED_INODE_LOGICAL_INDEX,
            old_root_key.page_index,
            inode_ref,
            new_root.reference,
            old_root_ref,
            right.reference,
            split,
        )?;
        let mark_ref = self.pool.persist_authoritative_metadata(
            PageKey::new(
                self.fs_id,
                self.inode,
                PENDING_EXTENT_MUTATION_LOGICAL_INDEX,
            ),
            &mark,
            fault,
        )?;
        inject(fault, FaultPoint::AfterExtentPreparedPersist)?;
        self.publish_fixed_inode_root(inode_ref, old_root_ref, new_root.reference, fault)?;
        inject(fault, FaultPoint::AfterExtentParentPersist)?;
        self.pool.mark_pending_extent_mutation_phase(
            &self.publication,
            mark_ref,
            ExtentMutationPhase::Prepared,
            ExtentMutationPhase::ParentPublished,
        )?;
        inject(fault, FaultPoint::AfterExtentPhasePersist)?;
        self.verify_split_completion(new_root.reference, old_root_ref, right.reference, split)?;
        self.clear_pending_mark(fault)?;
        self.pool.note_extent_mutation();
        Ok(())
    }

    fn verify_repair_image(
        &self,
        target: PersistentObjectRef,
        expected: &ExtentNodeRecord,
    ) -> Result<()> {
        let actual = self.read_locked_node(target)?;
        if actual.as_bytes() != expected.as_bytes() {
            return_errno_with_message!(Errno::EUCLEAN, "extent repair did not persist");
        }
        Ok(())
    }

    fn verify_split_completion(
        &self,
        parent_ref: PersistentObjectRef,
        old_child_ref: PersistentObjectRef,
        new_child_ref: PersistentObjectRef,
        split: u64,
    ) -> Result<()> {
        let parent = self.read_locked_node(parent_ref)?;
        self.validate_split_parent_topology(&parent, old_child_ref, new_child_ref, split)
    }

    fn clear_temporary_metadata(&self, reference: PersistentObjectRef) -> Result<()> {
        self.clear_metadata_object(reference, None)
    }

    fn clear_metadata_object(
        &self,
        reference: PersistentObjectRef,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        // Recovery can release a temporary repair image that was inspected above.
        self.node_guards
            .borrow_mut()
            .retain(|(current, _)| *current != reference);
        let slot = reference
            .slot()
            .and_then(|slot| usize::try_from(slot).ok())
            .and_then(|slot| NvFrameId::try_from(slot).ok())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        self.pool.clear_frame(slot, fault)?;
        self.pool.free_frame(slot)
    }

    fn grow_root_from_leaf_split(
        &self,
        split: LeafSplit,
        fault: Option<FaultPoint>,
    ) -> Result<InitializedExtentNode> {
        let old_root_ref = split.old_child;
        let old_root = self.read_locked_node(old_root_ref)?;
        self.validate_owner(&old_root)?;
        if old_root.level() != 0 || !old_root.parent().is_none() {
            return_errno_with_message!(Errno::EINVAL, "root growth requires a root leaf");
        }
        let generation = old_root
            .header()
            .generation()
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let right_key = self.transient_metadata_key(old_root_ref)?;
        let right = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            generation,
            right_key.page_index,
            0,
            PersistentObjectRef::NONE,
            self.right_sibling_ref(&old_root)?,
            &split.right_entries,
        )?;
        let right = self.persist_initialized_child(right_key, &right, fault)?;
        let left_boundary = old_root
            .entries()?
            .first()
            .map(ExtentRecordEntry::logical_block)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        let new_root_key = self.transient_metadata_key(right.reference)?;
        let new_root = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            generation,
            new_root_key.page_index,
            1,
            PersistentObjectRef::NONE,
            PersistentObjectRef::NONE,
            &[
                ExtentRecordEntry::child(left_boundary, old_root_ref)?,
                ExtentRecordEntry::child(split.split_logical_block, right.reference)?,
            ],
        )?;
        let new_root = self.persist_initialized_child(new_root_key, &new_root, fault)?;
        let inode_ref = self
            .pool
            .fixed_inode_ref(self.fs_id, self.inode)?
            .ok_or_else(|| Error::new(Errno::ENOENT))?;
        let old_root_key = self.pool.persistent_metadata_key(old_root_ref)?;
        let mark = PendingExtentMutationRecord::prepared_root_split(
            self.fs_id,
            self.inode,
            1,
            FIXED_INODE_LOGICAL_INDEX,
            old_root_key.page_index,
            inode_ref,
            new_root.reference,
            old_root_ref,
            right.reference,
            split.split_logical_block,
        )?;
        let mark_ref = self.pool.persist_authoritative_metadata(
            PageKey::new(
                self.fs_id,
                self.inode,
                PENDING_EXTENT_MUTATION_LOGICAL_INDEX,
            ),
            &mark,
            fault,
        )?;
        self.publish_fixed_inode_root(inode_ref, old_root_ref, new_root.reference, fault)?;
        let retained = retained_split_entries(&old_root, split.split_logical_block)?;
        let repaired_old_root = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            generation,
            old_root.node_id(),
            0,
            old_root.parent(),
            right.reference,
            &retained,
        )?;
        self.pool.update_authoritative_metadata_in_place(
            &self.publication,
            old_root_ref,
            &repaired_old_root,
            fault,
        )?;
        self.clear_metadata_object(mark_ref, fault)?;
        Ok(new_root)
    }

    fn grow_root_from_materialized_split(
        &self,
        old_root_ref: PersistentObjectRef,
        split: MaterializedNodeSplit,
        fault: Option<FaultPoint>,
    ) -> Result<InitializedExtentNode> {
        let old_root = self.read_locked_node(old_root_ref)?;
        self.validate_owner(&old_root)?;
        if !old_root.parent().is_none() {
            return_errno_with_message!(Errno::EINVAL, "root split references a non-root node");
        }
        let left = self.read_locked_node(split.left.reference)?;
        let right = self.read_locked_node(split.right.reference)?;
        let root_level = left
            .level()
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if right.level() != left.level() || root_level != old_root.level().saturating_add(1) {
            return_errno_with_message!(Errno::EINVAL, "invalid materialized root split");
        }
        let left_boundary = left
            .entries()?
            .first()
            .map(ExtentRecordEntry::logical_block)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        let root_key = self.transient_metadata_key(old_root_ref)?;
        let root = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            old_root
                .header()
                .generation()
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            root_key.page_index,
            root_level,
            PersistentObjectRef::NONE,
            PersistentObjectRef::NONE,
            &[
                ExtentRecordEntry::child(left_boundary, split.left.reference)?,
                ExtentRecordEntry::child(split.split_logical_block, split.right.reference)?,
            ],
        )?;
        let root = self.persist_initialized_child(root_key, &root, fault)?;
        let inode_ref = self
            .pool
            .fixed_inode_ref(self.fs_id, self.inode)?
            .ok_or_else(|| Error::new(Errno::ENOENT))?;
        let old_root_key = self.pool.persistent_metadata_key(old_root_ref)?;
        let mark = PendingExtentMutationRecord::prepared_root_split(
            self.fs_id,
            self.inode,
            1,
            FIXED_INODE_LOGICAL_INDEX,
            old_root_key.page_index,
            inode_ref,
            root.reference,
            old_root_ref,
            split.right.reference,
            split.split_logical_block,
        )?;
        let mark_ref = self.pool.persist_authoritative_metadata(
            PageKey::new(
                self.fs_id,
                self.inode,
                PENDING_EXTENT_MUTATION_LOGICAL_INDEX,
            ),
            &mark,
            fault,
        )?;
        self.publish_fixed_inode_root(inode_ref, old_root_ref, root.reference, fault)?;
        self.clear_metadata_object(mark_ref, fault)?;
        Ok(root)
    }

    fn replace_one_tree_extent(
        &self,
        current: PersistentObjectRef,
        replacement: ExtentRecordEntry,
        fault: Option<FaultPoint>,
    ) -> Result<TreeExtentUpdate> {
        let node = self.read_locked_node(current)?;
        self.validate_owner(&node)?;
        if node.level() == 0 {
            return self.replace_one_leaf_extent(current, node, replacement, fault);
        }

        self.replace_one_index_extent(current, node, replacement, fault)
    }

    fn replace_one_leaf_extent(
        &self,
        current: PersistentObjectRef,
        node: BoxedMetadataRecord<ExtentNodeRecord>,
        replacement: ExtentRecordEntry,
        fault: Option<FaultPoint>,
    ) -> Result<TreeExtentUpdate> {
        let mut entries = Vec::from(node.entries()?);
        replace_leaf_entry(&mut entries, replacement)?;
        merge_leaf_entries(&mut entries)?;
        if entries.len() <= EXTENT_NODE_ENTRY_CAPACITY {
            return self
                .replace_leaf(current, &node, &entries, fault)
                .map(TreeExtentUpdate::Replaced);
        }
        plan_leaf_split(current, &node, &entries, replacement).map(TreeExtentUpdate::Split)
    }

    fn replace_one_index_extent(
        &self,
        current: PersistentObjectRef,
        node: BoxedMetadataRecord<ExtentNodeRecord>,
        replacement: ExtentRecordEntry,
        fault: Option<FaultPoint>,
    ) -> Result<TreeExtentUpdate> {
        let entries = Vec::from(node.entries()?);
        if entries.is_empty() {
            return_errno_with_message!(Errno::EUCLEAN, "extent index node is empty");
        }
        let child_index = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.logical_block() <= replacement.logical_block())
            .max_by_key(|(_, entry)| entry.logical_block())
            .map(|(index, _)| index)
            .unwrap_or(0);
        let child_slot = entries[child_index].child_slot().ok_or_else(|| {
            Error::with_message(Errno::EUCLEAN, "extent index entry is not a child")
        })?;
        let child_ref = self.pool.persistent_metadata_ref_from_slot(child_slot)?;
        match self.replace_one_tree_extent(child_ref, replacement, fault)? {
            TreeExtentUpdate::Replaced(child) => {
                self.replace_index_child(current, &node, entries, child_index, child, fault)
            }
            TreeExtentUpdate::Split(split) => {
                if entries.len() == EXTENT_NODE_ENTRY_CAPACITY {
                    let split = self.materialize_leaf_split(split, fault)?;
                    return self.integrate_materialized_split(
                        current,
                        &node,
                        entries,
                        child_index,
                        split,
                        fault,
                    );
                }
                self.integrate_leaf_split(current, &node, entries, child_index, split, fault)
            }
            TreeExtentUpdate::Cascaded(split) => self.integrate_materialized_split(
                current,
                &node,
                entries,
                child_index,
                split,
                fault,
            ),
        }
    }

    fn integrate_leaf_split(
        &self,
        current: PersistentObjectRef,
        node: &ExtentNodeRecord,
        mut entries: Vec<ExtentRecordEntry>,
        child_index: usize,
        split: LeafSplit,
        fault: Option<FaultPoint>,
    ) -> Result<TreeExtentUpdate> {
        let old_child = self.read_locked_node(split.old_child)?;
        let child_key = self.transient_metadata_key(split.old_child)?;
        let right = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            old_child
                .header()
                .generation()
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            child_key.page_index,
            old_child.level(),
            current,
            self.right_sibling_ref(&old_child)?,
            &split.right_entries,
        )?;
        let initialized = self.persist_initialized_child(child_key, &right, fault)?;
        let parent_key = self.pool.persistent_metadata_key(current)?;
        let pending = self.begin_split(
            parent_key,
            current,
            split.old_child,
            &initialized,
            split.split_logical_block,
            fault,
        )?;
        entries.insert(
            child_index + 1,
            ExtentRecordEntry::child(split.split_logical_block, initialized.reference)?,
        );
        let split_parent = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            node.header()
                .generation()
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            node.node_id(),
            node.level(),
            node.parent(),
            self.right_sibling_ref(&node)?,
            &entries,
        )?;
        let (_, published) = self.publish_parent_reference_for_split(
            current,
            &split_parent,
            &initialized,
            pending,
            fault,
        )?;
        let (final_parent, _) = self.complete_split(published, fault, fault, fault)?;
        self.committed_node(final_parent)
            .map(TreeExtentUpdate::Replaced)
    }

    fn replace_index_child(
        &self,
        current: PersistentObjectRef,
        node: &ExtentNodeRecord,
        mut entries: Vec<ExtentRecordEntry>,
        child_index: usize,
        child: InitializedExtentNode,
        fault: Option<FaultPoint>,
    ) -> Result<TreeExtentUpdate> {
        let child_node = self.read_locked_node(child.reference)?;
        let child_boundary = child_node
            .entries()?
            .first()
            .map(ExtentRecordEntry::logical_block)
            .ok_or_else(|| {
                Error::with_message(Errno::EUCLEAN, "replacement extent child is empty")
            })?;
        if entries[child_index].logical_block() == child_boundary
            && entries[child_index].child_slot() == Some(child.reference.slot_plus_one())
        {
            return self.committed_node(current).map(TreeExtentUpdate::Replaced);
        }
        entries[child_index] = ExtentRecordEntry::child(child_boundary, child.reference)?;
        let replacement_parent = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            node.header()
                .generation()
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            node.node_id(),
            node.level(),
            node.parent(),
            self.right_sibling_ref(&node)?,
            &entries,
        )?;
        let reference =
            self.publish_parent_reference(current, &replacement_parent, &child, fault)?;
        self.committed_node(reference)
            .map(TreeExtentUpdate::Replaced)
    }

    fn materialize_leaf_split(
        &self,
        split: LeafSplit,
        fault: Option<FaultPoint>,
    ) -> Result<MaterializedNodeSplit> {
        let old_leaf = self.read_locked_node(split.old_child)?;
        self.validate_owner(&old_leaf)?;
        if old_leaf.level() != 0 {
            return_errno_with_message!(Errno::EINVAL, "leaf split references an index node");
        }
        let left_entries = retained_split_entries(&old_leaf, split.split_logical_block)?;
        let generation = old_leaf
            .header()
            .generation()
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;

        let right_key = self.transient_metadata_key(split.old_child)?;
        let right = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            generation,
            right_key.page_index,
            0,
            old_leaf.parent(),
            self.right_sibling_ref(&old_leaf)?,
            &split.right_entries,
        )?;
        let right = self.persist_initialized_child(right_key, &right, fault)?;
        let left_key = self.transient_metadata_key(split.old_child)?;
        let left = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            generation,
            left_key.page_index,
            0,
            old_leaf.parent(),
            right.reference,
            &left_entries,
        )?;
        let left = self.persist_initialized_child(left_key, &left, fault)?;
        Ok(MaterializedNodeSplit {
            left,
            right,
            split_logical_block: split.split_logical_block,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn integrate_materialized_split(
        &self,
        current: PersistentObjectRef,
        node: &ExtentNodeRecord,
        mut entries: Vec<ExtentRecordEntry>,
        child_index: usize,
        split: MaterializedNodeSplit,
        fault: Option<FaultPoint>,
    ) -> Result<TreeExtentUpdate> {
        let left = self.read_locked_node(split.left.reference)?;
        let left_boundary = left
            .entries()?
            .first()
            .map(ExtentRecordEntry::logical_block)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        entries[child_index] = ExtentRecordEntry::child(left_boundary, split.left.reference)?;
        entries.insert(
            child_index + 1,
            ExtentRecordEntry::child(split.split_logical_block, split.right.reference)?,
        );
        if entries.len() > EXTENT_NODE_ENTRY_CAPACITY {
            return self
                .materialize_internal_split(current, node, &entries, fault)
                .map(TreeExtentUpdate::Cascaded);
        }

        let replacement_parent = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            node.header()
                .generation()
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            node.node_id(),
            node.level(),
            node.parent(),
            self.right_sibling_ref(&node)?,
            &entries,
        )?;
        let reference =
            self.publish_parent_reference(current, &replacement_parent, &split.right, fault)?;
        self.committed_node(reference)
            .map(TreeExtentUpdate::Replaced)
    }

    fn materialize_internal_split(
        &self,
        current: PersistentObjectRef,
        node: &ExtentNodeRecord,
        entries: &[ExtentRecordEntry],
        fault: Option<FaultPoint>,
    ) -> Result<MaterializedNodeSplit> {
        if node.level() == 0 || entries.len() != EXTENT_NODE_ENTRY_CAPACITY + 1 {
            return_errno_with_message!(Errno::EINVAL, "invalid overflowing extent index");
        }
        let split_index = entries.len() / 2;
        let split_logical_block = entries[split_index].logical_block();
        let generation = node
            .header()
            .generation()
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let right_key = self.transient_metadata_key(current)?;
        let right = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            generation,
            right_key.page_index,
            node.level(),
            node.parent(),
            self.right_sibling_ref(node)?,
            &entries[split_index..],
        )?;
        let right = self.persist_initialized_child(right_key, &right, fault)?;
        let left_key = self.transient_metadata_key(current)?;
        let left = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            generation,
            left_key.page_index,
            node.level(),
            node.parent(),
            right.reference,
            &entries[..split_index],
        )?;
        let left = self.persist_initialized_child(left_key, &left, fault)?;
        Ok(MaterializedNodeSplit {
            left,
            right,
            split_logical_block,
        })
    }

    /// Converts the unwritten portions of `[logical_block, logical_block + length)`.
    pub(crate) fn convert_leaf_unwritten(
        &self,
        current_leaf: PersistentObjectRef,
        logical_block: u64,
        length: u32,
        fault: Option<FaultPoint>,
    ) -> Result<InitializedExtentNode> {
        if length == 0 {
            return_errno_with_message!(Errno::EINVAL, "unwritten conversion range is empty");
        }
        let range_end = logical_block
            .checked_add(length as u64)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let leaf = self.read_leaf(current_leaf)?;
        let mut entries = Vec::new();
        let mut changed = false;
        for entry in leaf.entries()? {
            let entry_end = entry_end(entry)?;
            let overlap_start = entry.logical_block().max(logical_block);
            let overlap_end = entry_end.min(range_end);
            if !entry.is_unwritten() || overlap_start >= overlap_end {
                entries.push(*entry);
                continue;
            }

            changed = true;
            match entry.data_ref()? {
                ExtentDataRef::Ssd(physical_block) => {
                    if entry.logical_block() < overlap_start {
                        entries.push(ExtentRecordEntry::leaf(
                            entry.logical_block(),
                            u32::try_from(overlap_start - entry.logical_block())
                                .map_err(|_| Error::new(Errno::EOVERFLOW))?,
                            physical_block,
                            true,
                        )?);
                    }
                    entries.push(ExtentRecordEntry::leaf(
                        overlap_start,
                        u32::try_from(overlap_end - overlap_start)
                            .map_err(|_| Error::new(Errno::EOVERFLOW))?,
                        physical_block + (overlap_start - entry.logical_block()),
                        false,
                    )?);
                    if overlap_end < entry_end {
                        entries.push(ExtentRecordEntry::leaf(
                            overlap_end,
                            u32::try_from(entry_end - overlap_end)
                                .map_err(|_| Error::new(Errno::EOVERFLOW))?,
                            physical_block + (overlap_end - entry.logical_block()),
                            true,
                        )?);
                    }
                }
                ExtentDataRef::Nv(reference) => entries.push(ExtentRecordEntry::nv_leaf_range(
                    entry.logical_block(),
                    entry.length(),
                    reference,
                )?),
            }
        }
        if !changed {
            return self.committed_node(current_leaf);
        }
        merge_leaf_entries(&mut entries)?;
        self.replace_leaf(current_leaf, &leaf, &entries, fault)
    }

    pub(crate) fn publish_inode_root(
        &self,
        current_inode: PersistentObjectRef,
        expected_root: PersistentObjectRef,
        replacement: &InitializedExtentNode,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef> {
        if replacement.fs_id != self.fs_id || replacement.inode != self.inode {
            return_errno_with_message!(Errno::EINVAL, "extent root belongs to another inode");
        }
        let inode = self
            .pool
            .read_authoritative_metadata::<InodeRecord>(current_inode)?;
        if inode.header().fs_id() != self.fs_id
            || inode.header().inode() != self.inode
            || inode.extent_root() != expected_root
        {
            return_errno_with_message!(Errno::ESTALE, "inode extent root was replaced");
        }
        self.read_locked_node(replacement.reference)?;
        let updated = inode.with_extent_root(
            inode
                .header()
                .generation()
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            replacement.reference,
        )?;
        self.pool
            .update_fixed_inode(&self.publication, current_inode, &updated, fault)
    }

    pub(crate) fn publish_inode_state(
        &self,
        current_inode: PersistentObjectRef,
        expected_root: PersistentObjectRef,
        replacement: &InitializedExtentNode,
        file_size: u64,
        block_count: u64,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef> {
        let inode = self
            .pool
            .read_authoritative_metadata::<InodeRecord>(current_inode)?;
        let generation = inode
            .header()
            .generation()
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.publish_inode_state_with_generation(
            current_inode,
            expected_root,
            replacement,
            generation,
            file_size,
            block_count,
            fault,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn publish_inode_state_with_generation(
        &self,
        current_inode: PersistentObjectRef,
        expected_root: PersistentObjectRef,
        replacement: &InitializedExtentNode,
        generation: u64,
        file_size: u64,
        block_count: u64,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef> {
        if replacement.fs_id != self.fs_id || replacement.inode != self.inode {
            return_errno_with_message!(Errno::EINVAL, "extent root belongs to another inode");
        }
        let inode = self
            .pool
            .read_authoritative_metadata::<InodeRecord>(current_inode)?;
        if inode.header().fs_id() != self.fs_id
            || inode.header().inode() != self.inode
            || inode.extent_root() != expected_root
        {
            return_errno_with_message!(Errno::ESTALE, "inode extent root was replaced");
        }
        self.read_locked_node(replacement.reference)?;
        let updated = inode.with_extent_root_and_size(
            generation,
            replacement.reference,
            file_size,
            block_count,
        )?;
        self.pool
            .update_fixed_inode(&self.publication, current_inode, &updated, fault)
    }

    pub(crate) fn committed_node(
        &self,
        reference: PersistentObjectRef,
    ) -> Result<InitializedExtentNode> {
        let node = self.read_locked_node(reference)?;
        self.validate_owner(&node)?;
        Ok(InitializedExtentNode {
            reference,
            fs_id: self.fs_id,
            inode: self.inode,
            node_id: node.node_id(),
            level: node.level(),
        })
    }

    fn validate_data_extent(&self, extent: &ExtentRecordEntry) -> Result<()> {
        if matches!(extent.data_ref()?, ExtentDataRef::Nv(_)) {
            for offset in 0..u64::from(extent.length()) {
                let logical_block = extent
                    .logical_block()
                    .checked_add(offset)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                let reference = self
                    .pool
                    .persistent_data_ref_from_slot(extent.nv_slot_at(logical_block)?)?;
                let key = self.pool.validate_persistent_data_ref(reference)?;
                if key.fs_id != self.fs_id
                    || key.inode != self.inode
                    || key.page_index != logical_block
                {
                    return_errno_with_message!(
                        Errno::EINVAL,
                        "NV extent data belongs to another inode"
                    );
                }
            }
        }
        Ok(())
    }

    fn read_leaf(
        &self,
        reference: PersistentObjectRef,
    ) -> Result<BoxedMetadataRecord<ExtentNodeRecord>> {
        let leaf = self.read_locked_node(reference)?;
        self.validate_owner(&leaf)?;
        if leaf.level() != 0 {
            return_errno_with_message!(Errno::EINVAL, "local extent mutation requires a leaf");
        }
        Ok(leaf)
    }

    fn replace_leaf(
        &self,
        current: PersistentObjectRef,
        leaf: &ExtentNodeRecord,
        entries: &[ExtentRecordEntry],
        fault: Option<FaultPoint>,
    ) -> Result<InitializedExtentNode> {
        if entries.is_empty() || entries.len() > EXTENT_NODE_ENTRY_CAPACITY {
            return_errno_with_message!(Errno::ENOSPC, "local extent mutation requires a split");
        }
        let replacement = ExtentNodeRecord::new(
            self.fs_id,
            self.inode,
            leaf.header()
                .generation()
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            leaf.node_id(),
            0,
            leaf.parent(),
            self.right_sibling_ref(leaf)?,
            entries,
        )?;
        let boundary = entries
            .first()
            .map(ExtentRecordEntry::logical_block)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        self.publish_repairable_node_rewrite(current, &replacement, None, boundary, fault)?;
        self.refresh_leaf_reverse(current)?;
        self.committed_node(current)
    }

    #[cfg(ktest)]
    fn refresh_leaf_reverse(&self, leaf_ref: PersistentObjectRef) -> Result<()> {
        let leaf = self.read_locked_node(leaf_ref)?;
        if leaf.level() != 0 {
            return Ok(());
        }
        let leaf_slot = leaf_ref.slot().ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        for (entry_index, entry) in leaf.entries()?.iter().enumerate() {
            if matches!(entry.data_ref()?, ExtentDataRef::Nv(_)) {
                let locator = ReverseExtentRef::new(leaf_slot, entry_index)?;
                for offset in 0..u64::from(entry.length()) {
                    let logical_block = entry
                        .logical_block()
                        .checked_add(offset)
                        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                    let reference = self
                        .pool
                        .persistent_data_ref_from_slot(entry.nv_slot_at(logical_block)?)?;
                    self.pool.replace_reverse(reference, locator)?;
                }
            }
        }
        Ok(())
    }

    #[cfg(ktest)]
    fn install_extent_reverse(
        &self,
        leaf_ref: PersistentObjectRef,
        entry_index: usize,
        entry: ExtentRecordEntry,
    ) -> Result<()> {
        if !matches!(entry.data_ref()?, ExtentDataRef::Nv(_)) {
            return Ok(());
        }
        let leaf_slot = leaf_ref.slot().ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        let locator = ReverseExtentRef::new(leaf_slot, entry_index)?;
        for offset in 0..u64::from(entry.length()) {
            let logical_block = entry
                .logical_block()
                .checked_add(offset)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            let reference = self
                .pool
                .persistent_data_ref_from_slot(entry.nv_slot_at(logical_block)?)?;
            self.pool.replace_reverse(reference, locator)?;
        }
        Ok(())
    }

    #[cfg(not(ktest))]
    fn refresh_leaf_reverse(&self, _leaf_ref: PersistentObjectRef) -> Result<()> {
        Ok(())
    }

    #[cfg(not(ktest))]
    fn install_extent_reverse(
        &self,
        _leaf_ref: PersistentObjectRef,
        _entry_index: usize,
        _entry: ExtentRecordEntry,
    ) -> Result<()> {
        Ok(())
    }
}

fn replace_leaf_entry(
    entries: &mut Vec<ExtentRecordEntry>,
    replacement: ExtentRecordEntry,
) -> Result<()> {
    let replacement_end = entry_end(&replacement)?;
    let mut updated = Vec::with_capacity(entries.len() + 2);
    let mut inserted = false;
    for entry in entries.drain(..) {
        let old_end = entry_end(&entry)?;
        if old_end <= replacement.logical_block() || entry.logical_block() >= replacement_end {
            if !inserted && entry.logical_block() >= replacement_end {
                updated.push(replacement);
                inserted = true;
            }
            updated.push(entry);
            continue;
        }
        if entry.logical_block() < replacement.logical_block() {
            updated.push(slice_extent(
                &entry,
                entry.logical_block(),
                replacement.logical_block(),
            )?);
        }
        if !inserted {
            updated.push(replacement);
            inserted = true;
        }
        if old_end > replacement_end {
            updated.push(slice_extent(&entry, replacement_end, old_end)?);
        }
    }
    if !inserted {
        updated.push(replacement);
    }
    updated.sort_unstable_by_key(ExtentRecordEntry::logical_block);
    *entries = updated;
    Ok(())
}

fn slice_extent(entry: &ExtentRecordEntry, start: u64, end: u64) -> Result<ExtentRecordEntry> {
    let length = u32::try_from(
        end.checked_sub(start)
            .ok_or_else(|| Error::new(Errno::EINVAL))?,
    )
    .map_err(|_| Error::new(Errno::EOVERFLOW))?;
    match entry.data_ref()? {
        ExtentDataRef::Ssd(block) => ExtentRecordEntry::leaf(
            start,
            length,
            block
                .checked_add(start - entry.logical_block())
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            entry.is_unwritten(),
        ),
        ExtentDataRef::Nv(_) => {
            ExtentRecordEntry::nv_leaf_range(start, length, entry.nv_slot_at(start)?)
        }
    }
}

fn plan_leaf_split(
    current: PersistentObjectRef,
    old_leaf: &ExtentNodeRecord,
    entries: &[ExtentRecordEntry],
    replacement: ExtentRecordEntry,
) -> Result<LeafSplit> {
    let old_entries = old_leaf.entries()?;
    let old_start = old_entries
        .first()
        .map(ExtentRecordEntry::logical_block)
        .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
    let old_last = old_entries
        .last()
        .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
    let old_end = entry_end(old_last)?;
    let split_logical_block =
        if replacement.logical_block() > old_start && replacement.logical_block() < old_end {
            replacement.logical_block()
        } else if replacement.logical_block() >= old_end && old_last.logical_block() > old_start {
            old_last.logical_block()
        } else {
            return_errno_with_message!(
                Errno::ENOSPC,
                "leaf split requires a new left-child protocol"
            );
        };

    let right_start = entries.partition_point(|entry| entry.logical_block() < split_logical_block);
    if right_start == 0 || right_start == entries.len() {
        return_errno_with_message!(Errno::EUCLEAN, "invalid leaf split boundary");
    }
    let right_entries = Vec::from(&entries[right_start..]);
    if right_entries.len() > EXTENT_NODE_ENTRY_CAPACITY {
        return_errno_with_message!(Errno::ENOSPC, "right split leaf remains full");
    }

    let mut expected_left = Vec::new();
    for entry in old_entries {
        if entry.logical_block() >= split_logical_block {
            break;
        }
        let end = entry_end(entry)?;
        expected_left.push(if end > split_logical_block {
            slice_extent(entry, entry.logical_block(), split_logical_block)?
        } else {
            *entry
        });
    }
    if entries[..right_start] != expected_left {
        return_errno_with_message!(Errno::ENOSPC, "split would rewrite the retained left child");
    }
    Ok(LeafSplit {
        old_child: current,
        split_logical_block,
        right_entries,
    })
}

fn retained_split_entries(
    old_node: &ExtentNodeRecord,
    split_logical_block: u64,
) -> Result<Vec<ExtentRecordEntry>> {
    let mut retained = Vec::new();
    for entry in old_node.entries()? {
        if entry.logical_block() >= split_logical_block {
            break;
        }
        if old_node.level() == 0 && entry_end(entry)? > split_logical_block {
            retained.push(slice_extent(
                entry,
                entry.logical_block(),
                split_logical_block,
            )?);
            break;
        }
        retained.push(*entry);
    }
    if retained.is_empty() || retained.len() > EXTENT_NODE_ENTRY_CAPACITY {
        return_errno_with_message!(Errno::EUCLEAN, "invalid retained split node");
    }
    Ok(retained)
}

fn entry_end(entry: &ExtentRecordEntry) -> Result<u64> {
    entry
        .logical_block()
        .checked_add(entry.length() as u64)
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))
}

fn merge_leaf_entries(entries: &mut Vec<ExtentRecordEntry>) -> Result<()> {
    let mut merged: Vec<ExtentRecordEntry> = Vec::with_capacity(entries.len());
    for entry in entries.drain(..) {
        let Some(previous) = merged.last_mut() else {
            merged.push(entry);
            continue;
        };
        let (ExtentDataRef::Ssd(previous_block), ExtentDataRef::Ssd(entry_block)) =
            (previous.data_ref()?, entry.data_ref()?)
        else {
            merged.push(entry);
            continue;
        };
        if entry_end(previous)? == entry.logical_block()
            && previous_block
                .checked_add(previous.length() as u64)
                .is_some_and(|end| end == entry_block)
            && previous.is_unwritten() == entry.is_unwritten()
        {
            let length = previous
                .length()
                .checked_add(entry.length())
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            *previous = ExtentRecordEntry::leaf(
                previous.logical_block(),
                length,
                previous_block,
                previous.is_unwritten(),
            )?;
        } else {
            merged.push(entry);
        }
    }
    *entries = merged;
    Ok(())
}
