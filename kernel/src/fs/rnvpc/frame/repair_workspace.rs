// SPDX-License-Identifier: MPL-2.0

//! Bounded cache of cleared metadata frames used by rNVPCe repair images.

use super::NvFrameId;
use crate::prelude::*;

pub(super) const MAX_REPAIR_WORKSPACE_FRAMES: usize = 64;
pub(super) const REPAIR_LOGICAL_INDEX_TAG: u64 = 1 << 62;

/// Holds allocated but uncommitted metadata frames. Frames enter this cache
/// only after their durable descriptor has been cleared.
pub(super) struct RepairWorkspace {
    frames: Vec<NvFrameId>,
    limit: usize,
}

impl RepairWorkspace {
    pub(super) fn new(metadata_capacity: usize) -> Self {
        Self {
            frames: Vec::new(),
            limit: MAX_REPAIR_WORKSPACE_FRAMES.min(metadata_capacity / 8),
        }
    }

    pub(super) fn take(&mut self) -> Option<NvFrameId> {
        self.frames.pop()
    }

    /// Returns `true` when the cache takes ownership of `frame`.
    pub(super) fn put(&mut self, frame: NvFrameId) -> bool {
        if self.frames.len() == self.limit {
            return false;
        }
        self.frames.push(frame);
        true
    }

    pub(super) fn drain_one(&mut self) -> Option<NvFrameId> {
        self.frames.pop()
    }
}

#[cfg(ktest)]
mod tests {
    use super::*;

    #[ostd::prelude::ktest]
    fn repair_workspace_is_bounded_by_one_eighth_of_metadata_capacity() {
        let mut workspace = RepairWorkspace::new(16);
        assert!(workspace.put(NvFrameId::try_from(1).unwrap()));
        assert!(workspace.put(NvFrameId::try_from(2).unwrap()));
        assert!(!workspace.put(NvFrameId::try_from(3).unwrap()));
    }

    #[ostd::prelude::ktest]
    fn repair_workspace_does_not_reserve_tiny_pool_capacity() {
        let mut workspace = RepairWorkspace::new(7);
        assert!(!workspace.put(NvFrameId::try_from(1).unwrap()));
        assert!(workspace.take().is_none());
    }
}
