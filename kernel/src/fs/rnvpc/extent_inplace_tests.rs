// SPDX-License-Identifier: MPL-2.0

//! Crash-state model tests for FAST/FAIR extent publication.

use ostd::prelude::ktest;

use super::{
    extent_state::{CrashExtentEntry, CrashExtentNode, CrashExtentValue, lookup_linked_extent},
    extent_target::{ExtentTarget, ExtentTargetKind},
};
use crate::prelude::*;

#[ktest]
fn extent_target_is_one_atomic_word_and_preserves_kind() {
    assert_eq!(size_of::<ExtentTarget>(), size_of::<u64>());
    let cases = [
        (
            ExtentTarget::ssd(17, false).unwrap(),
            ExtentTargetKind::SsdWritten,
            17,
        ),
        (
            ExtentTarget::ssd(23, true).unwrap(),
            ExtentTargetKind::SsdUnwritten,
            23,
        ),
        (
            ExtentTarget::nv_page(31).unwrap(),
            ExtentTargetKind::NvPage,
            31,
        ),
        (
            ExtentTarget::child(47).unwrap(),
            ExtentTargetKind::Child,
            47,
        ),
    ];
    for (target, kind, payload) in cases {
        assert_eq!(target.kind().unwrap(), kind);
        assert_eq!(target.payload().unwrap(), payload);
        assert_eq!(ExtentTarget::from_raw(target.raw()).unwrap(), target);
    }
}

#[ktest]
fn extent_target_rejects_reserved_kind_and_payload_overflow() {
    assert!(ExtentTarget::from_raw(7u64 << 61).is_err());
    assert!(ExtentTarget::ssd(1u64 << 61, false).is_err());
    assert!(ExtentTarget::nv_page(0).is_err());
    assert!(ExtentTarget::child(0).is_err());
}

#[ktest]
fn identical_ssd_targets_remain_distinct_valid_ranges() {
    let left = ModelEntry::ssd(0, 4, 100);
    let right = ModelEntry::ssd(8, 4, 100);
    let decoded = decode_stable_ranges(&[left, right]).unwrap();
    assert_eq!(decoded, vec![(0, 4, 100), (8, 12, 100)]);
}

#[ktest]
fn logical_split_is_readable_after_every_persisted_stage() {
    let old = CrashExtentEntry::ssd(0, 8, 100).unwrap();
    let right = CrashExtentEntry::ssd(5, 3, 105).unwrap();
    let page_on_ssd = CrashExtentEntry::ssd(4, 1, 104).unwrap();
    let left = CrashExtentEntry::ssd(0, 4, 100).unwrap();
    let page_on_nv = CrashExtentEntry::nv(4, 9).unwrap();
    let stages = [
        vec![old],
        vec![old, right],
        vec![old, right, page_on_ssd],
        vec![left, right, page_on_ssd],
        vec![left, right, page_on_nv],
    ];

    for (stage, entries) in stages.iter().enumerate() {
        let node = CrashExtentNode::leaf(entries.clone(), None);
        for logical in 0..8 {
            let actual = lookup_linked_extent(&[node.clone()], 0, logical).unwrap();
            let expected = if stage == 4 && logical == 4 {
                CrashExtentValue::NvPage(9)
            } else {
                CrashExtentValue::Ssd(100 + logical)
            };
            assert_eq!(actual, Some(expected));
        }
    }
}

#[ktest]
fn unpublished_entry_is_ignored_after_crash() {
    let old = CrashExtentEntry::ssd(0, 8, 100).unwrap();
    let prepared = CrashExtentEntry::unpublished(4, 1);
    let node = CrashExtentNode::leaf(vec![old, prepared], None);
    assert_eq!(
        lookup_linked_extent(&[node], 0, 4).unwrap(),
        Some(CrashExtentValue::Ssd(104))
    );
}

#[ktest]
fn fair_split_is_readable_before_and_after_old_entry_removal() {
    let left = CrashExtentEntry::ssd(0, 8, 100).unwrap();
    let right = CrashExtentEntry::ssd(8, 8, 108).unwrap();
    let sibling = CrashExtentNode::leaf(vec![right], None);

    let duplicate_stage = CrashExtentNode::leaf(vec![left, right], Some(1));
    assert_eq!(
        lookup_linked_extent(&[duplicate_stage, sibling.clone()], 0, 12).unwrap(),
        Some(CrashExtentValue::Ssd(112))
    );

    let removed_stage = CrashExtentNode::leaf(vec![left], Some(1));
    assert_eq!(
        lookup_linked_extent(&[removed_stage, sibling], 0, 12).unwrap(),
        Some(CrashExtentValue::Ssd(112))
    );
}

#[ktest]
fn inconsistent_overlap_is_rejected_instead_of_returning_wrong_data() {
    let old = CrashExtentEntry::ssd(0, 8, 100).unwrap();
    let wrong = CrashExtentEntry::ssd(4, 1, 999).unwrap();
    let node = CrashExtentNode::leaf(vec![old, wrong], None);
    assert!(lookup_linked_extent(&[node], 0, 4).is_err());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModelEntry {
    start: u64,
    len: u32,
    target: ExtentTarget,
}

impl ModelEntry {
    fn ssd(start: u64, len: u32, block: u64) -> Self {
        Self {
            start,
            len,
            target: ExtentTarget::ssd(block, false).unwrap(),
        }
    }
}

fn decode_stable_ranges(entries: &[ModelEntry]) -> Result<Vec<(u64, u64, u64)>> {
    let mut ranges = Vec::new();
    let mut previous_end = None;
    for entry in entries {
        let end = entry.start.checked_add(entry.len as u64).unwrap();
        if entry.len == 0 || previous_end.is_some_and(|old_end| entry.start < old_end) {
            return_errno!(Errno::EUCLEAN);
        }
        let block = match entry.target.kind()? {
            ExtentTargetKind::SsdWritten | ExtentTargetKind::SsdUnwritten => {
                entry.target.payload()?
            }
            _ => return_errno!(Errno::EUCLEAN),
        };
        ranges.push((entry.start, end, block));
        previous_end = Some(end);
    }
    Ok(ranges)
}
