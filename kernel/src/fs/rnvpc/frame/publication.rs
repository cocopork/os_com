// SPDX-License-Identifier: MPL-2.0

//! Inode-local publication exclusion shared by foreground and completion.

use super::*;

pub(crate) struct InodePublication {
    pool: Weak<Rnvpc>,
    fs_id: u64,
    inode: u64,
    owner: Mutex<Option<PublicationOwner>>,
    waiters: WaitQueue,
}

impl InodePublication {
    pub(super) fn new(pool: &Arc<Rnvpc>, fs_id: u64, inode: u64) -> Self {
        Self {
            pool: Arc::downgrade(pool),
            fs_id,
            inode,
            owner: Mutex::new(None),
            waiters: WaitQueue::new(),
        }
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
        pool: &Arc<Rnvpc>,
        role: PublicationRole,
    ) -> Result<PublicationGuard> {
        if !Weak::ptr_eq(&self.pool, &Arc::downgrade(pool)) {
            return_errno_with_message!(Errno::EPERM, "publication belongs to another pool");
        }
        let started = read_ns();
        let mut next_report_ns = 100_000_000u64;
        let claim = || {
            let t_lock = read_ns();
            let mut owner = self.owner.lock();
            if LATENCY_ENABLED && role == PublicationRole::Completion {
                LATENCY_STATS.migration_owner_lock_wait.record_since(t_lock);
            }
            if let Some(current) = *owner {
                if LATENCY_ENABLED && role == PublicationRole::Completion {
                    match current.role {
                        PublicationRole::Foreground => {
                            LATENCY_STATS.migration_busy_foreground.record_event()
                        }
                        PublicationRole::Completion => {
                            LATENCY_STATS.migration_busy_completion.record_event()
                        }
                    }
                }
                return None;
            }
            let acquired_at_ns = read_ns();
            *owner = Some(PublicationOwner {
                role,
                acquired_at_ns,
            });
            Some(acquired_at_ns)
        };
        let acquired_at_ns = if LATENCY_ENABLED && role == PublicationRole::Completion {
            loop {
                if let Ok(acquired) = self
                    .waiters
                    .wait_until_or_timeout(&claim, &Duration::from_millis(10))
                {
                    break acquired;
                }
                let waited_ns = read_ns().saturating_sub(started);
                if waited_ns >= next_report_ns {
                    let owner = *self.owner.lock();
                    if let Some(owner) = owner {
                        error!(
                            "[RNVPCE-PUBLICATION-WAIT] waiter=completion fs_id={} inode={} waited_ms={} owner={:?} owner_held_ms={}",
                            self.fs_id,
                            self.inode,
                            waited_ns / 1_000_000,
                            owner.role,
                            read_ns().saturating_sub(owner.acquired_at_ns) / 1_000_000,
                        );
                    }
                    next_report_ns = waited_ns.saturating_add(1_000_000_000);
                }
            }
        } else {
            self.waiters.wait_until(claim)
        };
        LATENCY_STATS.rnvpce_publication_wait.record_since(started);
        Ok(PublicationGuard {
            pool: pool.clone(),
            fs_id: self.fs_id,
            inode: self.inode,
            acquired_at_ns,
            context: self.clone(),
            role,
        })
    }

    pub(super) fn release(&self) {
        *self.owner.lock() = None;
        self.waiters.wake_one();
    }
}

impl Drop for InodePublication {
    fn drop(&mut self) {
        let Some(pool) = self.pool.upgrade() else {
            return;
        };
        let mut contexts = pool.publication_contexts.lock();
        let key = (self.fs_id, self.inode);
        // A new context may have replaced this expired Weak before Drop ran.
        if contexts
            .get(&key)
            .is_some_and(|entry| core::ptr::eq(entry.as_ptr(), self))
        {
            contexts.remove(&key);
        }
    }
}
