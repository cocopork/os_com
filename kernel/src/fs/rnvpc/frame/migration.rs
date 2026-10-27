// SPDX-License-Identifier: MPL-2.0

//! Lease-protected, bounded descriptor phases for rNVPCe migration.

use super::*;

impl Rnvpc {
    /// Allocation ownership is already durable. The caller retains every
    /// eviction lease until SSD publication or migration failure.
    pub(crate) fn assign_migration_targets(
        &self,
        targets: &[(PersistentObjectRef, u64)],
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        let mut updates = Vec::with_capacity(targets.len());
        let mut keys = Vec::with_capacity(targets.len());
        let mut expected = Vec::with_capacity(targets.len());
        for (reference, target) in targets {
            let slot = self.migration_slot(*reference, true)?;
            let mut descriptor = self
                .persist
                .read_entry(slot)?
                .filter(|d| d.allocation_seq() == reference.allocation_seq())
                .ok_or_else(|| Error::new(Errno::ESTALE))?;
            keys.push(PageKey::new(
                descriptor.fs_id,
                descriptor.inode,
                descriptor.logical_index,
            ));
            expected.push(DescriptorHandle {
                reference: *reference,
                logical_version: descriptor.recovery_id,
                state: descriptor.state()?,
                ssd_target: descriptor.ssd_backing_block,
                migration_target: descriptor.has_migration_target(),
            });
            descriptor.assign_migration_target(*target)?;
            updates.push((slot, descriptor));
        }
        self.persist.update_descriptors(&updates, fault)?;
        // One short control-index critical section per batch, never across persist.
        {
            let mut state = self.state.lock();
            for ((key, expected), (_, target)) in keys.iter().zip(&expected).zip(targets) {
                if let Some(entry) = state.index.lookup_mut(*key) {
                    if entry.reference == expected.reference
                        && entry.logical_version == expected.logical_version
                    {
                        entry.ssd_backing_block = *target;
                        entry.state = NvObjectState::NvWriteback;
                    }
                }
            }
        }
        for ((key, expected), (_, target)) in keys.iter().zip(expected).zip(targets) {
            let replacement = DescriptorHandle {
                ssd_target: *target,
                migration_target: true,
                ..expected
            };
            match self.redirects.lookup(*key) {
                Some(current) if current == expected => {
                    if !self.redirects.compare_update(*key, expected, replacement) {
                        return_errno_with_message!(
                            Errno::ESTALE,
                            "migration redirect generation changed"
                        );
                    }
                }
                // A newer foreground CoW owns the PageKey. The old descriptor
                // still carries this BIO target and is retired by old completion.
                Some(_) => {}
                None => {}
            }
        }
        Ok(())
    }

    /// Forward SSD extents have already been published for these pages.
    /// Persist the phase before the caller returns runtime capacity credits.
    pub(crate) fn mark_migration_credits_released(
        &self,
        references: &[PersistentObjectRef],
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        let mut updates = Vec::with_capacity(references.len());
        for reference in references {
            let slot = self.migration_slot(*reference, false)?;
            let mut descriptor = self
                .persist
                .read_entry(slot)?
                .filter(|d| d.allocation_seq() == reference.allocation_seq())
                .ok_or_else(|| Error::new(Errno::ESTALE))?;
            descriptor.mark_migration_credit_released()?;
            updates.push((slot, descriptor));
        }
        // Descriptor selector publication is intentionally bounded to 128
        // slots. A larger completion round advances it in durable chunks;
        // recovery can finish ledger accounting from any partial prefix.
        for updates in updates.chunks(128) {
            self.persist.update_descriptors(updates, fault)?;
        }
        Ok(())
    }

    fn migration_slot(&self, reference: PersistentObjectRef, require_lease: bool) -> Result<usize> {
        let slot = usize::try_from(reference.slot().ok_or_else(|| Error::new(Errno::EINVAL))?)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let state = self.objects.lock_checked(slot)?;
        let record = &state.records[slot];
        if !matches!(record.kind, ObjectKind::Data(_))
            || !record.committed
            || record.alloc_seq != reference.allocation_seq()
            || (require_lease
                && (record.kind != ObjectKind::Data(DataPageState::InFlight) || record.pins != 1))
        {
            return_errno!(Errno::ESTALE);
        }
        Ok(slot)
    }

    /// Invalidates the successful set before making any slot reusable. A failed
    /// clear conservatively retains all slots; recovery finishes reclamation.
    pub(crate) fn reclaim_migrated_pages(
        self: &Arc<Self>,
        mut pages: Vec<(EvictingDataPage, PersistentObjectRef)>,
        fault: Option<FaultPoint>,
    ) -> Result<()> {
        if pages.len() > 128 {
            return_errno!(Errno::EINVAL);
        }
        let mut slots = Vec::with_capacity(pages.len());
        let mut seen = BTreeSet::new();
        let started = read_ns();
        for (lease, reference) in &pages {
            if !Arc::ptr_eq(self, &lease.pool)
                || !lease.active
                || reference.slot() != Some(lease.id.index() as u64)
                || !seen.insert(lease.id.index())
            {
                return_errno!(Errno::ESTALE);
            }
            let slot = self.migration_slot(*reference, true)?;
            slots.push(slot);
        }
        LATENCY_STATS
            .nvpc_finish_validate_lock
            .record_since(started);
        let started = read_ns();
        self.persist.clear_frames(&slots, fault)?;
        LATENCY_STATS
            .nvpc_finish_clear_descriptor
            .record_since(started);
        // Bound the shared allocation lock hold time; never nest slot locks.
        for chunk in pages.chunks_mut(32) {
            let started = read_ns();
            let mut allocation = self.allocation.lock();
            LATENCY_STATS.reclaim_allocation_wait.record_since(started);
            for (lease, reference) in chunk {
                let slot = lease.id.index();
                let mut state = self.objects.lock(slot);
                debug_assert_eq!(state.records[slot].alloc_seq, reference.allocation_seq());
                state.records[slot] = ObjectRecord::free();
                allocation.free_ids.push(slot);
                allocation.capacity.account_free(PoolClass::Data);
                self.account_unpin();
                lease.active = false;
            }
        }
        // Notify once after the complete successful batch is reusable.
        if !pages.is_empty() {
            note_data_page_lease_change();
        }
        Ok(())
    }
}
