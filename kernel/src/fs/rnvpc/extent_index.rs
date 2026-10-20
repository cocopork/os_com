// SPDX-License-Identifier: MPL-2.0

//! Rebuildable DRAM indexes over the persistent NV extent tree.
//!
//! The persistent root reference is the only tree ownership kept here. The
//! node cache is deliberately bounded, and the ES map contains leaf mapping
//! facts rather than a second copy of the tree topology. Both indexes may be
//! discarded at any time and rebuilt by walking committed NV nodes.

use core::sync::atomic::{AtomicU64, Ordering};

use spin::{Mutex, RwLock};

use super::{
    frame::Rnvpc,
    metadata::{ExtentNodeRecord, ExtentRecordEntry, InodeRecord, PersistentObjectRef},
};
use crate::prelude::*;

const EXTENT_NODE_CACHE_CAPACITY: usize = 8;
const EXTENT_STATUS_CACHE_CAPACITY: usize = 512;

#[derive(Default)]
struct RebuildableExtentIndexes {
    nodes: Vec<(PersistentObjectRef, ExtentNodeRecord)>,
    extents: BTreeMap<u64, ExtentRecordEntry>,
}

/// Runtime acceleration for one persistent NV extent tree.
pub(crate) struct ExtentRuntimeIndex {
    pool: Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    root: RwLock<PersistentObjectRef>,
    observed_mutation_epoch: AtomicU64,
    extent_capacity: usize,
    indexes: Mutex<RebuildableExtentIndexes>,
}

impl ExtentRuntimeIndex {
    pub(crate) fn new(
        pool: Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
        root: PersistentObjectRef,
    ) -> Result<Self> {
        Self::new_inner(pool, fs_id, inode, root, EXTENT_STATUS_CACHE_CAPACITY)
    }

    #[cfg(ktest)]
    pub(crate) fn new_with_extent_capacity(
        pool: Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
        root: PersistentObjectRef,
        extent_capacity: usize,
    ) -> Result<Self> {
        Self::new_inner(pool, fs_id, inode, root, extent_capacity)
    }

    fn new_inner(
        pool: Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
        root: PersistentObjectRef,
        extent_capacity: usize,
    ) -> Result<Self> {
        if extent_capacity == 0 {
            return_errno_with_message!(Errno::EINVAL, "extent cache capacity is zero");
        }
        if !root.is_none() {
            let root_record = pool.read_authoritative_metadata::<ExtentNodeRecord>(root)?;
            Self::validate_owner(&root_record, fs_id, inode)?;
        }
        let observed_mutation_epoch = pool.extent_mutation_epoch();
        Ok(Self {
            pool,
            fs_id,
            inode,
            root: RwLock::new(root),
            observed_mutation_epoch: AtomicU64::new(observed_mutation_epoch),
            extent_capacity,
            indexes: Mutex::new(RebuildableExtentIndexes::default()),
        })
    }

    /// Looks up a leaf extent without materializing the persistent tree in DRAM.
    pub(crate) fn lookup(&self, logical_block: u64) -> Result<Option<ExtentRecordEntry>> {
        loop {
            self.refresh_after_mutation()?;
            let epoch = self.observed_mutation_epoch.load(Ordering::Acquire);
            let found = self.lookup_current(logical_block)?;
            if self.pool.extent_mutation_epoch() == epoch {
                return Ok(found);
            }
        }
    }

    fn lookup_current(&self, logical_block: u64) -> Result<Option<ExtentRecordEntry>> {
        let root = self.root.read();
        if let Some(extent) = self.cached_extent(logical_block) {
            return Ok(Some(extent));
        }

        if root.is_none() {
            return Ok(None);
        }
        let mut current = *root;
        let mut expected_level = None;
        loop {
            let mut node = self.read_node(current)?;
            if let Some(level) = expected_level
                && node.level() != level
            {
                return_errno_with_message!(Errno::EUCLEAN, "NV extent child depth mismatch");
            }

            while let Some(sibling_slot) = node.right_sibling_slot() {
                let sibling_ref = self.pool.persistent_metadata_ref_from_slot(sibling_slot)?;
                let sibling = self.read_node(sibling_ref)?;
                let boundary = sibling
                    .entries()?
                    .first()
                    .map(ExtentRecordEntry::logical_block)
                    .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
                if logical_block < boundary {
                    break;
                }
                node = sibling;
            }

            let entries = node.entries()?;
            if node.level() == 0 {
                let found = entries.iter().copied().find(|entry| {
                    entry.logical_block() <= logical_block
                        && logical_block
                            < entry.logical_block().saturating_add(entry.length() as u64)
                });
                if let Some(extent) = found {
                    let mut indexes = self.indexes.lock();
                    if !indexes.extents.contains_key(&extent.logical_block())
                        && indexes.extents.len() == self.extent_capacity
                    {
                        indexes.extents.clear();
                    }
                    indexes.extents.insert(extent.logical_block(), extent);
                }
                return Ok(found);
            }

            let child_slot = entries
                .iter()
                .filter(|entry| entry.logical_block() <= logical_block)
                .max_by_key(|entry| entry.logical_block())
                .or_else(|| entries.first())
                .and_then(ExtentRecordEntry::child_slot);
            let Some(child_slot) = child_slot else {
                return Ok(None);
            };
            expected_level = Some(node.level() - 1);
            current = self.pool.persistent_metadata_ref_from_slot(child_slot)?;
        }
    }

    /// Installs an already durable root and invalidates every old-root fact.
    pub(crate) fn install_committed_root(
        &self,
        expected: PersistentObjectRef,
        replacement: PersistentObjectRef,
    ) -> Result<()> {
        if !replacement.is_none() {
            let node = self
                .pool
                .read_authoritative_metadata::<ExtentNodeRecord>(replacement)?;
            Self::validate_owner(&node, self.fs_id, self.inode)?;
        }

        let mut root = self.root.write();
        if *root != expected {
            return_errno_with_message!(Errno::ESTALE, "NV extent root was replaced");
        }
        *root = replacement;
        self.clear_rebuildable_indexes();
        self.observed_mutation_epoch
            .store(self.pool.extent_mutation_epoch(), Ordering::Release);
        Ok(())
    }

    /// Drops all DRAM acceleration state; the persistent root remains current.
    pub(crate) fn clear_rebuildable_indexes(&self) {
        let mut indexes = self.indexes.lock();
        indexes.nodes.clear();
        indexes.extents.clear();
    }

    pub(crate) const fn node_cache_capacity(&self) -> usize {
        EXTENT_NODE_CACHE_CAPACITY
    }

    pub(crate) const fn extent_cache_capacity(&self) -> usize {
        self.extent_capacity
    }

    pub(crate) fn cached_node_count(&self) -> usize {
        self.indexes.lock().nodes.len()
    }

    pub(crate) fn cached_extent_count(&self) -> usize {
        self.indexes.lock().extents.len()
    }

    fn cached_extent(&self, logical_block: u64) -> Option<ExtentRecordEntry> {
        let indexes = self.indexes.lock();
        let (_, extent) = indexes.extents.range(..=logical_block).next_back()?;
        (logical_block
            < extent
                .logical_block()
                .saturating_add(extent.length() as u64))
        .then_some(*extent)
    }

    fn refresh_after_mutation(&self) -> Result<()> {
        let current_epoch = self.pool.extent_mutation_epoch();
        if self.observed_mutation_epoch.load(Ordering::Acquire) == current_epoch {
            return Ok(());
        }

        if let Some(inode_ref) = self.pool.fixed_inode_ref(self.fs_id, self.inode)? {
            let inode = self
                .pool
                .read_authoritative_metadata::<InodeRecord>(inode_ref)?;
            *self.root.write() = inode.extent_root();
        }
        self.clear_rebuildable_indexes();
        self.observed_mutation_epoch
            .store(current_epoch, Ordering::Release);
        Ok(())
    }

    fn read_node(&self, reference: PersistentObjectRef) -> Result<ExtentNodeRecord> {
        if let Some((_, node)) = self
            .indexes
            .lock()
            .nodes
            .iter()
            .find(|(cached_ref, _)| *cached_ref == reference)
        {
            return Ok(*node);
        }

        let node = self
            .pool
            .read_authoritative_metadata::<ExtentNodeRecord>(reference)?;
        Self::validate_owner(&node, self.fs_id, self.inode)?;

        let mut indexes = self.indexes.lock();
        if indexes.nodes.len() == EXTENT_NODE_CACHE_CAPACITY {
            indexes.nodes.remove(0);
        }
        indexes.nodes.push((reference, node));
        Ok(node)
    }

    fn validate_owner(node: &ExtentNodeRecord, fs_id: u64, inode: u64) -> Result<()> {
        if node.header().fs_id() != fs_id || node.header().inode() != inode {
            return_errno_with_message!(Errno::EUCLEAN, "NV extent node belongs to another inode");
        }
        Ok(())
    }
}
