// SPDX-License-Identifier: MPL-2.0

//! Deterministic fault points at RNVPC persistence protocol boundaries.

use crate::prelude::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum FaultPoint {
    BeforeContentPersist,
    AfterDependencyPersist,
    AfterVisibilityPersist,
    AfterExtentPreparedPersist,
    AfterExtentParentPersist,
    AfterExtentPhasePersist,
    AfterExtentChildPersist,
    AfterContentPersist,
    AfterCommitStore,
    AfterCommitPersist,
    AfterCommitFence,
    BeforeClear,
    AfterClearStore,
    AfterClearPersist,
    AfterClearFence,
}

#[cfg(ktest)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FaultSpec {
    pub(crate) point: FaultPoint,
    pub(crate) occurrence: usize,
}

#[cfg(ktest)]
pub(super) fn inject_counted(
    selected: Option<FaultSpec>,
    current: FaultPoint,
    seen: &mut usize,
) -> Result<()> {
    let Some(selected) = selected.filter(|selected| selected.point == current) else {
        return Ok(());
    };
    *seen = seen
        .checked_add(1)
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
    if selected.occurrence == 0 || *seen == selected.occurrence {
        return_errno_with_message!(Errno::EIO, "injected counted RNVPC persistence fault");
    }
    Ok(())
}

pub(super) fn inject(selected: Option<FaultPoint>, current: FaultPoint) -> Result<()> {
    if selected == Some(current) {
        return_errno_with_message!(Errno::EIO, "injected RNVPC persistence fault");
    }
    Ok(())
}
