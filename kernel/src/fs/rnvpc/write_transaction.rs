// SPDX-License-Identifier: MPL-2.0

//! Syscall-sized rNVPCe soft-update write transactions.
//!
//! Task 4 supports initial writes and updates. The VFS path supplies final SSD
//! blocks and page-range descriptors; user bytes flow directly into pinned
//! NVPages. The transaction persists allocation ownership before data, data
//! before extent references, and updates the fixed persistent inode only after
//! its extent state is durable. Legacy full-page-image helpers remain only for
//! pre-migration unit coverage and are not used by the VFS path.

use super::{
    data_page::DataPageState,
    extent_mutation::{ExtentMutation, SsdExtentUpdate},
    fault::FaultPoint,
    frame::{MigrationExtentIdentity, Rnvpc},
    index::{PageKey, ReverseExtentRef},
    metadata::{
        AllocationRecord, AuthoritativeMetadataRecord, EXTENT_NODE_ENTRY_CAPACITY, ExtentDataRef,
        ExtentNodeRecord, ExtentRecordEntry, InodeRecord, PersistentObjectRef,
        SpaceReservationRecord,
    },
};
use crate::{
    fs::fs_impls::ext4::{LATENCY_STATS, read_ns},
    prelude::*,
};

pub(super) const INITIAL_ALLOCATION_LOGICAL_INDEX: u64 = (1 << 61) | 1;
pub(super) const SPACE_RESERVATION_LOGICAL_INDEX: u64 = INITIAL_ALLOCATION_LOGICAL_INDEX;
pub(crate) const SSD_MIGRATION_BATCH_PAGES: usize = 128;
const EXTENT_ROOT_LOGICAL_INDEX: u64 = (1 << 61) | 2;
const BOOTSTRAP_EXTENT_LOGICAL_INDEX_TAG: u64 = 1 << 59;

#[derive(Debug)]
pub(crate) struct AuthoritySnapshot<'a> {
    generation: u64,
    extents: &'a [ExtentRecordEntry],
    file_size: u64,
    block_count: u64,
    mode: u32,
    flags: u32,
}

impl<'a> AuthoritySnapshot<'a> {
    pub(crate) fn new(
        generation: u64,
        extents: &'a [ExtentRecordEntry],
        file_size: u64,
        block_count: u64,
        mode: u32,
        flags: u32,
    ) -> Result<Self> {
        if generation == 0 {
            return_errno_with_message!(Errno::EINVAL, "authority generation is zero");
        }
        let mut previous_end = None;
        for extent in extents {
            if !matches!(extent.data_ref()?, ExtentDataRef::Ssd(_)) {
                return_errno_with_message!(Errno::EINVAL, "bootstrap extent is not SSD-backed");
            }
            if previous_end.is_some_and(|end| extent.logical_block() < end) {
                return_errno_with_message!(Errno::EINVAL, "bootstrap extents overlap");
            }
            previous_end = Some(
                extent
                    .logical_block()
                    .checked_add(extent.length() as u64)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            );
        }
        Ok(Self {
            generation,
            extents,
            file_size,
            block_count,
            mode,
            flags,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct InodeAuthority {
    inode: PersistentObjectRef,
    extent_root: PersistentObjectRef,
}

impl InodeAuthority {
    pub(crate) const fn inode(&self) -> PersistentObjectRef {
        self.inode
    }

    pub(crate) const fn extent_root(&self) -> PersistentObjectRef {
        self.extent_root
    }
}

#[derive(Clone, Copy)]
struct BootstrapNode {
    boundary: u64,
    reference: PersistentObjectRef,
}

pub(crate) fn bootstrap_inode_authority(
    pool: Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    snapshot: &AuthoritySnapshot<'_>,
    fault: Option<FaultPoint>,
) -> Result<InodeAuthority> {
    let _publication = pool.begin_publication_for(fs_id, inode)?;
    if let Some(reference) = pool.fixed_inode_ref(fs_id, inode)? {
        let record = pool.read_authoritative_metadata::<InodeRecord>(reference)?;
        if record.header().fs_id() != fs_id || record.header().inode() != inode {
            return_errno_with_message!(Errno::EUCLEAN, "inode authority has the wrong owner");
        }
        return Ok(InodeAuthority {
            inode: reference,
            extent_root: record.extent_root(),
        });
    }

    let extent_root = build_bootstrap_extent_tree(
        &pool,
        fs_id,
        inode,
        snapshot.generation,
        snapshot.extents,
        fault,
    )?;
    let record = InodeRecord::new(
        fs_id,
        inode,
        snapshot.generation,
        extent_root,
        snapshot.file_size,
        snapshot.block_count,
        snapshot.mode,
        snapshot.flags,
    )?;
    let inode_ref = pool.create_fixed_inode(&record, fault)?;
    Ok(InodeAuthority {
        inode: inode_ref,
        extent_root,
    })
}

fn build_bootstrap_extent_tree(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    generation: u64,
    extents: &[ExtentRecordEntry],
    fault: Option<FaultPoint>,
) -> Result<PersistentObjectRef> {
    if extents.is_empty() {
        let key = PageKey::new(fs_id, inode, EXTENT_ROOT_LOGICAL_INDEX);
        let root = ExtentNodeRecord::new(
            fs_id,
            inode,
            generation,
            key.page_index,
            0,
            PersistentObjectRef::NONE,
            PersistentObjectRef::NONE,
            &[],
        )?;
        return persist_or_replace_with_fault(pool, key, &root, fault);
    }
    let mut level = 0u16;
    let mut ordinal = 0u64;
    let mut entries = Vec::from(extents);
    loop {
        if entries.len() <= EXTENT_NODE_ENTRY_CAPACITY {
            let key = PageKey::new(fs_id, inode, EXTENT_ROOT_LOGICAL_INDEX);
            let root = ExtentNodeRecord::new(
                fs_id,
                inode,
                generation,
                key.page_index,
                level,
                PersistentObjectRef::NONE,
                PersistentObjectRef::NONE,
                &entries,
            )?;
            return persist_or_replace_with_fault(pool, key, &root, fault);
        }

        let nodes = persist_bootstrap_level(
            pool,
            fs_id,
            inode,
            generation,
            level,
            &entries,
            &mut ordinal,
            fault,
        )?;
        entries.clear();
        entries
            .try_reserve_exact(nodes.len())
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for node in nodes {
            entries.push(ExtentRecordEntry::child(node.boundary, node.reference)?);
        }
        level = level
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
    }
}

#[allow(clippy::too_many_arguments)]
fn persist_bootstrap_level(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    generation: u64,
    level: u16,
    entries: &[ExtentRecordEntry],
    ordinal: &mut u64,
    fault: Option<FaultPoint>,
) -> Result<Vec<BootstrapNode>> {
    let node_count = entries.len().div_ceil(EXTENT_NODE_ENTRY_CAPACITY);
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(node_count)
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    let mut right_sibling = PersistentObjectRef::NONE;
    for chunk in entries.rchunks(EXTENT_NODE_ENTRY_CAPACITY) {
        if *ordinal >= BOOTSTRAP_EXTENT_LOGICAL_INDEX_TAG {
            return_errno_with_message!(Errno::EOVERFLOW, "bootstrap extent key space exhausted");
        }
        let key = PageKey::new(fs_id, inode, BOOTSTRAP_EXTENT_LOGICAL_INDEX_TAG | *ordinal);
        *ordinal = (*ordinal)
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let node = ExtentNodeRecord::new(
            fs_id,
            inode,
            generation,
            key.page_index,
            level,
            PersistentObjectRef::NONE,
            right_sibling,
            chunk,
        )?;
        let reference = persist_or_replace_with_fault(pool, key, &node, fault)?;
        right_sibling = reference;
        nodes.push(BootstrapNode {
            boundary: chunk[0].logical_block(),
            reference,
        });
    }
    nodes.reverse();
    Ok(nodes)
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct WritePageImage {
    logical_block: u64,
    contents: Vec<u8>,
}

impl WritePageImage {
    pub(crate) const fn logical_block(&self) -> u64 {
        self.logical_block
    }

    pub(crate) fn contents(&self) -> &[u8] {
        &self.contents
    }
}

pub(crate) fn compose_write_page_images<F>(
    file_offset: u64,
    input: &[u8],
    mut load_old_page: F,
) -> Result<Vec<WritePageImage>>
where
    F: FnMut(u64, &mut [u8]) -> Result<()>,
{
    if input.is_empty() {
        return_errno_with_message!(Errno::EINVAL, "write page composition input is empty");
    }
    let write_len = u64::try_from(input.len()).map_err(|_| Error::new(Errno::EOVERFLOW))?;
    let write_end = file_offset
        .checked_add(write_len)
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
    let page_size = PAGE_SIZE as u64;
    let first_page = file_offset / page_size;
    let last_page = write_end.div_ceil(page_size);
    let page_count =
        usize::try_from(last_page - first_page).map_err(|_| Error::new(Errno::EOVERFLOW))?;
    let mut images = Vec::new();
    images
        .try_reserve_exact(page_count)
        .map_err(|_| Error::new(Errno::ENOMEM))?;

    for logical_block in first_page..last_page {
        let page_start = logical_block
            .checked_mul(page_size)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let copy_start = file_offset.max(page_start);
        let copy_end = write_end.min(page_start + page_size);
        let page_copy_start =
            usize::try_from(copy_start - page_start).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let page_copy_end =
            usize::try_from(copy_end - page_start).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let input_start =
            usize::try_from(copy_start - file_offset).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let input_end = input_start
            .checked_add(page_copy_end - page_copy_start)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;

        let mut contents = vec![0u8; PAGE_SIZE];
        if page_copy_start != 0 || page_copy_end != PAGE_SIZE {
            load_old_page(logical_block, &mut contents)?;
        }
        contents[page_copy_start..page_copy_end].copy_from_slice(&input[input_start..input_end]);
        images.push(WritePageImage {
            logical_block,
            contents,
        });
    }
    Ok(images)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct InitialWritePage<'a> {
    logical_block: u64,
    ssd_block: u64,
    contents: &'a [u8],
}

impl<'a> InitialWritePage<'a> {
    pub(crate) fn new(logical_block: u64, ssd_block: u64, contents: &'a [u8]) -> Result<Self> {
        if contents.len() != PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "initial write page is not page-sized");
        }
        Ok(Self {
            logical_block,
            ssd_block,
            contents,
        })
    }
}

#[derive(Debug)]
pub(crate) struct InitialWriteBatch<'a> {
    generation: u64,
    file_offset: u64,
    write_len: u64,
    block_group: u32,
    first_ssd_block: u64,
    pages: &'a [InitialWritePage<'a>],
    file_size: u64,
    block_count: u64,
    mode: u32,
    flags: u32,
}

impl<'a> InitialWriteBatch<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        generation: u64,
        file_offset: u64,
        write_len: u64,
        block_group: u32,
        first_ssd_block: u64,
        pages: &'a [InitialWritePage<'a>],
        file_size: u64,
        block_count: u64,
        mode: u32,
        flags: u32,
    ) -> Result<Self> {
        if generation == 0
            || write_len == 0
            || pages.is_empty()
            || pages.len() > EXTENT_NODE_ENTRY_CAPACITY
        {
            return_errno_with_message!(Errno::EINVAL, "invalid initial write batch");
        }

        let expected_len = u64::try_from(pages.len())
            .map_err(|_| Error::new(Errno::EOVERFLOW))?
            .checked_mul(PAGE_SIZE as u64)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        let first_logical = file_offset / PAGE_SIZE as u64;
        if file_offset % PAGE_SIZE as u64 != 0
            || write_len != expected_len
            || file_offset
                .checked_add(write_len)
                .filter(|end| *end <= file_size)
                .is_none()
        {
            return_errno_with_message!(Errno::EINVAL, "initial write range does not match pages");
        }

        for (index, page) in pages.iter().enumerate() {
            let offset = u64::try_from(index).map_err(|_| Error::new(Errno::EOVERFLOW))?;
            if page.ssd_block
                != first_ssd_block
                    .checked_add(offset)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?
                || page.logical_block
                    != first_logical
                        .checked_add(offset)
                        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?
            {
                return_errno_with_message!(
                    Errno::EINVAL,
                    "initial write pages do not match the logical and SSD ranges"
                );
            }
        }

        Ok(Self {
            generation,
            file_offset,
            write_len,
            block_group,
            first_ssd_block,
            pages,
            file_size,
            block_count,
            mode,
            flags,
        })
    }
}

#[derive(Debug)]
pub(crate) struct CommittedInitialWriteBatch {
    allocation: PersistentObjectRef,
    data_pages: Vec<PersistentObjectRef>,
    extent_root: PersistentObjectRef,
    inode: PersistentObjectRef,
}

impl CommittedInitialWriteBatch {
    pub(crate) const fn allocation(&self) -> PersistentObjectRef {
        self.allocation
    }

    pub(crate) fn data_pages(&self) -> &[PersistentObjectRef] {
        &self.data_pages
    }

    pub(crate) const fn extent_root(&self) -> PersistentObjectRef {
        self.extent_root
    }

    pub(crate) const fn inode(&self) -> PersistentObjectRef {
        self.inode
    }
}

pub(crate) fn commit_initial_write_batch(
    pool: Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    request: &InitialWriteBatch<'_>,
    inode_fault: Option<FaultPoint>,
) -> Result<CommittedInitialWriteBatch> {
    let _publication = pool.begin_publication_for(fs_id, inode)?;
    if pool.fixed_inode_ref(fs_id, inode)?.is_some() {
        return_errno_with_message!(Errno::EEXIST, "fixed inode root already exists");
    }

    // All fallible DRAM allocations and NV data capacity reservation happen
    // before any persistent object is changed.
    let mut data_refs = Vec::new();
    let mut new_mappings = Vec::new();
    data_refs
        .try_reserve_exact(request.pages.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    new_mappings
        .try_reserve_exact(request.pages.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    let mut reservation = pool.reserve_data_pages(request.pages.len())?;

    let allocation = AllocationRecord::new(
        fs_id,
        inode,
        request.generation,
        request.block_group,
        request.first_ssd_block,
        u32::try_from(request.pages.len()).map_err(|_| Error::new(Errno::EOVERFLOW))?,
    )?;
    let allocation_key = PageKey::new(fs_id, inode, INITIAL_ALLOCATION_LOGICAL_INDEX);
    let allocation_ref = persist_or_replace(&pool, allocation_key, &allocation)?;
    for page in request.pages {
        let id = pool.stage_reserved_data_page(&mut reservation, page.contents)?;
        pool.transition_data_page(id, DataPageState::Staged, DataPageState::NvDirty)?;
        pool.replace_data_page_in_owner_at(
            id,
            PageKey::new(fs_id, inode, page.logical_block),
            page.ssd_block,
            None,
        )?;
        let reference = pool.persistent_data_ref(id)?;
        data_refs.push(reference);
        new_mappings.push((page.logical_block, reference));
    }
    let extent_entries = coalesce_nv_extent_entries(&new_mappings)?;

    let extent = ExtentNodeRecord::new(
        fs_id,
        inode,
        request.generation,
        1,
        0,
        PersistentObjectRef::NONE,
        PersistentObjectRef::NONE,
        &extent_entries,
    )?;
    let extent_key = PageKey::new(fs_id, inode, EXTENT_ROOT_LOGICAL_INDEX);
    let extent_ref = persist_or_replace(&pool, extent_key, &extent)?;

    let inode_record = InodeRecord::new(
        fs_id,
        inode,
        request.generation,
        extent_ref,
        request.file_size,
        request.block_count,
        request.mode,
        request.flags,
    )?;
    let inode_ref = pool.create_fixed_inode(&inode_record, inode_fault)?;
    Ok(CommittedInitialWriteBatch {
        allocation: allocation_ref,
        data_pages: data_refs,
        extent_root: extent_ref,
        inode: inode_ref,
    })
}

fn persist_or_replace<T>(pool: &Arc<Rnvpc>, key: PageKey, record: &T) -> Result<PersistentObjectRef>
where
    T: AuthoritativeMetadataRecord,
{
    persist_or_replace_with_fault(pool, key, record, None)
}

fn persist_or_replace_with_fault<T>(
    pool: &Arc<Rnvpc>,
    key: PageKey,
    record: &T,
    fault: Option<FaultPoint>,
) -> Result<PersistentObjectRef>
where
    T: AuthoritativeMetadataRecord,
{
    match pool.current_metadata_ref(key) {
        Some(current) => pool.replace_authoritative_metadata(current, record, fault),
        None => pool.persist_authoritative_metadata(key, record, fault),
    }
}

pub(crate) fn space_reservation(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
) -> Result<Option<SpaceReservationRecord>> {
    let key = PageKey::new(fs_id, inode, SPACE_RESERVATION_LOGICAL_INDEX);
    let Some(reference) = pool.current_metadata_ref(key) else {
        return Ok(None);
    };
    if pool.persistent_metadata_kind(reference)?
        != super::metadata::PersistentMetadataKind::SpaceReservation
    {
        return_errno_with_message!(
            Errno::EUCLEAN,
            "reservation key names another metadata kind"
        );
    }
    Ok(Some(
        pool.read_authoritative_metadata::<SpaceReservationRecord>(reference)?,
    ))
}

/// Persists count-only SSD capacity before a new NV extent can reference the
/// corresponding pages. The caller holds this inode's publication exclusion.
pub(crate) fn reserve_ssd_capacity(
    pool: &Arc<Rnvpc>,
    mutation: &ExtentMutation,
    fs_id: u64,
    inode: u64,
    blocks: u64,
) -> Result<PersistentObjectRef> {
    if blocks == 0 {
        return_errno_with_message!(Errno::EINVAL, "zero SSD reservation requested");
    }
    let started = read_ns();
    let key = PageKey::new(fs_id, inode, SPACE_RESERVATION_LOGICAL_INDEX);
    let existing = pool.current_metadata_ref(key);
    let guard = existing
        .map(|reference| pool.lock_metadata_mutation(reference))
        .transpose()?;
    let current = space_reservation(pool, fs_id, inode)?;
    LATENCY_STATS.fg_reserve_lookup.record_since(started);
    let (generation, reserved, consumed) = match current {
        Some(record) => (
            record
                .header()
                .generation()
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            record
                .reserved_blocks()
                .checked_add(blocks)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            record.consumed_blocks(),
        ),
        None => (1, blocks, 0),
    };
    let record = SpaceReservationRecord::new(fs_id, inode, generation, reserved, consumed)?;
    let started = read_ns();
    let result = match (existing, guard.as_ref()) {
        (Some(reference), Some(guard)) => {
            pool.update_authoritative_metadata_with_guard(guard, reference, &record, None)
        }
        _ => mutation.persist_or_update_control(key, &record, None),
    };
    LATENCY_STATS.fg_reserve_update.record_since(started);
    result
}

pub(crate) fn consume_ssd_capacity(
    pool: &Arc<Rnvpc>,
    _mutation: &ExtentMutation,
    fs_id: u64,
    inode: u64,
    blocks: u64,
) -> Result<PersistentObjectRef> {
    consume_ssd_capacity_locked(pool, fs_id, inode, blocks)
}

pub(super) fn consume_ssd_capacity_locked(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    blocks: u64,
) -> Result<PersistentObjectRef> {
    if blocks == 0 {
        return_errno_with_message!(Errno::EINVAL, "zero SSD reservation consumption");
    }
    let started = read_ns();
    let reference = pool
        .current_metadata_ref(PageKey::new(fs_id, inode, SPACE_RESERVATION_LOGICAL_INDEX))
        .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "missing SSD reservation"))?;
    let lock_started = read_ns();
    let guard = pool.lock_metadata_mutation(reference)?;
    LATENCY_STATS
        .migration_reservation_lock_wait
        .record_since(lock_started);
    let current = space_reservation(pool, fs_id, inode)?
        .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "missing SSD reservation"))?;
    LATENCY_STATS
        .migration_reservation_lookup
        .record_since(started);
    let consumed = current
        .consumed_blocks()
        .checked_add(blocks)
        .filter(|value| *value <= current.reserved_blocks())
        .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "SSD reservation over-consumed"))?;
    let generation = current
        .header()
        .generation()
        .checked_add(1)
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
    let record = SpaceReservationRecord::new(
        fs_id,
        inode,
        generation,
        current.reserved_blocks(),
        consumed,
    )?;
    let started = read_ns();
    let result = pool.update_authoritative_metadata_with_guard(&guard, reference, &record, None);
    LATENCY_STATS
        .migration_reservation_update
        .record_since(started);
    result
}

/// Recovery-only repair for the count reservation. The extent traversal has
/// already established how many mapped NVPages still require newly allocated
/// SSD homes, so rewriting the aggregate to that exact outstanding count is
/// idempotent across every migration crash cut.
pub(crate) fn reconcile_ssd_capacity(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    outstanding: u64,
) -> Result<Option<PersistentObjectRef>> {
    let mutation = ExtentMutation::new(pool.clone(), fs_id, inode)?;
    let current = space_reservation(pool, fs_id, inode)?;
    let Some(current) = current else {
        if outstanding == 0 {
            return Ok(None);
        }
        let record = SpaceReservationRecord::new(fs_id, inode, 1, outstanding, 0)?;
        return mutation
            .persist_or_update_control(
                PageKey::new(fs_id, inode, SPACE_RESERVATION_LOGICAL_INDEX),
                &record,
                None,
            )
            .map(Some);
    };
    if current.outstanding_blocks() == outstanding {
        return Ok(pool.current_metadata_ref(PageKey::new(
            fs_id,
            inode,
            SPACE_RESERVATION_LOGICAL_INDEX,
        )));
    }
    let reserved = current
        .reserved_blocks()
        .max(current.consumed_blocks().saturating_add(outstanding));
    let consumed = reserved
        .checked_sub(outstanding)
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
    let generation = current
        .header()
        .generation()
        .checked_add(1)
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
    let record = SpaceReservationRecord::new(fs_id, inode, generation, reserved, consumed)?;
    mutation
        .persist_or_update_control(
            PageKey::new(fs_id, inode, SPACE_RESERVATION_LOGICAL_INDEX),
            &record,
            None,
        )
        .map(Some)
}

pub(crate) fn outstanding_ssd_capacity(pool: &Arc<Rnvpc>, fs_id: u64) -> Result<u64> {
    let mut total = 0u64;
    for reference in pool.committed_metadata_refs() {
        let key = pool.persistent_metadata_key(reference)?;
        if key.fs_id != fs_id || super::pcache_store::typed_object_type(key.page_index).is_some() {
            continue;
        }
        if pool.persistent_metadata_kind(reference)?
            != super::metadata::PersistentMetadataKind::SpaceReservation
        {
            continue;
        }
        let record = pool.read_authoritative_metadata::<SpaceReservationRecord>(reference)?;
        if record.header().fs_id() == fs_id {
            total = total
                .checked_add(record.outstanding_blocks())
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        }
    }
    Ok(total)
}

const UPDATE_ALLOCATION_LOGICAL_INDEX_TAG: u64 = 1 << 60;
const UPDATE_ALLOCATION_MAX_GENERATION: u64 = (1 << 44) - 1;

#[derive(Clone, Copy, Debug)]
pub(crate) struct WriteAllocation {
    block_group: u32,
    first_ssd_block: u64,
    block_count: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RangeWritePage {
    logical_block: u64,
    ssd_block: u64,
    page_offset: usize,
    len: usize,
    newly_allocated: bool,
}

impl RangeWritePage {
    pub(crate) fn new(
        logical_block: u64,
        ssd_block: u64,
        page_offset: usize,
        len: usize,
        newly_allocated: bool,
    ) -> Result<Self> {
        if len == 0
            || page_offset
                .checked_add(len)
                .is_none_or(|end| end > PAGE_SIZE)
        {
            return_errno_with_message!(Errno::EINVAL, "invalid NVPage write range");
        }
        Ok(Self {
            logical_block,
            ssd_block,
            page_offset,
            len,
            newly_allocated,
        })
    }
}

#[derive(Debug)]
pub(crate) struct RangeWriteBatch<'a> {
    generation: u64,
    file_offset: u64,
    allocations: &'a [WriteAllocation],
    pages: &'a [RangeWritePage],
    old_file_size: u64,
    old_block_count: u64,
    sectors_per_block: u64,
}

impl<'a> RangeWriteBatch<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        generation: u64,
        file_offset: u64,
        write_len: usize,
        allocations: &'a [WriteAllocation],
        pages: &'a [RangeWritePage],
        old_file_size: u64,
        old_block_count: u64,
        sectors_per_block: u64,
    ) -> Result<Self> {
        if generation == 0
            || generation > UPDATE_ALLOCATION_MAX_GENERATION
            || write_len == 0
            || pages.is_empty()
            || sectors_per_block == 0
        {
            return_errno_with_message!(Errno::EINVAL, "invalid range write batch");
        }
        let described = pages.iter().try_fold(0usize, |total, page| {
            total
                .checked_add(page.len)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))
        })?;
        if described != write_len
            || pages[0].logical_block != file_offset / PAGE_SIZE as u64
            || pages[0].page_offset != file_offset as usize % PAGE_SIZE
        {
            return_errno_with_message!(Errno::EINVAL, "write range does not match NVPage spans");
        }
        for pair in pages.windows(2) {
            if pair[1].logical_block != pair[0].logical_block + 1
                || pair[1].page_offset != 0
                || pair[0].page_offset + pair[0].len != PAGE_SIZE
            {
                return_errno_with_message!(Errno::EINVAL, "NVPage spans are not contiguous");
            }
        }
        Ok(Self {
            generation,
            file_offset,
            allocations,
            pages,
            old_file_size,
            old_block_count,
            sectors_per_block,
        })
    }
}

#[derive(Debug)]
pub(crate) struct CommittedRangeWrite {
    copied: usize,
    inode: PersistentObjectRef,
    reserved_blocks: u64,
}

impl CommittedRangeWrite {
    pub(crate) const fn copied(&self) -> usize {
        self.copied
    }

    pub(crate) const fn inode(&self) -> PersistentObjectRef {
        self.inode
    }

    pub(crate) const fn reserved_blocks(&self) -> u64 {
        self.reserved_blocks
    }
}

/// Commits a syscall range without constructing page-sized DRAM images.
///
/// Existing NV pages are changed in place. New pages are initialized while
/// private, optionally from their SSD home block, and become reachable only
/// after their complete contents are durable.
pub(crate) fn commit_range_write<F>(
    pool: Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    request: &RangeWriteBatch<'_>,
    reader: &mut VmReader,
    load_ssd_page: F,
    inode_fault: Option<FaultPoint>,
) -> Result<CommittedRangeWrite>
where
    F: FnMut(u64, &mut super::frame::StagingDataPage) -> Result<()>,
{
    let mutation = ExtentMutation::new(pool.clone(), fs_id, inode)?;
    let pages = range_write_new_pages(&pool, fs_id, inode, request)?;
    let reservation = pool.reserve_data_pages(pages)?;
    commit_range_write_with_mutation(
        pool,
        fs_id,
        inode,
        request,
        reader,
        load_ssd_page,
        inode_fault,
        mutation,
        reservation,
    )
}

/// Counts new data pages before consuming user bytes or SSD credits.
/// The caller holds the inode publication guard through reservation and commit.
pub(crate) fn range_write_new_pages(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    request: &RangeWriteBatch<'_>,
) -> Result<usize> {
    let mut pages = 0;
    for page in request.pages {
        if mapped_nv_page_ref(pool, fs_id, inode, page.logical_block)?.is_none() {
            pages += 1;
        }
    }
    Ok(pages)
}

pub(crate) fn commit_range_write_with_mutation<F>(
    pool: Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    request: &RangeWriteBatch<'_>,
    reader: &mut VmReader,
    mut load_ssd_page: F,
    inode_fault: Option<FaultPoint>,
    mutation: ExtentMutation,
    mut reservation: super::frame::CapacityReservation,
) -> Result<CommittedRangeWrite>
where
    F: FnMut(u64, &mut super::frame::StagingDataPage) -> Result<()>,
{
    let body_started = read_ns();
    let started = read_ns();
    let current_inode_ref =
        authoritative_inode_ref(&pool, fs_id, inode)?.ok_or_else(|| Error::new(Errno::ENOENT))?;
    let current_inode = pool.read_authoritative_metadata::<InodeRecord>(current_inode_ref)?;
    if request.generation <= current_inode.header().generation() {
        return_errno_with_message!(Errno::ESTALE, "write generation is not newer than inode");
    }
    let current_root = current_inode.extent_root();
    LATENCY_STATS.fg_inode_read.record_since(started);

    let started = read_ns();
    let mut existing_refs = Vec::new();
    existing_refs
        .try_reserve_exact(request.pages.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    for (index, page) in request.pages.iter().enumerate() {
        if page.ssd_block != 0
            && request.pages[..index]
                .iter()
                .any(|previous| previous.ssd_block == page.ssd_block)
        {
            return_errno_with_message!(Errno::EINVAL, "write pages alias one SSD block");
        }
        if page.newly_allocated
            && !request.allocations.is_empty()
            && !request.allocations.iter().any(|allocation| {
                allocation
                    .first_ssd_block
                    .checked_add(allocation.block_count as u64)
                    .is_some_and(|end| {
                        page.ssd_block >= allocation.first_ssd_block && page.ssd_block < end
                    })
            })
        {
            return_errno_with_message!(Errno::EINVAL, "new SSD target is not allocated by batch");
        }
        let entry = lookup_tree_extent(&pool, fs_id, inode, current_root, page.logical_block)?;
        let existing = match entry {
            Some(entry) => match entry.data_ref()? {
                ExtentDataRef::Nv(_) => {
                    let reference =
                        pool.persistent_data_ref_from_slot(entry.nv_slot_at(page.logical_block)?)?;
                    if pool.persistent_data_ssd_backing(reference)? != page.ssd_block {
                        return_errno_with_message!(Errno::ESTALE, "NVPage SSD target changed");
                    }
                    Some(reference)
                }
                ExtentDataRef::Ssd(first) => {
                    let block = first
                        .checked_add(page.logical_block - entry.logical_block())
                        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                    if block != page.ssd_block || page.newly_allocated {
                        return_errno_with_message!(Errno::ESTALE, "SSD extent target changed");
                    }
                    None
                }
            },
            None => {
                if !page.newly_allocated {
                    return_errno_with_message!(Errno::ESTALE, "write target lost its allocation");
                }
                None
            }
        };
        existing_refs.push((entry, existing));
    }
    LATENCY_STATS.fg_lookup.record_since(started);

    let started = read_ns();
    let new_page_count = existing_refs
        .iter()
        .filter(|(_, reference)| reference.is_none())
        .count();
    let mut allocation_refs = Vec::new();
    allocation_refs
        .try_reserve_exact(request.allocations.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    if !request.allocations.is_empty() {
        for (index, allocation) in request.allocations.iter().enumerate() {
            let record = AllocationRecord::new(
                fs_id,
                inode,
                request.generation,
                allocation.block_group,
                allocation.first_ssd_block,
                allocation.block_count,
            )?;
            let logical_index = update_allocation_logical_index(request.generation, index)?;
            allocation_refs.push(persist_or_replace(
                &pool,
                PageKey::new(fs_id, inode, logical_index),
                &record,
            )?);
        }
    }

    LATENCY_STATS.fg_allocate.record_since(started);
    let started = read_ns();
    let mut copied = 0usize;
    let mut newly_mapped = 0u64;
    let mut committed_pages = Vec::new();
    committed_pages
        .try_reserve_exact(request.pages.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    let mut pending_new_pages = Vec::new();
    pending_new_pages
        .try_reserve_exact(new_page_count)
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    for (page, (old_entry, existing)) in request.pages.iter().zip(existing_refs) {
        let page_result = (|| -> Result<(usize, Option<PersistentObjectRef>)> {
            if let Some(reference) = existing {
                if pool.validate_persistent_data_ref(reference)?
                    != PageKey::new(fs_id, inode, page.logical_block)
                {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "extent data object identity mismatch"
                    );
                }
                let data = pool.begin_persistent_data_mutation(reference)?;
                let t_copy = read_ns();
                let amount = data.copy_from_reader(page.page_offset, reader, page.len, None)?;
                LATENCY_STATS.fg_copy.record_since(t_copy);
                data.finish();
                Ok((amount, Some(reference)))
            } else {
                let t_init = read_ns();
                let mut staging = pool.begin_reserved_data_page(&mut reservation)?;
                staging.zero_all()?;
                LATENCY_STATS.fg_data_init.record_since(t_init);
                let full_overwrite = page.page_offset == 0 && page.len == PAGE_SIZE;
                if !full_overwrite
                    && let Some(entry) = old_entry
                    && !entry.is_unwritten()
                    && matches!(entry.data_ref()?, ExtentDataRef::Ssd(_))
                {
                    let t_read = read_ns();
                    load_ssd_page(page.ssd_block, &mut staging)?;
                    LATENCY_STATS.fg_ssd_read.record_since(t_read);
                }
                let t_copy = read_ns();
                let amount = staging.copy_from_reader(page.page_offset, reader, page.len)?;
                LATENCY_STATS.fg_copy.record_since(t_copy);
                if amount == 0 {
                    return_errno_with_message!(Errno::EFAULT, "write copied no user bytes");
                }
                let id = staging.finish()?;
                pool.transition_data_page(id, DataPageState::Staged, DataPageState::NvDirty)?;
                pending_new_pages.push((
                    id,
                    PageKey::new(fs_id, inode, page.logical_block),
                    page.ssd_block,
                ));
                if page.newly_allocated {
                    newly_mapped = newly_mapped
                        .checked_add(1)
                        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                }
                Ok((amount, None))
            }
        })();
        let (page_copied, reference) = match page_result {
            Ok(result) => result,
            Err(_) if copied != 0 => break,
            Err(error) => return Err(error),
        };
        if let Some(reference) = reference {
            committed_pages.push(reference);
        }
        copied = copied
            .checked_add(page_copied)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if page_copied != page.len {
            break;
        }
    }
    let t_register = read_ns();
    for batch in pending_new_pages.chunks(128) {
        pool.replace_data_pages_in_owner_at(batch, None)?;
    }
    let mut new_mappings = Vec::new();
    new_mappings
        .try_reserve_exact(pending_new_pages.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    for (id, key, _) in &pending_new_pages {
        let reference = pool.persistent_data_ref(*id)?;
        new_mappings.push((key.page_index, reference));
        committed_pages.push(reference);
    }
    let replacements = coalesce_nv_extent_entries(&new_mappings)?;
    LATENCY_STATS.fg_data_register.record_since(t_register);
    if copied == 0 {
        return_errno_with_message!(Errno::EFAULT, "write copied no user bytes");
    }
    LATENCY_STATS.fg_data.record_since(started);

    let started = read_ns();
    // Count-only SSD credit is the final dependency before a new NV reference
    // becomes reachable. Delaying it until the actual copied prefix is known
    // avoids over-reserving when copy_from_user returns a short write.
    if request.allocations.is_empty() && newly_mapped != 0 {
        allocation_refs.push(reserve_ssd_capacity(
            &pool,
            &mutation,
            fs_id,
            inode,
            newly_mapped,
        )?);
    }

    LATENCY_STATS.fg_reserve.record_since(started);
    let started = read_ns();
    let new_root = if replacements.is_empty() {
        mutation.committed_node(current_root)?
    } else {
        mutation.replace_tree_extents(current_root, &replacements, None)?
    };
    LATENCY_STATS.fg_extent.record_since(started);
    let started = read_ns();
    let visible_end = request
        .file_offset
        .checked_add(copied as u64)
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
    let file_size = request.old_file_size.max(visible_end);
    let block_count = request
        .old_block_count
        .checked_add(
            newly_mapped
                .checked_mul(request.sectors_per_block)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
        )
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
    let inode_ref = mutation.publish_inode_state_with_generation(
        current_inode_ref,
        new_root.reference(),
        &new_root,
        request.generation,
        file_size,
        block_count,
        inode_fault,
    )?;
    LATENCY_STATS.fg_inode_publish.record_since(started);
    let started = read_ns();
    // Local extent mutation installs the reverse locators under the same
    // publication exclusion. Validate and queue the whole copied prefix while
    // holding the pool control lock once.
    pool.enqueue_dirty_nvrefs(&committed_pages)?;
    LATENCY_STATS.fg_enqueue.record_since(started);
    LATENCY_STATS.fg_body.record_since(body_started);
    Ok(CommittedRangeWrite {
        copied,
        inode: inode_ref,
        reserved_blocks: newly_mapped,
    })
}

impl WriteAllocation {
    pub(crate) fn new(block_group: u32, first_ssd_block: u64, block_count: u32) -> Result<Self> {
        if block_count == 0 || first_ssd_block.checked_add(block_count as u64).is_none() {
            return_errno_with_message!(Errno::EINVAL, "invalid write allocation run");
        }
        Ok(Self {
            block_group,
            first_ssd_block,
            block_count,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WritePage<'a> {
    logical_block: u64,
    ssd_block: u64,
    contents: &'a [u8],
}

impl<'a> WritePage<'a> {
    pub(crate) fn new(logical_block: u64, ssd_block: u64, contents: &'a [u8]) -> Result<Self> {
        if contents.len() != PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "write page is not page-sized");
        }
        Ok(Self {
            logical_block,
            ssd_block,
            contents,
        })
    }
}

#[derive(Debug)]
pub(crate) struct WriteBatch<'a> {
    generation: u64,
    file_offset: u64,
    write_len: u64,
    allocations: &'a [WriteAllocation],
    pages: &'a [WritePage<'a>],
    file_size: u64,
    block_count: u64,
}

impl<'a> WriteBatch<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        generation: u64,
        file_offset: u64,
        write_len: u64,
        allocations: &'a [WriteAllocation],
        pages: &'a [WritePage<'a>],
        file_size: u64,
        block_count: u64,
    ) -> Result<Self> {
        if generation == 0
            || generation > UPDATE_ALLOCATION_MAX_GENERATION
            || write_len == 0
            || pages.is_empty()
        {
            return_errno_with_message!(Errno::EINVAL, "invalid write batch");
        }
        let end = file_offset
            .checked_add(write_len)
            .filter(|end| *end <= file_size)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let first_page = file_offset / PAGE_SIZE as u64;
        let last_page = end.div_ceil(PAGE_SIZE as u64);
        if usize::try_from(last_page - first_page).ok() != Some(pages.len()) {
            return_errno_with_message!(
                Errno::EINVAL,
                "write range does not match full page images"
            );
        }
        for (index, page) in pages.iter().enumerate() {
            let expected = first_page
                .checked_add(u64::try_from(index).map_err(|_| Error::new(Errno::EOVERFLOW))?)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            if page.logical_block != expected {
                return_errno_with_message!(
                    Errno::EINVAL,
                    "write pages are not logically contiguous"
                );
            }
        }
        Ok(Self {
            generation,
            file_offset,
            write_len,
            allocations,
            pages,
            file_size,
            block_count,
        })
    }
}

#[derive(Debug)]
pub(crate) struct CommittedUpdateWriteBatch {
    allocations: Vec<PersistentObjectRef>,
    data_pages: Vec<PersistentObjectRef>,
    extent_root: PersistentObjectRef,
    inode: PersistentObjectRef,
}

impl CommittedUpdateWriteBatch {
    pub(crate) fn allocations(&self) -> &[PersistentObjectRef] {
        &self.allocations
    }
    pub(crate) fn data_pages(&self) -> &[PersistentObjectRef] {
        &self.data_pages
    }
    pub(crate) const fn extent_root(&self) -> PersistentObjectRef {
        self.extent_root
    }
    pub(crate) const fn inode(&self) -> PersistentObjectRef {
        self.inode
    }
}

pub(crate) fn commit_write_batch(
    pool: Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    request: &WriteBatch<'_>,
    inode_fault: Option<FaultPoint>,
) -> Result<CommittedUpdateWriteBatch> {
    let mutation = ExtentMutation::new(pool.clone(), fs_id, inode)?;
    let current_inode_ref =
        authoritative_inode_ref(&pool, fs_id, inode)?.ok_or_else(|| Error::new(Errno::ENOENT))?;
    let current_inode = pool.read_authoritative_metadata::<InodeRecord>(current_inode_ref)?;
    if request.generation <= current_inode.header().generation() {
        return_errno_with_message!(Errno::ESTALE, "write generation is not newer than inode");
    }
    let current_root = current_inode.extent_root();
    validate_ssd_targets(&pool, fs_id, inode, current_root, request)?;

    let mut allocation_refs = Vec::new();
    let mut data_refs = Vec::new();
    let mut replacements = Vec::new();
    allocation_refs
        .try_reserve_exact(request.allocations.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    data_refs
        .try_reserve_exact(request.pages.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    replacements
        .try_reserve_exact(request.pages.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    let mut existing_data_refs = Vec::new();
    existing_data_refs
        .try_reserve_exact(request.pages.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    for page in request.pages {
        let existing =
            match lookup_tree_extent(&pool, fs_id, inode, current_root, page.logical_block)? {
                Some(entry) => match entry.data_ref()? {
                    ExtentDataRef::Nv(_) => {
                        let reference = pool
                            .persistent_data_ref_from_slot(entry.nv_slot_at(page.logical_block)?)?;
                        (pool.persistent_data_ssd_backing(reference)? == page.ssd_block)
                            .then_some(reference)
                    }
                    ExtentDataRef::Ssd(_) => None,
                },
                None => None,
            };
        existing_data_refs.push(existing);
    }
    let new_page_count = existing_data_refs
        .iter()
        .filter(|entry| entry.is_none())
        .count();
    let mut reservation = pool.reserve_data_pages(new_page_count)?;
    let mut new_mappings = Vec::new();
    new_mappings
        .try_reserve_exact(new_page_count)
        .map_err(|_| Error::new(Errno::ENOMEM))?;

    for (index, allocation) in request.allocations.iter().enumerate() {
        let record = AllocationRecord::new(
            fs_id,
            inode,
            request.generation,
            allocation.block_group,
            allocation.first_ssd_block,
            allocation.block_count,
        )?;
        let logical_index = update_allocation_logical_index(request.generation, index)?;
        let reference =
            persist_or_replace(&pool, PageKey::new(fs_id, inode, logical_index), &record)?;
        allocation_refs.push(reference);
    }

    for (page, existing) in request.pages.iter().zip(existing_data_refs) {
        let keeps_extent_reference = existing.is_some();
        let reference = if let Some(reference) = existing {
            if pool.validate_persistent_data_ref(reference)?
                != PageKey::new(fs_id, inode, page.logical_block)
            {
                return_errno_with_message!(Errno::EUCLEAN, "extent data object identity mismatch");
            }
            let mutation = pool.begin_persistent_data_mutation(reference)?;
            mutation.write_range(0, page.contents, None)?;
            mutation.finish();
            reference
        } else {
            let id = pool.stage_reserved_data_page(&mut reservation, page.contents)?;
            pool.transition_data_page(id, DataPageState::Staged, DataPageState::NvDirty)?;
            pool.replace_data_page_in_owner_at(
                id,
                PageKey::new(fs_id, inode, page.logical_block),
                page.ssd_block,
                None,
            )?;
            pool.persistent_data_ref(id)?
        };
        data_refs.push(reference);
        if !keeps_extent_reference {
            new_mappings.push((page.logical_block, reference));
        }
    }
    replacements.extend(coalesce_nv_extent_entries(&new_mappings)?);
    let new_root = if replacements.is_empty() {
        mutation.committed_node(current_root)?
    } else {
        mutation.replace_tree_extents(current_root, &replacements, None)?
    };
    let inode_ref = mutation.publish_inode_state_with_generation(
        current_inode_ref,
        new_root.reference(),
        &new_root,
        request.generation,
        request.file_size,
        request.block_count,
        inode_fault,
    )?;
    Ok(CommittedUpdateWriteBatch {
        allocations: allocation_refs,
        data_pages: data_refs,
        extent_root: new_root.reference(),
        inode: inode_ref,
    })
}

fn update_allocation_logical_index(generation: u64, index: usize) -> Result<u64> {
    let ordinal = u64::try_from(index).map_err(|_| Error::new(Errno::EOVERFLOW))?;
    if ordinal > u16::MAX as u64 {
        return_errno_with_message!(Errno::E2BIG, "too many allocation runs in one write");
    }
    Ok(UPDATE_ALLOCATION_LOGICAL_INDEX_TAG | (generation << 16) | ordinal)
}

fn coalesce_nv_extent_entries(
    mappings: &[(u64, PersistentObjectRef)],
) -> Result<Vec<ExtentRecordEntry>> {
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(mappings.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    let mut start = 0;
    while start < mappings.len() {
        let (logical_start, base) = mappings[start];
        if base.is_none() {
            return_errno_with_message!(Errno::EINVAL, "NV extent base reference is null");
        }
        let mut end = start + 1;
        while end < mappings.len() && end - start < SSD_MIGRATION_BATCH_PAGES {
            let offset = u64::try_from(end - start).map_err(|_| Error::new(Errno::EOVERFLOW))?;
            if mappings[end].0
                != logical_start
                    .checked_add(offset)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?
                || mappings[end].1.slot_plus_one()
                    != base
                        .slot_plus_one()
                        .checked_add(offset)
                        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?
            {
                break;
            }
            end += 1;
        }
        entries.push(ExtentRecordEntry::nv_leaf_range(
            logical_start,
            u32::try_from(end - start).map_err(|_| Error::new(Errno::EOVERFLOW))?,
            base.slot_plus_one(),
        )?);
        start = end;
    }
    Ok(entries)
}

fn validate_ssd_targets(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    current_root: PersistentObjectRef,
    request: &WriteBatch<'_>,
) -> Result<()> {
    for (index, page) in request.pages.iter().enumerate() {
        if request.pages[..index]
            .iter()
            .any(|previous| previous.ssd_block == page.ssd_block)
        {
            return_errno_with_message!(Errno::EINVAL, "write pages alias one SSD block");
        }
        let existing_ssd =
            match lookup_tree_extent(pool, fs_id, inode, current_root, page.logical_block)? {
                Some(entry) => Some(match entry.data_ref()? {
                    ExtentDataRef::Ssd(block) => block
                        .checked_add(page.logical_block - entry.logical_block())
                        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
                    ExtentDataRef::Nv(_) => pool.persistent_data_ssd_backing(
                        pool.persistent_data_ref_from_slot(entry.nv_slot_at(page.logical_block)?)?,
                    )?,
                }),
                None => None,
            };
        let reuses_existing = existing_ssd == Some(page.ssd_block);
        let allocated_by_batch = request.allocations.iter().any(|allocation| {
            allocation
                .first_ssd_block
                .checked_add(allocation.block_count as u64)
                .is_some_and(|end| {
                    page.ssd_block >= allocation.first_ssd_block && page.ssd_block < end
                })
        });
        if !reuses_existing && !allocated_by_batch {
            return_errno_with_message!(
                Errno::EINVAL,
                "new SSD target is not owned by the write batch"
            );
        }
    }
    for (index, allocation) in request.allocations.iter().enumerate() {
        let end = allocation
            .first_ssd_block
            .checked_add(allocation.block_count as u64)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        for previous in &request.allocations[..index] {
            let previous_end = previous
                .first_ssd_block
                .checked_add(previous.block_count as u64)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            if allocation.first_ssd_block < previous_end && previous.first_ssd_block < end {
                return_errno_with_message!(Errno::EINVAL, "write allocation runs overlap");
            }
        }
    }
    Ok(())
}

fn authoritative_inode_ref(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
) -> Result<Option<PersistentObjectRef>> {
    pool.fixed_inode_ref(fs_id, inode)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AuthoritativeInodeState {
    generation: u64,
    file_size: u64,
    block_count: u64,
    mode: u32,
    flags: u32,
}

impl AuthoritativeInodeState {
    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) const fn file_size(&self) -> u64 {
        self.file_size
    }

    pub(crate) const fn block_count(&self) -> u64 {
        self.block_count
    }

    pub(crate) const fn mode(&self) -> u32 {
        self.mode
    }

    pub(crate) const fn flags(&self) -> u32 {
        self.flags
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CurrentPageMapping {
    ssd_block: u64,
    written: bool,
    in_flight: bool,
}

impl CurrentPageMapping {
    pub(crate) const fn ssd_block(&self) -> u64 {
        self.ssd_block
    }

    pub(crate) const fn is_written(&self) -> bool {
        self.written
    }

    pub(crate) const fn is_in_flight(&self) -> bool {
        self.in_flight
    }
}

pub(crate) fn authoritative_inode_state(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
) -> Result<Option<AuthoritativeInodeState>> {
    let Some(reference) = authoritative_inode_ref(pool, fs_id, inode)? else {
        return Ok(None);
    };
    let record = pool.read_authoritative_metadata::<InodeRecord>(reference)?;
    Ok(Some(AuthoritativeInodeState {
        generation: record.header().generation(),
        file_size: record.file_size(),
        block_count: record.block_count(),
        mode: record.mode(),
        flags: record.flags(),
    }))
}

pub(crate) fn mapped_nv_page_ref(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    logical_block: u64,
) -> Result<Option<PersistentObjectRef>> {
    let Some(inode_ref) = authoritative_inode_ref(pool, fs_id, inode)? else {
        return Ok(None);
    };
    let inode_record = pool.read_authoritative_metadata::<InodeRecord>(inode_ref)?;
    if inode_record.extent_root().is_none() {
        return Ok(None);
    }
    let Some(entry) = lookup_tree_extent(
        pool,
        fs_id,
        inode,
        inode_record.extent_root(),
        logical_block,
    )?
    else {
        return Ok(None);
    };
    match entry.data_ref()? {
        ExtentDataRef::Nv(_) => {
            let reference = pool.persistent_data_ref_from_slot(entry.nv_slot_at(logical_block)?)?;
            if pool.validate_persistent_data_ref(reference)?
                != PageKey::new(fs_id, inode, logical_block)
            {
                return_errno_with_message!(Errno::EUCLEAN, "extent data object identity mismatch");
            }
            Ok(Some(reference))
        }
        ExtentDataRef::Ssd(_) => Ok(None),
    }
}

/// Returns true only while the inode's extent tree has no recoverable local
/// mutation in progress. Acquiring publication authority also excludes a
/// concurrent foreground tree update during this check.
pub(crate) fn persistent_extent_is_stable(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
) -> Result<bool> {
    let mutation = ExtentMutation::new(pool.clone(), fs_id, inode)?;
    Ok(mutation.pending_split()?.is_none())
}

/// Visits SSD-selected leaf entries while the caller excludes inode publication.
/// This is a metadata checkpoint path, never a data-completion scan.
pub(crate) fn visit_ssd_extents<F>(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(ExtentRecordEntry) -> Result<()>,
{
    let Some(reference) = authoritative_inode_ref(pool, fs_id, inode)? else {
        return Ok(());
    };
    let record = pool.read_authoritative_metadata::<InodeRecord>(reference)?;
    if record.extent_root().is_none() {
        return Ok(());
    }
    let mut pending = vec![(record.extent_root(), None)];
    let mut visited_slots = BTreeSet::new();
    while let Some((reference, expected_level)) = pending.pop() {
        let slot = reference.slot().ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        if !visited_slots.insert(slot) {
            continue;
        }
        let node = pool.read_authoritative_metadata_boxed::<ExtentNodeRecord>(reference)?;
        if node.header().fs_id() != fs_id
            || node.header().inode() != inode
            || expected_level.is_some_and(|level| node.level() != level)
        {
            return_errno_with_message!(Errno::EUCLEAN, "invalid checkpoint extent node");
        }
        if let Some(sibling_slot) = node.right_sibling_slot() {
            pending.push((
                pool.persistent_metadata_ref_from_slot(sibling_slot)?,
                Some(node.level()),
            ));
        }
        for entry in node.entries()?.iter().rev() {
            if node.level() == 0 {
                if matches!(entry.data_ref()?, ExtentDataRef::Ssd(_)) && !entry.is_unwritten() {
                    visit(*entry)?;
                }
            } else {
                let child = entry
                    .child_slot()
                    .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
                pending.push((
                    pool.persistent_metadata_ref_from_slot(child)?,
                    Some(node.level() - 1),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn current_page_mapping(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    logical_block: u64,
) -> Result<Option<CurrentPageMapping>> {
    let Some(inode_ref) = authoritative_inode_ref(pool, fs_id, inode)? else {
        return Ok(None);
    };
    let inode_record = pool.read_authoritative_metadata::<InodeRecord>(inode_ref)?;
    if inode_record.extent_root().is_none() {
        return Ok(None);
    }
    let Some(entry) = lookup_tree_extent(
        pool,
        fs_id,
        inode,
        inode_record.extent_root(),
        logical_block,
    )?
    else {
        return Ok(None);
    };
    let (ssd_block, in_flight) = match entry.data_ref()? {
        ExtentDataRef::Ssd(first) => (
            first
                .checked_add(logical_block - entry.logical_block())
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            false,
        ),
        ExtentDataRef::Nv(_) => {
            let reference = pool.persistent_data_ref_from_slot(entry.nv_slot_at(logical_block)?)?;
            let slot = usize::try_from(reference.slot().ok_or_else(|| Error::new(Errno::EUCLEAN))?)
                .map_err(|_| Error::new(Errno::EOVERFLOW))?;
            let page =
                super::id::NvPageId::try_from(slot).map_err(|_| Error::new(Errno::EOVERFLOW))?;
            (
                pool.persistent_data_ssd_backing(reference)?,
                pool.data_page_state(page)? == DataPageState::InFlight,
            )
        }
    };
    Ok(Some(CurrentPageMapping {
        ssd_block,
        written: !entry.is_unwritten(),
        in_flight,
    }))
}

/// Publishes one completed NV-to-SSD migration.
///
/// The caller must keep the exact NV page pinned from BIO submission through
/// this call. SSD data is already durable when this function runs. The
/// currently mapped extent is changed from the generation-checked NV reference
/// to its preallocated SSD home block before the caller clears or reclaims the
/// NV frame.
#[cfg(ktest)]
pub(crate) fn migrate_mapped_nv_page_to_ssd(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    logical_block: u64,
    expected: PersistentObjectRef,
    ssd_block: u64,
    fault: Option<FaultPoint>,
) -> Result<()> {
    if expected.is_none() || ssd_block == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid rTIO migration identity");
    }
    let mutation = ExtentMutation::new(pool.clone(), fs_id, inode)?;
    let inode_ref =
        authoritative_inode_ref(pool, fs_id, inode)?.ok_or_else(|| Error::new(Errno::ESTALE))?;
    let inode_record = pool.read_authoritative_metadata::<InodeRecord>(inode_ref)?;
    let current_root = inode_record.extent_root();
    let entry = lookup_tree_extent(pool, fs_id, inode, current_root, logical_block)?
        .ok_or_else(|| Error::new(Errno::ESTALE))?;
    if entry.logical_block() != logical_block
        || entry.length() != 1
        || entry.data_ref()? != ExtentDataRef::Nv(expected.slot_plus_one())
        || pool.validate_persistent_data_ref(expected)? != PageKey::new(fs_id, inode, logical_block)
        || pool.persistent_data_ssd_backing(expected)? != ssd_block
    {
        return_errno_with_message!(Errno::ESTALE, "rTIO migration candidate is stale");
    }

    let replacement = ExtentRecordEntry::leaf(logical_block, 1, ssd_block, entry.is_unwritten())?;
    let new_root = mutation.replace_tree_extents(current_root, &[replacement], fault)?;
    mutation.publish_inode_state_with_generation(
        inode_ref,
        current_root,
        &new_root,
        inode_record
            .header()
            .generation()
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
        inode_record.file_size(),
        inode_record.block_count(),
        fault,
    )?;
    reclaim_unused_allocations(pool, fs_id, inode)?;
    Ok(())
}

/// Single-page test adapter for the production batched migration path.
#[cfg(ktest)]
pub(crate) fn migrate_mapped_nv_page_to_ssd_by_reverse(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    logical_block: u64,
    expected: PersistentObjectRef,
    _submitted_reverse: ReverseExtentRef,
    ssd_block: u64,
    fault: Option<FaultPoint>,
) -> Result<bool> {
    migrate_mapped_nv_pages_to_ssd_by_reverse(
        pool,
        fs_id,
        inode,
        &[SsdMigrationPage {
            logical_block,
            reference: expected,
            ssd_block,
        }],
        fault,
    )?
    .pop()
    .expect("one migration result")
}

#[derive(Clone, Copy)]
pub(crate) struct SsdMigrationPage {
    pub(crate) logical_block: u64,
    pub(crate) reference: PersistentObjectRef,
    pub(crate) ssd_block: u64,
}

/// Completes one claimed contiguous NV extent as a single authority change.
pub(crate) fn migrate_claimed_nv_extent_to_ssd(
    pool: &Arc<Rnvpc>,
    identity: &MigrationExtentIdentity,
    pages: &[SsdMigrationPage],
    fault: Option<FaultPoint>,
) -> Result<Vec<Result<bool>>> {
    if pages.is_empty()
        || pages.len() > SSD_MIGRATION_BATCH_PAGES
        || pages.len() != identity.length() as usize
    {
        return_errno_with_message!(Errno::EINVAL, "invalid claimed extent completion size");
    }
    let mutation =
        ExtentMutation::new_completion(pool.clone(), identity.fs_id(), identity.inode())?;
    let inode_ref = authoritative_inode_ref(pool, identity.fs_id(), identity.inode())?
        .ok_or_else(|| Error::new(Errno::ESTALE))?;
    let inode = pool.read_authoritative_metadata::<InodeRecord>(inode_ref)?;
    let root = inode.extent_root();
    if root.is_none() {
        return_errno_with_message!(Errno::ESTALE, "claimed extent inode has no tree");
    }

    let mut updates = Vec::with_capacity(pages.len());
    let mut identities = BTreeSet::new();
    for page in pages {
        if page.reference.is_none()
            || !identities.insert((page.reference.slot(), page.reference.allocation_seq()))
            || pool.validate_persistent_data_ref(page.reference)?
                != PageKey::new(identity.fs_id(), identity.inode(), page.logical_block)
            || pool.persistent_data_ssd_backing(page.reference)? != page.ssd_block
        {
            return_errno_with_message!(Errno::ESTALE, "claimed extent member changed");
        }
        updates.push(SsdExtentUpdate {
            locator: identity.locator_hint(),
            expected: page.reference,
            logical_block: page.logical_block,
            ssd_block: page.ssd_block,
        });
    }
    let started = read_ns();
    let published = mutation.publish_claimed_ssd_extent(
        root,
        identity.logical_start(),
        identity.length(),
        identity.nv_base_slot_plus_one(),
        &updates,
        fault,
    )?;
    LATENCY_STATS.migration_leaf.record_since(started);

    let references: Vec<_> = pages.iter().map(|page| page.reference).collect();
    let mut outcomes: Vec<Result<bool>> = references.iter().map(|_| Ok(false)).collect();
    let releasable: Result<Vec<_>> = references
        .iter()
        .copied()
        .filter_map(|reference| {
            match (
                pool.persistent_data_has_migration_target(reference),
                pool.persistent_data_migration_credit_released(reference),
            ) {
                (Ok(true), Ok(false)) => Some(Ok(reference)),
                (Ok(_), Ok(_)) => None,
                (Err(error), _) | (_, Err(error)) => Some(Err(error)),
            }
        })
        .collect();
    let releasable = releasable?;
    pool.mark_migration_credits_released(&releasable, None)?;
    for (index, reference) in references.iter().enumerate() {
        outcomes[index] = Ok(releasable.contains(reference));
    }
    let consumed = releasable.len() as u64;
    if consumed != 0
        && let Err(error) =
            consume_ssd_capacity_locked(pool, identity.fs_id(), identity.inode(), consumed)
    {
        warn!("rNVPCe batch reservation ledger update deferred to recovery: {error:?}");
    }
    let _ = published;
    Ok(outcomes)
}

/// Holds one leaf modification right at a time and persists each touched leaf once.
/// Per-page results allow successful leaves to reclaim even if another fails.
#[cfg(ktest)]
pub(crate) fn migrate_mapped_nv_pages_to_ssd_by_reverse(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    pages: &[SsdMigrationPage],
    fault: Option<FaultPoint>,
) -> Result<Vec<Result<bool>>> {
    if pages.is_empty() || pages.len() > SSD_MIGRATION_BATCH_PAGES {
        return_errno_with_message!(Errno::EINVAL, "invalid completion group size");
    }
    let mut outcomes: Vec<Result<bool>> = pages
        .iter()
        .map(|_| Err(Error::new(Errno::ESTALE)))
        .collect();
    let mut extents: BTreeMap<(u64, usize), Vec<(usize, SsdExtentUpdate)>> = BTreeMap::new();
    let mut identities = BTreeSet::new();
    let started = read_ns();
    for (index, page) in pages.iter().enumerate() {
        let validated = (|| {
            if page.reference.is_none()
                || page.ssd_block == 0
                || !identities.insert((page.reference.slot(), page.reference.allocation_seq()))
            {
                return_errno_with_message!(Errno::ESTALE, "rTIO migration identity changed");
            }
            // The leaf helper revalidates this hint after acquiring its node guard.
            pool.validate_migration_and_reverse(
                page.reference,
                PageKey::new(fs_id, inode, page.logical_block),
                page.ssd_block,
            )
        })();
        match validated {
            Ok(locator) => extents
                .entry((locator.leaf_slot(), locator.entry_index()))
                .or_default()
                .push((
                    index,
                    SsdExtentUpdate {
                        locator,
                        expected: page.reference,
                        logical_block: page.logical_block,
                        ssd_block: page.ssd_block,
                    },
                )),
            Err(error) => outcomes[index] = Err(error),
        }
    }
    LATENCY_STATS.migration_validate.record_since(started);
    let mut consumed = 0;
    let mut credits = Vec::new();
    for group in extents.values() {
        let started = read_ns();
        let updates: Vec<_> = group.iter().map(|(_, update)| *update).collect();
        let profile_start = super::batch_metrics::now();
        let published =
            ExtentMutation::publish_ssd_extents_locked(pool, fs_id, inode, &updates, fault);
        super::batch_metrics::elapsed(super::batch_metrics::Phase::LeafPublish, profile_start);
        LATENCY_STATS.migration_leaf.record_since(started);
        let started = read_ns();
        for (index, update) in group {
            let release = (|| {
                match published {
                    Ok(_) => {}
                    Err(error) if error.error() == Errno::ESTALE && fault.is_none() => {
                        // A concurrent split can move entries after candidate grouping.
                        // Re-resolve only the affected pages; the normal path stays batched.
                        publish_relocated_ssd_extent(pool, fs_id, inode, *update)?;
                    }
                    Err(error) => return Err(error),
                }
                if !pool.persistent_data_has_migration_target(update.expected)?
                    || pool.persistent_data_migration_credit_released(update.expected)?
                {
                    return Ok(false);
                }
                // Collect only successfully published pages. Persist this entire
                // phase before returning any live admission credits.
                Ok(true)
            })();
            if matches!(release, Ok(true)) {
                credits.push((*index, update.expected));
            }
            outcomes[*index] = release;
        }
        LATENCY_STATS.migration_credit.record_since(started);
    }
    let references: Vec<_> = credits.iter().map(|(_, reference)| *reference).collect();
    let started = read_ns();
    let profile_start = super::batch_metrics::now();
    match pool.mark_migration_credits_released(&references, None) {
        Ok(()) => consumed = credits.len() as u64,
        Err(error) => {
            // Partial publication is recoverable but cannot return live credits.
            for (index, _) in credits {
                outcomes[index] = Err(error);
            }
        }
    }
    super::batch_metrics::elapsed(super::batch_metrics::Phase::CreditPublish, profile_start);
    super::batch_metrics::count(super::batch_metrics::Event::CreditPages, consumed as usize);
    LATENCY_STATS.migration_credit.record_since(started);
    if consumed != 0 {
        let started = read_ns();
        if let Err(error) = consume_ssd_capacity_locked(pool, fs_id, inode, consumed) {
            // Recovery reconstructs this conservative ledger from the forward graph.
            warn!("rNVPCe batch reservation ledger update deferred to recovery: {error:?}");
        }
        LATENCY_STATS.migration_reservation.record_since(started);
    }
    Ok(outcomes)
}

#[cfg(ktest)]
fn publish_relocated_ssd_extent(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    mut update: SsdExtentUpdate,
) -> Result<()> {
    for _ in 0..8 {
        update.locator = pool.validate_migration_and_reverse(
            update.expected,
            PageKey::new(fs_id, inode, update.logical_block),
            update.ssd_block,
        )?;
        match ExtentMutation::publish_ssd_extents_locked(pool, fs_id, inode, &[update], None) {
            Ok(_) => return Ok(()),
            Err(error) if error.error() == Errno::ESTALE => {}
            Err(error) => return Err(error),
        }
    }
    Err(Error::with_message(
        Errno::EAGAIN,
        "extent relocation did not settle",
    ))
}

#[cfg(ktest)]
fn release_migration_credit_with<F>(
    pool: &Arc<Rnvpc>,
    expected: PersistentObjectRef,
    owns_new_ssd_block: bool,
    refresh_persistent_ledger: F,
) -> Result<bool>
where
    F: FnOnce() -> Result<()>,
{
    let release_global_credit =
        owns_new_ssd_block && !pool.persistent_data_migration_credit_released(expected)?;
    if !release_global_credit {
        return Ok(false);
    }

    // Persist the one-way phase before releasing the runtime credit. The
    // EXT4 atomic is the live admission authority; this per-inode record
    // is a conservative persistent ledger repaired from the forward graph
    // at recovery. Failure to refresh it must therefore not strand live
    // capacity or prevent reclaim after the SSD extent is durable.
    pool.mark_migration_credit_released(expected)?;
    if let Err(error) = refresh_persistent_ledger() {
        warn!("rNVPCe persistent reservation ledger update deferred to recovery: {error:?}");
    }
    Ok(true)
}

#[cfg(ktest)]
pub(crate) fn test_release_migration_credit_with<F>(
    pool: &Arc<Rnvpc>,
    expected: PersistentObjectRef,
    owns_new_ssd_block: bool,
    refresh_persistent_ledger: F,
) -> Result<bool>
where
    F: FnOnce() -> Result<()>,
{
    release_migration_credit_with(
        pool,
        expected,
        owns_new_ssd_block,
        refresh_persistent_ledger,
    )
}

/// Reclaims allocation ownership records once no mapped NV extent still depends
/// on any SSD block covered by the record. The caller holds the inode
/// publication right through `ExtentMutation`, so allocation records for this
/// owner cannot be replaced while they are checked and cleared.
#[cfg(ktest)]
fn reclaim_unused_allocations(pool: &Arc<Rnvpc>, fs_id: u64, inode: u64) -> Result<()> {
    let mut mapped_nv_backing_blocks = Vec::new();
    for (logical_block, page) in pool.committed_data_pages_of(fs_id, inode) {
        let reference = pool.persistent_data_ref(page)?;
        if mapped_nv_page_ref(pool, fs_id, inode, logical_block)? == Some(reference) {
            mapped_nv_backing_blocks.push(pool.persistent_data_ssd_backing(reference)?);
        }
    }

    for reference in pool.committed_metadata_refs() {
        let key = pool.persistent_metadata_key(reference)?;
        if (key.fs_id, key.inode) != (fs_id, inode)
            || super::pcache_store::typed_object_type(key.page_index).is_some()
        {
            continue;
        }
        if pool.persistent_metadata_kind(reference)?
            != super::metadata::PersistentMetadataKind::Allocation
        {
            continue;
        }
        let allocation = pool.read_authoritative_metadata::<AllocationRecord>(reference)?;
        let end = allocation
            .first_block()
            .checked_add(allocation.block_count() as u64)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if mapped_nv_backing_blocks
            .iter()
            .any(|block| *block >= allocation.first_block() && *block < end)
        {
            continue;
        }
        pool.reclaim_committed_metadata(reference)?;
    }
    Ok(())
}

fn lookup_tree_extent(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    root: PersistentObjectRef,
    logical_block: u64,
) -> Result<Option<ExtentRecordEntry>> {
    Ok(locate_tree_extent(pool, fs_id, inode, root, logical_block)?.map(|(entry, _)| entry))
}

fn locate_tree_extent(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    root: PersistentObjectRef,
    logical_block: u64,
) -> Result<Option<(ExtentRecordEntry, ReverseExtentRef)>> {
    let mut current = root;
    let mut expected_level = None;
    loop {
        let mut node = pool.read_authoritative_metadata_boxed::<ExtentNodeRecord>(current)?;
        if node.header().fs_id() != fs_id || node.header().inode() != inode {
            return_errno_with_message!(Errno::EUCLEAN, "extent node belongs to another inode");
        }
        if let Some(level) = expected_level
            && node.level() != level
        {
            return_errno_with_message!(Errno::EUCLEAN, "extent child depth mismatch");
        }
        while let Some(sibling_slot) = node.right_sibling_slot() {
            let sibling_ref = pool.persistent_metadata_ref_from_slot(sibling_slot)?;
            let sibling =
                pool.read_authoritative_metadata_boxed::<ExtentNodeRecord>(sibling_ref)?;
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
        let entries = node.entries()?;
        if node.level() == 0 {
            let Some((entry_index, entry)) =
                entries.iter().copied().enumerate().find(|(_, entry)| {
                    entry.logical_block() <= logical_block
                        && logical_block
                            < entry.logical_block().saturating_add(entry.length() as u64)
                })
            else {
                return Ok(None);
            };
            let leaf_slot = current.slot().ok_or_else(|| Error::new(Errno::EUCLEAN))?;
            return Ok(Some((
                entry,
                ReverseExtentRef::new(leaf_slot, entry_index)?,
            )));
        }
        let Some(child_slot) = entries
            .iter()
            .filter(|entry| entry.logical_block() <= logical_block)
            .max_by_key(|entry| entry.logical_block())
            .and_then(ExtentRecordEntry::child_slot)
        else {
            return Ok(None);
        };
        expected_level = Some(node.level() - 1);
        current = pool.persistent_metadata_ref_from_slot(child_slot)?;
    }
}
