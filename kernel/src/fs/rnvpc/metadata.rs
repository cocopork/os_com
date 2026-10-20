// SPDX-License-Identifier: MPL-2.0

//! Persistent rNVPCe metadata records stored in NVBH payloads.

use core::ops::Deref;

use super::extent_target::{ExtentTarget, ExtentTargetKind};
use crate::prelude::*;

const PERSISTENT_METADATA_MAGIC: u64 = 0x524e_5650_434d_4431;
const PERSISTENT_METADATA_VERSION: u16 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub(crate) enum PersistentMetadataKind {
    SpaceReservation = 3,
    ExtentNode = 4,
    Inode = 5,
    PendingExtentMutation = 6,
    /// Transitional kind used only by pre-rNVPCe batch helpers. New writes
    /// must use `SpaceReservation` and choose SSD blocks during migration.
    Allocation = 7,
}

impl PersistentMetadataKind {
    fn from_u16(value: u16) -> Result<Self> {
        match value {
            3 => Ok(Self::SpaceReservation),
            4 => Ok(Self::ExtentNode),
            5 => Ok(Self::Inode),
            6 => Ok(Self::PendingExtentMutation),
            7 => Ok(Self::Allocation),
            _ => return_errno_with_message!(Errno::EINVAL, "unknown persistent metadata kind"),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Pod)]
pub(crate) struct PersistentMetadataHeader {
    magic: u64,
    version: u16,
    kind: u16,
    header_len: u16,
    flags: u16,
    payload_len: u32,
    reserved0: u32,
    fs_id: u64,
    inode: u64,
    generation: u64,
    reserved: [u8; 16],
}

const _: () = {
    assert!(size_of::<PersistentMetadataHeader>() == 64);
    assert!(align_of::<PersistentMetadataHeader>() == 8);
};

impl PersistentMetadataHeader {
    fn new(
        kind: PersistentMetadataKind,
        payload_len: usize,
        fs_id: u64,
        inode: u64,
        generation: u64,
    ) -> Result<Self> {
        let payload_len = u32::try_from(payload_len).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        Ok(Self {
            magic: PERSISTENT_METADATA_MAGIC,
            version: PERSISTENT_METADATA_VERSION,
            kind: kind as u16,
            header_len: size_of::<Self>() as u16,
            flags: 0,
            payload_len,
            reserved0: 0,
            fs_id,
            inode,
            generation,
            reserved: [0; 16],
        })
    }

    pub(crate) fn kind(&self) -> Result<PersistentMetadataKind> {
        PersistentMetadataKind::from_u16(self.kind)
    }

    pub(crate) fn decode_kind(bytes: &[u8]) -> Result<PersistentMetadataKind> {
        if bytes.len() != size_of::<Self>() {
            return_errno_with_message!(Errno::EINVAL, "invalid persistent metadata header size");
        }
        let header = Self::from_bytes(bytes);
        if header.magic != PERSISTENT_METADATA_MAGIC
            || header.version != PERSISTENT_METADATA_VERSION
            || header.header_len as usize != size_of::<Self>()
        {
            return_errno_with_message!(Errno::EINVAL, "invalid persistent metadata header");
        }
        header.kind()
    }

    pub(crate) const fn fs_id(&self) -> u64 {
        self.fs_id
    }

    pub(crate) const fn inode(&self) -> u64 {
        self.inode
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    fn validate(
        &self,
        expected_kind: PersistentMetadataKind,
        expected_payload_len: usize,
    ) -> Result<()> {
        if self.magic != PERSISTENT_METADATA_MAGIC
            || self.version != PERSISTENT_METADATA_VERSION
            || self.header_len as usize != size_of::<Self>()
            || self.payload_len as usize != expected_payload_len
            || self.kind()? != expected_kind
        {
            return_errno_with_message!(Errno::EINVAL, "invalid persistent metadata header");
        }
        Ok(())
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Pod)]
pub(crate) struct PersistentObjectRef {
    slot_plus_one: u64,
    allocation_seq: u64,
}

impl PersistentObjectRef {
    pub(crate) const NONE: Self = Self {
        slot_plus_one: 0,
        allocation_seq: 0,
    };

    pub(crate) fn new(slot: u64, allocation_seq: u64) -> Result<Self> {
        if allocation_seq == 0 {
            return_errno_with_message!(Errno::EINVAL, "persistent object generation is zero");
        }
        let slot_plus_one = slot
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        Ok(Self {
            slot_plus_one,
            allocation_seq,
        })
    }

    pub(crate) const fn is_none(&self) -> bool {
        self.slot_plus_one == 0
    }

    pub(crate) fn slot(&self) -> Option<u64> {
        self.slot_plus_one.checked_sub(1)
    }

    pub(crate) const fn allocation_seq(&self) -> u64 {
        self.allocation_seq
    }

    pub(crate) const fn slot_plus_one(&self) -> u64 {
        self.slot_plus_one
    }

    fn validate(&self) -> Result<()> {
        if self.is_none() {
            if self.allocation_seq != 0 {
                return_errno_with_message!(Errno::EINVAL, "null object reference has a generation");
            }
        } else if self.allocation_seq == 0 {
            return_errno_with_message!(Errno::EINVAL, "persistent object generation is zero");
        }
        Ok(())
    }
}

const _: () = {
    assert!(size_of::<PersistentObjectRef>() == 16);
    assert!(align_of::<PersistentObjectRef>() == 8);
};

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Pod)]
pub(crate) struct SpaceReservationRecord {
    header: PersistentMetadataHeader,
    reserved_blocks: u64,
    consumed_blocks: u64,
    flags: u32,
    reserved0: u32,
    reserved: [u8; 40],
}

impl SpaceReservationRecord {
    pub(crate) fn new(
        fs_id: u64,
        inode: u64,
        generation: u64,
        reserved_blocks: u64,
        consumed_blocks: u64,
    ) -> Result<Self> {
        let record = Self {
            header: PersistentMetadataHeader::new(
                PersistentMetadataKind::SpaceReservation,
                size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
                fs_id,
                inode,
                generation,
            )?,
            reserved_blocks,
            consumed_blocks,
            flags: 0,
            reserved0: 0,
            reserved: [0; 40],
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != size_of::<Self>() {
            return_errno_with_message!(Errno::EINVAL, "invalid space reservation record size");
        }
        let record = Self::from_bytes(bytes);
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.header.validate(
            PersistentMetadataKind::SpaceReservation,
            size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
        )?;
        if self.reserved_blocks == 0 {
            return_errno_with_message!(Errno::EINVAL, "space reservation is empty");
        }
        if self.consumed_blocks > self.reserved_blocks {
            return_errno_with_message!(Errno::EINVAL, "space reservation is over-consumed");
        }
        Ok(())
    }

    pub(crate) const fn header(&self) -> &PersistentMetadataHeader {
        &self.header
    }

    pub(crate) const fn reserved_blocks(&self) -> u64 {
        self.reserved_blocks
    }

    pub(crate) const fn consumed_blocks(&self) -> u64 {
        self.consumed_blocks
    }

    pub(crate) const fn outstanding_blocks(&self) -> u64 {
        self.reserved_blocks - self.consumed_blocks
    }
}

const _: () = {
    assert!(size_of::<SpaceReservationRecord>() == 128);
    assert!(align_of::<SpaceReservationRecord>() == 8);
};

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Pod)]
pub(crate) struct AllocationRecord {
    header: PersistentMetadataHeader,
    block_group: u32,
    block_count: u32,
    first_block: u64,
    flags: u32,
    reserved0: u32,
    reserved: [u8; 40],
}

impl AllocationRecord {
    pub(crate) fn new(
        fs_id: u64,
        inode: u64,
        generation: u64,
        block_group: u32,
        first_block: u64,
        block_count: u32,
    ) -> Result<Self> {
        let record = Self {
            header: PersistentMetadataHeader::new(
                PersistentMetadataKind::Allocation,
                size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
                fs_id,
                inode,
                generation,
            )?,
            block_group,
            block_count,
            first_block,
            flags: 0,
            reserved0: 0,
            reserved: [0; 40],
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != size_of::<Self>() {
            return_errno_with_message!(Errno::EINVAL, "invalid allocation record size");
        }
        let record = Self::from_bytes(bytes);
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.header.validate(
            PersistentMetadataKind::Allocation,
            size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
        )?;
        if self.block_count == 0 {
            return_errno_with_message!(Errno::EINVAL, "allocation record is empty");
        }
        self.first_block
            .checked_add(self.block_count as u64)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        Ok(())
    }

    pub(crate) const fn header(&self) -> &PersistentMetadataHeader {
        &self.header
    }

    pub(crate) const fn block_group(&self) -> u32 {
        self.block_group
    }

    pub(crate) const fn first_block(&self) -> u64 {
        self.first_block
    }

    pub(crate) const fn block_count(&self) -> u32 {
        self.block_count
    }
}

const _: () = {
    assert!(size_of::<AllocationRecord>() == 128);
    assert!(align_of::<AllocationRecord>() == 8);
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentDataRef {
    Ssd(u64),
    Nv(u64),
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Pod)]
pub(crate) struct ExtentRecordEntry {
    logical_block: u64,
    range_tail: u64,
    target: ExtentTarget,
}

impl ExtentRecordEntry {
    pub(crate) fn leaf(
        logical_block: u64,
        length: u32,
        physical_block: u64,
        unwritten: bool,
    ) -> Result<Self> {
        if length == 0 {
            return_errno_with_message!(Errno::EINVAL, "persistent extent is empty");
        }
        logical_block
            .checked_add(length as u64)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        physical_block
            .checked_add(length as u64)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        Ok(Self {
            logical_block,
            range_tail: u64::from(length),
            target: ExtentTarget::ssd(physical_block, unwritten)?,
        })
    }

    pub(crate) fn nv_leaf(
        logical_block: u64,
        data: PersistentObjectRef,
        _unwritten: bool,
    ) -> Result<Self> {
        data.validate()?;
        if data.is_none() {
            return_errno_with_message!(Errno::EINVAL, "NV extent data reference is null");
        }
        Self::nv_leaf_slot(logical_block, data.slot_plus_one)
    }

    pub(crate) fn nv_leaf_slot(logical_block: u64, slot_plus_one: u64) -> Result<Self> {
        Ok(Self {
            logical_block,
            range_tail: 1,
            target: ExtentTarget::nv_page(slot_plus_one)?,
        })
    }

    pub(crate) fn child(logical_block: u64, child: PersistentObjectRef) -> Result<Self> {
        child.validate()?;
        if child.is_none() {
            return_errno_with_message!(Errno::EINVAL, "extent child reference is null");
        }
        Ok(Self {
            logical_block,
            range_tail: 0,
            target: ExtentTarget::child(child.slot_plus_one)?,
        })
    }

    pub(crate) const fn logical_block(&self) -> u64 {
        self.logical_block
    }

    pub(crate) const fn length(&self) -> u32 {
        self.range_tail as u32
    }

    pub(crate) fn ssd_block(&self) -> Result<u64> {
        match self.data_ref()? {
            ExtentDataRef::Ssd(block) => Ok(block),
            ExtentDataRef::Nv(_) => {
                return_errno_with_message!(Errno::EINVAL, "NV extent has no SSD block")
            }
        }
    }

    pub(crate) fn is_unwritten(&self) -> bool {
        matches!(self.target.kind(), Ok(ExtentTargetKind::SsdUnwritten))
    }

    pub(crate) fn is_child(&self) -> bool {
        matches!(self.target.kind(), Ok(ExtentTargetKind::Child))
    }

    pub(crate) fn is_nv_data(&self) -> bool {
        matches!(self.target.kind(), Ok(ExtentTargetKind::NvPage))
    }

    pub(crate) fn data_ref(&self) -> Result<ExtentDataRef> {
        if self.is_child() {
            return_errno_with_message!(Errno::EINVAL, "extent index is not a data mapping");
        }
        if self.is_nv_data() {
            Ok(ExtentDataRef::Nv(self.target.payload()?))
        } else {
            match self.target.kind()? {
                ExtentTargetKind::SsdWritten | ExtentTargetKind::SsdUnwritten => {
                    Ok(ExtentDataRef::Ssd(self.target.payload()?))
                }
                _ => return_errno_with_message!(Errno::EINVAL, "invalid extent data target"),
            }
        }
    }

    pub(crate) fn child_slot(&self) -> Option<u64> {
        self.is_child()
            .then(|| self.target.payload().ok())
            .flatten()
    }

    pub(crate) const fn target(&self) -> ExtentTarget {
        self.target
    }

    pub(crate) const fn range_tail(&self) -> u64 {
        self.range_tail
    }

    pub(crate) fn validate_for_level(&self, level: u16) -> Result<()> {
        if self.range_tail >> 32 != 0 || !self.target.is_published() {
            return_errno_with_message!(Errno::EINVAL, "invalid persistent extent entry");
        }
        if level == 0 {
            if self.is_child() || self.length() == 0 {
                return_errno_with_message!(Errno::EINVAL, "invalid persistent leaf extent");
            }
            self.logical_block
                .checked_add(self.length() as u64)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            if self.is_nv_data() {
                if self.length() != 1 {
                    return_errno_with_message!(
                        Errno::EINVAL,
                        "NV extent entries map exactly one NVPage"
                    );
                }
                self.data_ref()?;
            } else {
                self.target
                    .payload()?
                    .checked_add(self.length() as u64)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            }
        } else {
            if !self.is_child() || self.length() != 0 {
                return_errno_with_message!(Errno::EINVAL, "invalid persistent extent index");
            }
            self.target.payload()?;
        }
        Ok(())
    }
}

const _: () = {
    assert!(size_of::<ExtentRecordEntry>() == 24);
    assert!(align_of::<ExtentRecordEntry>() == 8);
};

pub(crate) const EXTENT_NODE_ENTRY_CAPACITY: usize = 165;

pub(crate) struct BoxedExtentNodeRecord {
    bytes: Box<[u8]>,
}

impl Deref for BoxedExtentNodeRecord {
    type Target = ExtentNodeRecord;

    fn deref(&self) -> &Self::Target {
        ExtentNodeRecord::ref_from_bytes(&self.bytes)
            .expect("heap extent-node storage has the validated layout")
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Pod)]
pub(crate) struct ExtentNodeRecord {
    header: PersistentMetadataHeader,
    node_id: u64,
    parent: PersistentObjectRef,
    right_sibling_slot_plus_one: u64,
    level: u16,
    entry_count: u16,
    flags: u32,
    entries: [ExtentRecordEntry; EXTENT_NODE_ENTRY_CAPACITY],
    reserved: [u8; 32],
}

impl Default for ExtentNodeRecord {
    fn default() -> Self {
        Self {
            header: PersistentMetadataHeader::default(),
            node_id: 0,
            parent: PersistentObjectRef::NONE,
            right_sibling_slot_plus_one: 0,
            level: 0,
            entry_count: 0,
            flags: 0,
            entries: [ExtentRecordEntry::default(); EXTENT_NODE_ENTRY_CAPACITY],
            reserved: [0; 32],
        }
    }
}

impl ExtentNodeRecord {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        fs_id: u64,
        inode: u64,
        generation: u64,
        node_id: u64,
        level: u16,
        parent: PersistentObjectRef,
        right_sibling: PersistentObjectRef,
        entries: &[ExtentRecordEntry],
    ) -> Result<Self> {
        if entries.len() > EXTENT_NODE_ENTRY_CAPACITY
            || (entries.is_empty() && (level != 0 || !parent.is_none() || !right_sibling.is_none()))
        {
            return_errno_with_message!(Errno::EINVAL, "invalid persistent extent node size");
        }
        let mut record = Self {
            header: PersistentMetadataHeader::new(
                PersistentMetadataKind::ExtentNode,
                size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
                fs_id,
                inode,
                generation,
            )?,
            node_id,
            parent,
            right_sibling_slot_plus_one: right_sibling.slot_plus_one(),
            level,
            entry_count: entries.len() as u16,
            flags: 0,
            entries: [ExtentRecordEntry::default(); EXTENT_NODE_ENTRY_CAPACITY],
            reserved: [0; 32],
        };
        record.entries[..entries.len()].copy_from_slice(entries);
        record.validate()?;
        Ok(record)
    }

    /// Constructs a 4 KiB extent node directly in heap storage.
    ///
    /// VFS write paths have limited kernel stacks. Building `Self` by value in
    /// a deep syscall path can overflow that stack before the value is moved
    /// into a `Box`, so hot mutation paths use this constructor instead.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_boxed(
        fs_id: u64,
        inode: u64,
        generation: u64,
        node_id: u64,
        level: u16,
        parent: PersistentObjectRef,
        right_sibling: PersistentObjectRef,
        entries: &[ExtentRecordEntry],
    ) -> Result<BoxedExtentNodeRecord> {
        if entries.len() > EXTENT_NODE_ENTRY_CAPACITY
            || (entries.is_empty() && (level != 0 || !parent.is_none() || !right_sibling.is_none()))
        {
            return_errno_with_message!(Errno::EINVAL, "invalid persistent extent node size");
        }
        let header = PersistentMetadataHeader::new(
            PersistentMetadataKind::ExtentNode,
            size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
            fs_id,
            inode,
            generation,
        )?;
        let mut bytes = vec![0u8; size_of::<Self>()].into_boxed_slice();
        let record = Self::mut_from_bytes(&mut bytes).map_err(|_| Error::new(Errno::EUCLEAN))?;
        record.header = header;
        record.node_id = node_id;
        record.parent = parent;
        record.right_sibling_slot_plus_one = right_sibling.slot_plus_one();
        record.level = level;
        record.entry_count = entries.len() as u16;
        record.flags = 0;
        record.entries.fill(ExtentRecordEntry::default());
        record.entries[..entries.len()].copy_from_slice(entries);
        record.reserved.fill(0);
        record.validate()?;
        Ok(BoxedExtentNodeRecord { bytes })
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != size_of::<Self>() {
            return_errno_with_message!(Errno::EINVAL, "invalid persistent extent node size");
        }
        let record = Self::from_bytes(bytes);
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.header.validate(
            PersistentMetadataKind::ExtentNode,
            size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
        )?;
        self.parent.validate()?;
        let count = self.entry_count as usize;
        if count > EXTENT_NODE_ENTRY_CAPACITY
            || (count == 0
                && (self.level != 0
                    || !self.parent.is_none()
                    || self.right_sibling_slot_plus_one != 0))
        {
            return_errno_with_message!(Errno::EINVAL, "invalid persistent extent entry count");
        }
        for entry in &self.entries[..count] {
            entry.validate_for_level(self.level)?;
        }
        if self.level == 0 {
            validate_leaf_overlaps(&self.entries[..count])?;
        } else {
            for (index, entry) in self.entries[..count].iter().enumerate() {
                if self.entries[index + 1..count]
                    .iter()
                    .any(|other| other.logical_block() == entry.logical_block())
                {
                    return_errno_with_message!(Errno::EINVAL, "duplicate persistent extent route");
                }
            }
        }
        Ok(())
    }

    pub(crate) const fn header(&self) -> &PersistentMetadataHeader {
        &self.header
    }

    pub(crate) const fn node_id(&self) -> u64 {
        self.node_id
    }

    pub(crate) const fn level(&self) -> u16 {
        self.level
    }

    pub(crate) const fn parent(&self) -> PersistentObjectRef {
        self.parent
    }

    pub(crate) const fn right_sibling_slot(&self) -> Option<u64> {
        if self.right_sibling_slot_plus_one == 0 {
            None
        } else {
            Some(self.right_sibling_slot_plus_one)
        }
    }

    pub(crate) const fn entry_count(&self) -> usize {
        self.entry_count as usize
    }

    pub(crate) const fn level_count_flags_word(&self, entry_count: u16) -> u64 {
        (self.level as u64) | ((entry_count as u64) << 16) | ((self.flags as u64) << 32)
    }

    pub(crate) fn entries(&self) -> Result<&[ExtentRecordEntry]> {
        self.validate()?;
        Ok(&self.entries[..self.entry_count as usize])
    }

    pub(crate) fn entry(&self, index: usize) -> Result<ExtentRecordEntry> {
        self.entries()?
            .get(index)
            .copied()
            .ok_or_else(|| Error::new(Errno::EINVAL))
    }
}

fn validate_leaf_overlaps(entries: &[ExtentRecordEntry]) -> Result<()> {
    for (index, left) in entries.iter().enumerate() {
        let left_end = left.logical_block() + left.length() as u64;
        for right in &entries[index + 1..] {
            let right_end = right.logical_block() + right.length() as u64;
            let start = left.logical_block().max(right.logical_block());
            let end = left_end.min(right_end);
            if start >= end {
                continue;
            }
            let (ExtentDataRef::Ssd(left_block), ExtentDataRef::Ssd(right_block)) =
                (left.data_ref()?, right.data_ref()?)
            else {
                return_errno_with_message!(Errno::EINVAL, "persistent extents conflict");
            };
            if left.is_unwritten() != right.is_unwritten()
                || left_block + (start - left.logical_block())
                    != right_block + (start - right.logical_block())
            {
                return_errno_with_message!(Errno::EINVAL, "persistent extents conflict");
            }
        }
    }
    Ok(())
}

pub(crate) const EXTENT_NODE_ENTRIES_OFFSET: usize =
    core::mem::offset_of!(ExtentNodeRecord, entries);
pub(crate) const EXTENT_NODE_LEVEL_COUNT_FLAGS_OFFSET: usize =
    core::mem::offset_of!(ExtentNodeRecord, level);
pub(crate) const EXTENT_NODE_RIGHT_SIBLING_OFFSET: usize =
    core::mem::offset_of!(ExtentNodeRecord, right_sibling_slot_plus_one);
pub(crate) const EXTENT_ENTRY_LOGICAL_OFFSET: usize =
    core::mem::offset_of!(ExtentRecordEntry, logical_block);
pub(crate) const EXTENT_ENTRY_RANGE_OFFSET: usize =
    core::mem::offset_of!(ExtentRecordEntry, range_tail);
pub(crate) const EXTENT_ENTRY_TARGET_OFFSET: usize =
    core::mem::offset_of!(ExtentRecordEntry, target);

pub(crate) fn extent_entry_field_offset(index: usize, field: usize) -> Result<usize> {
    if index >= EXTENT_NODE_ENTRY_CAPACITY || field >= size_of::<ExtentRecordEntry>() {
        return_errno_with_message!(Errno::EINVAL, "extent entry field is out of bounds");
    }
    EXTENT_NODE_ENTRIES_OFFSET
        .checked_add(index * size_of::<ExtentRecordEntry>())
        .and_then(|offset| offset.checked_add(field))
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))
}

const _: () = {
    assert!(size_of::<ExtentNodeRecord>() == PAGE_SIZE);
    assert!(align_of::<ExtentNodeRecord>() == 8);
};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Pod)]
pub(crate) struct InodeRecord {
    header: PersistentMetadataHeader,
    extent_root: PersistentObjectRef,
    file_size: u64,
    block_count: u64,
    mode: u32,
    flags: u32,
    reserved: [u8; 24],
}

impl InodeRecord {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        fs_id: u64,
        inode: u64,
        generation: u64,
        extent_root: PersistentObjectRef,
        file_size: u64,
        block_count: u64,
        mode: u32,
        flags: u32,
    ) -> Result<Self> {
        let record = Self {
            header: PersistentMetadataHeader::new(
                PersistentMetadataKind::Inode,
                size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
                fs_id,
                inode,
                generation,
            )?,
            extent_root,
            file_size,
            block_count,
            mode,
            flags,
            reserved: [0; 24],
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != size_of::<Self>() {
            return_errno_with_message!(Errno::EINVAL, "invalid inode record size");
        }
        let record = Self::from_bytes(bytes);
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.header.validate(
            PersistentMetadataKind::Inode,
            size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
        )?;
        self.extent_root.validate()
    }

    pub(crate) const fn header(&self) -> &PersistentMetadataHeader {
        &self.header
    }

    pub(crate) const fn extent_root(&self) -> PersistentObjectRef {
        self.extent_root
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

    pub(crate) fn with_extent_root_and_size(
        &self,
        generation: u64,
        extent_root: PersistentObjectRef,
        file_size: u64,
        block_count: u64,
    ) -> Result<Self> {
        Self::new(
            self.header.fs_id(),
            self.header.inode(),
            generation,
            extent_root,
            file_size,
            block_count,
            self.mode,
            self.flags,
        )
    }

    pub(crate) fn with_extent_root(
        &self,
        generation: u64,
        extent_root: PersistentObjectRef,
    ) -> Result<Self> {
        Self::new(
            self.header.fs_id(),
            self.header.inode(),
            generation,
            extent_root,
            self.file_size,
            self.block_count,
            self.mode,
            self.flags,
        )
    }
}

const _: () = {
    assert!(size_of::<InodeRecord>() == 128);
    assert!(align_of::<InodeRecord>() == 8);
};

pub(crate) const INODE_GENERATION_OFFSET: usize = core::mem::offset_of!(InodeRecord, header)
    + core::mem::offset_of!(PersistentMetadataHeader, generation);
pub(crate) const INODE_FILE_SIZE_OFFSET: usize = core::mem::offset_of!(InodeRecord, file_size);
pub(crate) const INODE_BLOCK_COUNT_OFFSET: usize = core::mem::offset_of!(InodeRecord, block_count);
pub(crate) const INODE_EXTENT_ROOT_OFFSET: usize = core::mem::offset_of!(InodeRecord, extent_root);

impl PersistentMetadataRecord for SpaceReservationRecord {
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

impl PersistentMetadataRecord for AllocationRecord {
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

impl PersistentMetadataRecord for ExtentNodeRecord {
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

impl PersistentMetadataRecord for InodeRecord {
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

pub(crate) trait AuthoritativeMetadataRecord: PersistentMetadataRecord {}

impl AuthoritativeMetadataRecord for SpaceReservationRecord {}
impl AuthoritativeMetadataRecord for AllocationRecord {}
impl AuthoritativeMetadataRecord for ExtentNodeRecord {}
impl AuthoritativeMetadataRecord for InodeRecord {}

pub(crate) trait PersistentMetadataRecord: Copy + Pod {
    fn metadata_header(&self) -> &PersistentMetadataHeader;

    fn validate_record(&self) -> Result<()>;

    fn decode_record(bytes: &[u8]) -> Result<Self>;
}

pub(crate) const PENDING_EXTENT_MUTATION_LOGICAL_INDEX: u64 = u64::MAX - 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ExtentMutationPhase {
    Prepared = 1,
    ParentPublished = 2,
}

impl ExtentMutationPhase {
    fn from_u8(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Prepared),
            2 => Ok(Self::ParentPublished),
            _ => return_errno_with_message!(Errno::EINVAL, "invalid extent mutation phase"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ExtentMutationOperation {
    NodeSplit = 1,
    RootSplit = 2,
    NodeRewrite = 3,
}

impl ExtentMutationOperation {
    fn from_u8(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::NodeSplit),
            2 => Ok(Self::RootSplit),
            3 => Ok(Self::NodeRewrite),
            _ => return_errno_with_message!(Errno::EINVAL, "unknown extent mutation operation"),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Pod)]
pub(crate) struct PendingExtentMutationRecord {
    header: PersistentMetadataHeader,
    // Low byte is the phase and the next byte is the operation. Keeping the
    // transition state in one naturally aligned word lets recovery advance it
    // with one durable store.
    state: u64,
    parent_logical_index: u64,
    old_child_logical_index: u64,
    old_parent: PersistentObjectRef,
    new_parent: PersistentObjectRef,
    old_child: PersistentObjectRef,
    new_child: PersistentObjectRef,
    repair_parent: PersistentObjectRef,
    repair_child: PersistentObjectRef,
    split_logical_block: u64,
    reserved: [u8; 16],
}

impl PendingExtentMutationRecord {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepared_node_rewrite(
        fs_id: u64,
        inode: u64,
        generation: u64,
        parent_logical_index: u64,
        child_logical_index: u64,
        parent: PersistentObjectRef,
        child: PersistentObjectRef,
        repair_parent: PersistentObjectRef,
        repair_child: PersistentObjectRef,
        first_logical_block: u64,
    ) -> Result<Self> {
        let record = Self {
            header: PersistentMetadataHeader::new(
                PersistentMetadataKind::PendingExtentMutation,
                size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
                fs_id,
                inode,
                generation,
            )?,
            state: Self::state_for(
                ExtentMutationPhase::Prepared,
                ExtentMutationOperation::NodeRewrite,
            ),
            parent_logical_index,
            old_child_logical_index: child_logical_index,
            old_parent: parent,
            new_parent: PersistentObjectRef::NONE,
            old_child: child,
            new_child: PersistentObjectRef::NONE,
            repair_parent,
            repair_child,
            split_logical_block: first_logical_block,
            reserved: [0; 16],
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn prepared_split(
        fs_id: u64,
        inode: u64,
        generation: u64,
        parent_logical_index: u64,
        old_child_logical_index: u64,
        old_parent: PersistentObjectRef,
        old_child: PersistentObjectRef,
        new_child: PersistentObjectRef,
        split_logical_block: u64,
    ) -> Result<Self> {
        let record = Self {
            header: PersistentMetadataHeader::new(
                PersistentMetadataKind::PendingExtentMutation,
                size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
                fs_id,
                inode,
                generation,
            )?,
            state: Self::state_for(
                ExtentMutationPhase::Prepared,
                ExtentMutationOperation::NodeSplit,
            ),
            parent_logical_index,
            old_child_logical_index,
            old_parent,
            new_parent: old_parent,
            old_child,
            new_child,
            repair_parent: PersistentObjectRef::NONE,
            repair_child: PersistentObjectRef::NONE,
            split_logical_block,
            reserved: [0; 16],
        };
        record.validate()?;
        Ok(record)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepared_root_split(
        fs_id: u64,
        inode: u64,
        generation: u64,
        inode_logical_index: u64,
        old_root_logical_index: u64,
        inode_ref: PersistentObjectRef,
        new_root: PersistentObjectRef,
        old_root: PersistentObjectRef,
        right_child: PersistentObjectRef,
        split_logical_block: u64,
    ) -> Result<Self> {
        let record = Self {
            header: PersistentMetadataHeader::new(
                PersistentMetadataKind::PendingExtentMutation,
                size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
                fs_id,
                inode,
                generation,
            )?,
            state: Self::state_for(
                ExtentMutationPhase::Prepared,
                ExtentMutationOperation::RootSplit,
            ),
            parent_logical_index: inode_logical_index,
            old_child_logical_index: old_root_logical_index,
            old_parent: inode_ref,
            new_parent: new_root,
            old_child: old_root,
            new_child: right_child,
            repair_parent: PersistentObjectRef::NONE,
            repair_child: PersistentObjectRef::NONE,
            split_logical_block,
            reserved: [0; 16],
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn with_parent_published(
        &self,
        _generation: u64,
        _new_parent: PersistentObjectRef,
    ) -> Result<Self> {
        if self.phase()? != ExtentMutationPhase::Prepared {
            return_errno_with_message!(
                Errno::EINVAL,
                "extent mutation parent is already published"
            );
        }
        let record = Self {
            state: Self::state_for(ExtentMutationPhase::ParentPublished, self.operation()?),
            ..*self
        };
        record.validate()?;
        Ok(record)
    }

    /// Associates committed repair images with this mutation.
    pub(crate) fn with_repair_refs(
        &self,
        repair_parent: PersistentObjectRef,
        repair_child: PersistentObjectRef,
    ) -> Result<Self> {
        let record = Self {
            repair_parent,
            repair_child,
            ..*self
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != size_of::<Self>() {
            return_errno_with_message!(Errno::EINVAL, "invalid pending extent mutation size");
        }
        let record = Self::from_bytes(bytes);
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.header.validate(
            PersistentMetadataKind::PendingExtentMutation,
            size_of::<Self>() - size_of::<PersistentMetadataHeader>(),
        )?;
        if self.header.generation() == 0
            || self.state >> 16 != 0
            || self.phase().is_err()
            || self.operation().is_err()
            || self.parent_logical_index == PENDING_EXTENT_MUTATION_LOGICAL_INDEX
            || self.old_child_logical_index == PENDING_EXTENT_MUTATION_LOGICAL_INDEX
        {
            return_errno_with_message!(Errno::EINVAL, "invalid pending extent mutation");
        }
        self.old_parent.validate()?;
        self.new_parent.validate()?;
        self.old_child.validate()?;
        self.new_child.validate()?;
        self.repair_parent.validate()?;
        self.repair_child.validate()?;
        match (self.operation()?, self.phase()?) {
            (ExtentMutationOperation::NodeRewrite, _)
                if self.old_child.is_none()
                    || self.repair_child.is_none()
                    || self.old_parent.is_none() != self.repair_parent.is_none()
                    || !self.new_parent.is_none()
                    || !self.new_child.is_none() =>
            {
                return_errno_with_message!(Errno::EINVAL, "invalid node rewrite mutation");
            }
            (ExtentMutationOperation::NodeSplit, _)
                if self.old_parent.is_none()
                    || self.old_child.is_none()
                    || self.new_child.is_none()
                    || self.split_logical_block == 0
                    || self.repair_parent.is_none() != self.repair_child.is_none() =>
            {
                return_errno_with_message!(Errno::EINVAL, "invalid node split mutation");
            }
            (ExtentMutationOperation::RootSplit, _)
                if self.old_parent.is_none()
                    || self.new_parent.is_none()
                    || self.old_child.is_none()
                    || self.new_child.is_none()
                    || self.split_logical_block == 0
                    || (!self.repair_parent.is_none() && self.repair_child.is_none()) =>
            {
                return_errno_with_message!(Errno::EINVAL, "invalid root split mutation");
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) const fn header(&self) -> &PersistentMetadataHeader {
        &self.header
    }

    pub(crate) fn phase(&self) -> Result<ExtentMutationPhase> {
        ExtentMutationPhase::from_u8(self.state_word() as u8)
    }

    pub(crate) fn operation(&self) -> Result<ExtentMutationOperation> {
        ExtentMutationOperation::from_u8((self.state_word() >> 8) as u8)
    }

    pub(crate) const fn state_word(&self) -> u64 {
        self.state
    }

    pub(crate) const fn parent_logical_index(&self) -> u64 {
        self.parent_logical_index
    }

    pub(crate) const fn old_child_logical_index(&self) -> u64 {
        self.old_child_logical_index
    }

    pub(crate) const fn old_parent(&self) -> PersistentObjectRef {
        self.old_parent
    }

    pub(crate) const fn new_parent(&self) -> PersistentObjectRef {
        self.new_parent
    }

    pub(crate) const fn old_child(&self) -> PersistentObjectRef {
        self.old_child
    }

    pub(crate) const fn new_child(&self) -> PersistentObjectRef {
        self.new_child
    }

    pub(crate) const fn repair_parent(&self) -> PersistentObjectRef {
        self.repair_parent
    }

    pub(crate) const fn repair_child(&self) -> PersistentObjectRef {
        self.repair_child
    }

    pub(crate) const fn split_logical_block(&self) -> u64 {
        self.split_logical_block
    }

    const fn state_for(phase: ExtentMutationPhase, operation: ExtentMutationOperation) -> u64 {
        (phase as u64) | ((operation as u64) << 8)
    }
}

const _: () = {
    assert!(size_of::<PendingExtentMutationRecord>() == 208);
    assert!(align_of::<PendingExtentMutationRecord>() == 8);
    assert!(core::mem::offset_of!(PendingExtentMutationRecord, state) % align_of::<u64>() == 0);
};

pub(crate) const PENDING_EXTENT_MUTATION_STATE_OFFSET: usize =
    core::mem::offset_of!(PendingExtentMutationRecord, state);
impl PersistentMetadataRecord for PendingExtentMutationRecord {
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

impl AuthoritativeMetadataRecord for PendingExtentMutationRecord {}

// Recovery regression tests: these are newly authored, not recovered source.
#[cfg(any(test, ktest))]
mod recovery_tests {
    use super::*;

    #[cfg_attr(ktest, ostd::prelude::ktest)]
    #[cfg_attr(not(ktest), test)]
    fn recovered_layout_and_kind_tags_match_persistent_schema() {
        assert_eq!(size_of::<PersistentMetadataHeader>(), 64);
        assert_eq!(size_of::<PersistentObjectRef>(), 16);
        assert_eq!(size_of::<SpaceReservationRecord>(), 128);
        assert_eq!(size_of::<AllocationRecord>(), 128);
        assert_eq!(size_of::<ExtentRecordEntry>(), 24);
        assert_eq!(EXTENT_NODE_ENTRY_CAPACITY, 165);
        assert_eq!(size_of::<ExtentNodeRecord>(), 4096);
        assert_eq!(size_of::<InodeRecord>(), 128);
        assert_eq!(size_of::<PendingExtentMutationRecord>(), 208);
        assert_eq!(INODE_GENERATION_OFFSET, 40);
        assert_eq!(INODE_EXTENT_ROOT_OFFSET, 64);
        assert_eq!(INODE_FILE_SIZE_OFFSET, 80);
        assert_eq!(INODE_BLOCK_COUNT_OFFSET, 88);
        assert_eq!(PENDING_EXTENT_MUTATION_STATE_OFFSET, 64);
        assert_eq!(PersistentMetadataKind::SpaceReservation as u16, 3);
        assert_eq!(PersistentMetadataKind::Allocation as u16, 7);
    }

    #[cfg_attr(ktest, ostd::prelude::ktest)]
    #[cfg_attr(not(ktest), test)]
    fn recovered_metadata_rejects_malformed_references_and_headers() {
        assert!(PersistentObjectRef::new(u64::MAX, 1).is_err());
        assert!(PersistentObjectRef::new(0, 0).is_err());
        assert!(
            PersistentObjectRef {
                slot_plus_one: 0,
                allocation_seq: 1
            }
            .validate()
            .is_err()
        );
        assert!(
            PersistentObjectRef {
                slot_plus_one: 1,
                allocation_seq: 0
            }
            .validate()
            .is_err()
        );
        assert!(SpaceReservationRecord::new(1, 2, 1, 2, 3).is_err());
        let record = SpaceReservationRecord::new(1, 2, 1, 4, 2).unwrap();
        assert_eq!(
            SpaceReservationRecord::decode(record.as_bytes()).unwrap(),
            record
        );
        assert!(SpaceReservationRecord::decode(&record.as_bytes()[..127]).is_err());
        let mut bytes = record.as_bytes().to_vec();
        bytes[0] ^= 1;
        assert!(SpaceReservationRecord::decode(&bytes).is_err());
        let reference = PersistentObjectRef::new(0, 1).unwrap();
        let rewrite = PendingExtentMutationRecord::prepared_node_rewrite(
            1,
            2,
            1,
            0,
            0,
            PersistentObjectRef::NONE,
            reference,
            PersistentObjectRef::NONE,
            PersistentObjectRef::new(1, 1).unwrap(),
            0,
        )
        .unwrap();
        assert_eq!(rewrite.phase().unwrap(), ExtentMutationPhase::Prepared);
        assert_eq!(
            rewrite
                .with_parent_published(2, reference)
                .unwrap()
                .phase()
                .unwrap(),
            ExtentMutationPhase::ParentPublished
        );
        let mut corrupt = rewrite;
        corrupt.state |= 1 << 16;
        assert!(corrupt.validate().is_err());
        corrupt = rewrite;
        corrupt.repair_child = PersistentObjectRef::NONE;
        assert!(corrupt.validate().is_err());
    }

    #[cfg_attr(ktest, ostd::prelude::ktest)]
    #[cfg_attr(not(ktest), test)]
    fn recovered_extent_node_rejects_overlaps_and_invalid_range_words() {
        let a = ExtentRecordEntry::leaf(0, 2, 10, false).unwrap();
        let b = ExtentRecordEntry::leaf(1, 1, 20, false).unwrap();
        assert!(
            ExtentNodeRecord::new(
                1,
                2,
                1,
                0,
                0,
                PersistentObjectRef::NONE,
                PersistentObjectRef::NONE,
                &[a, b]
            )
            .is_err()
        );
        let mut record = ExtentNodeRecord::new(
            1,
            2,
            1,
            0,
            0,
            PersistentObjectRef::NONE,
            PersistentObjectRef::NONE,
            &[a],
        )
        .unwrap();
        record.entries[0].range_tail |= 1 << 63;
        assert!(record.validate().is_err());
        assert!(ExtentRecordEntry::nv_leaf(0, PersistentObjectRef::NONE, false).is_err());
        assert!(ExtentRecordEntry::child(0, PersistentObjectRef::NONE).is_err());
    }
}
