// SPDX-License-Identifier: MPL-2.0

//! Reconstruction of volatile RNVPC indexes from committed control-image entries.

#[cfg(ktest)]
use super::id::NvPageId;
use super::{
    capacity::PoolClass,
    descriptor::{NvObjectKind, NvObjectState},
    extent_mutation::ExtentMutation,
    frame::{FIXED_INODE_LOGICAL_INDEX, Rnvpc},
    id::NvFrameId,
    index::{PageKey, ReverseExtentRef},
    metadata::{
        AllocationRecord, ExtentDataRef, ExtentNodeRecord, ExtentRecordEntry, InodeRecord,
        PersistentMetadataKind, PersistentObjectRef, SpaceReservationRecord,
    },
    pcache_layout::{InodePcAnchor, PcObjectType, PersistentPageRef, WAL_FRAME_COUNT},
    pcache_store::{PersistentMetadataStore, typed_object_index, typed_object_type},
    persist::PersistEngine,
    write_transaction::{
        INITIAL_ALLOCATION_LOGICAL_INDEX, SPACE_RESERVATION_LOGICAL_INDEX, reconcile_ssd_capacity,
    },
};
use crate::prelude::*;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ObjectIdentity {
    slot: u64,
    allocation_seq: u64,
}

impl ObjectIdentity {
    fn from_ref(reference: PersistentObjectRef) -> Result<Self> {
        Ok(Self {
            slot: reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?,
            allocation_seq: reference.allocation_seq(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct InodeIdentity {
    fs_id: u64,
    inode: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RecoveryProfile {
    pub(crate) total_ns: u64,
    pub(crate) descriptor_scan_ns: u64,
    pub(crate) runtime_rebuild_ns: u64,
    pub(crate) metadata_discovery_ns: u64,
    pub(crate) pending_repair_ns: u64,
    pub(crate) extent_scan_ns: u64,
    pub(crate) reverse_build_ns: u64,
    pub(crate) allocation_scan_ns: u64,
    pub(crate) reclaim_ns: u64,
    pub(crate) data_index_restore_ns: u64,
    pub(crate) inode_roots: usize,
    pub(crate) extent_nodes: usize,
    pub(crate) extent_entries: usize,
    pub(crate) reverse_entries: usize,
}

#[derive(Default)]
struct TraversalStats {
    extent_nodes: usize,
    extent_entries: usize,
}

fn now_ns(enabled: bool) -> u64 {
    if enabled {
        aster_time::read_monotonic_time().as_nanos() as u64
    } else {
        0
    }
}

fn elapsed_ns(enabled: bool, start: u64) -> u64 {
    now_ns(enabled).saturating_sub(start)
}

pub(super) struct RecoveredEntry {
    pub(super) index: usize,
    pub(super) class: PoolClass,
    pub(super) key: PageKey,
    pub(super) version: u64,
    pub(super) allocation_seq: u64,
    pub(super) ssd_backing_block: u64,
    pub(super) state: NvObjectState,
    pub(super) migration_target: bool,
}

pub(super) struct RecoveredDataSet {
    pub(super) selected: BTreeMap<PageKey, RecoveredEntry>,
    pub(super) obsolete: Vec<RecoveredEntry>,
}

pub(super) struct RecoveredScan {
    pub(super) metadata: Vec<RecoveredEntry>,
    pub(super) wal_data: Vec<RecoveredEntry>,
    pub(super) data: RecoveredDataSet,
    pub(super) max_allocation_seq: u64,
}

pub(super) fn select_data_versions(entries: Vec<RecoveredEntry>) -> Result<RecoveredDataSet> {
    let mut selected = BTreeMap::<PageKey, RecoveredEntry>::new();
    let mut obsolete = Vec::new();
    for entry in entries {
        match selected.entry(entry.key) {
            alloc::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(entry);
            }
            alloc::collections::btree_map::Entry::Occupied(mut slot) => {
                if entry.version == slot.get().version {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "duplicate RNVPC data logical version"
                    );
                }
                if entry.version > slot.get().version {
                    obsolete.push(slot.insert(entry));
                } else {
                    obsolete.push(entry);
                }
            }
        }
    }
    Ok(RecoveredDataSet { selected, obsolete })
}

pub(super) fn scan(engine: &PersistEngine, capacity_pages: usize) -> Result<RecoveredScan> {
    engine.read_header()?;
    let mut metadata_identities = BTreeSet::new();
    let mut fixed_roots = BTreeSet::new();
    let mut metadata_entries = Vec::new();
    let mut data_entries = Vec::new();
    let mut max_allocation_seq = 0;
    for index in 0..capacity_pages {
        // 自描述：只靠条目自己的描述符就能知道“这页是谁、数据在哪”，
        // 不需要预建“SSD block → NV object”的全局映射。
        let Some(descriptor) = engine.read_entry(index)? else {
            continue;
        };
        let class = match descriptor.kind()? {
            NvObjectKind::NvPage => PoolClass::Data,
            NvObjectKind::Nvbh => PoolClass::Metadata,
        };
        let key = PageKey::new(descriptor.fs_id, descriptor.inode, descriptor.logical_index);
        let version = descriptor.recovery_id;
        let state = descriptor.state()?;
        max_allocation_seq = max_allocation_seq.max(descriptor.allocation_seq());
        let candidate = RecoveredEntry {
            index,
            class,
            key,
            version,
            allocation_seq: descriptor.allocation_seq(),
            ssd_backing_block: descriptor.ssd_backing_block,
            state,
            migration_target: descriptor.has_migration_target(),
        };
        if class == PoolClass::Data {
            data_entries.push(candidate);
            continue;
        }
        if key.page_index == FIXED_INODE_LOGICAL_INDEX
            && !fixed_roots.insert((key.fs_id, key.inode))
        {
            return_errno_with_message!(Errno::EUCLEAN, "duplicate fixed inode roots");
        }
        if !metadata_identities.insert((key, version)) {
            return_errno_with_message!(Errno::EINVAL, "duplicate RNVPC metadata version");
        }
        metadata_entries.push(candidate);
    }
    let wal_owners: BTreeSet<_> = metadata_entries
        .iter()
        .filter(|entry| typed_object_type(entry.key.page_index).is_some())
        .map(|entry| (entry.key.fs_id, entry.key.inode))
        .collect();
    let mut wal_data = Vec::new();
    let mut rnvpce_data = Vec::new();
    for entry in data_entries {
        if wal_owners.contains(&(entry.key.fs_id, entry.key.inode)) {
            wal_data.push(entry);
        } else {
            rnvpce_data.push(entry);
        }
    }
    Ok(RecoveredScan {
        metadata: metadata_entries,
        wal_data,
        data: select_data_versions(rnvpce_data)?,
        max_allocation_seq,
    })
}

/// Completes persistent local mutations and removes objects that are not
/// reachable from a fixed inode root before the recovered pool is exposed.
pub(super) fn reconcile(pool: &Arc<Rnvpc>) -> Result<()> {
    reconcile_profiled(pool, None)
}

pub(super) fn reconcile_profiled(
    pool: &Arc<Rnvpc>,
    mut profile: Option<&mut RecoveryProfile>,
) -> Result<()> {
    let profiling = profile.is_some();
    let discovery_start = now_ns(profiling);
    let metadata = pool.committed_metadata_refs();
    let mut managed = BTreeSet::new();
    let mut rooted = BTreeSet::new();
    let mut pcache_owners = BTreeSet::new();

    for reference in metadata {
        let key = pool.persistent_metadata_key(reference)?;
        let owner = InodeIdentity {
            fs_id: key.fs_id,
            inode: key.inode,
        };
        // rNVPC uses a distinct typed payload schema. Never decode or sweep it
        // as an rNVPCe soft-update object.
        if typed_object_type(key.page_index).is_some() {
            pcache_owners.insert(owner);
            continue;
        }
        match pool.persistent_metadata_kind(reference)? {
            PersistentMetadataKind::Inode if key.page_index == FIXED_INODE_LOGICAL_INDEX => {
                managed.insert(owner);
                rooted.insert(owner);
            }
            PersistentMetadataKind::Allocation
                if key.page_index == INITIAL_ALLOCATION_LOGICAL_INDEX =>
            {
                // These records also identify a write/mutation prefix whose
                // objects must be reclaimed if inode publication never became durable.
                managed.insert(owner);
            }
            PersistentMetadataKind::SpaceReservation
                if key.page_index == SPACE_RESERVATION_LOGICAL_INDEX =>
            {
                managed.insert(owner);
            }
            PersistentMetadataKind::PendingExtentMutation => {
                managed.insert(owner);
            }
            _ => {}
        }
    }
    if let Some(profile) = profile.as_deref_mut() {
        profile.metadata_discovery_ns = elapsed_ns(true, discovery_start);
        profile.inode_roots = rooted.len();
    }

    if managed.iter().any(|owner| pcache_owners.contains(owner)) {
        return_errno_with_message!(
            Errno::EUCLEAN,
            "inode has both WAL and soft-update ownership"
        );
    }
    if managed.is_empty() {
        // Compatibility for low-level descriptor tests and legacy pools that
        // predate fixed inode roots.
        return Ok(());
    }

    let repair_start = now_ns(profiling);
    for owner in rooted.iter().copied() {
        let mutation = ExtentMutation::new(pool.clone(), owner.fs_id, owner.inode)?;
        while let Some(pending) = mutation.pending_split()? {
            mutation.complete_split(pending, None, None, None)?;
        }
    }
    if let Some(profile) = profile.as_deref_mut() {
        profile.pending_repair_ns = elapsed_ns(true, repair_start);
    }

    let mut live = BTreeSet::new();
    let mut reachable_ssd_blocks = BTreeSet::new();
    let mut reachable_data = Vec::new();
    let mut reachable_data_keys = BTreeMap::new();
    let mut reachable_node_routes = BTreeMap::new();
    let mut reverse_candidates = Vec::new();
    let mut traversal_stats = TraversalStats::default();
    let extent_start = now_ns(profiling);
    for owner in rooted.iter().copied() {
        let inode_ref = pool
            .fixed_inode_ref(owner.fs_id, owner.inode)?
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        live.insert(ObjectIdentity::from_ref(inode_ref)?);
        let inode = pool.read_authoritative_metadata::<InodeRecord>(inode_ref)?;
        if inode.header().fs_id() != owner.fs_id || inode.header().inode() != owner.inode {
            return_errno_with_message!(Errno::EUCLEAN, "fixed inode owner is corrupt");
        }
        trace_extent_tree(
            pool,
            owner,
            inode.extent_root(),
            &mut live,
            &mut reachable_ssd_blocks,
            &mut reachable_data,
            &mut reachable_data_keys,
            &mut reachable_node_routes,
            &mut reverse_candidates,
            &mut traversal_stats,
        )?;
    }

    // In image v5, selected data descriptors are the persistent redirect
    // authority and intentionally do not appear in SSD-only extent leaves.
    // Keep descriptors only for owners with a durable inode root; orphaned
    // publications from an interrupted first write remain reclaimable.
    for (key, page) in pool.committed_data_pages() {
        let owner = InodeIdentity {
            fs_id: key.fs_id,
            inode: key.inode,
        };
        if !rooted.contains(&owner) {
            continue;
        }
        let reference = pool.persistent_data_ref(page)?;
        let identity = ObjectIdentity::from_ref(reference)?;
        if reachable_data_keys
            .get(&key)
            .is_some_and(|existing| *existing != identity)
        {
            return_errno_with_message!(
                Errno::EUCLEAN,
                "extent and redirect select different NV generations"
            );
        }
        reachable_data_keys.insert(key, identity);
        if live.insert(identity) {
            reachable_data.push(reference);
        }
        let ssd_target = pool.persistent_data_ssd_backing(reference)?;
        if ssd_target != 0 {
            reachable_ssd_blocks.insert((owner, ssd_target));
        }
    }
    if let Some(profile) = profile.as_deref_mut() {
        profile.extent_scan_ns = elapsed_ns(true, extent_start);
        profile.extent_nodes = traversal_stats.extent_nodes;
        profile.extent_entries = traversal_stats.extent_entries;
    }

    #[cfg(ktest)]
    {
        let reverse_start = now_ns(profiling);
        let mut installed = BTreeSet::new();
        for (data, locator) in reverse_candidates {
            let identity = ObjectIdentity::from_ref(data)?;
            if !installed.insert(identity) {
                return_errno_with_message!(Errno::EUCLEAN, "NVPage has multiple extent owners");
            }
            pool.replace_reverse(data, locator)?;
            let page = NvPageId::try_from(
                usize::try_from(data.slot().ok_or_else(|| Error::new(Errno::EUCLEAN))?)
                    .map_err(|_| Error::new(Errno::EOVERFLOW))?,
            )?;
            let _ = pool.enqueue_dirty_nvpage(page, data.allocation_seq())?;
        }
        if let Some(profile) = profile.as_deref_mut() {
            profile.reverse_build_ns = elapsed_ns(true, reverse_start);
            profile.reverse_entries = installed.len();
        }
    }

    // Repair the aggregate count from the recovered forward graph. A fresh
    // NV page has no target yet; an interrupted migration owns its newly
    // allocated target through the descriptor flag. An NV overwrite of an
    // already allocated SSD block has a nonzero backing block without that
    // flag and therefore consumes no reservation credit.
    let mut expected_reservations = BTreeMap::<InodeIdentity, u64>::new();
    for reference in &reachable_data {
        let key = pool.validate_persistent_data_ref(*reference)?;
        if pool.persistent_data_ssd_backing(*reference)? == 0
            || (pool.persistent_data_has_migration_target(*reference)?
                && !pool.persistent_data_migration_credit_released(*reference)?)
        {
            let owner = InodeIdentity {
                fs_id: key.fs_id,
                inode: key.inode,
            };
            let count = expected_reservations.entry(owner).or_default();
            *count = count
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        }
    }
    for owner in rooted.iter().copied() {
        reconcile_ssd_capacity(
            pool,
            owner.fs_id,
            owner.inode,
            expected_reservations.get(&owner).copied().unwrap_or(0),
        )?;
    }

    // Count-only reservations are one-per-inode roots. They remain live while
    // the inode exists; their outstanding count is rebuilt by Task 6E before
    // foreground admission starts.
    for reference in pool.committed_metadata_refs() {
        let key = pool.persistent_metadata_key(reference)?;
        if typed_object_type(key.page_index).is_some() {
            continue;
        }
        if pool.persistent_metadata_kind(reference)? != PersistentMetadataKind::SpaceReservation {
            continue;
        }
        let key = pool.persistent_metadata_key(reference)?;
        let owner = InodeIdentity {
            fs_id: key.fs_id,
            inode: key.inode,
        };
        if rooted.contains(&owner) {
            let reservation =
                pool.read_authoritative_metadata::<SpaceReservationRecord>(reference)?;
            if reservation.outstanding_blocks() != 0 {
                live.insert(ObjectIdentity::from_ref(reference)?);
            }
        }
    }

    // Legacy allocation records are referenced implicitly by reachable NVPages'
    // final SSD homes. They remain live until rTIO has made the corresponding
    // SSD allocation metadata durable and reclaimed the obsolete NV mapping.
    let allocation_start = now_ns(profiling);
    for reference in pool.committed_metadata_refs() {
        let key = pool.persistent_metadata_key(reference)?;
        if typed_object_type(key.page_index).is_some() {
            continue;
        }
        if pool.persistent_metadata_kind(reference)? != PersistentMetadataKind::Allocation {
            continue;
        }
        let key = pool.persistent_metadata_key(reference)?;
        let owner = InodeIdentity {
            fs_id: key.fs_id,
            inode: key.inode,
        };
        if !rooted.contains(&owner) {
            continue;
        }
        let allocation = pool.read_authoritative_metadata::<AllocationRecord>(reference)?;
        let last = allocation
            .first_block()
            .checked_add(allocation.block_count() as u64 - 1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if reachable_ssd_blocks
            .range((owner, allocation.first_block())..=(owner, last))
            .next()
            .is_some()
        {
            live.insert(ObjectIdentity::from_ref(reference)?);
        }
    }
    if let Some(profile) = profile.as_deref_mut() {
        profile.allocation_scan_ns = elapsed_ns(true, allocation_start);
    }

    let reclaim_start = now_ns(profiling);
    reclaim_unreachable(pool, &managed, &live)?;
    if let Some(profile) = profile.as_deref_mut() {
        profile.reclaim_ns = elapsed_ns(true, reclaim_start);
    }

    // Reclaim removes PageIndex entries by logical key. If an abandoned old
    // generation and a reachable replacement share a key, restore the exact
    // generation selected by the persistent extent graph after reclamation.
    let restore_start = now_ns(profiling);
    for reference in reachable_data {
        pool.recover_data_index_entry(reference)?;
    }
    if let Some(profile) = profile.as_deref_mut() {
        profile.data_index_restore_ns = elapsed_ns(true, restore_start);
    }
    Ok(())
}

fn trace_extent_tree(
    pool: &Arc<Rnvpc>,
    owner: InodeIdentity,
    root: PersistentObjectRef,
    live: &mut BTreeSet<ObjectIdentity>,
    reachable_ssd_blocks: &mut BTreeSet<(InodeIdentity, u64)>,
    reachable_data: &mut Vec<PersistentObjectRef>,
    reachable_data_keys: &mut BTreeMap<PageKey, ObjectIdentity>,
    reachable_node_routes: &mut BTreeMap<ObjectIdentity, (Option<u64>, Option<u64>)>,
    reverse_candidates: &mut Vec<(PersistentObjectRef, ReverseExtentRef)>,
    stats: &mut TraversalStats,
) -> Result<()> {
    if root.is_none() {
        return Ok(());
    }

    let mut pending = vec![(root, None, None, None)];
    while let Some((reference, expected_level, lower_bound, upper_bound)) = pending.pop() {
        if pool.persistent_metadata_kind(reference)? != PersistentMetadataKind::ExtentNode {
            return_errno_with_message!(
                Errno::EUCLEAN,
                "extent reference targets non-extent metadata"
            );
        }
        let node = pool.read_authoritative_metadata_boxed::<ExtentNodeRecord>(reference)?;
        if node.header().fs_id() != owner.fs_id
            || node.header().inode() != owner.inode
            || expected_level.is_some_and(|level| node.level() != level)
        {
            return_errno_with_message!(Errno::EUCLEAN, "extent tree ownership or depth is corrupt");
        }
        let identity = ObjectIdentity::from_ref(reference)?;
        let route = (lower_bound, upper_bound);
        if reachable_node_routes
            .get(&identity)
            .is_some_and(|existing| *existing != route)
        {
            return_errno_with_message!(Errno::EUCLEAN, "extent node has conflicting routes");
        }
        reachable_node_routes.insert(identity, route);
        if !live.insert(identity) {
            continue;
        }
        stats.extent_nodes += 1;

        if let Some(sibling_slot) = node.right_sibling_slot() {
            let sibling_ref = pool.persistent_metadata_ref_from_slot(sibling_slot)?;
            if sibling_ref == reference {
                return_errno_with_message!(Errno::EUCLEAN, "extent node links to itself");
            }
            let sibling =
                pool.read_authoritative_metadata_boxed::<ExtentNodeRecord>(sibling_ref)?;
            if sibling.header().fs_id() != owner.fs_id
                || sibling.header().inode() != owner.inode
                || sibling.level() != node.level()
            {
                return_errno_with_message!(Errno::EUCLEAN, "extent sibling identity is corrupt");
            }
            let sibling_boundary = sibling
                .entries()?
                .first()
                .map(ExtentRecordEntry::logical_block)
                .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
            // Once the parent route is durable, this node's upper bound equals
            // the sibling boundary and the parent traversal owns that edge.
            // Before then, the B-link is the sibling's sole reachability path.
            if upper_bound.is_none_or(|upper| sibling_boundary < upper) {
                pending.push((
                    sibling_ref,
                    Some(node.level()),
                    Some(sibling_boundary),
                    upper_bound,
                ));
            }
        }

        if node.level() == 0 {
            for (entry_index, entry) in node.entries()?.iter().enumerate() {
                stats.extent_entries += 1;
                let extent_end = entry
                    .logical_block()
                    .checked_add(entry.length() as u64)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                if lower_bound.is_some_and(|lower| entry.logical_block() < lower)
                    || upper_bound.is_some_and(|upper| extent_end > upper)
                {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "extent leaf escapes its parent route"
                    );
                }
                if matches!(entry.data_ref()?, ExtentDataRef::Nv(_)) {
                    for offset in 0..u64::from(entry.length()) {
                        let logical_block = entry
                            .logical_block()
                            .checked_add(offset)
                            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                        let data_ref =
                            pool.persistent_data_ref_from_slot(entry.nv_slot_at(logical_block)?)?;
                        let key = pool.validate_persistent_data_ref(data_ref)?;
                        if key.fs_id != owner.fs_id
                            || key.inode != owner.inode
                            || key.page_index != logical_block
                        {
                            return_errno_with_message!(
                                Errno::EUCLEAN,
                                "NV extent data identity is corrupt"
                            );
                        }
                        let identity = ObjectIdentity::from_ref(data_ref)?;
                        if reachable_data_keys
                            .get(&key)
                            .is_some_and(|existing| *existing != identity)
                        {
                            return_errno_with_message!(
                                Errno::EUCLEAN,
                                "multiple reachable NV generations map one logical page"
                            );
                        }
                        reachable_data_keys.insert(key, identity);
                        if live.insert(identity) {
                            reachable_data.push(data_ref);
                        }
                        reverse_candidates.push((
                            data_ref,
                            ReverseExtentRef::new(
                                reference.slot().ok_or_else(|| Error::new(Errno::EUCLEAN))?,
                                entry_index,
                            )?,
                        ));
                        reachable_ssd_blocks
                            .insert((owner, pool.persistent_data_ssd_backing(data_ref)?));
                    }
                }
            }
        } else {
            let entries = node.entries()?;
            stats.extent_entries += entries.len();
            for (index, entry) in entries.iter().enumerate() {
                let child_lower = entry.logical_block();
                let child_upper = entries
                    .get(index + 1)
                    .map(ExtentRecordEntry::logical_block)
                    .or(upper_bound);
                if lower_bound.is_some_and(|lower| child_lower < lower)
                    || upper_bound.is_some_and(|upper| child_lower >= upper)
                    || child_upper.is_some_and(|upper| child_lower >= upper)
                {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "extent child route escapes its parent"
                    );
                }
                let child = entry
                    .child_slot()
                    .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
                pending.push((
                    pool.persistent_metadata_ref_from_slot(child)?,
                    Some(node.level() - 1),
                    Some(child_lower),
                    child_upper,
                ));
            }
        }
    }
    Ok(())
}

fn reclaim_unreachable(
    pool: &Arc<Rnvpc>,
    managed: &BTreeSet<InodeIdentity>,
    live: &BTreeSet<ObjectIdentity>,
) -> Result<()> {
    // Remove unreachable metadata first. Otherwise an abandoned extent node
    // would conservatively keep its equally abandoned NVPage alive.
    for reference in pool.committed_metadata_refs() {
        let key = pool.persistent_metadata_key(reference)?;
        let owner = InodeIdentity {
            fs_id: key.fs_id,
            inode: key.inode,
        };
        if !managed.contains(&owner) || live.contains(&ObjectIdentity::from_ref(reference)?) {
            continue;
        }
        let slot = usize::try_from(reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let frame = NvFrameId::try_from(slot).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        pool.clear_frame(frame, None)?;
        pool.free_frame(frame)?;
    }

    for (key, page) in pool.committed_data_pages() {
        let owner = InodeIdentity {
            fs_id: key.fs_id,
            inode: key.inode,
        };
        if !managed.contains(&owner) {
            continue;
        }
        let reference = pool.persistent_data_ref(page)?;
        if live.contains(&ObjectIdentity::from_ref(reference)?) {
            continue;
        }
        pool.clear_data_page(page, None)?;
        pool.free_data_page(page)?;
    }
    Ok(())
}

/// Typed page-cache state held aside from the ordinary DRAM index until the
/// corresponding inode has recovered its B-link tree and WAL.
#[derive(Clone)]
pub(crate) struct RecoveredInodePcache {
    pub(crate) anchor_ref: PersistentPageRef,
    pub(crate) anchor: InodePcAnchor,
    pub(crate) wal_frames: [PersistentPageRef; WAL_FRAME_COUNT],
    pub(crate) node_frames: BTreeMap<u64, PersistentPageRef>,
}

pub(crate) type RecoveredPcacheCatalog = BTreeMap<(u64, u64), RecoveredInodePcache>;

pub(super) fn pcache_catalog(pool: Arc<super::frame::Rnvpc>) -> Result<RecoveredPcacheCatalog> {
    let store = PersistentMetadataStore::new(pool.clone());
    let mut catalog = BTreeMap::new();
    for (key, reference) in pool.committed_typed_metadata() {
        if key.page_index != typed_object_index(PcObjectType::Anchor, 0)? {
            continue;
        }
        store.validate_typed_ref(reference, key.fs_id, key.inode, PcObjectType::Anchor, 0)?;
        let anchor = store.read::<InodePcAnchor>(reference)?;
        let replace =
            catalog
                .get(&(key.fs_id, key.inode))
                .is_none_or(|old: &RecoveredInodePcache| {
                    old.anchor.images[0].generation < anchor.images[0].generation
                });
        if replace {
            catalog.insert(
                (key.fs_id, key.inode),
                RecoveredInodePcache {
                    anchor_ref: reference,
                    anchor,
                    wal_frames: [PersistentPageRef::NONE; WAL_FRAME_COUNT],
                    node_frames: BTreeMap::new(),
                },
            );
        }
    }
    for ((fs_id, inode), recovered) in catalog.iter_mut() {
        for (logical_index, reference) in pool.committed_typed_metadata_for(*fs_id, *inode) {
            match typed_object_type(logical_index) {
                Some(PcObjectType::WalFrame) => {
                    let index =
                        super::pcache_store::typed_object_local_index(logical_index) as usize;
                    if index >= WAL_FRAME_COUNT
                        || recovered.wal_frames[index] != PersistentPageRef::NONE
                    {
                        return_errno_with_message!(Errno::EINVAL, "duplicate persistent WAL frame");
                    }
                    store.validate_typed_ref(
                        reference,
                        *fs_id,
                        *inode,
                        PcObjectType::WalFrame,
                        index as u64,
                    )?;
                    recovered.wal_frames[index] = reference;
                }
                Some(PcObjectType::BtreeNode) => {
                    let node_id = super::pcache_store::typed_object_local_index(logical_index);
                    store.validate_typed_ref(
                        reference,
                        *fs_id,
                        *inode,
                        PcObjectType::BtreeNode,
                        node_id,
                    )?;
                    if recovered.node_frames.insert(node_id, reference).is_some() {
                        return_errno_with_message!(
                            Errno::EINVAL,
                            "duplicate persistent B-tree node"
                        );
                    }
                }
                _ => {}
            }
        }
        if recovered
            .wal_frames
            .iter()
            .any(|frame| *frame == PersistentPageRef::NONE)
            || !recovered
                .node_frames
                .values()
                .any(|frame| *frame == recovered.anchor.images[0].root)
        {
            return_errno_with_message!(
                Errno::EINVAL,
                "anchor names missing persistent page-cache objects"
            );
        }
    }
    Ok(catalog)
}

#[cfg(ktest)]
mod integration_tests {
    use ostd::prelude::ktest;

    use super::{
        super::{
            config::{NvSimMode, RnvpcConfig},
            descriptor::NvDescriptor,
            dram::DramNvBackend,
        },
        *,
    };

    #[ktest]
    fn recovery_rejects_distinct_version_fixed_roots_before_reclamation() {
        let config = RnvpcConfig::from_bytes(
            Some(4 * PAGE_SIZE),
            Some(4 * PAGE_SIZE),
            Some(0),
            Some(NvSimMode::Fast),
        )
        .unwrap()
        .unwrap();
        let backend = Arc::new(DramNvBackend::allocate(config).unwrap());
        let engine = PersistEngine::new(backend, 4).unwrap();
        engine.format().unwrap();
        for slot in 0..2 {
            let descriptor =
                NvDescriptor::for_nvbh(41, 7, FIXED_INODE_LOGICAL_INDEX, 128, 0, slot as u64 + 1)
                    .unwrap()
                    .with_allocation_seq(slot as u64 + 1)
                    .unwrap();
            engine.commit_frame(slot, &descriptor, None).unwrap();
        }
        assert!(matches!(scan(&engine, 4), Err(error) if error.error() == Errno::EUCLEAN));
        assert!(engine.read_entry(0).unwrap().is_some());
        assert!(engine.read_entry(1).unwrap().is_some());
    }
}
