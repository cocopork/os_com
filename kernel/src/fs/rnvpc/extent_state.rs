// SPDX-License-Identifier: MPL-2.0

//! Shared crash-state interpretation for in-place extent publication.

use super::extent_target::{ExtentTarget, ExtentTargetKind};
use crate::prelude::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CrashExtentValue {
    Ssd(u64),
    NvPage(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CrashExtentEntry {
    logical_block: u64,
    length: u32,
    target: Option<ExtentTarget>,
}

impl CrashExtentEntry {
    pub(crate) fn ssd(logical_block: u64, length: u32, block: u64) -> Result<Self> {
        Self::new(
            logical_block,
            length,
            Some(ExtentTarget::ssd(block, false)?),
        )
    }

    pub(crate) fn nv(logical_block: u64, slot_plus_one: u64) -> Result<Self> {
        Self::new(
            logical_block,
            1,
            Some(ExtentTarget::nv_page(slot_plus_one)?),
        )
    }

    pub(crate) const fn unpublished(logical_block: u64, length: u32) -> Self {
        Self {
            logical_block,
            length,
            target: None,
        }
    }

    fn new(logical_block: u64, length: u32, target: Option<ExtentTarget>) -> Result<Self> {
        if length == 0 {
            return_errno_with_message!(Errno::EINVAL, "crash-state extent is empty");
        }
        logical_block
            .checked_add(length as u64)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        Ok(Self {
            logical_block,
            length,
            target,
        })
    }

    fn value_at(self, logical_block: u64) -> Result<Option<CrashExtentValue>> {
        let end = self
            .logical_block
            .checked_add(self.length as u64)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if logical_block < self.logical_block || logical_block >= end {
            return Ok(None);
        }
        let Some(target) = self.target else {
            return Ok(None);
        };
        let value = match target.kind()? {
            ExtentTargetKind::SsdWritten | ExtentTargetKind::SsdUnwritten => CrashExtentValue::Ssd(
                target
                    .payload()?
                    .checked_add(logical_block - self.logical_block)
                    .ok_or_else(|| Error::new(Errno::EOVERFLOW))?,
            ),
            ExtentTargetKind::NvPage if self.length == 1 => {
                CrashExtentValue::NvPage(target.payload()?)
            }
            _ => return_errno_with_message!(Errno::EUCLEAN, "invalid crash-state leaf target"),
        };
        Ok(Some(value))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CrashExtentNode {
    entries: Vec<CrashExtentEntry>,
    sibling: Option<usize>,
}

impl CrashExtentNode {
    pub(crate) const fn leaf(entries: Vec<CrashExtentEntry>, sibling: Option<usize>) -> Self {
        Self { entries, sibling }
    }
}

/// Resolves one logical block while accepting only mapping-equivalent overlaps.
pub(crate) fn lookup_linked_extent(
    nodes: &[CrashExtentNode],
    first: usize,
    logical_block: u64,
) -> Result<Option<CrashExtentValue>> {
    let mut current = first;
    for _ in 0..nodes.len() {
        let node = nodes
            .get(current)
            .ok_or_else(|| Error::new(Errno::EUCLEAN))?;
        let mut found = None;
        for entry in &node.entries {
            let Some(value) = entry.value_at(logical_block)? else {
                continue;
            };
            if found.is_some_and(|old| old != value) {
                return_errno_with_message!(Errno::EUCLEAN, "conflicting extent overlap");
            }
            found = Some(value);
        }
        if found.is_some() {
            return Ok(found);
        }
        let Some(sibling) = node.sibling else {
            return Ok(None);
        };
        current = sibling;
    }
    return_errno_with_message!(Errno::EUCLEAN, "extent sibling cycle")
}
