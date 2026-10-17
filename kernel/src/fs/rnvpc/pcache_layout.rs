// SPDX-License-Identifier: MPL-2.0

//! Byte-exact persistent rNVPC payload layouts.

use core::ops::Range;

use crate::prelude::*;

pub(crate) const WAL_CAPACITY_PER_INODE: usize = 1024;
pub(crate) const WAL_OP_UPSERT: u16 = 1;
pub(crate) const WAL_RECORD_BYTES: usize = 64;
pub(crate) const WAL_RECORDS_PER_FRAME: usize = PAGE_SIZE / WAL_RECORD_BYTES;
pub(crate) const WAL_FRAME_COUNT: usize = WAL_CAPACITY_PER_INODE.div_ceil(WAL_RECORDS_PER_FRAME);
const U16_WIRE_BYTES: usize = 2;
const U32_WIRE_BYTES: usize = 4;
const U64_WIRE_BYTES: usize = 8;

const PAGE_MAGIC: u64 = 0x524e_5650_4341_4348;
const PAGE_VERSION: u32 = 1;
const HEADER_BYTES: usize = 64;
const HEADER_MAGIC_OFFSET: usize = 0;
const HEADER_VERSION_OFFSET: usize = HEADER_MAGIC_OFFSET + U64_WIRE_BYTES;
const HEADER_OBJECT_TYPE_OFFSET: usize = HEADER_VERSION_OFFSET + U32_WIRE_BYTES;
const HEADER_SIZE_OFFSET: usize = HEADER_OBJECT_TYPE_OFFSET + U16_WIRE_BYTES;
const HEADER_FS_ID_OFFSET: usize = HEADER_SIZE_OFFSET + U16_WIRE_BYTES;
const HEADER_INODE_OFFSET: usize = HEADER_FS_ID_OFFSET + U64_WIRE_BYTES;
const HEADER_OBJECT_ID_OFFSET: usize = HEADER_INODE_OFFSET + U64_WIRE_BYTES;
const HEADER_SELECTOR_OFFSET: usize = HEADER_OBJECT_ID_OFFSET + U64_WIRE_BYTES;
const HEADER_FIELDS_END: usize = HEADER_SELECTOR_OFFSET + U64_WIRE_BYTES;

const PAGE_REF_SLOT_OFFSET: usize = 0;
const PAGE_REF_GENERATION_OFFSET: usize = PAGE_REF_SLOT_OFFSET + U32_WIRE_BYTES;
const PAGE_REF_BYTES: usize = PAGE_REF_GENERATION_OFFSET + U64_WIRE_BYTES;

const WAL_LSN_OFFSET: usize = 0;
const WAL_OPERATION_OFFSET: usize = WAL_LSN_OFFSET + U64_WIRE_BYTES;
const WAL_TAG_OFFSET: usize = WAL_OPERATION_OFFSET + U16_WIRE_BYTES;
const WAL_PAGE_INDEX_OFFSET: usize = WAL_TAG_OFFSET + U16_WIRE_BYTES;
const WAL_OLD_PAGE_OFFSET: usize = WAL_PAGE_INDEX_OFFSET + U64_WIRE_BYTES;
const WAL_NEW_PAGE_OFFSET: usize = WAL_OLD_PAGE_OFFSET + PAGE_REF_BYTES;
const WAL_SSD_BLOCK_OFFSET: usize = WAL_NEW_PAGE_OFFSET + PAGE_REF_BYTES;
const WAL_CHECKSUM_OFFSET: usize = WAL_SSD_BLOCK_OFFSET + U64_WIRE_BYTES;
const WAL_COMMIT_OFFSET: usize = WAL_CHECKSUM_OFFSET + U32_WIRE_BYTES;
const WAL_RECORD_END: usize = WAL_COMMIT_OFFSET + U64_WIRE_BYTES;

const IMAGE_BYTES: usize = (PAGE_SIZE - HEADER_BYTES) / 2;
const IMAGE0_OFFSET: usize = HEADER_BYTES;
const IMAGE1_OFFSET: usize = IMAGE0_OFFSET + IMAGE_BYTES;

const ANCHOR_GENERATION_OFFSET: usize = 0;
const ANCHOR_ROOT_OFFSET: usize = ANCHOR_GENERATION_OFFSET + U64_WIRE_BYTES;
const ANCHOR_HEIGHT_OFFSET: usize = ANCHOR_ROOT_OFFSET + PAGE_REF_BYTES;
const ANCHOR_WAL_HEAD_OFFSET: usize = 24;
const ANCHOR_WAL_TAIL_OFFSET: usize = ANCHOR_WAL_HEAD_OFFSET + U16_WIRE_BYTES;
const ANCHOR_WAL_USED_OFFSET: usize = ANCHOR_WAL_TAIL_OFFSET + U16_WIRE_BYTES;
const ANCHOR_NEXT_LSN_OFFSET: usize = 32;
const ANCHOR_CHECKPOINTED_LSN_OFFSET: usize = ANCHOR_NEXT_LSN_OFFSET + U64_WIRE_BYTES;
const ANCHOR_CHECKSUM_OFFSET: usize = ANCHOR_CHECKPOINTED_LSN_OFFSET + U64_WIRE_BYTES;
const ANCHOR_FIELDS_END: usize = ANCHOR_CHECKSUM_OFFSET + U32_WIRE_BYTES;

const NODE_GENERATION_OFFSET: usize = 0;
const NODE_PAGE_LSN_OFFSET: usize = NODE_GENERATION_OFFSET + U64_WIRE_BYTES;
const NODE_HIGH_KEY_OFFSET: usize = NODE_PAGE_LSN_OFFSET + U64_WIRE_BYTES;
const NODE_RIGHT_SIBLING_OFFSET: usize = NODE_HIGH_KEY_OFFSET + U64_WIRE_BYTES;
const NODE_LEVEL_OFFSET: usize = NODE_RIGHT_SIBLING_OFFSET + PAGE_REF_BYTES;
const NODE_ENTRY_COUNT_OFFSET: usize = NODE_LEVEL_OFFSET + U16_WIRE_BYTES;
const NODE_CHECKSUM_OFFSET: usize = NODE_ENTRY_COUNT_OFFSET + U16_WIRE_BYTES;
const NODE_ENTRIES_OFFSET: usize = 48;
const NODE_METADATA_END: usize = NODE_CHECKSUM_OFFSET + U32_WIRE_BYTES;

const ENTRY_SLOT_BYTES: usize = 28;
const LEAF_PAGE_INDEX_OFFSET: usize = 0;
const LEAF_PAGE_REF_OFFSET: usize = LEAF_PAGE_INDEX_OFFSET + U64_WIRE_BYTES;
const LEAF_SSD_BLOCK_OFFSET: usize = LEAF_PAGE_REF_OFFSET + PAGE_REF_BYTES;
const LEAF_ENTRY_END: usize = LEAF_SSD_BLOCK_OFFSET + U64_WIRE_BYTES;
const INTERNAL_SEPARATOR_OFFSET: usize = 0;
const INTERNAL_CHILD_OFFSET: usize = INTERNAL_SEPARATOR_OFFSET + U64_WIRE_BYTES;
const INTERNAL_RESERVED_OFFSET: usize = INTERNAL_CHILD_OFFSET + PAGE_REF_BYTES;
const INTERNAL_RESERVED_BYTES: usize = 8;
const INTERNAL_CONTENT_END: usize = INTERNAL_RESERVED_OFFSET + INTERNAL_RESERVED_BYTES;

const WAL_TAG: u16 = 0x5701;
const WAL_COMMIT_XOR: u64 = 0x5741_4c43_4f4d_4d49;

const _: () = {
    assert!(PAGE_SIZE == 4096);
    assert!(PAGE_REF_BYTES == 12);
    assert!(WAL_RECORD_END == WAL_RECORD_BYTES);
    assert!(WAL_RECORDS_PER_FRAME == 64);
    assert!(WAL_RECORDS_PER_FRAME * WAL_RECORD_BYTES == PAGE_SIZE);
    assert!(HEADER_FIELDS_END <= HEADER_BYTES);
    assert!(HEADER_BYTES + IMAGE_BYTES * 2 == PAGE_SIZE);
    assert!(ANCHOR_FIELDS_END <= IMAGE_BYTES);
    assert!(NODE_METADATA_END <= NODE_ENTRIES_OFFSET);
    assert!(NODE_ENTRIES_OFFSET + BTREE_NODE_ENTRY_CAPACITY * ENTRY_SLOT_BYTES <= IMAGE_BYTES);
    assert!(LEAF_ENTRY_END == ENTRY_SLOT_BYTES);
    assert!(INTERNAL_CONTENT_END <= ENTRY_SLOT_BYTES);
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentPageRef {
    pub(crate) slot: u32,
    pub(crate) generation: u64,
}

impl PersistentPageRef {
    pub(crate) const NONE: Self = Self {
        slot: u32::MAX,
        generation: 0,
    };

    pub(crate) const fn new(slot: u32, generation: u64) -> Self {
        Self { slot, generation }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub(crate) enum PcObjectType {
    Anchor = 1,
    WalFrame = 2,
    BtreeNode = 3,
}

pub(crate) trait PcPageCodec: Sized {
    const OBJECT_TYPE: PcObjectType;
    fn encode_page(&self, output: &mut [u8; PAGE_SIZE]) -> Result<()>;
    fn decode_page(input: &[u8; PAGE_SIZE]) -> Result<Self>;

    /// Repairs a selector after decode falls back to the other complete image.
    fn repair_selector(_input: &mut [u8; PAGE_SIZE], _value: &Self) -> Option<Range<usize>> {
        None
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentWalRecord {
    pub(crate) lsn: u64,
    pub(crate) operation: u16,
    pub(crate) reserved: u16,
    pub(crate) page_index: u64,
    pub(crate) old_page: PersistentPageRef,
    pub(crate) new_page: PersistentPageRef,
    pub(crate) ssd_backing_block: u64,
    pub(crate) checksum: u32,
    pub(crate) commit_word: u64,
}

impl PersistentWalRecord {
    pub(crate) fn upsert(
        lsn: u64,
        page_index: u64,
        old_page: PersistentPageRef,
        new_page: PersistentPageRef,
        ssd_backing_block: u64,
    ) -> Self {
        let mut record = Self {
            lsn,
            operation: WAL_OP_UPSERT,
            reserved: WAL_TAG,
            page_index,
            old_page,
            new_page,
            ssd_backing_block,
            checksum: 0,
            commit_word: wal_commit_word(lsn),
        };
        let mut bytes = [0; WAL_RECORD_BYTES];
        record.encode(&mut bytes);
        record.checksum = u32_at(&bytes, WAL_CHECKSUM_OFFSET);
        record
    }

    pub(crate) fn encode(&self, output: &mut [u8; WAL_RECORD_BYTES]) {
        output.fill(0);
        put_u64(output, WAL_LSN_OFFSET, self.lsn);
        put_u16(output, WAL_OPERATION_OFFSET, self.operation);
        put_u16(output, WAL_TAG_OFFSET, self.reserved);
        put_u64(output, WAL_PAGE_INDEX_OFFSET, self.page_index);
        put_ref(output, WAL_OLD_PAGE_OFFSET, self.old_page);
        put_ref(output, WAL_NEW_PAGE_OFFSET, self.new_page);
        put_u64(output, WAL_SSD_BLOCK_OFFSET, self.ssd_backing_block);
        put_u64(output, WAL_COMMIT_OFFSET, self.commit_word);
        let checksum = crc32c_zeroed(output, WAL_CHECKSUM_OFFSET, U32_WIRE_BYTES);
        put_u32(output, WAL_CHECKSUM_OFFSET, checksum);
    }

    pub(crate) fn decode(input: &[u8; WAL_RECORD_BYTES]) -> Result<Self> {
        if input.iter().all(|byte| *byte == 0) {
            return Ok(Self::default());
        }
        if u16_at(input, WAL_TAG_OFFSET) != WAL_TAG
            || u16_at(input, WAL_OPERATION_OFFSET) != WAL_OP_UPSERT
        {
            return_errno_with_message!(Errno::EINVAL, "invalid persistent WAL magic/version");
        }
        if u32_at(input, WAL_CHECKSUM_OFFSET)
            != crc32c_zeroed(input, WAL_CHECKSUM_OFFSET, U32_WIRE_BYTES)
        {
            return_errno_with_message!(Errno::EINVAL, "invalid persistent WAL checksum");
        }
        let lsn = u64_at(input, WAL_LSN_OFFSET);
        if u64_at(input, WAL_COMMIT_OFFSET) != wal_commit_word(lsn) {
            return_errno_with_message!(Errno::EINVAL, "invalid persistent WAL commit word");
        }
        Ok(Self {
            lsn,
            operation: u16_at(input, WAL_OPERATION_OFFSET),
            reserved: u16_at(input, WAL_TAG_OFFSET),
            page_index: u64_at(input, WAL_PAGE_INDEX_OFFSET),
            old_page: ref_at(input, WAL_OLD_PAGE_OFFSET),
            new_page: ref_at(input, WAL_NEW_PAGE_OFFSET),
            ssd_backing_block: u64_at(input, WAL_SSD_BLOCK_OFFSET),
            checksum: u32_at(input, WAL_CHECKSUM_OFFSET),
            commit_word: u64_at(input, WAL_COMMIT_OFFSET),
        })
    }
}

pub(crate) fn wal_record_range(record_index: usize) -> Range<usize> {
    let start = record_index * WAL_RECORD_BYTES;
    start..start + WAL_RECORD_BYTES
}

pub(crate) fn wal_record_commit_range(record_index: usize) -> Range<usize> {
    let record = wal_record_range(record_index);
    record.start + WAL_COMMIT_OFFSET..record.start + WAL_COMMIT_OFFSET + U64_WIRE_BYTES
}

pub(crate) const fn wal_commit_word(lsn: u64) -> u64 {
    lsn.rotate_left(17) ^ WAL_COMMIT_XOR
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WalFrameImage {
    pub(crate) records: [PersistentWalRecord; WAL_RECORDS_PER_FRAME],
}

impl Default for WalFrameImage {
    fn default() -> Self {
        Self {
            records: [PersistentWalRecord::default(); WAL_RECORDS_PER_FRAME],
        }
    }
}

impl WalFrameImage {
    pub(crate) fn encode_page(&self, output: &mut [u8; PAGE_SIZE]) -> Result<()> {
        for (index, record) in self.records.iter().enumerate() {
            let offset = index * WAL_RECORD_BYTES;
            let bytes: &mut [u8; WAL_RECORD_BYTES] = (&mut output
                [offset..offset + WAL_RECORD_BYTES])
                .try_into()
                .unwrap();
            record.encode(bytes);
        }
        Ok(())
    }

    pub(crate) fn decode_page(input: &[u8; PAGE_SIZE]) -> Result<Self> {
        let mut frame = Self::default();
        for (index, record) in frame.records.iter_mut().enumerate() {
            let bytes: &[u8; WAL_RECORD_BYTES] = input[wal_record_range(index)].try_into().unwrap();
            let lsn = u64_at(bytes, WAL_LSN_OFFSET);
            *record = if u64_at(bytes, WAL_COMMIT_OFFSET) == wal_commit_word(lsn) {
                PersistentWalRecord::decode(bytes)?
            } else {
                PersistentWalRecord::default()
            };
        }
        Ok(frame)
    }
}

impl PcPageCodec for WalFrameImage {
    const OBJECT_TYPE: PcObjectType = PcObjectType::WalFrame;
    fn encode_page(&self, output: &mut [u8; PAGE_SIZE]) -> Result<()> {
        Self::encode_page(self, output)
    }
    fn decode_page(input: &[u8; PAGE_SIZE]) -> Result<Self> {
        Self::decode_page(input)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AnchorImage {
    pub(crate) generation: u64,
    pub(crate) root: PersistentPageRef,
    pub(crate) height: u16,
    pub(crate) wal_head: u16,
    pub(crate) wal_tail: u16,
    pub(crate) wal_used: u16,
    pub(crate) next_lsn: u64,
    pub(crate) checkpointed_lsn: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct InodePcAnchor {
    pub(crate) fs_id: u64,
    pub(crate) inode: u64,
    pub(crate) images: [AnchorImage; 2],
    pub(crate) active_slot: u64,
}

impl InodePcAnchor {
    pub(crate) fn empty(fs_id: u64, inode: u64, root: PersistentPageRef) -> Self {
        Self {
            fs_id,
            inode,
            images: [
                AnchorImage {
                    generation: 1,
                    root,
                    next_lsn: 1,
                    ..AnchorImage::default()
                },
                AnchorImage::default(),
            ],
            active_slot: 0,
        }
    }

    pub(crate) fn encode_page(&self, output: &mut [u8; PAGE_SIZE]) -> Result<()> {
        if self.active_slot > 1 {
            return_errno_with_message!(Errno::EINVAL, "invalid anchor selector");
        }
        header(
            output,
            PcObjectType::Anchor,
            self.fs_id,
            self.inode,
            0,
            self.active_slot,
        );
        for slot in 0..2 {
            let image = &self.images[slot];
            let bytes = &mut output[image_offset(slot)..image_offset(slot) + IMAGE_BYTES];
            bytes.fill(0);
            if image.generation == 0 {
                continue;
            }
            if !valid_anchor_geometry(image) {
                return_errno_with_message!(Errno::EINVAL, "invalid anchor WAL geometry");
            }
            put_u64(bytes, ANCHOR_GENERATION_OFFSET, image.generation);
            put_ref(bytes, ANCHOR_ROOT_OFFSET, image.root);
            put_u16(bytes, ANCHOR_HEIGHT_OFFSET, image.height);
            put_u16(bytes, ANCHOR_WAL_HEAD_OFFSET, image.wal_head);
            put_u16(bytes, ANCHOR_WAL_TAIL_OFFSET, image.wal_tail);
            put_u16(bytes, ANCHOR_WAL_USED_OFFSET, image.wal_used);
            put_u64(bytes, ANCHOR_NEXT_LSN_OFFSET, image.next_lsn);
            put_u64(
                bytes,
                ANCHOR_CHECKPOINTED_LSN_OFFSET,
                image.checkpointed_lsn,
            );
            put_u32(
                bytes,
                ANCHOR_CHECKSUM_OFFSET,
                crc32c_zeroed(bytes, ANCHOR_CHECKSUM_OFFSET, U32_WIRE_BYTES),
            );
        }
        Ok(())
    }

    pub(crate) fn decode_page(input: &[u8; PAGE_SIZE], fs_id: u64, inode: u64) -> Result<Self> {
        valid_header(input, PcObjectType::Anchor, fs_id, inode, 0)?;
        let active_slot = u64_at(input, HEADER_SELECTOR_OFFSET);
        let images = [
            anchor_image(&input[IMAGE0_OFFSET..IMAGE0_OFFSET + IMAGE_BYTES]),
            anchor_image(&input[IMAGE1_OFFSET..IMAGE1_OFFSET + IMAGE_BYTES]),
        ];
        let Some(slot) =
            select_valid_slot(active_slot, images.map(|image| image.map(|i| i.generation)))
        else {
            return_errno_with_message!(Errno::EINVAL, "no valid anchor image");
        };
        let image = images[slot].unwrap();
        Ok(Self {
            fs_id,
            inode,
            images: [image, AnchorImage::default()],
            active_slot: 0,
        })
    }

    pub(crate) fn active_slot_from_page(input: &[u8; PAGE_SIZE]) -> Result<usize> {
        let images = [
            anchor_image(&input[IMAGE0_OFFSET..IMAGE0_OFFSET + IMAGE_BYTES]),
            anchor_image(&input[IMAGE1_OFFSET..IMAGE1_OFFSET + IMAGE_BYTES]),
        ];
        select_valid_slot(
            u64_at(input, HEADER_SELECTOR_OFFSET),
            images.map(|image| image.map(|i| i.generation)),
        )
        .ok_or_else(|| Error::new(Errno::EINVAL))
    }
}

pub(crate) fn anchor_image_range(slot: usize) -> Range<usize> {
    let start = image_offset(slot);
    start..start + IMAGE_BYTES
}

pub(crate) fn btree_image_range(slot: usize) -> Range<usize> {
    let offset = image_offset(slot);
    offset..offset + IMAGE_BYTES
}

pub(crate) fn btree_selector_range() -> Range<usize> {
    HEADER_SELECTOR_OFFSET..HEADER_SELECTOR_OFFSET + U64_WIRE_BYTES
}

pub(crate) fn anchor_selector_range() -> Range<usize> {
    HEADER_SELECTOR_OFFSET..HEADER_SELECTOR_OFFSET + U64_WIRE_BYTES
}
impl PcPageCodec for InodePcAnchor {
    const OBJECT_TYPE: PcObjectType = PcObjectType::Anchor;
    fn encode_page(&self, output: &mut [u8; PAGE_SIZE]) -> Result<()> {
        Self::encode_page(self, output)
    }
    fn decode_page(input: &[u8; PAGE_SIZE]) -> Result<Self> {
        Self::decode_page(
            input,
            u64_at(input, HEADER_FS_ID_OFFSET),
            u64_at(input, HEADER_INODE_OFFSET),
        )
    }
    fn repair_selector(input: &mut [u8; PAGE_SIZE], _value: &Self) -> Option<Range<usize>> {
        let slot = Self::active_slot_from_page(input).ok()? as u64;
        if u64_at(input, HEADER_SELECTOR_OFFSET) == slot {
            return None;
        }
        put_u64(input, HEADER_SELECTOR_OFFSET, slot);
        Some(anchor_selector_range())
    }
}

pub(crate) const BTREE_NODE_ENTRY_CAPACITY: usize =
    (IMAGE_BYTES - NODE_ENTRIES_OFFSET) / ENTRY_SLOT_BYTES;
const _: () = assert!(BTREE_NODE_ENTRY_CAPACITY == 70);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct LeafEntry {
    pub(crate) page_index: u64,
    pub(crate) page: PersistentPageRef,
    pub(crate) ssd_backing_block: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct InternalEntry {
    pub(crate) separator: u64,
    pub(crate) child: PersistentPageRef,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BtreeNodeImage {
    pub(crate) generation: u64,
    pub(crate) page_lsn: u64,
    pub(crate) high_key: u64,
    pub(crate) right_sibling: PersistentPageRef,
    pub(crate) level: u16,
    pub(crate) entry_count: u16,
    pub(crate) leaf_entries: [LeafEntry; BTREE_NODE_ENTRY_CAPACITY],
    pub(crate) internal_entries: [InternalEntry; BTREE_NODE_ENTRY_CAPACITY],
}

impl Default for BtreeNodeImage {
    fn default() -> Self {
        Self {
            generation: 0,
            page_lsn: 0,
            high_key: 0,
            right_sibling: PersistentPageRef::NONE,
            level: 0,
            entry_count: 0,
            leaf_entries: [LeafEntry::default(); BTREE_NODE_ENTRY_CAPACITY],
            internal_entries: [InternalEntry::default(); BTREE_NODE_ENTRY_CAPACITY],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct BtreeNodeFrame {
    pub(crate) fs_id: u64,
    pub(crate) inode: u64,
    pub(crate) node_id: u64,
    pub(crate) images: [BtreeNodeImage; 2],
    pub(crate) active_slot: u64,
}

impl BtreeNodeFrame {
    pub(crate) fn empty_leaf(fs_id: u64, inode: u64, node_id: u64) -> Self {
        Self {
            fs_id,
            inode,
            node_id,
            images: [
                BtreeNodeImage {
                    generation: 1,
                    ..BtreeNodeImage::default()
                },
                BtreeNodeImage::default(),
            ],
            active_slot: 0,
        }
    }

    pub(crate) fn encode_page(&self, output: &mut [u8; PAGE_SIZE]) -> Result<()> {
        if self.active_slot > 1 {
            return_errno_with_message!(Errno::EINVAL, "invalid B-tree selector");
        }
        header(
            output,
            PcObjectType::BtreeNode,
            self.fs_id,
            self.inode,
            self.node_id,
            self.active_slot,
        );
        for slot in 0..2 {
            let image = &self.images[slot];
            let bytes = &mut output[image_offset(slot)..image_offset(slot) + IMAGE_BYTES];
            bytes.fill(0);
            if image.generation == 0 {
                continue;
            }
            put_u64(bytes, NODE_GENERATION_OFFSET, image.generation);
            put_u64(bytes, NODE_PAGE_LSN_OFFSET, image.page_lsn);
            put_u64(bytes, NODE_HIGH_KEY_OFFSET, image.high_key);
            put_ref(bytes, NODE_RIGHT_SIBLING_OFFSET, image.right_sibling);
            put_u16(bytes, NODE_LEVEL_OFFSET, image.level);
            if image.entry_count as usize > BTREE_NODE_ENTRY_CAPACITY {
                return_errno_with_message!(Errno::EINVAL, "invalid B-tree entry count");
            }
            put_u16(bytes, NODE_ENTRY_COUNT_OFFSET, image.entry_count);
            for index in 0..image.entry_count as usize {
                let offset = NODE_ENTRIES_OFFSET + index * ENTRY_SLOT_BYTES;
                if image.level == 0 {
                    let entry = image.leaf_entries[index];
                    put_u64(bytes, offset + LEAF_PAGE_INDEX_OFFSET, entry.page_index);
                    put_ref(bytes, offset + LEAF_PAGE_REF_OFFSET, entry.page);
                    put_u64(
                        bytes,
                        offset + LEAF_SSD_BLOCK_OFFSET,
                        entry.ssd_backing_block,
                    );
                } else {
                    let entry = image.internal_entries[index];
                    put_u64(bytes, offset + INTERNAL_SEPARATOR_OFFSET, entry.separator);
                    put_ref(bytes, offset + INTERNAL_CHILD_OFFSET, entry.child);
                }
            }
            put_u32(
                bytes,
                NODE_CHECKSUM_OFFSET,
                crc32c_zeroed(bytes, NODE_CHECKSUM_OFFSET, U32_WIRE_BYTES),
            );
        }
        Ok(())
    }

    pub(crate) fn decode_page(
        input: &[u8; PAGE_SIZE],
        fs_id: u64,
        inode: u64,
        node_id: u64,
    ) -> Result<Self> {
        valid_header(input, PcObjectType::BtreeNode, fs_id, inode, node_id)?;
        let selector = u64_at(input, HEADER_SELECTOR_OFFSET);
        let images = [
            node_image(&input[IMAGE0_OFFSET..IMAGE0_OFFSET + IMAGE_BYTES]),
            node_image(&input[IMAGE1_OFFSET..IMAGE1_OFFSET + IMAGE_BYTES]),
        ];
        let Some(slot) =
            select_valid_slot(selector, images.map(|image| image.map(|i| i.generation)))
        else {
            return_errno_with_message!(Errno::EINVAL, "no valid B-tree node image");
        };
        Ok(Self {
            fs_id,
            inode,
            node_id,
            images: [images[0].unwrap_or_default(), images[1].unwrap_or_default()],
            active_slot: slot as u64,
        })
    }
}

impl PcPageCodec for BtreeNodeFrame {
    const OBJECT_TYPE: PcObjectType = PcObjectType::BtreeNode;
    fn encode_page(&self, output: &mut [u8; PAGE_SIZE]) -> Result<()> {
        Self::encode_page(self, output)
    }
    fn decode_page(input: &[u8; PAGE_SIZE]) -> Result<Self> {
        Self::decode_page(
            input,
            u64_at(input, HEADER_FS_ID_OFFSET),
            u64_at(input, HEADER_INODE_OFFSET),
            u64_at(input, HEADER_OBJECT_ID_OFFSET),
        )
    }
    fn repair_selector(input: &mut [u8; PAGE_SIZE], value: &Self) -> Option<Range<usize>> {
        if u64_at(input, HEADER_SELECTOR_OFFSET) == value.active_slot {
            return None;
        }
        put_u64(input, HEADER_SELECTOR_OFFSET, value.active_slot);
        Some(btree_selector_range())
    }
}

// A valid selector is the publication boundary. Only an invalid selected image
// or torn selector permits recovery to choose the newest complete fallback.
fn select_valid_slot(selector: u64, generations: [Option<u64>; 2]) -> Option<usize> {
    if selector <= 1 && generations[selector as usize].is_some() {
        return Some(selector as usize);
    }
    match generations {
        [Some(left), Some(right)] => Some(if left >= right { 0 } else { 1 }),
        [Some(_), None] => Some(0),
        [None, Some(_)] => Some(1),
        [None, None] => None,
    }
}

fn header(
    output: &mut [u8; PAGE_SIZE],
    ty: PcObjectType,
    fs_id: u64,
    inode: u64,
    object_id: u64,
    selector: u64,
) {
    output.fill(0);
    put_u64(output, HEADER_MAGIC_OFFSET, PAGE_MAGIC);
    put_u32(output, HEADER_VERSION_OFFSET, PAGE_VERSION);
    put_u16(output, HEADER_OBJECT_TYPE_OFFSET, ty as u16);
    put_u16(output, HEADER_SIZE_OFFSET, HEADER_BYTES as u16);
    put_u64(output, HEADER_FS_ID_OFFSET, fs_id);
    put_u64(output, HEADER_INODE_OFFSET, inode);
    put_u64(output, HEADER_OBJECT_ID_OFFSET, object_id);
    put_u64(output, HEADER_SELECTOR_OFFSET, selector);
}

fn valid_header(
    input: &[u8; PAGE_SIZE],
    ty: PcObjectType,
    fs_id: u64,
    inode: u64,
    object_id: u64,
) -> Result<()> {
    if u64_at(input, HEADER_MAGIC_OFFSET) != PAGE_MAGIC
        || u32_at(input, HEADER_VERSION_OFFSET) != PAGE_VERSION
        || u16_at(input, HEADER_OBJECT_TYPE_OFFSET) != ty as u16
        || u16_at(input, HEADER_SIZE_OFFSET) != HEADER_BYTES as u16
        || u64_at(input, HEADER_FS_ID_OFFSET) != fs_id
        || u64_at(input, HEADER_INODE_OFFSET) != inode
        || u64_at(input, HEADER_OBJECT_ID_OFFSET) != object_id
    {
        return_errno_with_message!(Errno::EINVAL, "invalid persistent page header");
    }
    Ok(())
}

fn anchor_image(input: &[u8]) -> Option<AnchorImage> {
    if input.iter().all(|byte| *byte == 0)
        || u32_at(input, ANCHOR_CHECKSUM_OFFSET)
            != crc32c_zeroed(input, ANCHOR_CHECKSUM_OFFSET, U32_WIRE_BYTES)
    {
        return None;
    }
    let image = AnchorImage {
        generation: u64_at(input, ANCHOR_GENERATION_OFFSET),
        root: ref_at(input, ANCHOR_ROOT_OFFSET),
        height: u16_at(input, ANCHOR_HEIGHT_OFFSET),
        wal_head: u16_at(input, ANCHOR_WAL_HEAD_OFFSET),
        wal_tail: u16_at(input, ANCHOR_WAL_TAIL_OFFSET),
        wal_used: u16_at(input, ANCHOR_WAL_USED_OFFSET),
        next_lsn: u64_at(input, ANCHOR_NEXT_LSN_OFFSET),
        checkpointed_lsn: u64_at(input, ANCHOR_CHECKPOINTED_LSN_OFFSET),
    };
    valid_anchor_geometry(&image).then_some(image)
}

fn valid_anchor_geometry(image: &AnchorImage) -> bool {
    let head = image.wal_head as usize;
    let tail = image.wal_tail as usize;
    let used = image.wal_used as usize;
    head < WAL_CAPACITY_PER_INODE
        && tail < WAL_CAPACITY_PER_INODE
        && used <= WAL_CAPACITY_PER_INODE
        && tail == (head + used) % WAL_CAPACITY_PER_INODE
}

fn node_image(input: &[u8]) -> Option<BtreeNodeImage> {
    if input.iter().all(|byte| *byte == 0)
        || u32_at(input, NODE_CHECKSUM_OFFSET)
            != crc32c_zeroed(input, NODE_CHECKSUM_OFFSET, U32_WIRE_BYTES)
    {
        return None;
    }
    let level = u16_at(input, NODE_LEVEL_OFFSET);
    let entry_count = u16_at(input, NODE_ENTRY_COUNT_OFFSET);
    if entry_count as usize > BTREE_NODE_ENTRY_CAPACITY {
        return None;
    }
    let mut leaf_entries = [LeafEntry::default(); BTREE_NODE_ENTRY_CAPACITY];
    let mut internal_entries = [InternalEntry::default(); BTREE_NODE_ENTRY_CAPACITY];
    for index in 0..entry_count as usize {
        let offset = NODE_ENTRIES_OFFSET + index * ENTRY_SLOT_BYTES;
        if level == 0 {
            leaf_entries[index] = LeafEntry {
                page_index: u64_at(input, offset + LEAF_PAGE_INDEX_OFFSET),
                page: ref_at(input, offset + LEAF_PAGE_REF_OFFSET),
                ssd_backing_block: u64_at(input, offset + LEAF_SSD_BLOCK_OFFSET),
            };
        } else {
            internal_entries[index] = InternalEntry {
                separator: u64_at(input, offset + INTERNAL_SEPARATOR_OFFSET),
                child: ref_at(input, offset + INTERNAL_CHILD_OFFSET),
            };
        }
    }
    Some(BtreeNodeImage {
        generation: u64_at(input, NODE_GENERATION_OFFSET),
        page_lsn: u64_at(input, NODE_PAGE_LSN_OFFSET),
        high_key: u64_at(input, NODE_HIGH_KEY_OFFSET),
        right_sibling: ref_at(input, NODE_RIGHT_SIBLING_OFFSET),
        level,
        entry_count,
        leaf_entries,
        internal_entries,
    })
}

fn image_offset(slot: usize) -> usize {
    if slot == 0 {
        IMAGE0_OFFSET
    } else {
        IMAGE1_OFFSET
    }
}
fn put_ref(output: &mut [u8], offset: usize, reference: PersistentPageRef) {
    put_u32(output, offset + PAGE_REF_SLOT_OFFSET, reference.slot);
    put_u64(
        output,
        offset + PAGE_REF_GENERATION_OFFSET,
        reference.generation,
    );
}
fn ref_at(input: &[u8], offset: usize) -> PersistentPageRef {
    PersistentPageRef::new(
        u32_at(input, offset + PAGE_REF_SLOT_OFFSET),
        u64_at(input, offset + PAGE_REF_GENERATION_OFFSET),
    )
}
fn put_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + U16_WIRE_BYTES].copy_from_slice(&value.to_le_bytes());
}
fn put_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + U32_WIRE_BYTES].copy_from_slice(&value.to_le_bytes());
}
fn put_u64(output: &mut [u8], offset: usize, value: u64) {
    output[offset..offset + U64_WIRE_BYTES].copy_from_slice(&value.to_le_bytes());
}
fn u16_at(input: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(input[offset..offset + U16_WIRE_BYTES].try_into().unwrap())
}
fn u32_at(input: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(input[offset..offset + U32_WIRE_BYTES].try_into().unwrap())
}
fn u64_at(input: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(input[offset..offset + U64_WIRE_BYTES].try_into().unwrap())
}

fn crc32c_zeroed(input: &[u8], zero_offset: usize, zero_len: usize) -> u32 {
    let mut crc = !0u32;
    for (index, byte) in input.iter().enumerate() {
        crc ^= if index >= zero_offset && index < zero_offset + zero_len {
            0
        } else {
            *byte
        } as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0x82f6_3b78
            };
        }
    }
    !crc
}
