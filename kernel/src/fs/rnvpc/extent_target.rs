// SPDX-License-Identifier: MPL-2.0

//! Failure-atomic target word for persistent extent entries.

use crate::prelude::*;

const TARGET_KIND_SHIFT: u32 = 61;
const TARGET_PAYLOAD_MASK: u64 = (1u64 << TARGET_KIND_SHIFT) - 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ExtentTargetKind {
    SsdWritten = 1,
    SsdUnwritten = 2,
    NvPage = 3,
    Child = 4,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Pod)]
#[repr(transparent)]
pub(crate) struct ExtentTarget(u64);

impl ExtentTarget {
    pub(crate) const UNPUBLISHED: Self = Self(0);

    pub(crate) fn ssd(block: u64, unwritten: bool) -> Result<Self> {
        Self::new(
            if unwritten {
                ExtentTargetKind::SsdUnwritten
            } else {
                ExtentTargetKind::SsdWritten
            },
            block,
            false,
        )
    }

    pub(crate) fn nv_page(slot_plus_one: u64) -> Result<Self> {
        Self::new(ExtentTargetKind::NvPage, slot_plus_one, false)
    }

    pub(crate) fn child(slot_plus_one: u64) -> Result<Self> {
        Self::new(ExtentTargetKind::Child, slot_plus_one, false)
    }

    pub(crate) fn from_raw(raw: u64) -> Result<Self> {
        let target = Self(raw);
        target.kind()?;
        target.payload()?;
        Ok(target)
    }

    pub(crate) const fn raw(self) -> u64 {
        self.0
    }

    pub(crate) const fn is_published(self) -> bool {
        self.0 != 0
    }

    pub(crate) fn kind(self) -> Result<ExtentTargetKind> {
        match self.0 >> TARGET_KIND_SHIFT {
            1 => Ok(ExtentTargetKind::SsdWritten),
            2 => Ok(ExtentTargetKind::SsdUnwritten),
            3 => Ok(ExtentTargetKind::NvPage),
            4 => Ok(ExtentTargetKind::Child),
            _ => return_errno_with_message!(Errno::EINVAL, "invalid extent target kind"),
        }
    }

    pub(crate) fn payload(self) -> Result<u64> {
        let payload = self.0 & TARGET_PAYLOAD_MASK;
        if payload == 0 {
            return_errno_with_message!(Errno::EINVAL, "null persistent extent target");
        }
        Ok(payload)
    }

    fn new(kind: ExtentTargetKind, payload: u64, allow_zero: bool) -> Result<Self> {
        if payload > TARGET_PAYLOAD_MASK || (!allow_zero && payload == 0) {
            return_errno_with_message!(Errno::EINVAL, "extent target payload is out of range");
        }
        Ok(Self(((kind as u64) << TARGET_KIND_SHIFT) | payload))
    }
}

const _: () = assert!(size_of::<ExtentTarget>() == size_of::<u64>());

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn ssd_block_zero_is_not_a_persistent_extent_target() {
        assert_eq!(
            ExtentTarget::ssd(0, false).unwrap_err().error(),
            Errno::EINVAL
        );
        assert_eq!(
            ExtentTarget::ssd(0, true).unwrap_err().error(),
            Errno::EINVAL
        );
        assert_eq!(
            ExtentTarget::from_raw((ExtentTargetKind::SsdWritten as u64) << TARGET_KIND_SHIFT)
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
    }
}
