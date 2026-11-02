// SPDX-License-Identifier: MPL-2.0

//! Opt-in CPU placement for I/O experiments, independent of the scheduler policy.

use core::ops::Range;

use ostd::{
    cpu::{CpuId, CpuSet, num_cpus},
    util::id_set::Id,
};
use spin::Once;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Role {
    User,
    Kernel,
    Flush,
    Virtio,
    Journal,
}

fn cpu_range(role: Role, cpus: usize) -> Range<usize> {
    assert!(cpus >= 10, "I/O CPU isolation requires at least 10 CPUs");
    // Leave at least one Kernel CPU plus Flush, completion reserve, and Journal.
    // The one-slot layout retains all historical tail-role CPU numbers.
    let virtio_slots = 16.min((cpus - cpus / 2 - 4).max(1));
    let flush_cpu = cpus - virtio_slots - 3;
    match role {
        Role::User => 0..cpus / 2,
        Role::Kernel => cpus / 2..flush_cpu,
        Role::Flush => flush_cpu..flush_cpu + 1,
        // Reserve flush_cpu + 1 for the historical completion-worker role.
        Role::Virtio => flush_cpu + 2..cpus - 1,
        Role::Journal => cpus - 1..cpus,
    }
}

fn virtio_queue_cpu(queue_id: u16, queue_count: u16, cpus: usize) -> usize {
    assert!(queue_count > 0 && queue_id < queue_count);
    let region = cpu_range(Role::Virtio, cpus);
    region.start + usize::from(queue_id) % region.len()
}

/// Places queue forwarders on distinct reserved CPUs when capacity permits.
///
/// CPU sharing does not affect the strict queue identity carried by each BIO.
pub(crate) fn virtio_queue_affinity(queue_id: u16, queue_count: u16) -> CpuSet {
    if !enabled() {
        return CpuSet::new_full();
    }
    let mut cpus = CpuSet::new_empty();
    cpus.add(CpuId::try_from(virtio_queue_cpu(queue_id, queue_count, num_cpus())).unwrap());
    cpus
}

fn enabled() -> bool {
    static ENABLED: Once<bool> = Once::new();
    *ENABLED.call_once(|| {
        let enabled = ostd::boot::boot_info()
            .kernel_cmdline
            .split_ascii_whitespace()
            .any(|arg| arg == "rtio_cpu_isolation=on");
        assert!(
            !enabled || num_cpus() >= 10,
            "I/O CPU isolation requires at least 10 CPUs"
        );
        enabled
    })
}

pub(crate) fn affinity(role: Role) -> CpuSet {
    if !enabled() {
        return CpuSet::new_full();
    }
    let mut cpus = CpuSet::new_empty();
    for cpu in cpu_range(role, num_cpus()) {
        cpus.add(CpuId::try_from(cpu).unwrap());
    }
    cpus
}

/// Checks initial placement and optionally reports it before the workload starts.
pub(crate) fn trace(role: Role) {
    if !enabled() {
        return;
    }
    static TRACE: Once<bool> = Once::new();
    if !*TRACE.call_once(|| {
        ostd::boot::boot_info()
            .kernel_cmdline
            .split_ascii_whitespace()
            .any(|arg| arg == "rtio_cpu_trace=on")
    }) {
        return;
    }
    let cpu = CpuId::current_racy().as_usize();
    assert!(
        cpu_range(role, num_cpus()).contains(&cpu),
        "I/O CPU placement mismatch"
    );
    ostd::early_println!("[CPU-ISOLATION] role={:?} cpu={}", role, cpu);
}

#[cfg(ktest)]
mod tests {
    use super::*;

    #[ostd::prelude::ktest]
    fn virtio_queue_cpu_sets_are_disjoint_when_capacity_allows() {
        for (cpus, count, first) in [(10, 1, 8), (16, 4, 11), (64, 16, 47)] {
            for queue_id in 0..count {
                assert_eq!(
                    virtio_queue_cpu(queue_id, count, cpus),
                    first + usize::from(queue_id)
                );
            }
        }
        // Limited CPU capacity permits sharing without changing queue identity.
        assert_eq!(virtio_queue_cpu(3, 4, 10), 8);
        assert_eq!(virtio_queue_cpu(4, 8, 16), 11);
        assert_eq!(cpu_range(Role::Flush, 10), 6..7);
        assert_eq!(cpu_range(Role::Virtio, 10), 8..9);
        assert_eq!(cpu_range(Role::Journal, 10), 9..10);
    }

    #[ostd::prelude::ktest]
    fn io_cpu_roles_are_disjoint_and_preserve_single_slot_layout() {
        let roles = [
            Role::User,
            Role::Kernel,
            Role::Flush,
            Role::Virtio,
            Role::Journal,
        ];
        for cpus in [10, 16, 64] {
            for (index, role) in roles.iter().enumerate() {
                let selected = cpu_range(*role, cpus);
                assert!(!selected.is_empty());
                assert!(selected.end <= cpus);
                for other in &roles[index + 1..] {
                    assert!(
                        selected
                            .clone()
                            .all(|cpu| !cpu_range(*other, cpus).contains(&cpu)),
                        "CPU roles must not overlap"
                    );
                }
            }
        }
        assert_eq!(cpu_range(Role::User, 64), 0..32);
        assert_eq!(cpu_range(Role::Kernel, 64), 32..45);
        assert_eq!(cpu_range(Role::Virtio, 64), 47..63);
        assert_eq!(cpu_range(Role::Journal, 64), 63..64);
        assert_eq!(cpu_range(Role::User, 16), 0..8);
        assert_eq!(cpu_range(Role::Flush, 16), 9..10);
        assert_eq!(cpu_range(Role::Virtio, 16), 11..15);
    }
}
