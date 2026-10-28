// SPDX-License-Identifier: MPL-2.0

//! NVM-authoritative EXT4 block-allocation metadata.

use super::metadata::{
    AuthoritativeMetadataRecord, PersistentMetadataHeader, PersistentMetadataKind,
    PersistentMetadataRecord, PersistentObjectRef,
};
use crate::prelude::*;

pub(crate) const ALLOCATION_BITMAP_CHUNK_BYTES: usize = PAGE_SIZE / 2;
const ALLOCATION_BITMAP_CHUNKS: u16 = 2;

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

    const fn is_resident(self) -> bool {
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
    chunks: [PersistentObjectRef; 2],
    reserved: [u8; 64],
}

impl AllocationGroupDirectoryRecord {
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
            [PersistentObjectRef::NONE; 2],
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
                u64::from(group),
                generation,
            )?,
            group,
            state: state as u32,
            free_blocks,
            bitmap_checksum,
            checkpoint_generation,
            chunks,
            reserved: [0; 64],
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.header.validate(
            PersistentMetadataKind::AllocationGroupDirectory,
            size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
        )?;
        if self.header.inode() != u64::from(self.group)
            || self.header.generation() == 0
            || self.checkpoint_generation > self.header.generation()
        {
            return_errno_with_message!(Errno::EINVAL, "invalid allocation group directory");
        }
        self.state()?;
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
                u64::from(group),
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
        if self.header.inode() != u64::from(self.group)
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
    bitmap: [u8; PAGE_SIZE],
    free_blocks: u32,
    checksum: u32,
}

impl AllocationGroupImage {
    pub(crate) fn new(generation: u64, bitmap: [u8; PAGE_SIZE], free_blocks: u32) -> Result<Self> {
        if generation == 0 {
            return_errno_with_message!(Errno::EINVAL, "allocation image generation is zero");
        }
        let checksum = allocation_bitmap_checksum(&bitmap);
        Ok(Self {
            generation,
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
    let image = AllocationGroupImage::new(generation, bitmap, directory.free_blocks())?;
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

const _: () = {
    assert!(size_of::<AllocationGroupDirectoryRecord>() <= PAGE_SIZE);
    assert!(size_of::<AllocationBitmapChunkRecord>() <= PAGE_SIZE);
};
