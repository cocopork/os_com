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
    match role {
        Role::User => 0..cpus / 2,
        Role::Kernel => cpus / 2..cpus - 4,
        Role::Flush => cpus - 4..cpus - 3,
        // Reserve cpus - 3 for the historical completion-worker role.
        Role::Virtio => cpus - 2..cpus - 1,
        Role::Journal => cpus - 1..cpus,
    }
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
    fn io_cpu_roles_are_disjoint_and_preserve_historical_layout() {
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
        assert_eq!(cpu_range(Role::Kernel, 64), 32..60);
        assert_eq!(cpu_range(Role::Virtio, 64), 62..63);
        assert_eq!(cpu_range(Role::Journal, 64), 63..64);
        assert_eq!(cpu_range(Role::User, 16), 0..8);
        assert_eq!(cpu_range(Role::Flush, 16), 12..13);
        assert_eq!(cpu_range(Role::Virtio, 16), 14..15);
    }
}
