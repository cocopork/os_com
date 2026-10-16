// SPDX-License-Identifier: MPL-2.0

//! Runtime state machine for NV-resident data pages.
//!
//! 池内状态必须与持久侧的 `NvObjectState`（`design.md §5.2`）一致：
//! `NvDirty`/`InFlight`/`Synced` 分别对应 `NvDirty`/`NvWriteback`/`NvReclaimable`。
//! `NvReclaimable` 是终态：已写回的 clean NVPage 只能被回收，不得改写成新版本，
//! 更不得转成 DRAM 页（`design-evolution §6` 的废弃项）。

use super::descriptor::NvObjectState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DataPageState {
    Free,
    Staged,
    NvDirty,
    InFlight,
    Synced,
}

impl DataPageState {
    pub(super) fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Staged, Self::NvDirty)
                | (Self::NvDirty, Self::InFlight)
                | (Self::InFlight, Self::NvDirty)
                | (Self::InFlight, Self::Synced)
        )
    }

    pub(super) fn can_free(self) -> bool {
        matches!(self, Self::Staged | Self::Synced)
    }

    /// 该运行时状态对应的持久侧 NV 对象状态。
    ///
    /// `None` 表示"还不是已发布的 NV 对象"（`Free`/`Staged`）——`DramClean` 不在
    /// 这个映射里：它只属于 DRAM 读缓存（索引侧的 `Residency::Dram`），不是 NV 对象
    /// 的持久状态。
    pub(super) const fn nv_object_state(self) -> Option<NvObjectState> {
        match self {
            Self::Free | Self::Staged => None,
            Self::NvDirty => Some(NvObjectState::NvDirty),
            Self::InFlight => Some(NvObjectState::NvWriteback),
            Self::Synced => Some(NvObjectState::NvReclaimable),
        }
    }
}
