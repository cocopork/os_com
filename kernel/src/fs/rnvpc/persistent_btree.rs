// SPDX-License-Identifier: MPL-2.0

//! Persistent B-link B+tree metadata index.

use super::{
    frame::Rnvpc,
    index::PageKey,
    pcache_layout::{
        BTREE_NODE_ENTRY_CAPACITY, BtreeNodeFrame, BtreeNodeImage, InternalEntry, LeafEntry,
        PcObjectType, PersistentPageRef, PersistentWalRecord, WAL_OP_UPSERT, btree_image_range,
        btree_selector_range,
    },
    pcache_store::{PcFaultPoint, PersistPlan, PersistentMetadataStore, inject_fault},
};
use crate::prelude::*;

const LEFT_SPLIT_COUNT: usize = BTREE_NODE_ENTRY_CAPACITY / 2;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentMapping {
    pub(crate) page_index: u64,
    pub(crate) page: PersistentPageRef,
    pub(crate) ssd_backing_block: u64,
    pub(crate) lsn: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct BtreeReplayStats {
    pub(crate) records_replayed: u64,
    pub(crate) node_updates: u64,
    pub(crate) btree_splits: u64,
    pub(crate) persist_fences: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RootUpdate {
    pub(crate) root: PersistentPageRef,
    pub(crate) height: u16,
}

#[derive(Clone, Copy)]
struct BtreeRuntimeState {
    root: PersistentPageRef,
    height: u16,
    next_node_id: u64,
}

#[derive(Clone, Copy)]
struct PathEntry {
    page: PersistentPageRef,
}

#[derive(Clone, Copy)]
struct SplitResult {
    left: PersistentPageRef,
    right: PersistentPageRef,
    left_high: u64,
    right_high: u64,
    level: u16,
}

pub(crate) struct PersistentBtree {
    fs_id: u64,
    inode: u64,
    store: PersistentMetadataStore,
    state: Mutex<BtreeRuntimeState>,
    counters: Mutex<BtreeReplayStats>,
}

impl PersistentBtree {
    pub(crate) fn create(pool: Arc<Rnvpc>, fs_id: u64, inode: u64) -> Result<Arc<Self>> {
        let store = PersistentMetadataStore::new(pool);
        let mut root = BtreeNodeFrame::empty_leaf(fs_id, inode, 0);
        root.images[0].high_key = u64::MAX;
        let root = store.allocate(fs_id, inode, PcObjectType::BtreeNode, 0, &root, None)?;
        Ok(Arc::new(Self {
            fs_id,
            inode,
            store,
            state: Mutex::new(BtreeRuntimeState {
                root,
                height: 0,
                next_node_id: 1,
            }),
            counters: Mutex::new(BtreeReplayStats::default()),
        }))
    }

    pub(crate) fn recover(
        pool: Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
        root: PersistentPageRef,
        height: u16,
    ) -> Result<Arc<Self>> {
        let store = PersistentMetadataStore::new(pool);
        let mut max_id = None;
        for (node_id, reference) in store.typed_objects(fs_id, inode, PcObjectType::BtreeNode)? {
            store.validate_typed_ref(reference, fs_id, inode, PcObjectType::BtreeNode, node_id)?;
            let frame = store.read::<BtreeNodeFrame>(reference)?;
            if frame.node_id != node_id {
                return_errno_with_message!(Errno::EINVAL, "B-tree descriptor/node ID mismatch");
            }
            validate_node(&frame, fs_id, inode)?;
            max_id = Some(max_id.map_or(node_id, |old: u64| old.max(node_id)));
        }
        let next_node_id = max_id
            .ok_or_else(|| Error::new(Errno::EINVAL))?
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let tree = Arc::new(Self {
            fs_id,
            inode,
            store,
            state: Mutex::new(BtreeRuntimeState {
                root,
                height,
                next_node_id,
            }),
            counters: Mutex::new(BtreeReplayStats::default()),
        });
        tree.validate_recoverable()?;
        Ok(tree)
    }

    pub(crate) fn root_ref(&self) -> PersistentPageRef {
        self.state.lock().root
    }
    pub(crate) fn height(&self) -> u16 {
        self.state.lock().height
    }
    pub(crate) fn stats(&self) -> BtreeReplayStats {
        *self.counters.lock()
    }

    pub(crate) fn install_published_root(&self, update: RootUpdate) -> Result<()> {
        let mut state = self.state.lock();
        if update.height
            != state
                .height
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?
        {
            return_errno_with_message!(Errno::EINVAL, "unexpected B-tree root handoff");
        }
        self.store.validate_typed_ref(
            update.root,
            self.fs_id,
            self.inode,
            PcObjectType::BtreeNode,
            self.store.read::<BtreeNodeFrame>(update.root)?.node_id,
        )?;
        state.root = update.root;
        state.height = update.height;
        Ok(())
    }

    pub(crate) fn lookup(&self, page_index: u64) -> Result<Option<PersistentMapping>> {
        let state = *self.state.lock();
        let (_, leaf) = self.descend(state.root, page_index, false)?;
        let leaf_frame = self.store.read::<BtreeNodeFrame>(leaf)?;
        let image = selected_image(&leaf_frame)?;
        for entry in &image.leaf_entries[..image.entry_count as usize] {
            if entry.page_index == page_index {
                self.store.validate_data_ref(
                    entry.page,
                    PageKey::new(self.fs_id, self.inode, page_index),
                )?;
                return Ok(Some(PersistentMapping {
                    page_index,
                    page: entry.page,
                    ssd_backing_block: entry.ssd_backing_block,
                    lsn: image.page_lsn,
                }));
            }
        }
        Ok(None)
    }

    /// Persists a latest SSD target through the leaf's inactive image.
    pub(crate) fn update_ssd_backing_block(&self, page_index: u64, target: u64) -> Result<()> {
        let state = *self.state.lock();
        let (_, leaf) = self.descend(state.root, page_index, false)?;
        let frame = self.store.read::<BtreeNodeFrame>(leaf)?;
        let image = *selected_image(&frame)?;
        let count = image.entry_count as usize;
        let Some(index) = image.leaf_entries[..count]
            .iter()
            .position(|entry| entry.page_index == page_index)
        else {
            return_errno_with_message!(Errno::ENOENT, "persistent mapping is absent");
        };
        let mut entries = image.leaf_entries;
        entries[index].ssd_backing_block = target;
        self.write_leaf(
            leaf,
            image,
            entries,
            count,
            image.high_key,
            image.right_sibling,
            image.page_lsn,
            None,
        )
    }

    /// Removes an exact leaf mapping through the leaf's inactive image.
    ///
    /// Empty leaves and conservative high keys are retained: rTIO reclamation never
    /// restructures the tree.
    pub(crate) fn remove_mapping(
        &self,
        page_index: u64,
        expected_page: PersistentPageRef,
    ) -> Result<bool> {
        let state = *self.state.lock();
        let (_, leaf) = self.descend(state.root, page_index, false)?;
        let frame = self.store.read::<BtreeNodeFrame>(leaf)?;
        let image = *selected_image(&frame)?;
        let count = image.entry_count as usize;
        let Some(index) = image.leaf_entries[..count]
            .iter()
            .position(|entry| entry.page_index == page_index)
        else {
            return Ok(false);
        };
        if image.leaf_entries[index].page != expected_page {
            return Ok(false);
        }
        let mut entries = image.leaf_entries;
        entries.copy_within(index + 1..count, index);
        entries[count - 1] = LeafEntry::default();
        self.write_leaf(
            leaf,
            image,
            entries,
            count - 1,
            image.high_key,
            image.right_sibling,
            image.page_lsn,
            None,
        )?;
        Ok(true)
    }

    /// Persists all node changes, but deliberately leaves a newly durable root
    /// out of DRAM until its owner publishes it through the anchor.
    pub(crate) fn replay_upsert(
        &self,
        record: &PersistentWalRecord,
        fault: Option<PcFaultPoint>,
    ) -> Result<Option<RootUpdate>> {
        if record.operation != WAL_OP_UPSERT {
            return_errno_with_message!(Errno::EINVAL, "unsupported persistent WAL operation");
        }
        let _page_guard = self.store.pin_data_ref(
            record.new_page,
            PageKey::new(self.fs_id, self.inode, record.page_index),
        )?;
        let mut state = self.state.lock();
        let (path, leaf) = self.descend(state.root, record.page_index, true)?;
        let leaf_frame = self.store.read::<BtreeNodeFrame>(leaf)?;
        let leaf_image = *selected_image(&leaf_frame)?;
        let mut split = None;
        if leaf_image.page_lsn < record.lsn {
            split = self.upsert_leaf(leaf, leaf_image, record, &mut state, fault)?;
        }
        let mut repair_index = None;
        if split.is_none() {
            if let Some((index, repaired)) = self.missing_path_split(&path, record.lsn)? {
                split = Some(repaired);
                repair_index = Some(index);
            }
        }

        // A prior attempt can have persisted the left link and the record but
        // not its parent. A height-zero root makes that state unambiguous.
        if split.is_none() && state.height == 0 {
            let root_image = *selected_image(&self.store.read::<BtreeNodeFrame>(state.root)?)?;
            if root_image.right_sibling != PersistentPageRef::NONE {
                split = Some(SplitResult {
                    left: state.root,
                    right: root_image.right_sibling,
                    left_high: root_image.high_key,
                    right_high: selected_image(
                        &self
                            .store
                            .read::<BtreeNodeFrame>(root_image.right_sibling)?,
                    )?
                    .high_key,
                    level: 0,
                });
            }
        }
        let update = if let Some(split) = split {
            self.insert_split(
                &path[..repair_index.unwrap_or(path.len())],
                split,
                record.lsn,
                &mut state,
                fault,
            )?
        } else {
            None
        };
        if leaf_image.page_lsn < record.lsn {
            let mut counters = self.counters.lock();
            counters.records_replayed += 1;
            counters.node_updates += 1;
            counters.persist_fences += 2;
        }
        Ok(update)
    }

    /// Returns every validated leaf mapping and every metadata frame reachable
    /// from the selected root, including B-link right siblings.
    pub(crate) fn reachable_mappings_and_nodes(
        &self,
    ) -> Result<(Vec<PersistentMapping>, Vec<PersistentPageRef>)> {
        self.validate_recoverable()?;
        let state = *self.state.lock();
        let mut nodes = Vec::new();
        let mut mappings = Vec::new();
        let mut visited = BTreeSet::new();
        self.collect_reachable(
            state.root,
            state.height,
            &mut visited,
            &mut nodes,
            &mut mappings,
        )?;
        Ok((mappings, nodes))
    }

    fn collect_reachable(
        &self,
        first: PersistentPageRef,
        level: u16,
        visited: &mut BTreeSet<(u32, u64)>,
        nodes: &mut Vec<PersistentPageRef>,
        mappings: &mut Vec<PersistentMapping>,
    ) -> Result<()> {
        let mut page = first;
        loop {
            if !visited.insert(page_key(page)) {
                return Ok(());
            }
            let image = self.validated_image(page)?;
            if image.level != level {
                return_errno_with_message!(Errno::EINVAL, "B-tree reachability level mismatch");
            }
            nodes.push(page);
            if level == 0 {
                for entry in &image.leaf_entries[..image.entry_count as usize] {
                    self.store.validate_data_ref(
                        entry.page,
                        PageKey::new(self.fs_id, self.inode, entry.page_index),
                    )?;
                    mappings.push(PersistentMapping {
                        page_index: entry.page_index,
                        page: entry.page,
                        ssd_backing_block: entry.ssd_backing_block,
                        lsn: image.page_lsn,
                    });
                }
            } else {
                for entry in &image.internal_entries[..image.entry_count as usize] {
                    self.collect_reachable(entry.child, level - 1, visited, nodes, mappings)?;
                }
            }
            if image.right_sibling == PersistentPageRef::NONE {
                return Ok(());
            }
            page = image.right_sibling;
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        let state = *self.state.lock();
        let root = self.store.read::<BtreeNodeFrame>(state.root)?;
        let image = selected_image(&root)?;
        if image.level != state.height {
            return_errno_with_message!(Errno::EINVAL, "root level/height mismatch");
        }
        self.validate_tree(state.root, state.height, false)
    }

    fn validate_recoverable(&self) -> Result<()> {
        let state = *self.state.lock();
        let root = self.store.read::<BtreeNodeFrame>(state.root)?;
        self.store.validate_typed_ref(
            state.root,
            self.fs_id,
            self.inode,
            PcObjectType::BtreeNode,
            root.node_id,
        )?;
        if state.height == 0 && root.node_id != 0 {
            return_errno_with_message!(Errno::EINVAL, "height-zero root must use node ID zero");
        }
        if selected_image(&root)?.level != state.height {
            return_errno_with_message!(Errno::EINVAL, "root level/height mismatch");
        }
        validate_node(&root, self.fs_id, self.inode)?;
        self.validate_tree(state.root, state.height, true)
    }

    fn descend(
        &self,
        mut page: PersistentPageRef,
        key: u64,
        keep_path: bool,
    ) -> Result<(Vec<PathEntry>, PersistentPageRef)> {
        let mut path = Vec::new();
        let mut visited = Vec::new();
        let node_budget = self
            .store
            .typed_objects(self.fs_id, self.inode, PcObjectType::BtreeNode)?
            .len();
        loop {
            if visited.len() >= node_budget || visited.contains(&page) {
                return_errno_with_message!(Errno::EINVAL, "B-tree sibling cycle");
            }
            visited.push(page);
            let frame = self.store.read::<BtreeNodeFrame>(page)?;
            validate_node(&frame, self.fs_id, self.inode)?;
            let image = selected_image(&frame)?;
            if key > image.high_key {
                if image.right_sibling == PersistentPageRef::NONE {
                    return Ok((path, page));
                }
                let sibling = self.store.read::<BtreeNodeFrame>(image.right_sibling)?;
                let next = selected_image(&sibling)?;
                if next.level != image.level || next.high_key <= image.high_key {
                    return_errno_with_message!(Errno::EINVAL, "invalid B-tree sibling ordering");
                }
                page = image.right_sibling;
                continue;
            }
            if image.level == 0 {
                return Ok((path, page));
            }
            if keep_path {
                path.push(PathEntry { page });
            }
            let child = image.internal_entries[..image.entry_count as usize]
                .iter()
                .find(|entry| key <= entry.separator)
                .ok_or_else(|| Error::new(Errno::EINVAL))?
                .child;
            page = child;
        }
    }

    fn missing_path_split(
        &self,
        path: &[PathEntry],
        lsn: u64,
    ) -> Result<Option<(usize, SplitResult)>> {
        for (index, entry) in path.iter().enumerate().rev() {
            let parent = *selected_image(&self.store.read::<BtreeNodeFrame>(entry.page)?)?;
            let children = &parent.internal_entries[..parent.entry_count as usize];
            for (child_index, child) in children.iter().enumerate() {
                let left = *selected_image(&self.store.read::<BtreeNodeFrame>(child.child)?)?;
                if left.right_sibling == PersistentPageRef::NONE {
                    continue;
                }
                let right =
                    *selected_image(&self.store.read::<BtreeNodeFrame>(left.right_sibling)?)?;
                if left.level != right.level || left.page_lsn != lsn || right.page_lsn != lsn {
                    continue;
                }
                if children
                    .iter()
                    .any(|candidate| candidate.child == left.right_sibling)
                {
                    continue;
                }
                if child_index + 1 < children.len()
                    && left.high_key >= children[child_index + 1].separator
                {
                    return_errno_with_message!(Errno::EIO, "conflicting B-tree sibling repair");
                }
                return Ok(Some((
                    index,
                    SplitResult {
                        left: child.child,
                        right: left.right_sibling,
                        left_high: left.high_key,
                        right_high: right.high_key,
                        level: left.level,
                    },
                )));
            }
        }
        Ok(None)
    }

    fn upsert_leaf(
        &self,
        leaf: PersistentPageRef,
        image: BtreeNodeImage,
        record: &PersistentWalRecord,
        state: &mut BtreeRuntimeState,
        fault: Option<PcFaultPoint>,
    ) -> Result<Option<SplitResult>> {
        let count = image.entry_count as usize;
        let mut entries = image.leaf_entries;
        match entries[..count].binary_search_by_key(&record.page_index, |entry| entry.page_index) {
            Ok(index) => {
                entries[index] = entry_for(record);
                self.write_leaf(
                    leaf,
                    image,
                    entries,
                    count,
                    image.high_key,
                    image.right_sibling,
                    record.lsn,
                    None,
                )?;
                Ok(None)
            }
            Err(index) if count < BTREE_NODE_ENTRY_CAPACITY => {
                entries.copy_within(index..count, index + 1);
                entries[index] = entry_for(record);
                self.write_leaf(
                    leaf,
                    image,
                    entries,
                    count + 1,
                    image.high_key,
                    image.right_sibling,
                    record.lsn,
                    None,
                )?;
                Ok(None)
            }
            Err(index) => {
                let mut all = [LeafEntry::default(); BTREE_NODE_ENTRY_CAPACITY + 1];
                all[..count].copy_from_slice(&entries[..count]);
                all.copy_within(index..count, index + 1);
                all[index] = entry_for(record);
                let left_high = all[LEFT_SPLIT_COUNT - 1].page_index;
                let right_entries = {
                    let mut result = [LeafEntry::default(); BTREE_NODE_ENTRY_CAPACITY];
                    result[..BTREE_NODE_ENTRY_CAPACITY + 1 - LEFT_SPLIT_COUNT]
                        .copy_from_slice(&all[LEFT_SPLIT_COUNT..]);
                    result
                };
                let right = match self.find_candidate(
                    0,
                    record.lsn,
                    image.high_key,
                    image.right_sibling,
                    &right_entries,
                    None,
                    BTREE_NODE_ENTRY_CAPACITY + 1 - LEFT_SPLIT_COUNT,
                )? {
                    Some(reference) => reference,
                    None => self.allocate_leaf(
                        self.next_node_id(state)?,
                        record.lsn,
                        image.high_key,
                        image.right_sibling,
                        right_entries,
                        BTREE_NODE_ENTRY_CAPACITY + 1 - LEFT_SPLIT_COUNT,
                    )?,
                };
                inject_fault(fault, PcFaultPoint::AfterRightNodePersist)?;
                let mut left_entries = [LeafEntry::default(); BTREE_NODE_ENTRY_CAPACITY];
                left_entries[..LEFT_SPLIT_COUNT].copy_from_slice(&all[..LEFT_SPLIT_COUNT]);
                self.write_leaf(
                    leaf,
                    image,
                    left_entries,
                    LEFT_SPLIT_COUNT,
                    left_high,
                    right,
                    record.lsn,
                    None,
                )?;
                self.counters.lock().btree_splits += 1;
                inject_fault(fault, PcFaultPoint::AfterLeftSiblingLinkPersist)?;
                Ok(Some(SplitResult {
                    left: leaf,
                    right,
                    left_high,
                    right_high: image.high_key,
                    level: 0,
                }))
            }
        }
    }

    fn insert_split(
        &self,
        path: &[PathEntry],
        split: SplitResult,
        lsn: u64,
        state: &mut BtreeRuntimeState,
        fault: Option<PcFaultPoint>,
    ) -> Result<Option<RootUpdate>> {
        if path.is_empty() {
            return self.new_root(split, lsn, state, fault).map(Some);
        }
        let parent = path[path.len() - 1].page;
        let image = *selected_image(&self.store.read::<BtreeNodeFrame>(parent)?)?;
        let count = image.entry_count as usize;
        let pos = image.internal_entries[..count]
            .iter()
            .position(|entry| entry.child == split.left)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let mut all = [InternalEntry::default(); BTREE_NODE_ENTRY_CAPACITY + 1];
        all[..count].copy_from_slice(&image.internal_entries[..count]);
        all.copy_within(pos + 1..count, pos + 2);
        all[pos] = InternalEntry {
            separator: split.left_high,
            child: split.left,
        };
        all[pos + 1] = InternalEntry {
            separator: split.right_high,
            child: split.right,
        };
        if count < BTREE_NODE_ENTRY_CAPACITY {
            self.write_internal(
                parent,
                image,
                &all,
                count + 1,
                image.high_key,
                image.right_sibling,
                lsn,
                None,
            )?;
            inject_fault(fault, PcFaultPoint::AfterParentSeparatorPersist)?;
            return Ok(None);
        }
        let left_count = LEFT_SPLIT_COUNT;
        let right_count = BTREE_NODE_ENTRY_CAPACITY + 1 - left_count;
        let left_high = all[left_count - 1].separator;
        let mut right_entries = [InternalEntry::default(); BTREE_NODE_ENTRY_CAPACITY];
        right_entries[..right_count].copy_from_slice(&all[left_count..]);
        let right = match self.find_candidate(
            image.level,
            lsn,
            image.high_key,
            image.right_sibling,
            &[],
            Some(&right_entries),
            right_count,
        )? {
            Some(reference) => reference,
            None => self.allocate_internal(
                self.next_node_id(state)?,
                image.level,
                lsn,
                image.high_key,
                image.right_sibling,
                &right_entries,
                right_count,
            )?,
        };
        inject_fault(fault, PcFaultPoint::AfterRightNodePersist)?;
        inject_fault(fault, PcFaultPoint::AfterInternalRightNodePersist)?;
        let mut left_entries = [InternalEntry::default(); BTREE_NODE_ENTRY_CAPACITY];
        left_entries[..left_count].copy_from_slice(&all[..left_count]);
        self.write_internal(
            parent,
            image,
            &left_entries,
            left_count,
            left_high,
            right,
            lsn,
            None,
        )?;
        inject_fault(fault, PcFaultPoint::AfterLeftSiblingLinkPersist)?;
        self.counters.lock().btree_splits += 1;
        let parent_split = SplitResult {
            left: parent,
            right,
            left_high,
            right_high: image.high_key,
            level: image.level,
        };
        self.insert_split(&path[..path.len() - 1], parent_split, lsn, state, fault)
    }

    fn new_root(
        &self,
        split: SplitResult,
        lsn: u64,
        state: &mut BtreeRuntimeState,
        fault: Option<PcFaultPoint>,
    ) -> Result<RootUpdate> {
        let entries = [
            InternalEntry {
                separator: split.left_high,
                child: split.left,
            },
            InternalEntry {
                separator: split.right_high,
                child: split.right,
            },
        ];
        let level = split
            .level
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let root = match self.find_candidate(
            level,
            lsn,
            u64::MAX,
            PersistentPageRef::NONE,
            &[],
            Some(&entries),
            2,
        )? {
            Some(reference) => reference,
            None => self.allocate_internal(
                self.next_node_id(state)?,
                level,
                lsn,
                u64::MAX,
                PersistentPageRef::NONE,
                &entries,
                2,
            )?,
        };
        inject_fault(fault, PcFaultPoint::AfterNewRootPersist)?;
        Ok(RootUpdate {
            root,
            height: split.level + 1,
        })
    }

    fn next_node_id(&self, state: &mut BtreeRuntimeState) -> Result<u64> {
        let id = state.next_node_id;
        state.next_node_id = state
            .next_node_id
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        Ok(id)
    }

    fn allocate_internal(
        &self,
        node_id: u64,
        level: u16,
        lsn: u64,
        high_key: u64,
        sibling: PersistentPageRef,
        entries: &[InternalEntry],
        count: usize,
    ) -> Result<PersistentPageRef> {
        let mut frame = BtreeNodeFrame::empty_leaf(self.fs_id, self.inode, node_id);
        let image = &mut frame.images[0];
        image.level = level;
        image.page_lsn = lsn;
        image.high_key = high_key;
        image.right_sibling = sibling;
        image.entry_count = count as u16;
        if level == 0 {
            return_errno_with_message!(Errno::EINVAL, "leaf allocation requires leaf entries");
        }
        image.internal_entries[..count].copy_from_slice(&entries[..count]);
        self.store.allocate(
            self.fs_id,
            self.inode,
            PcObjectType::BtreeNode,
            node_id,
            &frame,
            None,
        )
    }

    fn find_candidate(
        &self,
        level: u16,
        lsn: u64,
        high_key: u64,
        sibling: PersistentPageRef,
        leaf_entries: &[LeafEntry],
        internal_entries: Option<&[InternalEntry]>,
        count: usize,
    ) -> Result<Option<PersistentPageRef>> {
        let mut found = None;
        for (node_id, reference) in
            self.store
                .typed_objects(self.fs_id, self.inode, PcObjectType::BtreeNode)?
        {
            let frame = self.store.read::<BtreeNodeFrame>(reference)?;
            if frame.node_id != node_id {
                return_errno_with_message!(Errno::EIO, "B-tree descriptor/node ID mismatch");
            }
            let image = selected_image(&frame)?;
            if image.level != level
                || image.page_lsn != lsn
                || image.high_key != high_key
                || image.right_sibling != sibling
                || image.entry_count as usize != count
            {
                continue;
            }
            let exact = if level == 0 {
                internal_entries.is_none() && image.leaf_entries[..count] == leaf_entries[..count]
            } else {
                internal_entries
                    .is_some_and(|entries| image.internal_entries[..count] == entries[..count])
            };
            if !exact {
                return_errno_with_message!(Errno::EIO, "conflicting B-tree split candidate");
            }
            if found.replace(reference).is_some() {
                return_errno_with_message!(Errno::EIO, "duplicate B-tree split candidate");
            }
        }
        Ok(found)
    }
    fn allocate_leaf(
        &self,
        node_id: u64,
        lsn: u64,
        high_key: u64,
        sibling: PersistentPageRef,
        entries: [LeafEntry; BTREE_NODE_ENTRY_CAPACITY],
        count: usize,
    ) -> Result<PersistentPageRef> {
        let mut frame = BtreeNodeFrame::empty_leaf(self.fs_id, self.inode, node_id);
        let image = &mut frame.images[0];
        image.page_lsn = lsn;
        image.high_key = high_key;
        image.right_sibling = sibling;
        image.entry_count = count as u16;
        image.leaf_entries = entries;
        self.store.allocate(
            self.fs_id,
            self.inode,
            PcObjectType::BtreeNode,
            node_id,
            &frame,
            None,
        )
    }

    fn write_leaf(
        &self,
        page: PersistentPageRef,
        old: BtreeNodeImage,
        entries: [LeafEntry; BTREE_NODE_ENTRY_CAPACITY],
        count: usize,
        high_key: u64,
        sibling: PersistentPageRef,
        lsn: u64,
        fault: Option<PcFaultPoint>,
    ) -> Result<()> {
        self.store
            .update(page, fault, |frame: &mut BtreeNodeFrame| {
                let active = frame.active_slot as usize;
                let inactive = 1 - active;
                let mut next = old;
                next.generation = old
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                next.page_lsn = lsn;
                next.high_key = high_key;
                next.right_sibling = sibling;
                next.entry_count = count as u16;
                next.leaf_entries = entries;
                frame.images[inactive] = next;
                frame.active_slot = inactive as u64;
                Ok(PersistPlan {
                    image_range: btree_image_range(inactive),
                    selector_range: btree_selector_range(),
                })
            })
    }

    fn write_internal(
        &self,
        page: PersistentPageRef,
        old: BtreeNodeImage,
        entries: &[InternalEntry],
        count: usize,
        high_key: u64,
        sibling: PersistentPageRef,
        lsn: u64,
        fault: Option<PcFaultPoint>,
    ) -> Result<()> {
        self.store
            .update(page, fault, |frame: &mut BtreeNodeFrame| {
                let active = frame.active_slot as usize;
                let inactive = 1 - active;
                let mut next = old;
                next.generation = old
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                next.page_lsn = lsn;
                next.high_key = high_key;
                next.right_sibling = sibling;
                next.entry_count = count as u16;
                next.internal_entries[..count].copy_from_slice(&entries[..count]);
                frame.images[inactive] = next;
                frame.active_slot = inactive as u64;
                Ok(PersistPlan {
                    image_range: btree_image_range(inactive),
                    selector_range: btree_selector_range(),
                })
            })
    }

    fn validate_tree(&self, root: PersistentPageRef, height: u16, recoverable: bool) -> Result<()> {
        let mut reachable = BTreeSet::new();
        let mut ancestors = BTreeSet::new();
        let image = self.validate_node_contents(
            root,
            height,
            None,
            u64::MAX,
            recoverable,
            &mut reachable,
            &mut ancestors,
        )?;
        self.validate_root_topology(
            root,
            image,
            height,
            recoverable,
            &mut reachable,
            &mut ancestors,
        )
    }

    fn validated_image(&self, page: PersistentPageRef) -> Result<BtreeNodeImage> {
        let frame = self.store.read::<BtreeNodeFrame>(page)?;
        self.store.validate_typed_ref(
            page,
            self.fs_id,
            self.inode,
            PcObjectType::BtreeNode,
            frame.node_id,
        )?;
        validate_node(&frame, self.fs_id, self.inode)?;
        Ok(*selected_image(&frame)?)
    }

    fn validate_node_contents(
        &self,
        page: PersistentPageRef,
        level: u16,
        lower: Option<u64>,
        upper: u64,
        recoverable: bool,
        reachable: &mut BTreeSet<(u32, u64)>,
        ancestors: &mut BTreeSet<(u32, u64)>,
    ) -> Result<BtreeNodeImage> {
        if ancestors.contains(&page_key(page)) {
            return_errno_with_message!(Errno::EINVAL, "B-tree cycle");
        }
        if !reachable.insert(page_key(page)) {
            return_errno_with_message!(Errno::EINVAL, "B-tree child alias");
        }
        ancestors.insert(page_key(page));
        let result = (|| {
            let image = self.validated_image(page)?;
            if image.level != level
                || image.high_key > upper
                || lower.is_some_and(|key| image.high_key <= key)
            {
                return_errno_with_message!(Errno::EINVAL, "invalid B-tree level/range");
            }
            if level == 0 {
                let mut previous = lower;
                for entry in &image.leaf_entries[..image.entry_count as usize] {
                    if previous.is_some_and(|key| entry.page_index <= key)
                        || entry.page_index > image.high_key
                    {
                        return_errno_with_message!(
                            Errno::EINVAL,
                            "invalid B-tree leaf order/range"
                        );
                    }
                    self.store.validate_data_ref(
                        entry.page,
                        PageKey::new(self.fs_id, self.inode, entry.page_index),
                    )?;
                    previous = Some(entry.page_index);
                }
            } else {
                let entries = &image.internal_entries[..image.entry_count as usize];
                if entries.last().map(|entry| entry.separator) != Some(image.high_key) {
                    return_errno_with_message!(Errno::EINVAL, "internal final separator mismatch");
                }
                let mut child_lower = lower;
                for entry in entries {
                    if child_lower.is_some_and(|key| entry.separator <= key) {
                        return_errno_with_message!(Errno::EINVAL, "unordered internal separators");
                    }
                    self.validate_node_contents(
                        entry.child,
                        level - 1,
                        child_lower,
                        entry.separator,
                        recoverable,
                        reachable,
                        ancestors,
                    )?;
                    child_lower = Some(entry.separator);
                }
                self.validate_child_topology(
                    entries,
                    level - 1,
                    recoverable,
                    reachable,
                    ancestors,
                )?;
            }
            Ok(image)
        })();
        ancestors.remove(&page_key(page));
        result
    }

    fn validate_child_topology(
        &self,
        entries: &[InternalEntry],
        child_level: u16,
        recoverable: bool,
        reachable: &mut BTreeSet<(u32, u64)>,
        ancestors: &mut BTreeSet<(u32, u64)>,
    ) -> Result<()> {
        for (index, entry) in entries.iter().enumerate() {
            let mut page = entry.child;
            let mut image = self.validated_image(page)?;
            let mut chain = BTreeSet::new();
            chain.insert(page_key(page));
            while image.high_key < entry.separator {
                if !recoverable || image.right_sibling == PersistentPageRef::NONE {
                    return_errno_with_message!(Errno::EINVAL, "unlisted B-tree sibling extension");
                }
                page = image.right_sibling;
                if !chain.insert(page_key(page))
                    || entries.iter().any(|listed| listed.child == page)
                {
                    return_errno_with_message!(Errno::EINVAL, "invalid B-tree sibling topology");
                }
                let lower = image.high_key;
                image = self.validated_image(page)?;
                if image.level != child_level
                    || image.high_key <= lower
                    || image.high_key > entry.separator
                {
                    return_errno_with_message!(Errno::EINVAL, "invalid B-tree sibling topology");
                }
                self.validate_node_contents(
                    page,
                    child_level,
                    Some(lower),
                    entry.separator,
                    recoverable,
                    reachable,
                    ancestors,
                )?;
            }
            if image.high_key != entry.separator {
                return_errno_with_message!(Errno::EINVAL, "invalid B-tree sibling topology");
            }
            let expected = entries
                .get(index + 1)
                .map_or(PersistentPageRef::NONE, |next| next.child);
            if image.right_sibling != expected {
                return_errno_with_message!(Errno::EINVAL, "invalid B-tree sibling topology");
            }
        }
        Ok(())
    }

    fn validate_root_topology(
        &self,
        mut page: PersistentPageRef,
        mut image: BtreeNodeImage,
        level: u16,
        recoverable: bool,
        reachable: &mut BTreeSet<(u32, u64)>,
        ancestors: &mut BTreeSet<(u32, u64)>,
    ) -> Result<()> {
        let mut chain = BTreeSet::new();
        chain.insert(page_key(page));
        while image.right_sibling != PersistentPageRef::NONE {
            if !recoverable {
                return_errno_with_message!(Errno::EINVAL, "unlisted B-tree root sibling");
            }
            let lower = image.high_key;
            page = image.right_sibling;
            if !chain.insert(page_key(page)) {
                return_errno_with_message!(Errno::EINVAL, "B-tree sibling cycle");
            }
            image = self.validated_image(page)?;
            if image.level != level || image.high_key <= lower {
                return_errno_with_message!(Errno::EINVAL, "invalid B-tree sibling topology");
            }
            self.validate_node_contents(
                page,
                level,
                Some(lower),
                u64::MAX,
                recoverable,
                reachable,
                ancestors,
            )?;
        }
        if image.high_key != u64::MAX {
            return_errno_with_message!(Errno::EINVAL, "invalid B-tree root sibling topology");
        }
        Ok(())
    }
}

fn page_key(reference: PersistentPageRef) -> (u32, u64) {
    (reference.slot, reference.generation)
}

fn entry_for(record: &PersistentWalRecord) -> LeafEntry {
    LeafEntry {
        page_index: record.page_index,
        page: record.new_page,
        ssd_backing_block: record.ssd_backing_block,
    }
}
fn selected_image(frame: &BtreeNodeFrame) -> Result<&BtreeNodeImage> {
    if frame.active_slot > 1 || frame.images[frame.active_slot as usize].generation == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid selected B-tree image");
    }
    Ok(&frame.images[frame.active_slot as usize])
}
fn validate_node(frame: &BtreeNodeFrame, fs_id: u64, inode: u64) -> Result<()> {
    if frame.fs_id != fs_id || frame.inode != inode {
        return_errno_with_message!(Errno::EINVAL, "B-tree node owner mismatch");
    }
    let image = selected_image(frame)?;
    if image.entry_count as usize > BTREE_NODE_ENTRY_CAPACITY
        || image.entry_count == 0 && image.level != 0
    {
        return_errno_with_message!(Errno::EINVAL, "invalid B-tree entry count");
    }
    if image.right_sibling == PersistentPageRef::NONE && image.high_key != u64::MAX {
        return_errno_with_message!(
            Errno::EINVAL,
            "terminal B-tree node must have maximum high key"
        );
    }
    Ok(())
}
