// SPDX-License-Identifier: MPL-2.0

pub(super) mod cpu_local;
mod guard;

pub use self::guard::{DisabledPreemptGuard, disable_preempt};

/// Halts the CPU until interrupts if no preemption is required.
///
/// This function will return if:
///  - preemption is required when calling this function,
///  - preemption is required during halting the CPU, or
///  - interrupts occur during halting the CPU.
///
/// This function will perform preemption before returning if
/// preemption is required.
///
/// # Panics
///
/// This function will panic if it is called in the atomic mode
/// ([`crate::task::atomic_mode`]).
#[track_caller]
pub fn halt_cpu() {
    crate::task::atomic_mode::might_sleep();

    // Idle CPUs may run no other task for many grace periods. Reporting only
    // on a context switch would retain every RCU-retired cache page until each
    // idle CPU happened to schedule work, defeating bounded page-cache reclaim.
    // Bootstrap bypasses `might_sleep`'s atomic-mode checks.
    if !crate::IN_BOOTSTRAP_CONTEXT.load(core::sync::atomic::Ordering::Relaxed) {
        // Recheck after observing the one-way transition out of bootstrap.
        crate::task::atomic_mode::might_sleep();
        // SAFETY: Outside bootstrap, `might_sleep` checked that no
        // atomic-mode/RCU read guard is held.
        unsafe { crate::sync::finish_grace_period() };
    }

    let irq_guard = crate::irq::disable_local();

    if cpu_local::need_preempt() {
        drop(irq_guard);
    } else {
        core::mem::forget(irq_guard);
        // IRQs were previously enabled (checked by `might_sleep`). So we can re-enable them now.
        crate::arch::irq::enable_local_and_halt();
    }

    super::scheduler::might_preempt();
}
