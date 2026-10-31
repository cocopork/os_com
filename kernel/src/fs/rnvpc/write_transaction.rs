// SPDX-License-Identifier: MPL-2.0

//! Syscall-sized rNVPCe soft-update write transactions.
//!
//! Task 4 supports initial writes and updates. The VFS path supplies final SSD
//! blocks and page-range descriptors; user bytes flow directly into pinned
//! NVPages. The transaction persists allocation ownership before data, data
//! before extent references, and updates the fixed persistent inode only after
//! its extent state is durable. Legacy full-page-image helpers remain only for
//! pre-migration unit coverage and are not used by the VFS path.

use core::{fmt, ops::Range};

use super::{
    data_page::DataPageState,
    extent_mutation::ExtentMutation,
    fault::FaultPoint,
    frame::Rnvpc,
    id::NvPageId,
    index::PageKey,
    metadata::{
        AllocationRecord, AuthoritativeMetadataRecord, EXTENT_NODE_ENTRY_CAPACITY, ExtentDataRef,
        ExtentNodeRecord, ExtentRecordEntry, InodeRecord, PersistentObjectRef,
        SpaceReservationRecord,
    },
};
#[cfg(ktest)]
use super::{
    extent_mutation::SsdExtentUpdate, frame::MigrationExtentIdentity, index::ReverseExtentRef,
};
use crate::{
    fs::fs_impls::ext4::{LATENCY_STATS, pipeline_profile as pp, read_ns},
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
        pool.transition_redirect_data_page(id, DataPageState::Staged, DataPageState::NvDirty)?;
        new_mappings.push((
            id,
            PageKey::new(fs_id, inode, page.logical_block),
            page.ssd_block,
        ));
    }
    for batch in new_mappings.chunks(128) {
        data_refs.extend(
            pool.publish_redirect_batch(batch, None)?
                .into_iter()
                .map(|handle| handle.reference),
        );
    }
    let ssd_mappings: Vec<_> = request
        .pages
        .iter()
        .map(|page| (page.logical_block, page.ssd_block))
        .collect();
    let extent_entries = coalesce_ssd_extent_entries(&ssd_mappings)?;

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

pub(crate) fn consume_ssd_capacity_locked(
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

pub(crate) struct RangeWritePlan {
    pub(crate) inode_generation: u64,
    pub(crate) file_offset: u64,
    pub(crate) write_len: usize,
    pub(crate) first_page_offset: usize,
    pub(crate) last_page_end: usize,
    pub(crate) extents: Vec<WriteExtent>,
}

impl RangeWritePlan {
    pub(crate) fn new_data_pages(&self) -> Result<usize> {
        self.extents.iter().try_fold(0usize, |pages, extent| {
            let additional = match extent {
                WriteExtent::ExistingNv { .. } => 0,
                WriteExtent::ExistingSsd { logical_range, .. }
                | WriteExtent::NewAllocation { logical_range } => {
                    usize::try_from(logical_range.end - logical_range.start)
                        .map_err(|_| Error::new(Errno::EOVERFLOW))?
                }
            };
            pages
                .checked_add(additional)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))
        })
    }

    pub(crate) fn extent_for(&self, logical_block: u64) -> Result<&WriteExtent> {
        self.extents
            .iter()
            .find(|extent| extent.logical_range().contains(&logical_block))
            .ok_or_else(|| Error::new(Errno::EUCLEAN))
    }
}

pub(crate) enum WriteExtent {
    ExistingNv {
        entry: Arc<super::redirect_index::DirtyNvExtentEntry>,
        logical_range: Range<u64>,
    },
    ExistingSsd {
        logical_range: Range<u64>,
        ssd_start: u64,
        written: bool,
    },
    NewAllocation {
        logical_range: Range<u64>,
    },
}

impl WriteExtent {
    pub(crate) fn logical_range(&self) -> &Range<u64> {
        match self {
            Self::ExistingNv { logical_range, .. }
            | Self::ExistingSsd { logical_range, .. }
            | Self::NewAllocation { logical_range } => logical_range,
        }
    }
}

#[derive(Clone, Debug)]
struct SsdWriteExtent {
    logical_range: Range<u64>,
    ssd_start: u64,
    written: bool,
}

pub(crate) fn plan_range_write(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    file_offset: u64,
    write_len: usize,
    old_file_size: u64,
) -> Result<RangeWritePlan> {
    if write_len == 0 {
        return_errno_with_message!(Errno::EINVAL, "empty range write");
    }
    let write_end = file_offset
        .checked_add(u64::try_from(write_len).map_err(|_| Error::new(Errno::EOVERFLOW))?)
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
    let first_page = file_offset / PAGE_SIZE as u64;
    let page_end = write_end
        .checked_add(PAGE_SIZE as u64 - 1)
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?
        / PAGE_SIZE as u64;
    let logical_range = first_page..page_end;
    let inode_ref =
        authoritative_inode_ref(pool, fs_id, inode)?.ok_or_else(|| Error::new(Errno::ENOENT))?;
    let inode_record = pool.read_authoritative_metadata::<InodeRecord>(inode_ref)?;
    let redirects = pool.lookup_dirty_nv_overlaps(fs_id, inode, logical_range.clone())?;
    let ssd_extents = lookup_tree_extent_ranges(
        pool,
        fs_id,
        inode,
        inode_record.extent_root(),
        logical_range.clone(),
    )?;
    let old_eof_page = (old_file_size % PAGE_SIZE as u64 == 0)
        .then_some(old_file_size / PAGE_SIZE as u64)
        .filter(|page| *page > logical_range.start && *page < logical_range.end);

    let mut extents = Vec::new();
    let mut cursor = logical_range.start;
    let mut redirect_index = 0usize;
    let mut ssd_index = 0usize;
    while cursor < logical_range.end {
        while redirect_index < redirects.len()
            && redirects[redirect_index].logical_range().end <= cursor
        {
            redirect_index += 1;
        }
        while ssd_index < ssd_extents.len() && ssd_extents[ssd_index].logical_range.end <= cursor {
            ssd_index += 1;
        }

        let limit =
            logical_range
                .end
                .min(cursor.saturating_add(
                    super::redirect_index::MAX_DIRTY_MIGRATION_EXTENT_PAGES as u64,
                ));
        let semantic_limit = old_eof_page
            .filter(|page| *page > cursor)
            .unwrap_or(limit)
            .min(limit);
        if let Some(entry) = redirects.get(redirect_index)
            && entry.logical_range().contains(&cursor)
        {
            let end = entry.logical_range().end.min(semantic_limit);
            extents.push(WriteExtent::ExistingNv {
                entry: entry.clone(),
                logical_range: cursor..end,
            });
            cursor = end;
            continue;
        }
        if let Some(ssd) = ssd_extents.get(ssd_index)
            && ssd.logical_range.contains(&cursor)
        {
            let end = ssd.logical_range.end.min(semantic_limit).min(
                redirects
                    .get(redirect_index)
                    .map_or(u64::MAX, |entry| entry.logical_start()),
            );
            let ssd_start = ssd
                .ssd_start
                .checked_add(cursor - ssd.logical_range.start)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            extents.push(WriteExtent::ExistingSsd {
                logical_range: cursor..end,
                ssd_start,
                written: ssd.written,
            });
            cursor = end;
            continue;
        }

        let next_redirect = redirects
            .get(redirect_index)
            .map_or(u64::MAX, |entry| entry.logical_start());
        let next_ssd = ssd_extents
            .get(ssd_index)
            .map_or(u64::MAX, |extent| extent.logical_range.start);
        let end = semantic_limit.min(next_redirect).min(next_ssd);
        if end <= cursor {
            return_errno_with_message!(Errno::EUCLEAN, "range planner made no progress");
        }
        extents.push(WriteExtent::NewAllocation {
            logical_range: cursor..end,
        });
        cursor = end;
    }

    let last_page_end =
        usize::try_from(write_end % PAGE_SIZE as u64).map_err(|_| Error::new(Errno::EOVERFLOW))?;
    Ok(RangeWritePlan {
        inode_generation: inode_record.header().generation(),
        file_offset,
        write_len,
        first_page_offset: file_offset as usize % PAGE_SIZE,
        last_page_end: if last_page_end == 0 {
            PAGE_SIZE
        } else {
            last_page_end
        },
        extents,
    })
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

    pub(crate) fn assign_new_ssd_block(&mut self, ssd_block: u64) -> Result<()> {
        if !self.newly_allocated || self.ssd_block != 0 || ssd_block == 0 {
            return_errno_with_message!(Errno::EINVAL, "invalid first-write SSD target");
        }
        self.ssd_block = ssd_block;
        Ok(())
    }

    pub(crate) const fn needs_ssd_block(&self) -> bool {
        self.newly_allocated && self.ssd_block == 0
    }

    pub(crate) const fn logical_block(&self) -> u64 {
        self.logical_block
    }
}

pub(crate) struct RangeWriteBatch<'a> {
    generation: u64,
    file_offset: u64,
    allocations: &'a [WriteAllocation],
    pages: &'a [RangeWritePage],
    old_file_size: u64,
    old_block_count: u64,
    sectors_per_block: u64,
    plan: Option<RangeWritePlan>,
}

impl fmt::Debug for RangeWriteBatch<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RangeWriteBatch")
            .field("generation", &self.generation)
            .field("file_offset", &self.file_offset)
            .field("allocations", &self.allocations)
            .field("pages", &self.pages)
            .field("old_file_size", &self.old_file_size)
            .field("old_block_count", &self.old_block_count)
            .field("sectors_per_block", &self.sectors_per_block)
            .field("has_plan", &self.plan.is_some())
            .finish()
    }
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
            || pages.len() > SSD_MIGRATION_BATCH_PAGES
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
            plan: None,
        })
    }

    pub(crate) fn with_plan(mut self, plan: RangeWritePlan) -> Result<Self> {
        if plan.file_offset != self.file_offset
            || plan.write_len
                != self
                    .pages
                    .iter()
                    .try_fold(0usize, |total, page| total.checked_add(page.len))
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?
            || plan
                .inode_generation
                .checked_add(1)
                .is_none_or(|generation| generation != self.generation)
        {
            return_errno_with_message!(Errno::ESTALE, "range write plan no longer matches request");
        }
        self.plan = Some(plan);
        Ok(self)
    }
}

#[derive(Debug)]
pub(crate) struct CommittedRangeWrite {
    copied: usize,
    inode: PersistentObjectRef,
    reserved_blocks: u64,
    unused_allocations: Vec<WriteAllocation>,
    allocation_records: Vec<PersistentObjectRef>,
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

    pub(crate) fn unused_allocations(&self) -> &[WriteAllocation] {
        &self.unused_allocations
    }

    pub(crate) fn allocation_records(&self) -> &[PersistentObjectRef] {
        &self.allocation_records
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
    let reservation = pool.reserve_native_data_pages(pages)?;
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
    if let Some(plan) = &request.plan {
        return plan.new_data_pages();
    }
    let write_len = request.pages.iter().try_fold(0usize, |total, page| {
        total
            .checked_add(page.len)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))
    })?;
    plan_range_write(
        pool,
        fs_id,
        inode,
        request.file_offset,
        write_len,
        request.old_file_size,
    )?
    .new_data_pages()
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
    pool.ensure_publication_certain()?;
    let body_started = read_ns();
    let started = read_ns();
    let current_inode_ref =
        authoritative_inode_ref(&pool, fs_id, inode)?.ok_or_else(|| Error::new(Errno::ENOENT))?;
    let current_inode = pool.read_authoritative_metadata::<InodeRecord>(current_inode_ref)?;
    if request.generation != current_inode.header().generation().saturating_add(1) {
        return_errno_with_message!(Errno::ESTALE, "write generation is not newer than inode");
    }
    let current_root = current_inode.extent_root();
    LATENCY_STATS.fg_inode_read.record_since(started);
    let computed_plan;
    let plan = if let Some(plan) = &request.plan {
        plan
    } else {
        computed_plan = plan_range_write(
            &pool,
            fs_id,
            inode,
            request.file_offset,
            request.pages.iter().try_fold(0usize, |total, page| {
                total
                    .checked_add(page.len)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))
            })?,
            request.old_file_size,
        )?;
        &computed_plan
    };
    if plan.inode_generation != current_inode.header().generation() {
        return_errno_with_message!(Errno::ESTALE, "range write plan generation changed");
    }

    if let Some(committed) = try_commit_existing_baseline_redirect_write(
        &pool,
        fs_id,
        inode,
        request,
        reader,
        &mut load_ssd_page,
        inode_fault,
        &mutation,
        &mut reservation,
        current_inode_ref,
        &current_inode,
        plan,
    )? {
        LATENCY_STATS.fg_body.record_since(body_started);
        return Ok(committed);
    }

    let baseline_keys = request
        .pages
        .iter()
        .map(|page| PageKey::new(fs_id, inode, page.logical_block))
        .collect::<Vec<_>>();
    let _baseline_guard = match pool.begin_baseline_mutation(&baseline_keys) {
        Ok(guard) => guard,
        Err(error) if error.error() == Errno::EBUSY => {
            return_errno_with_message!(
                Errno::EAGAIN,
                "SSD baseline migration won the foreground publication race"
            )
        }
        Err(error) => return Err(error),
    };
    let nv_mutations = begin_plan_nv_mutations(&pool, plan)?;

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
        // Planning established that a newly allocated logical page has no
        // baseline while this mutation's publication guard was already held.
        // Re-routing the immutable tree for every page only repeats that fact.
        let (entry, existing) = match plan.extent_for(page.logical_block)? {
            WriteExtent::ExistingNv { entry, .. } => (None, Some(entry.clone())),
            WriteExtent::ExistingSsd {
                logical_range,
                ssd_start,
                written,
            } => {
                let block = ssd_start
                    .checked_add(page.logical_block - logical_range.start)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                if block != page.ssd_block || page.newly_allocated {
                    return_errno_with_message!(Errno::ESTALE, "SSD extent target changed");
                }
                (
                    Some(ExtentRecordEntry::leaf(
                        page.logical_block,
                        1,
                        block,
                        !*written,
                    )?),
                    None,
                )
            }
            WriteExtent::NewAllocation { .. } => {
                if !page.newly_allocated {
                    return_errno_with_message!(Errno::ESTALE, "write target lost its allocation");
                }
                (None, None)
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
    let mut unused_allocations = Vec::new();
    unused_allocations
        .try_reserve_exact(request.allocations.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    if !request.allocations.is_empty() {
        let allocation_record_started = pp::now();
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
        pp::NV_FOREGROUND_ALLOCATION_RECORD_PUBLISH.record_since(
            allocation_record_started,
            request
                .allocations
                .iter()
                .map(|allocation| allocation.block_count as usize)
                .sum(),
        );
    }

    LATENCY_STATS.fg_allocate.record_since(started);
    let started = read_ns();
    let mut copied = 0usize;
    let mut newly_mapped = 0u64;
    let mut pending_new_pages = Vec::new();
    pending_new_pages
        .try_reserve_exact(new_page_count)
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    for (page, (old_entry, existing)) in request.pages.iter().zip(existing_refs) {
        let page_result = (|| -> Result<usize> {
            if existing.is_some() {
                let data = nv_mutations
                    .iter()
                    .find(|mutation| mutation.contains(page.logical_block))
                    .ok_or_else(|| Error::new(Errno::ESTALE))?;
                let t_copy = read_ns();
                let amount = data.copy_page_from_reader(
                    page.logical_block,
                    page.page_offset,
                    reader,
                    page.len,
                )?;
                LATENCY_STATS.fg_copy.record_since(t_copy);
                Ok(amount)
            } else {
                let t_init = read_ns();
                let mut staging = pool.begin_reserved_native_data_page(&mut reservation)?;
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
                Ok(amount)
            }
        })();
        let page_copied = match page_result {
            Ok(result) => result,
            Err(_) if copied != 0 => break,
            Err(error) => return Err(error),
        };
        copied = copied
            .checked_add(page_copied)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if page_copied != page.len {
            break;
        }
    }
    finish_plan_nv_mutations(nv_mutations)?;
    let t_register = read_ns();
    let mut new_mappings = Vec::new();
    new_mappings
        .try_reserve_exact(pending_new_pages.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    for (_, key, ssd_block) in &pending_new_pages {
        new_mappings.push((key.page_index, *ssd_block));
    }
    let replacements = coalesce_ssd_extent_entries(&new_mappings)?;
    let mut published_entries = Vec::new();
    published_entries
        .try_reserve_exact(pending_new_pages.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
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
    for batch in pending_new_pages.chunks(128) {
        match pool.publish_redirect_entries(batch, request.generation, None) {
            Ok(entries) => published_entries.extend(entries),
            Err(error) => {
                if !pool.redirect_publication_uncertain() {
                    pool.abort_redirect_entries(&published_entries)?;
                    for (page, _, _) in &pending_new_pages {
                        if !published_entries
                            .iter()
                            .any(|entry| entry.nv_slot_range().contains(&(page.index() as u64)))
                        {
                            let _ = pool.free_native_data_range(page.index()..page.index() + 1);
                        }
                    }
                }
                return Err(error);
            }
        }
    }
    LATENCY_STATS.fg_data_register.record_since(t_register);
    let inode_ref = match mutation.publish_inode_state_with_generation(
        current_inode_ref,
        new_root.reference(),
        &new_root,
        request.generation,
        file_size,
        block_count,
        inode_fault,
    ) {
        Ok(inode_ref) => inode_ref,
        Err(error) => {
            pool.abort_redirect_entries(&published_entries)?;
            return Err(error);
        }
    };
    pool.activate_redirect_entries(&published_entries);
    if !request.allocations.is_empty() {
        let mut used = newly_mapped;
        for allocation in request.allocations {
            let count = u64::from(allocation.block_count);
            if used >= count {
                used -= count;
                continue;
            }
            let first = allocation
                .first_ssd_block
                .checked_add(used)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            unused_allocations.push(WriteAllocation::new(
                allocation.block_group,
                first,
                u32::try_from(count - used).map_err(|_| Error::new(Errno::EOVERFLOW))?,
            )?);
            used = 0;
        }
        if used != 0 {
            return_errno_with_message!(Errno::EUCLEAN, "copied allocation prefix exceeds batch");
        }
        // These ownership records protect the interval between allocating the
        // NVM-authoritative bitmap and publishing the SSD baseline. The
        // baseline now names every allocated block, so retaining one metadata
        // page per write would leak the metadata reserve.
        let allocation_record_reclaim_started = pp::now();
        if unused_allocations.is_empty() {
            for reference in &allocation_refs {
                pool.reclaim_committed_metadata(*reference)?;
            }
            allocation_refs.clear();
        }
        pp::NV_FOREGROUND_ALLOCATION_RECORD_RECLAIM.record_since(
            allocation_record_reclaim_started,
            request
                .allocations
                .iter()
                .map(|allocation| allocation.block_count as usize)
                .sum(),
        );
    }
    LATENCY_STATS.fg_inode_publish.record_since(started);
    LATENCY_STATS.fg_enqueue.record_events(0);
    LATENCY_STATS.fg_body.record_since(body_started);
    Ok(CommittedRangeWrite {
        copied,
        inode: inode_ref,
        reserved_blocks: newly_mapped,
        unused_allocations,
        allocation_records: allocation_refs,
    })
}

#[allow(clippy::too_many_arguments)]
fn try_commit_existing_baseline_redirect_write<F>(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    request: &RangeWriteBatch<'_>,
    reader: &mut VmReader,
    load_ssd_page: &mut F,
    inode_fault: Option<FaultPoint>,
    mutation: &ExtentMutation,
    reservation: &mut super::frame::CapacityReservation,
    current_inode_ref: PersistentObjectRef,
    current_inode: &InodeRecord,
    plan: &RangeWritePlan,
) -> Result<Option<CommittedRangeWrite>>
where
    F: FnMut(u64, &mut super::frame::StagingDataPage) -> Result<()>,
{
    if request.pages.iter().any(|page| page.newly_allocated) {
        return Ok(None);
    }
    if plan.extents.iter().any(|extent| {
        matches!(
            extent,
            WriteExtent::NewAllocation { .. } | WriteExtent::ExistingSsd { written: false, .. }
        )
    }) {
        return Ok(None);
    }
    for page in request.pages {
        let target = match plan.extent_for(page.logical_block)? {
            WriteExtent::ExistingNv { entry, .. } => entry
                .ssd_block_range()
                .start
                .checked_add(page.logical_block - entry.logical_start())
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            WriteExtent::ExistingSsd {
                logical_range,
                ssd_start,
                ..
            } => ssd_start
                .checked_add(page.logical_block - logical_range.start)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            WriteExtent::NewAllocation { .. } => unreachable!(),
        };
        if target != page.ssd_block {
            return Ok(None);
        }
    }

    let nv_mutations = begin_plan_nv_mutations(pool, plan)?;

    let mut pending = Vec::new();
    pending
        .try_reserve_exact(request.pages.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    let mut copied = 0usize;
    let staging_result = (|| -> Result<()> {
        for page in request.pages {
            let key = PageKey::new(fs_id, inode, page.logical_block);
            if matches!(
                plan.extent_for(page.logical_block)?,
                WriteExtent::ExistingNv { .. }
            ) {
                let data = nv_mutations
                    .iter()
                    .find(|mutation| mutation.contains(page.logical_block))
                    .ok_or_else(|| Error::new(Errno::ESTALE))?;
                let amount = data.copy_page_from_reader(
                    page.logical_block,
                    page.page_offset,
                    reader,
                    page.len,
                )?;
                if amount == 0 {
                    return_errno_with_message!(Errno::EFAULT, "write copied no user bytes");
                }
                copied = copied
                    .checked_add(amount)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                if amount != page.len {
                    break;
                }
                continue;
            }
            let mut staging = pool.begin_reserved_native_data_page(reservation)?;
            let full_overwrite = page.page_offset == 0 && page.len == PAGE_SIZE;
            if full_overwrite {
                staging.zero_all()?;
            } else {
                staging.zero_all()?;
                load_ssd_page(page.ssd_block, &mut staging)?;
            }
            let amount = staging.copy_from_reader(page.page_offset, reader, page.len)?;
            if amount == 0 {
                return_errno_with_message!(Errno::EFAULT, "write copied no user bytes");
            }
            let id = staging.finish()?;
            pending.push((id, key, page.ssd_block));
            copied = copied
                .checked_add(amount)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            if amount != page.len {
                break;
            }
        }
        Ok(())
    })();
    if let Err(error) = staging_result {
        for (page, _, _) in pending {
            let _ = pool.free_native_data_range(page.index()..page.index() + 1);
        }
        finish_plan_nv_mutations(nv_mutations)?;
        return Err(error);
    }
    finish_plan_nv_mutations(nv_mutations)?;
    let root = mutation.committed_node(current_inode.extent_root())?;
    let visible_end = request
        .file_offset
        .checked_add(copied as u64)
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
    let mut published_entries = Vec::new();
    published_entries
        .try_reserve_exact(pending.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    for batch in pending.chunks(128) {
        match pool.publish_redirect_entries(batch, request.generation, None) {
            Ok(entries) => published_entries.extend(entries),
            Err(error) => {
                if !pool.redirect_publication_uncertain() {
                    pool.abort_redirect_entries(&published_entries)?;
                    for (page, _, _) in &pending {
                        if !published_entries
                            .iter()
                            .any(|entry| entry.nv_slot_range().contains(&(page.index() as u64)))
                        {
                            let _ = pool.free_native_data_range(page.index()..page.index() + 1);
                        }
                    }
                }
                return Err(error);
            }
        }
    }

    let inode_ref = match mutation.publish_inode_state_with_generation(
        current_inode_ref,
        current_inode.extent_root(),
        &root,
        request.generation,
        request.old_file_size.max(visible_end),
        request.old_block_count,
        inode_fault,
    ) {
        Ok(inode_ref) => inode_ref,
        Err(error) => {
            pool.abort_redirect_entries(&published_entries)?;
            return Err(error);
        }
    };
    pool.activate_redirect_entries(&published_entries);
    Ok(Some(CommittedRangeWrite {
        copied,
        inode: inode_ref,
        reserved_blocks: 0,
        unused_allocations: Vec::new(),
        allocation_records: Vec::new(),
    }))
}

fn begin_plan_nv_mutations(
    pool: &Arc<Rnvpc>,
    plan: &RangeWritePlan,
) -> Result<Vec<super::frame::RangeDataMutation>> {
    let mut seen = BTreeSet::new();
    let mut mutations = Vec::new();
    for extent in &plan.extents {
        if let WriteExtent::ExistingNv {
            entry,
            logical_range,
        } = extent
            && seen.insert(entry.logical_start())
        {
            mutations.push(
                pool.begin_range_data_mutation(entry, logical_range.clone())
                    .map_err(|error| {
                        if error.error() == Errno::EBUSY {
                            Error::with_message(
                                Errno::EAGAIN,
                                "NV extent entry migration won the foreground write race",
                            )
                        } else {
                            error
                        }
                    })?,
            );
        }
    }
    Ok(mutations)
}

fn finish_plan_nv_mutations(mutations: Vec<super::frame::RangeDataMutation>) -> Result<()> {
    for mutation in mutations {
        mutation.finish()?;
    }
    Ok(())
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

    pub(crate) const fn first_ssd_block(&self) -> u64 {
        self.first_ssd_block
    }

    pub(crate) const fn block_count(&self) -> u32 {
        self.block_count
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
            || pages.len() > SSD_MIGRATION_BATCH_PAGES
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
    let baseline_keys = request
        .pages
        .iter()
        .map(|page| PageKey::new(fs_id, inode, page.logical_block))
        .collect::<Vec<_>>();
    let _baseline_guard = pool.begin_baseline_mutation(&baseline_keys)?;

    let mut allocation_refs = Vec::new();
    let mut data_refs = Vec::new();
    allocation_refs
        .try_reserve_exact(request.allocations.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    data_refs
        .try_reserve_exact(request.pages.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    let mut legacy_nv_mappings = Vec::new();
    legacy_nv_mappings
        .try_reserve_exact(request.pages.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    for page in request.pages {
        if lookup_tree_extent(&pool, fs_id, inode, current_root, page.logical_block)?
            .is_some_and(|entry| matches!(entry.data_ref(), Ok(ExtentDataRef::Nv(_))))
        {
            legacy_nv_mappings.push((page.logical_block, page.ssd_block));
        }
    }
    let mut reservation = pool.reserve_data_pages(request.pages.len())?;
    let mut pending = Vec::new();
    pending
        .try_reserve_exact(request.pages.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    let mut old_redirects = Vec::new();

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

    for page in request.pages {
        let key = PageKey::new(fs_id, inode, page.logical_block);
        if let Some(handle) = pool.redirect_lookup(key) {
            old_redirects.push((key, handle));
        }
        let id = pool.stage_reserved_data_page(&mut reservation, page.contents)?;
        pool.transition_redirect_data_page(id, DataPageState::Staged, DataPageState::NvDirty)?;
        pending.push((id, key, page.ssd_block));
    }
    for batch in pending.chunks(128) {
        data_refs.extend(
            pool.publish_redirect_batch(batch, None)?
                .into_iter()
                .map(|handle| handle.reference),
        );
    }
    let replacements = coalesce_ssd_extent_entries(&legacy_nv_mappings)?;
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
    for old in old_redirects {
        if let Ok(pages) = pool.clear_redirects_exact(&[old], None) {
            for page in pages {
                let _ = pool.free_data_page(page);
            }
        }
    }
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

fn coalesce_ssd_extent_entries(mappings: &[(u64, u64)]) -> Result<Vec<ExtentRecordEntry>> {
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(mappings.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    let mut start = 0;
    while start < mappings.len() {
        let (logical_start, physical_start) = mappings[start];
        if physical_start == 0 {
            start += 1;
            continue;
        }
        let mut end = start + 1;
        while end < mappings.len() {
            let offset = u64::try_from(end - start).map_err(|_| Error::new(Errno::EOVERFLOW))?;
            if mappings[end].0 != logical_start.saturating_add(offset)
                || mappings[end].1 != physical_start.saturating_add(offset)
            {
                break;
            }
            end += 1;
        }
        entries.push(ExtentRecordEntry::leaf(
            logical_start,
            u32::try_from(end - start).map_err(|_| Error::new(Errno::EOVERFLOW))?,
            physical_start,
            false,
        )?);
        start = end;
    }
    Ok(entries)
}

/// Publishes SSD targets allocated by rTIO for pages that had no prior SSD
/// baseline. Existing-baseline migration never calls this path.
pub(crate) fn publish_ssd_baseline_batch(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    mappings: &[(u64, u64)],
) -> Result<()> {
    publish_ssd_baseline_batch_inner(pool, fs_id, inode, mappings, false)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SsdBaselineExtent {
    logical_start: u64,
    length: u32,
    physical_start: u64,
}

impl SsdBaselineExtent {
    pub(crate) fn new(logical_start: u64, length: u32, physical_start: u64) -> Result<Self> {
        if length == 0 {
            return_errno_with_message!(Errno::EINVAL, "SSD baseline extent is empty");
        }
        if physical_start == 0 {
            return_errno_with_message!(Errno::EINVAL, "SSD baseline target is block zero");
        }
        logical_start
            .checked_add(u64::from(length))
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        physical_start
            .checked_add(u64::from(length))
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        Ok(Self {
            logical_start,
            length,
            physical_start,
        })
    }

    pub(crate) const fn logical_start(self) -> u64 {
        self.logical_start
    }
}

/// Publishes multiple completed dirty extents through one inode publication
/// session. Extent boundaries are preserved; this API never coalesces them.
pub(crate) fn publish_ssd_baseline_extents(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    extents: &[SsdBaselineExtent],
) -> Result<()> {
    publish_ssd_baseline_extents_inner(pool, fs_id, inode, extents, false)
}

pub(crate) fn publish_claimed_ssd_baseline_extents(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    extents: &[SsdBaselineExtent],
) -> Result<()> {
    publish_ssd_baseline_extents_inner(pool, fs_id, inode, extents, true)
}

/// Publishes one contiguous SSD baseline extent in the NVM authority tree.
pub(crate) fn publish_ssd_baseline_extent(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    logical_start: u64,
    length: u32,
    physical_start: u64,
) -> Result<()> {
    publish_ssd_baseline_extent_inner(
        pool,
        fs_id,
        inode,
        logical_start,
        length,
        physical_start,
        false,
    )
}

fn baseline_range_keys(
    fs_id: u64,
    inode: u64,
    logical_start: u64,
    length: u32,
) -> Result<Vec<PageKey>> {
    if length == 0 {
        return_errno_with_message!(Errno::EINVAL, "SSD baseline extent is empty");
    }
    (0..u64::from(length))
        .map(|offset| {
            logical_start
                .checked_add(offset)
                .map(|logical| PageKey::new(fs_id, inode, logical))
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))
        })
        .collect()
}

fn publish_ssd_baseline_extent_inner(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    logical_start: u64,
    length: u32,
    physical_start: u64,
    claimed: bool,
) -> Result<()> {
    let extent = SsdBaselineExtent::new(logical_start, length, physical_start)?;
    publish_ssd_baseline_extents_inner(pool, fs_id, inode, &[extent], claimed)
}

fn publish_ssd_baseline_extents_inner(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    extents: &[SsdBaselineExtent],
    claimed: bool,
) -> Result<()> {
    if extents.is_empty() {
        return Ok(());
    }
    let page_count = extents.iter().try_fold(0usize, |count, extent| {
        count
            .checked_add(extent.length as usize)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))
    })?;
    let mut keys = Vec::new();
    let mut replacements = Vec::new();
    keys.try_reserve_exact(page_count)
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    replacements
        .try_reserve_exact(extents.len())
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    let mut previous_end = None;
    for extent in extents {
        if previous_end.is_some_and(|end| extent.logical_start < end) {
            return_errno_with_message!(
                Errno::EINVAL,
                "SSD baseline extents overlap or are unsorted"
            );
        }
        previous_end = Some(
            extent
                .logical_start
                .checked_add(u64::from(extent.length))
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
        );
        keys.extend(baseline_range_keys(
            fs_id,
            inode,
            extent.logical_start,
            extent.length,
        )?);
        replacements.push(ExtentRecordEntry::leaf(
            extent.logical_start,
            extent.length,
            extent.physical_start,
            false,
        )?);
    }
    publish_ssd_baseline_entries_inner(pool, fs_id, inode, &keys, &replacements, claimed)
}

fn publish_ssd_baseline_batch_inner(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    mappings: &[(u64, u64)],
    claimed: bool,
) -> Result<()> {
    if mappings.is_empty() {
        return Ok(());
    }
    if mappings.iter().any(|(_, block)| *block == 0) {
        return_errno_with_message!(Errno::EINVAL, "SSD baseline target is block zero");
    }
    let keys = mappings
        .iter()
        .map(|(logical, _)| PageKey::new(fs_id, inode, *logical))
        .collect::<Vec<_>>();
    let replacements = coalesce_ssd_extent_entries(mappings)?;
    publish_ssd_baseline_entries_inner(pool, fs_id, inode, &keys, &replacements, claimed)
}

fn publish_ssd_baseline_entries_inner(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    keys: &[PageKey],
    replacements: &[ExtentRecordEntry],
    claimed: bool,
) -> Result<()> {
    let metrics_started = super::batch_metrics::now();
    let mutation = ExtentMutation::new(pool.clone(), fs_id, inode)?;
    let _baseline_guard = if claimed {
        pool.advance_claimed_baseline(keys)?;
        None
    } else {
        Some(pool.begin_baseline_mutation(keys)?)
    };
    let inode_ref =
        authoritative_inode_ref(pool, fs_id, inode)?.ok_or_else(|| Error::new(Errno::ESTALE))?;
    let inode_record = pool.read_authoritative_metadata::<InodeRecord>(inode_ref)?;
    let new_root = mutation.replace_tree_extents(inode_record.extent_root(), replacements, None)?;
    if new_root.reference() != inode_record.extent_root() {
        mutation.publish_inode_state_with_generation(
            inode_ref,
            new_root.reference(),
            &new_root,
            inode_record
                .header()
                .generation()
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            inode_record.file_size(),
            inode_record.block_count(),
            None,
        )?;
    }
    super::batch_metrics::count(super::batch_metrics::Event::StructuralBaselineMutations, 1);
    super::batch_metrics::elapsed(
        super::batch_metrics::Phase::StructuralBaseline,
        metrics_started,
    );
    Ok(())
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
    let key = PageKey::new(fs_id, inode, logical_block);
    let Some(entry) = pool.lookup_dirty_nv_entry(key) else {
        return Ok(None);
    };
    let slot = entry
        .nv_slot_range()
        .start
        .checked_add(logical_block - entry.logical_start())
        .and_then(|slot| usize::try_from(slot).ok())
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
    pool.persistent_data_ref(NvPageId::try_from(slot)?)
        .map(Some)
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
                if matches!(entry.data_ref()?, ExtentDataRef::Ssd(block) if block != 0)
                    && !entry.is_unwritten()
                {
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
        ExtentDataRef::Ssd(0) => return Ok(None),
        ExtentDataRef::Ssd(first) => (
            first
                .checked_add(logical_block - entry.logical_block())
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            false,
        ),
        ExtentDataRef::Nv(_) => {
            return_errno_with_message!(Errno::EUCLEAN, "v5 extent leaf contains a legacy NV target")
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
#[cfg(ktest)]
pub(crate) struct SsdMigrationPage {
    pub(crate) logical_block: u64,
    pub(crate) reference: PersistentObjectRef,
    pub(crate) ssd_block: u64,
}

#[cfg(ktest)]
pub(crate) struct ClaimedSsdMigrationExtent {
    pub(crate) identity: Arc<MigrationExtentIdentity>,
    pub(crate) updates: Vec<SsdMigrationPage>,
}

/// Completes one claimed contiguous NV extent as a single authority change.
#[cfg(ktest)]
pub(crate) fn migrate_claimed_nv_extent_to_ssd(
    pool: &Arc<Rnvpc>,
    identity: &MigrationExtentIdentity,
    pages: &[SsdMigrationPage],
    fault: Option<FaultPoint>,
) -> Result<Vec<Result<bool>>> {
    let references = publish_claimed_nv_extent_to_ssd(pool, identity, pages, fault)?;
    finish_claimed_nv_extents_to_ssd(pool, identity.fs_id(), identity.inode(), &[references])
        .map(|mut outcomes| outcomes.pop().unwrap_or_default())
}

#[cfg(ktest)]
pub(crate) fn migrate_claimed_nv_extents_to_ssd(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    extents: &[ClaimedSsdMigrationExtent],
    fault: Option<FaultPoint>,
) -> Result<Vec<Vec<Result<bool>>>> {
    if extents.is_empty() {
        return Ok(Vec::new());
    }
    let mut references = Vec::with_capacity(extents.len());
    for extent in extents {
        if extent.identity.fs_id() != fs_id || extent.identity.inode() != inode {
            return_errno_with_message!(Errno::EINVAL, "mixed-owner migration batch");
        }
        references.push(publish_claimed_nv_extent_to_ssd(
            pool,
            &extent.identity,
            &extent.updates,
            fault,
        )?);
    }
    finish_claimed_nv_extents_to_ssd(pool, fs_id, inode, &references)
}

#[cfg(ktest)]
pub(crate) fn publish_claimed_nv_extent_to_ssd(
    pool: &Arc<Rnvpc>,
    identity: &MigrationExtentIdentity,
    pages: &[SsdMigrationPage],
    fault: Option<FaultPoint>,
) -> Result<Vec<PersistentObjectRef>> {
    if pages.is_empty()
        || pages.len() > SSD_MIGRATION_BATCH_PAGES
        || pages.len() != identity.length() as usize
    {
        return_errno_with_message!(Errno::EINVAL, "invalid claimed extent completion size");
    }
    let mut updates = Vec::with_capacity(pages.len());
    let mut identities = BTreeSet::new();
    let first_ssd = pages[0].ssd_block;
    if first_ssd == 0 {
        return_errno_with_message!(Errno::EAGAIN, "NV extent has no SSD target");
    }
    let reverse_started = pp::now();
    let locator = pool
        .reverse_lookup(pages[0].reference)?
        .ok_or_else(|| Error::new(Errno::ESTALE))?;
    pp::NV_EXTENT_REVERSE_LOOKUP.record_since(reverse_started, pages.len());
    let validation_started = pp::now();
    for (offset, page) in pages.iter().enumerate() {
        let offset = u64::try_from(offset).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let expected_ssd = first_ssd
            .checked_add(offset)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if page.ssd_block != expected_ssd {
            return_errno_with_message!(Errno::EAGAIN, "fragmented extent migration targets");
        }
        if page.reference.is_none()
            || !identities.insert((page.reference.slot(), page.reference.allocation_seq()))
            || page.logical_block != identity.logical_start().saturating_add(offset)
            || page.reference.slot_plus_one()
                != identity.nv_base_slot_plus_one().saturating_add(offset)
            || pool.validate_persistent_data_ref(page.reference)?
                != PageKey::new(identity.fs_id(), identity.inode(), page.logical_block)
            || pool.persistent_data_ssd_backing(page.reference)? != page.ssd_block
        {
            return_errno_with_message!(Errno::ESTALE, "claimed extent member changed");
        }
        updates.push(SsdExtentUpdate {
            locator,
            expected: page.reference,
            logical_block: page.logical_block,
            ssd_block: page.ssd_block,
        });
    }
    pp::NV_EXTENT_MEMBER_VALIDATE.record_since(validation_started, pages.len());
    let started = read_ns();
    let _published = ExtentMutation::publish_ssd_extents_locked(
        pool,
        identity.fs_id(),
        identity.inode(),
        &updates,
        fault,
    )?;
    pool.advance_baseline_generation(
        identity.fs_id(),
        identity.inode(),
        identity.logical_start(),
        pages.len(),
    )?;
    LATENCY_STATS.migration_leaf.record_since(started);

    Ok(pages.iter().map(|page| page.reference).collect())
}

#[cfg(ktest)]
pub(crate) fn finish_claimed_nv_extents_to_ssd(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    extent_references: &[Vec<PersistentObjectRef>],
) -> Result<Vec<Vec<Result<bool>>>> {
    let credit_scan_started = pp::now();
    let references: Vec<_> = extent_references.iter().flatten().copied().collect();
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
    pp::NV_EXTENT_CREDIT_SCAN.record_since(credit_scan_started, references.len());

    let credit_persist_started = pp::now();
    pool.mark_migration_credits_released(&releasable, None)?;
    pp::NV_EXTENT_CREDIT_PERSIST.record_since(credit_persist_started, releasable.len());

    let consumed = releasable.len() as u64;
    let ledger_started = pp::now();
    if consumed != 0
        && let Err(error) = consume_ssd_capacity_locked(pool, fs_id, inode, consumed)
    {
        warn!("rNVPCe batch reservation ledger update deferred to recovery: {error:?}");
    }
    pp::NV_EXTENT_LEDGER.record_since(ledger_started, consumed as usize);

    let outcome_started = pp::now();
    let releasable: BTreeSet<_> = releasable
        .into_iter()
        .map(|reference| (reference.slot(), reference.allocation_seq()))
        .collect();
    let outcomes = extent_references
        .iter()
        .map(|extent| {
            extent
                .iter()
                .map(|reference| {
                    Ok(releasable.contains(&(reference.slot(), reference.allocation_seq())))
                })
                .collect()
        })
        .collect();
    pp::NV_EXTENT_OUTCOME_BUILD.record_since(outcome_started, references.len());
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

#[cfg(ktest)]
pub(crate) fn allocation_record_count(pool: &Arc<Rnvpc>) -> usize {
    pool.committed_metadata_refs()
        .into_iter()
        .filter(|reference| {
            pool.persistent_metadata_kind(*reference).ok()
                == Some(super::metadata::PersistentMetadataKind::Allocation)
        })
        .count()
}

fn lookup_tree_extent(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    root: PersistentObjectRef,
    logical_block: u64,
) -> Result<Option<ExtentRecordEntry>> {
    #[cfg(ktest)]
    pool.note_extent_tree_lookup();
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
            let _ = entry_index;
            return Ok(Some(entry));
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

fn lookup_tree_extent_ranges(
    pool: &Arc<Rnvpc>,
    fs_id: u64,
    inode: u64,
    root: PersistentObjectRef,
    logical_range: Range<u64>,
) -> Result<Vec<SsdWriteExtent>> {
    #[cfg(ktest)]
    pool.note_extent_tree_lookup();
    if logical_range.start >= logical_range.end || root.is_none() {
        return Ok(Vec::new());
    }
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
        while node.level() != 0
            && let Some(sibling_slot) = node.right_sibling_slot()
        {
            let sibling_ref = pool.persistent_metadata_ref_from_slot(sibling_slot)?;
            let sibling =
                pool.read_authoritative_metadata_boxed::<ExtentNodeRecord>(sibling_ref)?;
            let boundary = sibling
                .entries()?
                .first()
                .map(ExtentRecordEntry::logical_block)
                .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
            if logical_range.start < boundary {
                break;
            }
            node = sibling;
        }
        if node.level() == 0 {
            let mut ranges = Vec::new();
            loop {
                for entry in node.entries()?.iter().copied() {
                    let entry_end = entry
                        .logical_block()
                        .checked_add(u64::from(entry.length()))
                        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                    let start = entry.logical_block().max(logical_range.start);
                    let end = entry_end.min(logical_range.end);
                    if start >= end {
                        continue;
                    }
                    let ExtentDataRef::Ssd(ssd_base) = entry.data_ref()? else {
                        return_errno_with_message!(
                            Errno::EUCLEAN,
                            "v5 extent leaf contains a legacy NV target"
                        );
                    };
                    if ssd_base == 0 {
                        continue;
                    }
                    let ssd_start = ssd_base
                        .checked_add(start - entry.logical_block())
                        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
                    let written = !entry.is_unwritten();
                    let can_merge = ranges.last().is_some_and(|previous: &SsdWriteExtent| {
                        previous.logical_range.end == start
                            && previous.written == written
                            && previous.ssd_start.checked_add(
                                previous.logical_range.end - previous.logical_range.start,
                            ) == Some(ssd_start)
                    });
                    if can_merge {
                        ranges.last_mut().expect("range exists").logical_range.end = end;
                    } else {
                        ranges.push(SsdWriteExtent {
                            logical_range: start..end,
                            ssd_start,
                            written,
                        });
                    }
                }
                let Some(sibling_slot) = node.right_sibling_slot() else {
                    break;
                };
                let sibling_ref = pool.persistent_metadata_ref_from_slot(sibling_slot)?;
                let sibling =
                    pool.read_authoritative_metadata_boxed::<ExtentNodeRecord>(sibling_ref)?;
                if sibling.header().fs_id() != fs_id
                    || sibling.header().inode() != inode
                    || sibling.level() != 0
                {
                    return_errno_with_message!(Errno::EUCLEAN, "invalid extent leaf sibling");
                }
                let boundary = sibling
                    .entries()?
                    .first()
                    .map(ExtentRecordEntry::logical_block)
                    .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
                if boundary >= logical_range.end {
                    break;
                }
                node = sibling;
            }
            return Ok(ranges);
        }
        let entries = node.entries()?;
        let child_slot = entries
            .iter()
            .filter(|entry| entry.logical_block() <= logical_range.start)
            .max_by_key(|entry| entry.logical_block())
            .or_else(|| entries.first())
            .and_then(ExtentRecordEntry::child_slot)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        expected_level = Some(node.level() - 1);
        current = pool.persistent_metadata_ref_from_slot(child_slot)?;
    }
}
