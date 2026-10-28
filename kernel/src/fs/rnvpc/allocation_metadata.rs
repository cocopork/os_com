// SPDX-License-Identifier: MPL-2.0

//! NVM-authoritative EXT4 block-allocation metadata.

use super::{
    fault::FaultPoint,
    frame::Rnvpc,
    index::PageKey,
    metadata::{
        AuthoritativeMetadataRecord, PersistentMetadataHeader, PersistentMetadataKind,
        PersistentMetadataRecord, PersistentObjectRef,
    },
};
use crate::prelude::*;

pub(crate) const ALLOCATION_BITMAP_CHUNK_BYTES: usize = PAGE_SIZE / 2;
const ALLOCATION_BITMAP_CHUNKS: u16 = 2;
const ALLOCATION_METADATA_LOGICAL_INDEX_TAG: u64 = 1 << 59;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub(crate) enum AllocationGroupState {
    CleanOnSsd = 1,
    ResidentClean = 2,
    ResidentDirty = 3,
    Checkpointing = 4,
}

impl AllocationGroupState {
    fn from_u32(value: u32) -> Result<Self> {
        match value {
            1 => Ok(Self::CleanOnSsd),
            2 => Ok(Self::ResidentClean),
            3 => Ok(Self::ResidentDirty),
            4 => Ok(Self::Checkpointing),
            _ => return_errno_with_message!(Errno::EINVAL, "invalid allocation group state"),
        }
    }

    pub(crate) const fn is_resident(self) -> bool {
        !matches!(self, Self::CleanOnSsd)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Pod)]
pub(crate) struct AllocationGroupDirectoryRecord {
    header: PersistentMetadataHeader,
    group: u32,
    state: u32,
    free_blocks: u32,
    bitmap_checksum: u32,
    checkpoint_generation: u64,
    mutation_epoch: u64,
    chunks: [PersistentObjectRef; 2],
    reserved: [u8; 56],
}

impl AllocationGroupDirectoryRecord {
    pub(crate) fn clean_on_ssd(
        fs_id: u64,
        group: u32,
        generation: u64,
        free_blocks: u32,
        bitmap_checksum: u32,
        mutation_epoch: u64,
    ) -> Result<Self> {
        Self::new(
            fs_id,
            group,
            generation,
            AllocationGroupState::CleanOnSsd,
            free_blocks,
            bitmap_checksum,
            generation,
            mutation_epoch,
            [PersistentObjectRef::NONE; 2],
        )
    }

    #[cfg(ktest)]
    pub(crate) fn resident_dirty(
        fs_id: u64,
        group: u32,
        generation: u64,
        free_blocks: u32,
        bitmap_checksum: u32,
    ) -> Result<Self> {
        Self::new(
            fs_id,
            group,
            generation,
            AllocationGroupState::ResidentDirty,
            free_blocks,
            bitmap_checksum,
            0,
            0,
            [
                PersistentObjectRef::new(1, 1)?,
                PersistentObjectRef::new(2, 1)?,
            ],
        )
    }

    pub(crate) fn resident_dirty_with_chunks(
        fs_id: u64,
        group: u32,
        generation: u64,
        free_blocks: u32,
        bitmap_checksum: u32,
        mutation_epoch: u64,
        chunks: [PersistentObjectRef; 2],
    ) -> Result<Self> {
        if chunks.iter().any(PersistentObjectRef::is_none) {
            return_errno_with_message!(Errno::EINVAL, "resident allocation image has no chunks");
        }
        Self::new(
            fs_id,
            group,
            generation,
            AllocationGroupState::ResidentDirty,
            free_blocks,
            bitmap_checksum,
            0,
            mutation_epoch,
            chunks,
        )
    }

    #[expect(clippy::too_many_arguments)]
    pub(crate) fn new(
        fs_id: u64,
        group: u32,
        generation: u64,
        state: AllocationGroupState,
        free_blocks: u32,
        bitmap_checksum: u32,
        checkpoint_generation: u64,
        mutation_epoch: u64,
        chunks: [PersistentObjectRef; 2],
    ) -> Result<Self> {
        if generation == 0 || checkpoint_generation > generation {
            return_errno_with_message!(Errno::EINVAL, "invalid allocation group generation");
        }
        let record = Self {
            header: PersistentMetadataHeader::new(
                PersistentMetadataKind::AllocationGroupDirectory,
                size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
                fs_id,
                0,
                generation,
            )?,
            group,
            state: state as u32,
            free_blocks,
            bitmap_checksum,
            checkpoint_generation,
            mutation_epoch,
            chunks,
            reserved: [0; 56],
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.header.validate(
            PersistentMetadataKind::AllocationGroupDirectory,
            size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
        )?;
        if self.header.inode() != 0
            || self.header.generation() == 0
            || self.checkpoint_generation > self.header.generation()
        {
            return_errno_with_message!(Errno::EINVAL, "invalid allocation group directory");
        }
        let state = self.state()?;
        if state.is_resident() && self.chunks.iter().any(PersistentObjectRef::is_none) {
            return_errno_with_message!(Errno::EUCLEAN, "resident allocation image has no chunks");
        }
        if !state.is_resident() && self.chunks.iter().any(|reference| !reference.is_none()) {
            return_errno_with_message!(Errno::EUCLEAN, "evicted allocation image retains chunks");
        }
        Ok(())
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != size_of::<Self>() {
            return_errno_with_message!(Errno::EINVAL, "invalid allocation directory size");
        }
        let record = Self::from_bytes(bytes);
        record.validate()?;
        Ok(record)
    }

    pub(crate) const fn header(&self) -> &PersistentMetadataHeader {
        &self.header
    }

    pub(crate) const fn group(&self) -> u32 {
        self.group
    }

    pub(crate) fn state(&self) -> Result<AllocationGroupState> {
        AllocationGroupState::from_u32(self.state)
    }

    pub(crate) const fn free_blocks(&self) -> u32 {
        self.free_blocks
    }

    pub(crate) const fn bitmap_checksum(&self) -> u32 {
        self.bitmap_checksum
    }

    pub(crate) const fn checkpoint_generation(&self) -> u64 {
        self.checkpoint_generation
    }

    pub(crate) const fn mutation_epoch(&self) -> u64 {
        self.mutation_epoch
    }

    pub(crate) const fn chunks(&self) -> [PersistentObjectRef; 2] {
        self.chunks
    }
}

impl PersistentMetadataRecord for AllocationGroupDirectoryRecord {
    fn metadata_header(&self) -> &PersistentMetadataHeader {
        self.header()
    }

    fn validate_record(&self) -> Result<()> {
        self.validate()
    }

    fn decode_record(bytes: &[u8]) -> Result<Self> {
        Self::decode(bytes)
    }
}

impl AuthoritativeMetadataRecord for AllocationGroupDirectoryRecord {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Pod)]
pub(crate) struct AllocationBitmapChunkRecord {
    header: PersistentMetadataHeader,
    group: u32,
    chunk_index: u16,
    chunk_count: u16,
    data_len: u32,
    checksum: u32,
    reserved0: u64,
    data: [u8; ALLOCATION_BITMAP_CHUNK_BYTES],
}

impl AllocationBitmapChunkRecord {
    pub(crate) fn new(
        fs_id: u64,
        group: u32,
        generation: u64,
        chunk_index: u16,
        data: &[u8],
    ) -> Result<Self> {
        if generation == 0
            || chunk_index >= ALLOCATION_BITMAP_CHUNKS
            || data.len() != ALLOCATION_BITMAP_CHUNK_BYTES
        {
            return_errno_with_message!(Errno::EINVAL, "invalid allocation bitmap chunk");
        }
        let mut payload = [0u8; ALLOCATION_BITMAP_CHUNK_BYTES];
        payload.copy_from_slice(data);
        let record = Self {
            header: PersistentMetadataHeader::new(
                PersistentMetadataKind::AllocationBitmapChunk,
                size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
                fs_id,
                0,
                generation,
            )?,
            group,
            chunk_index,
            chunk_count: ALLOCATION_BITMAP_CHUNKS,
            data_len: ALLOCATION_BITMAP_CHUNK_BYTES as u32,
            checksum: allocation_bitmap_checksum(&payload),
            reserved0: 0,
            data: payload,
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.header.validate(
            PersistentMetadataKind::AllocationBitmapChunk,
            size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
        )?;
        if self.header.inode() != 0
            || self.header.generation() == 0
            || self.chunk_index >= ALLOCATION_BITMAP_CHUNKS
            || self.chunk_count != ALLOCATION_BITMAP_CHUNKS
            || self.data_len as usize != ALLOCATION_BITMAP_CHUNK_BYTES
            || self.checksum != allocation_bitmap_checksum(&self.data)
        {
            return_errno_with_message!(Errno::EUCLEAN, "invalid allocation bitmap chunk");
        }
        Ok(())
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != size_of::<Self>() {
            return_errno_with_message!(Errno::EINVAL, "invalid allocation bitmap chunk size");
        }
        let record = Self::from_bytes(bytes);
        record.validate()?;
        Ok(record)
    }

    pub(crate) const fn header(&self) -> &PersistentMetadataHeader {
        &self.header
    }

    pub(crate) const fn group(&self) -> u32 {
        self.group
    }

    pub(crate) const fn chunk_index(&self) -> u16 {
        self.chunk_index
    }

    pub(crate) const fn data(&self) -> &[u8; ALLOCATION_BITMAP_CHUNK_BYTES] {
        &self.data
    }

    #[cfg(ktest)]
    pub(crate) fn corrupt_payload_for_test(&mut self, index: usize) {
        self.data[index] ^= 1;
    }
}

impl PersistentMetadataRecord for AllocationBitmapChunkRecord {
    fn metadata_header(&self) -> &PersistentMetadataHeader {
        self.header()
    }

    fn validate_record(&self) -> Result<()> {
        self.validate()
    }

    fn decode_record(bytes: &[u8]) -> Result<Self> {
        Self::decode(bytes)
    }
}

impl AuthoritativeMetadataRecord for AllocationBitmapChunkRecord {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AllocationGroupImage {
    generation: u64,
    mutation_epoch: u64,
    bitmap: [u8; PAGE_SIZE],
    free_blocks: u32,
    checksum: u32,
}

pub(crate) struct AllocationMetadataCache {
    pool: Arc<Rnvpc>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResidentAllocationGroup {
    pub(crate) group: u32,
    pub(crate) generation: u64,
    pub(crate) free_blocks: u32,
}

impl AllocationMetadataCache {
    pub(crate) fn new(pool: Arc<Rnvpc>) -> Self {
        Self { pool }
    }

    pub(crate) fn publish_initial(
        &self,
        fs_id: u64,
        group: u32,
        image: &AllocationGroupImage,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef> {
        let directory_key = allocation_metadata_key(fs_id, group, 0)?;
        if self.pool.current_metadata_ref(directory_key).is_some() {
            return_errno_with_message!(Errno::EEXIST, "allocation group is already admitted");
        }
        let chunks = allocation_chunk_records(fs_id, group, image)?;
        let chunk_refs = [
            self.pool.persist_authoritative_metadata(
                allocation_metadata_key(fs_id, group, 1)?,
                &chunks[0],
                None,
            )?,
            self.pool.persist_authoritative_metadata(
                allocation_metadata_key(fs_id, group, 2)?,
                &chunks[1],
                None,
            )?,
        ];
        let directory = AllocationGroupDirectoryRecord::resident_dirty_with_chunks(
            fs_id,
            group,
            image.generation(),
            image.free_blocks(),
            image.checksum(),
            image.mutation_epoch(),
            chunk_refs,
        )?;
        self.pool
            .persist_authoritative_metadata(directory_key, &directory, fault)
    }

    pub(crate) fn publish_update(
        &self,
        fs_id: u64,
        group: u32,
        image: &AllocationGroupImage,
        fault: Option<FaultPoint>,
    ) -> Result<PersistentObjectRef> {
        let directory_key = allocation_metadata_key(fs_id, group, 0)?;
        let current_directory_ref = self
            .pool
            .current_metadata_ref(directory_key)
            .ok_or_else(|| Error::new(Errno::ENOENT))?;
        let current_directory = self
            .pool
            .read_authoritative_metadata::<AllocationGroupDirectoryRecord>(current_directory_ref)?;
        if image.generation() <= current_directory.header().generation() {
            return_errno_with_message!(Errno::ESTALE, "allocation generation did not advance");
        }
        let current_chunks = current_directory.chunks();
        let chunks = allocation_chunk_records(fs_id, group, image)?;
        let chunk_refs = if current_directory.state()?.is_resident() {
            if current_chunks.iter().any(PersistentObjectRef::is_none) {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "resident allocation directory lost chunks"
                );
            }
            [
                self.pool
                    .replace_authoritative_metadata(current_chunks[0], &chunks[0], None)?,
                self.pool
                    .replace_authoritative_metadata(current_chunks[1], &chunks[1], None)?,
            ]
        } else {
            [
                self.pool.persist_authoritative_metadata(
                    allocation_metadata_key(fs_id, group, 1)?,
                    &chunks[0],
                    None,
                )?,
                self.pool.persist_authoritative_metadata(
                    allocation_metadata_key(fs_id, group, 2)?,
                    &chunks[1],
                    None,
                )?,
            ]
        };
        let directory = AllocationGroupDirectoryRecord::resident_dirty_with_chunks(
            fs_id,
            group,
            image.generation(),
            image.free_blocks(),
            image.checksum(),
            image.mutation_epoch(),
            chunk_refs,
        )?;
        let new_directory_ref =
            self.pool
                .replace_authoritative_metadata(current_directory_ref, &directory, fault)?;
        // Once the directory commit point selects the new chunks, the former
        // graph is no longer a recovery candidate. Reclaim it so repeated rTIO
        // batches consume a fixed three-frame working set per resident group.
        self.pool
            .reclaim_committed_metadata(current_directory_ref)?;
        for reference in current_chunks {
            if !reference.is_none() {
                self.pool.reclaim_committed_metadata(reference)?;
            }
        }
        Ok(new_directory_ref)
    }

    pub(crate) fn selected_image(
        &self,
        fs_id: u64,
        group: u32,
    ) -> Result<Option<AllocationGroupImage>> {
        let Some(directory_ref) = self
            .pool
            .current_metadata_ref(allocation_metadata_key(fs_id, group, 0)?)
        else {
            return Ok(None);
        };
        let directory = self
            .pool
            .read_authoritative_metadata::<AllocationGroupDirectoryRecord>(directory_ref)?;
        if !directory.state()?.is_resident() {
            return Ok(None);
        }
        let chunk_refs = directory.chunks();
        if chunk_refs.iter().any(PersistentObjectRef::is_none) {
            return_errno_with_message!(Errno::EUCLEAN, "resident allocation directory lost chunks");
        }
        let first = self
            .pool
            .read_authoritative_metadata::<AllocationBitmapChunkRecord>(chunk_refs[0])
            .map_err(|_| {
                Error::with_message(
                    Errno::EUCLEAN,
                    "selected allocation bitmap chunk is missing or corrupt",
                )
            })?;
        let second = self
            .pool
            .read_authoritative_metadata::<AllocationBitmapChunkRecord>(chunk_refs[1])
            .map_err(|_| {
                Error::with_message(
                    Errno::EUCLEAN,
                    "selected allocation bitmap chunk is missing or corrupt",
                )
            })?;
        select_allocation_group_image(&directory, [&first, &second]).map(Some)
    }

    /// Publishes one complete dirty group image. The directory replacement is
    /// the commit point and must precede persistent migration-target pointers.
    pub(crate) fn publish_dirty_bitmap(
        &self,
        fs_id: u64,
        group: u32,
        mutation_epoch: u64,
        bitmap: [u8; PAGE_SIZE],
        free_blocks: u32,
    ) -> Result<u64> {
        let directory_key = allocation_metadata_key(fs_id, group, 0)?;
        let current = self.pool.current_metadata_ref(directory_key);
        let generation = current.map_or(Ok(1), |reference| {
            self.pool
                .read_authoritative_metadata::<AllocationGroupDirectoryRecord>(reference)?
                .header()
                .generation()
                .checked_add(1)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))
        })?;
        let image =
            AllocationGroupImage::new_with_epoch(generation, mutation_epoch, bitmap, free_blocks)?;
        if current.is_some() {
            self.publish_update(fs_id, group, &image, None)?;
        } else {
            self.publish_initial(fs_id, group, &image, None)?;
        }
        Ok(generation)
    }

    /// Transfers authority to an SSD checkpoint of exactly `generation` and
    /// then releases the no-longer-needed resident bitmap chunks.
    pub(crate) fn mark_checkpointed(&self, fs_id: u64, group: u32, generation: u64) -> Result<()> {
        let key = allocation_metadata_key(fs_id, group, 0)?;
        let current_ref = self
            .pool
            .current_metadata_ref(key)
            .ok_or_else(|| Error::new(Errno::ENOENT))?;
        let current = self
            .pool
            .read_authoritative_metadata::<AllocationGroupDirectoryRecord>(current_ref)?;
        if current.header().generation() != generation || !current.state()?.is_resident() {
            return_errno_with_message!(Errno::ESTALE, "allocation checkpoint generation changed");
        }
        let chunks = current.chunks();
        let clean = AllocationGroupDirectoryRecord::clean_on_ssd(
            fs_id,
            group,
            generation,
            current.free_blocks(),
            current.bitmap_checksum(),
            current.mutation_epoch(),
        )?;
        self.pool
            .replace_authoritative_metadata(current_ref, &clean, None)?;
        self.pool.reclaim_committed_metadata(current_ref)?;
        for reference in chunks {
            self.pool.reclaim_committed_metadata(reference)?;
        }
        Ok(())
    }

    pub(crate) fn resident_groups(&self, fs_id: u64) -> Result<Vec<ResidentAllocationGroup>> {
        let mut groups = Vec::new();
        for reference in self.pool.committed_metadata_refs() {
            if self.pool.persistent_metadata_kind(reference)?
                != PersistentMetadataKind::AllocationGroupDirectory
            {
                continue;
            }
            let key = self.pool.persistent_metadata_key(reference)?;
            if key.fs_id != fs_id || self.pool.current_metadata_ref(key) != Some(reference) {
                continue;
            }
            let directory = self
                .pool
                .read_authoritative_metadata::<AllocationGroupDirectoryRecord>(reference)?;
            if directory.state()?.is_resident() {
                groups.push(ResidentAllocationGroup {
                    group: directory.group(),
                    generation: directory.header().generation(),
                    free_blocks: directory.free_blocks(),
                });
            }
        }
        Ok(groups)
    }
}

impl AllocationGroupImage {
    pub(crate) fn new(generation: u64, bitmap: [u8; PAGE_SIZE], free_blocks: u32) -> Result<Self> {
        Self::new_with_epoch(generation, 0, bitmap, free_blocks)
    }

    pub(crate) fn new_with_epoch(
        generation: u64,
        mutation_epoch: u64,
        bitmap: [u8; PAGE_SIZE],
        free_blocks: u32,
    ) -> Result<Self> {
        if generation == 0 {
            return_errno_with_message!(Errno::EINVAL, "allocation image generation is zero");
        }
        let checksum = allocation_bitmap_checksum(&bitmap);
        Ok(Self {
            generation,
            mutation_epoch,
            bitmap,
            free_blocks,
            checksum,
        })
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) const fn bitmap(&self) -> &[u8; PAGE_SIZE] {
        &self.bitmap
    }

    pub(crate) const fn mutation_epoch(&self) -> u64 {
        self.mutation_epoch
    }

    pub(crate) const fn free_blocks(&self) -> u32 {
        self.free_blocks
    }

    pub(crate) const fn checksum(&self) -> u32 {
        self.checksum
    }
}

pub(crate) fn select_allocation_group_image(
    directory: &AllocationGroupDirectoryRecord,
    chunks: [&AllocationBitmapChunkRecord; 2],
) -> Result<AllocationGroupImage> {
    directory
        .validate()
        .map_err(|_| Error::new(Errno::EUCLEAN))?;
    if !directory
        .state()
        .map_err(|_| Error::new(Errno::EUCLEAN))?
        .is_resident()
    {
        return_errno_with_message!(Errno::EINVAL, "SSD-authoritative group has no NVM bitmap");
    }
    let mut bitmap = [0u8; PAGE_SIZE];
    let mut selected_chunks = [false; ALLOCATION_BITMAP_CHUNKS as usize];
    let generation = directory.header().generation();
    for chunk in chunks {
        chunk.validate().map_err(|_| Error::new(Errno::EUCLEAN))?;
        if chunk.header().fs_id() != directory.header().fs_id()
            || chunk.group() != directory.group()
            || chunk.header().generation() != generation
        {
            return_errno_with_message!(Errno::EUCLEAN, "allocation bitmap identity mismatch");
        }
        let chunk_index = usize::from(chunk.chunk_index());
        if selected_chunks[chunk_index] {
            return_errno_with_message!(Errno::EUCLEAN, "duplicate allocation bitmap chunk");
        }
        selected_chunks[chunk_index] = true;
        let start = chunk_index * ALLOCATION_BITMAP_CHUNK_BYTES;
        bitmap[start..start + ALLOCATION_BITMAP_CHUNK_BYTES].copy_from_slice(chunk.data());
    }
    let image = AllocationGroupImage::new_with_epoch(
        generation,
        directory.mutation_epoch(),
        bitmap,
        directory.free_blocks(),
    )?;
    if image.checksum() != directory.bitmap_checksum() {
        return_errno_with_message!(Errno::EUCLEAN, "allocation bitmap checksum mismatch");
    }
    Ok(image)
}

fn allocation_bitmap_checksum(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn allocation_chunk_records(
    fs_id: u64,
    group: u32,
    image: &AllocationGroupImage,
) -> Result<[AllocationBitmapChunkRecord; 2]> {
    Ok([
        AllocationBitmapChunkRecord::new(
            fs_id,
            group,
            image.generation(),
            0,
            &image.bitmap()[..ALLOCATION_BITMAP_CHUNK_BYTES],
        )?,
        AllocationBitmapChunkRecord::new(
            fs_id,
            group,
            image.generation(),
            1,
            &image.bitmap()[ALLOCATION_BITMAP_CHUNK_BYTES..],
        )?,
    ])
}

fn allocation_metadata_key(fs_id: u64, group: u32, component: u64) -> Result<PageKey> {
    let group_index = u64::from(group)
        .checked_mul(4)
        .and_then(|value| value.checked_add(component))
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
    Ok(PageKey::new(
        fs_id,
        0,
        ALLOCATION_METADATA_LOGICAL_INDEX_TAG | group_index,
    ))
}

const _: () = {
    assert!(size_of::<AllocationGroupDirectoryRecord>() <= PAGE_SIZE);
    assert!(size_of::<AllocationBitmapChunkRecord>() <= PAGE_SIZE);
};
