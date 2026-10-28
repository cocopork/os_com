// SPDX-License-Identifier: MPL-2.0

//! Persistent RNVPC control-image layout.

use core::mem::offset_of;

use super::descriptor::NvDescriptor;
use crate::prelude::*;

pub(super) const NV_IMAGE_MAGIC: u64 = 0x4e56_504f_4f4c_3031;
/// Version 5 makes extent leaves SSD-only; selected data descriptors carry all
/// temporary NV residency. Older mixed-residency images require conversion.
pub(super) const NV_IMAGE_VERSION: u32 = 6;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct NvImageHeader {
    pub(super) magic: u64,
    pub(super) capacity_pages: u64,
    pub(super) version: u32,
    pub(super) header_size: u32,
    pub(super) entry_size: u32,
    pub(super) flags: u32,
    pub(super) reserved: [u8; 32],
}

/// Each copy occupies independent cache lines; writing the inactive copy must
/// never write back an incompletely prepared active descriptor or selector.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(crate) struct DescriptorCopy {
    pub(super) descriptor: NvDescriptor,
    reserved: [u8; 56],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(crate) struct NvEntry {
    /// Zero is unallocated; otherwise (selector - 1) % 2 selects the copy.
    /// Incrementing the selector also lets readers detect repeated A/B switches.
    pub(super) commit_word: u64,
    reserved: [u8; 56],
    copies: [DescriptorCopy; 2],
}

const _: () = {
    assert!(size_of::<NvImageHeader>() == 64);
    assert!(size_of::<DescriptorCopy>() == 128);
    assert!(size_of::<NvEntry>() == 320);
    assert!(offset_of!(NvEntry, commit_word) == 0);
    assert!(offset_of!(NvEntry, copies) == 64);
};

pub(super) fn descriptor_offset(index: usize, selector: u64) -> Result<usize> {
    if selector == 0 {
        return_errno!(Errno::EINVAL);
    }
    entry_offset(index)?
        .checked_add(offset_of!(NvEntry, copies))
        .and_then(|offset| {
            offset.checked_add(((selector - 1) % 2) as usize * size_of::<DescriptorCopy>())
        })
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))
}

pub(super) fn control_len(capacity_pages: usize) -> Result<usize> {
    size_of::<NvImageHeader>()
        .checked_add(
            capacity_pages
                .checked_mul(size_of::<NvEntry>())
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
        )
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))
}

pub(super) fn entry_offset(index: usize) -> Result<usize> {
    size_of::<NvImageHeader>()
        .checked_add(
            index
                .checked_mul(size_of::<NvEntry>())
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
        )
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))
}

pub(super) fn commit_offset(index: usize) -> Result<usize> {
    entry_offset(index)?
        .checked_add(offset_of!(NvEntry, commit_word))
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))
}
