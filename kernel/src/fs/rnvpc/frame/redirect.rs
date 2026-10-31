// SPDX-License-Identifier: MPL-2.0

//! Exact descriptor publication and retirement for rNVPCe redirects.

use super::*;
use crate::fs::rnvpc::batch_metrics;
#[cfg(ktest)]
use crate::fs::rnvpc::redirect_index::RedirectRange;

#[derive(Clone, Copy)]
enum RedirectEntryFault {
    Point(Option<FaultPoint>),
    #[cfg(ktest)]
    Counted(Option<crate::fs::rnvpc::fault::FaultSpec>),
}

impl Rnvpc {
    pub(crate) fn begin_reserved_data_ranges(
        self: &Arc<Self>,
        reservation: &mut CapacityReservation,
        pages: usize,
    ) -> Result<Vec<StagingDataRange>> {
        if pages == 0 || reservation.pages.len() < pages {
            return_errno_with_message!(Errno::EAGAIN, "insufficient reserved NV data pages");
        }
        let mut ranges = Vec::new();
        let mut current = Vec::new();
        for _ in 0..pages {
            let page = self.begin_reserved_data_page(reservation)?;
            let contiguous = current.last().is_some_and(|previous: &StagingDataPage| {
                previous.id().index().checked_add(1) == Some(page.id().index())
            });
            if !current.is_empty()
                && (!contiguous
                    || current.len()
                        == crate::fs::rnvpc::redirect_index::MAX_DIRTY_MIGRATION_EXTENT_PAGES)
            {
                ranges.push(StagingDataRange {
                    pages: core::mem::take(&mut current),
                });
            }
            current.push(page);
        }
        if !current.is_empty() {
            ranges.push(StagingDataRange { pages: current });
        }
        Ok(ranges)
    }

    pub(crate) fn begin_range_data_mutation(
        self: &Arc<Self>,
        entry: &Arc<DirtyNvExtentEntry>,
        logical_range: Range<u64>,
    ) -> Result<RangeDataMutation> {
        if logical_range.start >= logical_range.end
            || logical_range.start < entry.logical_start()
            || logical_range.end > entry.logical_range().end
        {
            return_errno_with_message!(Errno::EINVAL, "mutation is outside NV data range");
        }
        let guard = entry.try_write()?;
        Ok(RangeDataMutation {
            pool: self.clone(),
            guard,
            logical_range,
        })
    }

    pub(crate) fn install_data_range_owner(
        &self,
        entry: &Arc<DirtyNvExtentEntry>,
        slots: Range<usize>,
    ) -> Result<()> {
        self.objects.install_data_range_owner(entry, slots)
    }

    pub(crate) fn remove_data_range_owner(
        &self,
        entry: &Arc<DirtyNvExtentEntry>,
    ) -> Result<Range<NvPageId>> {
        let slots = self.objects.remove_data_range_owner(entry)?;
        Ok(NvPageId::try_from(slots.start)?..NvPageId::try_from(slots.end)?)
    }

    pub(crate) fn data_range_segments(
        &self,
        entry: &Arc<DirtyNvExtentEntry>,
        offset: usize,
        count: usize,
    ) -> Result<Vec<USegment>> {
        let slots = self.objects.data_range_owner(entry)?;
        let start = slots
            .start
            .checked_add(offset)
            .filter(|start| *start <= slots.end)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let end = start
            .checked_add(count)
            .filter(|end| *end <= slots.end)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        (start..end)
            .map(|slot| self.backend.data_page_segment(NvPageId::try_from(slot)?))
            .collect()
    }

    #[cfg(ktest)]
    pub(crate) fn persisted_redirect_extent_for_test(
        &self,
        head_slot: usize,
    ) -> Result<Option<NvRedirectExtentRecord>> {
        self.persist.read_redirect_extent(head_slot)
    }

    #[cfg(ktest)]
    pub(crate) fn persistent_slot_is_selected_for_test(&self, slot: usize) -> Result<bool> {
        self.persist.slot_is_selected_for_test(slot)
    }

    #[cfg(ktest)]
    pub(crate) fn mark_redirect_extent_pending_for_test(
        &self,
        entry: &DirtyNvExtentEntry,
    ) -> Result<()> {
        let pending = entry
            .record()
            .with_authority(NvRedirectAuthority::SsdAuthoritativePendingFree)?;
        self.persist.publish_redirect_extent(&pending, None)
    }

    pub(crate) fn claim_ready_nv_entries(
        &self,
        fs_id: u64,
        page_budget: usize,
    ) -> Result<Vec<NvExtentEntryLease>> {
        let leases = self.redirects.claim_ready_entries(fs_id, page_budget)?;
        for lease in &leases {
            self.objects.claim_data_range_owner(lease.entry())?;
            lease.wait_for_writers();
        }
        if !leases.is_empty() {
            note_data_page_lease_change();
        }
        Ok(leases)
    }

    pub(crate) fn lookup_dirty_nv_entry(&self, key: PageKey) -> Option<Arc<DirtyNvExtentEntry>> {
        self.redirects.lookup_dirty_entry(key)
    }

    pub(crate) fn lookup_dirty_nv_overlaps(
        &self,
        fs_id: u64,
        inode: u64,
        range: Range<u64>,
    ) -> Result<Vec<Arc<DirtyNvExtentEntry>>> {
        self.redirects.lookup_overlaps(fs_id, inode, range)
    }

    #[cfg(ktest)]
    pub(crate) fn redirect_overlap_lookups(&self) -> usize {
        self.redirects.overlap_lookup_count()
    }

    #[cfg(ktest)]
    pub(crate) fn dirty_nv_entry_count_for_test(&self, fs_id: u64, inode: u64) -> usize {
        self.redirects.dirty_entry_count(fs_id, inode)
    }

    #[cfg(ktest)]
    pub(crate) fn native_redirect_page_handle_count_for_test(
        &self,
        fs_id: u64,
        inode: u64,
    ) -> usize {
        self.redirects.page_handle_count(fs_id, inode)
    }

    pub(crate) fn redirect_publication_uncertain(&self) -> bool {
        self.persist.publication_uncertain()
    }

    #[cfg(ktest)]
    pub(crate) fn reset_data_range_lock_acquisitions_for_test(&self) {
        self.objects.reset_data_range_lock_acquisitions();
    }

    #[cfg(ktest)]
    pub(crate) fn data_range_lock_acquisitions_for_test(&self) -> usize {
        self.objects.data_range_lock_acquisitions()
    }

    pub(crate) fn data_segment_from_entry(
        &self,
        guard: &NvExtentReadGuard,
        logical_page: u64,
    ) -> Result<USegment> {
        let slot = usize::try_from(guard.nv_slot(logical_page)?)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        self.backend.data_page_segment(NvPageId::try_from(slot)?)
    }

    pub(crate) fn copy_entry_to_writer(
        &self,
        guard: &NvExtentReadGuard,
        logical_page: u64,
        offset: usize,
        writer: &mut VmWriter,
        len: usize,
    ) -> Result<usize> {
        let slot = usize::try_from(guard.nv_slot(logical_page)?)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        self.backend
            .copy_to_writer(NvPageId::try_from(slot)?, offset, writer, len)
    }

    pub(crate) fn complete_nv_entry(&self, lease: &NvExtentEntryLease) -> Result<()> {
        self.redirects.complete_entry(lease)?;
        note_data_page_lease_change();
        Ok(())
    }

    pub(crate) fn data_segment_for_migration(
        &self,
        lease: &NvExtentEntryLease,
        offset: usize,
    ) -> Result<USegment> {
        if offset >= usize::from(lease.entry().page_count()) {
            return_errno!(Errno::EINVAL);
        }
        let slot = lease
            .entry()
            .nv_slot_range()
            .start
            .checked_add(offset as u64)
            .and_then(|slot| usize::try_from(slot).ok())
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.backend.data_page_segment(NvPageId::try_from(slot)?)
    }

    pub(crate) fn reclaim_nv_entry(&self, lease: &NvExtentEntryLease) -> Result<usize> {
        lease.begin_reclaim_and_wait_for_readers()?;
        let entry = lease.entry();
        let pending = entry
            .record()
            .with_authority(NvRedirectAuthority::SsdAuthoritativePendingFree)?;
        self.persist.publish_redirect_extent(&pending, None)?;

        let slot_range = entry.nv_slot_range();
        let slots = slot_range
            .clone()
            .map(|slot| usize::try_from(slot).map_err(|_| Error::new(Errno::EOVERFLOW)))
            .collect::<Result<Vec<_>>>()?;
        let mut prepared = Vec::with_capacity(slots.len());
        for (offset, slot) in slots.iter().copied().enumerate() {
            let expected = PageKey::new(
                entry.fs_id(),
                entry.inode(),
                entry.logical_start() + offset as u64,
            );
            let mut objects = self.objects.lock_checked(slot)?;
            let record = &objects.records[slot];
            if !record.committed
                || !matches!(record.kind, ObjectKind::Data(DataPageState::NvDirty))
                || record.key != Some(expected)
            {
                drop(objects);
                self.release_entry_reclaim_pins(&prepared);
                return_errno_with_message!(Errno::ESTALE, "NV extent entry slot ownership changed");
            }
            if record.persisting || record.pins != 0 {
                drop(objects);
                self.release_entry_reclaim_pins(&prepared);
                return_errno_with_message!(Errno::EBUSY, "NV extent entry slot is busy");
            }
            if let Err(error) = self.account_pin() {
                drop(objects);
                self.release_entry_reclaim_pins(&prepared);
                return Err(error);
            }
            objects.records[slot].pins = 1;
            objects.records[slot].persisting = true;
            prepared.push(slot);
        }
        let head_slot = usize::try_from(entry.nv_slot_range().start)
            .map_err(|_| Error::new(Errno::EOVERFLOW))?;
        if let Err(error) = self.persist.clear_frame(head_slot, None) {
            self.release_entry_reclaim_pins(&prepared);
            return Err(error);
        }
        for slot in slots.iter().copied() {
            let mut objects = self.objects.lock(slot);
            let record = &mut objects.records[slot];
            record.kind = ObjectKind::Data(DataPageState::Synced);
            record.pins = 0;
            record.persisting = false;
            record.committed = false;
            record.key = None;
            record.version = 0;
            self.account_unpin();
        }
        note_data_page_lease_change();
        self.redirects.remove_reclaimed_entry(lease)?;
        let owned_pages = self.remove_data_range_owner(entry)?;
        let pages = slots
            .into_iter()
            .map(|slot| NvPageId::try_from(slot).map_err(|_| Error::new(Errno::EOVERFLOW)))
            .collect::<Result<Vec<_>>>()?;
        if owned_pages.start != pages[0]
            || owned_pages.end.index() != pages.last().unwrap().index() + 1
        {
            return_errno_with_message!(Errno::ESTALE, "NV extent entry range owner changed");
        }
        let reclaimed = pages.len();
        self.free_data_pages(&pages)?;
        lease.finish_reclaim()?;
        Ok(reclaimed)
    }

    fn release_entry_reclaim_pins(&self, slots: &[usize]) {
        for slot in slots {
            let mut objects = self.objects.lock(*slot);
            objects.records[*slot].pins = 0;
            objects.records[*slot].persisting = false;
            self.account_unpin();
        }
        if !slots.is_empty() {
            note_data_page_lease_change();
        }
    }

    #[cfg(ktest)]
    pub(crate) fn persisted_redirect_state_for_test(
        &self,
        handle: DescriptorHandle,
    ) -> Result<NvObjectState> {
        let slot = handle
            .reference
            .slot()
            .and_then(|slot| usize::try_from(slot).ok())
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        self.persist
            .read_entry(slot)?
            .ok_or_else(|| Error::new(Errno::ESTALE))?
            .state()
    }

    pub(crate) fn claim_ready_dirty_extents(
        &self,
        fs_id: u64,
        page_budget: usize,
    ) -> Result<Vec<MigrationExtentLease>> {
        self.redirects.claim_ready_extents(fs_id, page_budget)
    }

    pub(crate) fn claim_dirty_extent(
        self: &Arc<Self>,
        logical_lease: MigrationExtentLease,
    ) -> Result<ClaimedDirtyExtent> {
        if !self.redirects.validate_claimed_extent(&logical_lease) {
            let _ = self.redirects.requeue_extent(logical_lease);
            return_errno!(Errno::ESTALE);
        }
        let extent = logical_lease.extent();
        let base_slot =
            usize::try_from(extent.nv_base_slot()).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let slot_lease = match self.objects.claim_migration_range(base_slot, extent.len()) {
            Ok(lease) => lease,
            Err(error) => {
                let _ = self.redirects.requeue_extent(logical_lease);
                return Err(error);
            }
        };
        let keys = (0..extent.len())
            .map(|offset| {
                PageKey::new(
                    extent.fs_id(),
                    extent.inode(),
                    extent.logical_start() + offset as u64,
                )
            })
            .collect::<Vec<_>>();
        let baseline_claim = match self.baseline_generations.try_claim_range(&keys) {
            Ok(claim) => claim,
            Err(error) => {
                let _ = self.objects.release_migration_range(slot_lease);
                let _ = self.redirects.requeue_extent(logical_lease);
                return Err(error);
            }
        };
        if !self.redirects.validate_claimed_extent(&logical_lease) {
            self.baseline_generations.release_range(baseline_claim);
            let _ = self.objects.release_migration_range(slot_lease);
            let _ = self.redirects.requeue_extent(logical_lease);
            return_errno!(Errno::ESTALE);
        }
        let token_pages = keys
            .iter()
            .zip(baseline_claim.generations())
            .zip(extent.pages())
            .map(|((key, generation), handle)| {
                (
                    *key,
                    *generation,
                    if handle.migration_target {
                        0
                    } else {
                        handle.ssd_target
                    },
                )
            })
            .collect::<Vec<_>>();
        let baseline_token = match self.baseline_generations.claimed_token(&token_pages) {
            Ok(token) => token,
            Err(error) => {
                self.baseline_generations.release_range(baseline_claim);
                let _ = self.objects.release_migration_range(slot_lease);
                let _ = self.redirects.requeue_extent(logical_lease);
                return Err(error);
            }
        };
        let pages = extent.pages().to_vec();
        let fs_id = extent.fs_id();
        let inode = extent.inode();
        let logical_start = extent.logical_start();
        let slot_len = extent.len();
        let generation = logical_lease.generation();
        Ok(ClaimedDirtyExtent {
            pool: self.clone(),
            pages,
            fs_id,
            inode,
            logical_start,
            slot_base: base_slot,
            slot_len,
            generation,
            leases: Mutex::new(ClaimedDirtyExtentLeases {
                logical_lease: Some(logical_lease),
                slot_lease: Some(slot_lease),
                baseline_claim: Some(baseline_claim),
            }),
            baseline_token: Arc::new(baseline_token),
        })
    }

    pub(crate) fn migration_overlap(
        &self,
        fs_id: u64,
        inode: u64,
        logical_start: u64,
        pages: usize,
    ) -> bool {
        self.redirects
            .claimed_extent_overlaps(fs_id, inode, logical_start, pages)
    }

    pub(crate) fn first_migration_overlap(
        &self,
        fs_id: u64,
        inode: u64,
        logical_start: u64,
        pages: usize,
    ) -> Option<MigrationConflict> {
        self.redirects
            .first_claimed_overlap(fs_id, inode, logical_start, pages)
    }

    pub(crate) fn publish_redirect_batch(
        &self,
        pages: &[(NvPageId, PageKey, u64)],
        fault: Option<FaultPoint>,
    ) -> Result<Vec<DescriptorHandle>> {
        if pages.is_empty() {
            return Ok(Vec::new());
        }
        if pages.len() > 128 {
            return_errno_with_message!(Errno::EINVAL, "redirect publication exceeds 128 pages");
        }
        let metrics_started = batch_metrics::now();
        let keys: Vec<_> = pages.iter().map(|(_, key, _)| *key).collect();
        let was_unmapped = keys
            .iter()
            .map(|key| self.redirects.lookup(*key).is_none())
            .collect::<Vec<_>>();
        let mut prepared = Vec::new();
        prepared
            .try_reserve_exact(pages.len())
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        let reservation = loop {
            match self.redirects.reserve_versions(&keys) {
                Ok(reservation) => break reservation,
                Err(error) if error.error() == Errno::EBUSY => crate::thread::Thread::yield_now(),
                Err(error) => return Err(error),
            }
        };
        let mut slots = BTreeSet::new();
        let preparation = (|| -> Result<()> {
            for (page, key, ssd_target) in pages {
                if !slots.insert(page.index()) {
                    return_errno_with_message!(Errno::EINVAL, "duplicate redirect NVPage");
                }
                let logical_version = reservation
                    .logical_version(*key)
                    .ok_or_else(|| Error::new(Errno::EINVAL))?;
                let mut objects = self.objects.lock_checked(page.index())?;
                self.pin_persistent_update(
                    &mut objects,
                    page.index(),
                    ObjectKind::Data(DataPageState::NvDirty),
                )?;
                let allocation_seq = objects.records[page.index()].alloc_seq;
                let reference = PersistentObjectRef::new(page.index() as u64, allocation_seq)?;
                prepared.push((
                    *page,
                    *key,
                    *ssd_target,
                    DescriptorHandle {
                        reference,
                        logical_version,
                        state: NvObjectState::NvDirty,
                        ssd_target: *ssd_target,
                        migration_target: false,
                    },
                ));
            }
            Ok(())
        })();
        if let Err(error) = preparation {
            self.release_redirect_publication_pins(&prepared);
            self.redirects.abort_reserved(&reservation)?;
            return Err(error);
        }

        let mut records = Vec::new();
        let mut range_start = 0usize;
        while range_start < prepared.len() {
            if !was_unmapped[range_start] {
                range_start += 1;
                continue;
            }
            let mut range_end = range_start + 1;
            while range_end < prepared.len()
                && was_unmapped[range_end]
                && range_end - range_start < 128
                && prepared[range_end].1.fs_id == prepared[range_end - 1].1.fs_id
                && prepared[range_end].1.inode == prepared[range_end - 1].1.inode
                && prepared[range_end - 1].1.page_index.checked_add(1)
                    == Some(prepared[range_end].1.page_index)
                && prepared[range_end - 1].0.index().checked_add(1)
                    == Some(prepared[range_end].0.index())
                && prepared[range_end - 1].2.checked_add(1) == Some(prepared[range_end].2)
            {
                range_end += 1;
            }
            let (head_page, head_key, head_ssd, _) = prepared[range_start];
            records.push(NvRedirectExtentRecord::new(
                head_key.fs_id,
                head_key.inode,
                head_key.page_index,
                u16::try_from(range_end - range_start).map_err(|_| Error::new(Errno::EOVERFLOW))?,
                head_page.index() as u64,
                head_ssd,
                NvRedirectAuthority::NvAuthoritative,
            )?);
            range_start = range_end;
        }
        let new_slots = prepared
            .iter()
            .zip(&was_unmapped)
            .filter_map(|((page, _, _, _), is_new)| is_new.then_some(page.index()))
            .collect::<Vec<_>>();
        let existing_slots = prepared
            .iter()
            .zip(&was_unmapped)
            .filter_map(|((page, _, _, _), is_new)| (!is_new).then_some(page.index()))
            .collect::<Vec<_>>();
        let persist_result = self
            .persist
            .persist_data_slots(&existing_slots)
            .and_then(|()| {
                self.persist
                    .commit_redirect_extents(&records, &new_slots, fault)
            });
        if let Err(error) = persist_result {
            self.release_redirect_publication_pins(&prepared);
            self.redirects.abort_reserved(&reservation)?;
            if !self.persist.publication_uncertain() {
                for ((page, _, _, _), is_new) in prepared.iter().zip(&was_unmapped) {
                    if *is_new {
                        let _ = self.abort_data_page(*page);
                    }
                }
            }
            return Err(error);
        }
        batch_metrics::count(batch_metrics::Event::RedirectPublishBatches, 1);
        batch_metrics::count(batch_metrics::Event::RedirectPublishedPages, prepared.len());

        let redirect_updates: Vec<_> = prepared
            .iter()
            .map(|(_, key, _, handle)| (*key, *handle))
            .collect();
        for (page, key, _, handle) in &prepared {
            let mut objects = self.objects.lock(page.index());
            let record = &mut objects.records[page.index()];
            record.pins -= 1;
            record.persisting = false;
            record.committed = true;
            record.key = Some(*key);
            record.version = handle.logical_version;
            self.account_unpin();
        }
        let new_entries = records
            .into_iter()
            .map(DirtyNvExtentEntry::new)
            .map(|entry| entry.map(Arc::new))
            .collect::<Result<Vec<_>>>()?;
        self.redirects
            .commit_reserved(&reservation, &redirect_updates)?;
        for entry in new_entries {
            self.redirects.publish_dirty_entry(entry)?;
        }

        let handles = prepared
            .into_iter()
            .map(|(_, _, _, handle)| handle)
            .collect();
        batch_metrics::elapsed(batch_metrics::Phase::RedirectPublish, metrics_started);
        Ok(handles)
    }

    pub(crate) fn publish_redirect_entries(
        &self,
        pages: &[(NvPageId, PageKey, u64)],
        visibility_generation: u64,
        fault: Option<FaultPoint>,
    ) -> Result<Vec<Arc<DirtyNvExtentEntry>>> {
        self.publish_redirect_entries_inner(
            pages,
            visibility_generation,
            RedirectEntryFault::Point(fault),
        )
    }

    #[cfg(ktest)]
    pub(crate) fn publish_redirect_entries_counted(
        &self,
        pages: &[(NvPageId, PageKey, u64)],
        visibility_generation: u64,
        fault: Option<crate::fs::rnvpc::fault::FaultSpec>,
    ) -> Result<Vec<Arc<DirtyNvExtentEntry>>> {
        self.publish_redirect_entries_inner(
            pages,
            visibility_generation,
            RedirectEntryFault::Counted(fault),
        )
    }

    fn publish_redirect_entries_inner(
        &self,
        pages: &[(NvPageId, PageKey, u64)],
        visibility_generation: u64,
        fault: RedirectEntryFault,
    ) -> Result<Vec<Arc<DirtyNvExtentEntry>>> {
        if pages.is_empty() {
            return Ok(Vec::new());
        }
        if pages.len() > crate::fs::rnvpc::redirect_index::MAX_DIRTY_MIGRATION_EXTENT_PAGES {
            return_errno_with_message!(Errno::EINVAL, "redirect publication exceeds 128 pages");
        }
        let metrics_started = batch_metrics::now();
        let mut prepared = Vec::new();
        prepared
            .try_reserve_exact(pages.len())
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        let mut slots = BTreeSet::new();
        for (page, key, ssd_target) in pages {
            if *ssd_target == 0
                || !slots.insert(page.index())
                || self.lookup_dirty_nv_entry(*key).is_some()
            {
                self.release_entry_publication_pins(&prepared);
                return_errno_with_message!(Errno::EBUSY, "redirect entry publication overlaps");
            }
            let mut objects = self.objects.lock_checked(page.index())?;
            if let Err(error) = self.pin_persistent_update(
                &mut objects,
                page.index(),
                ObjectKind::Data(DataPageState::NvDirty),
            ) {
                drop(objects);
                self.release_entry_publication_pins(&prepared);
                return Err(error);
            }
            prepared.push((*page, *key, *ssd_target));
        }

        let mut records = Vec::new();
        records
            .try_reserve_exact(prepared.len())
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        let mut range_start = 0usize;
        while range_start < prepared.len() {
            let mut range_end = range_start + 1;
            while range_end < prepared.len()
                && range_end - range_start
                    < crate::fs::rnvpc::redirect_index::MAX_DIRTY_MIGRATION_EXTENT_PAGES
                && prepared[range_end].1.fs_id == prepared[range_end - 1].1.fs_id
                && prepared[range_end].1.inode == prepared[range_end - 1].1.inode
                && prepared[range_end - 1].1.page_index.checked_add(1)
                    == Some(prepared[range_end].1.page_index)
                && prepared[range_end - 1].0.index().checked_add(1)
                    == Some(prepared[range_end].0.index())
                && prepared[range_end - 1].2.checked_add(1) == Some(prepared[range_end].2)
            {
                range_end += 1;
            }
            let (head_page, head_key, head_ssd) = prepared[range_start];
            records.push(NvRedirectExtentRecord::new_with_visibility(
                head_key.fs_id,
                head_key.inode,
                head_key.page_index,
                u16::try_from(range_end - range_start).map_err(|_| Error::new(Errno::EOVERFLOW))?,
                head_page.index() as u64,
                head_ssd,
                NvRedirectAuthority::NvAuthoritative,
                visibility_generation,
            )?);
            range_start = range_end;
        }
        let entries = records
            .iter()
            .copied()
            .map(DirtyNvExtentEntry::new)
            .map(|entry| entry.map(Arc::new))
            .collect::<Result<Vec<_>>>()?;
        let mut owners_installed = 0usize;
        for entry in &entries {
            let slots = entry.nv_slot_range();
            let slots = usize::try_from(slots.start).map_err(|_| Error::new(Errno::EOVERFLOW))?
                ..usize::try_from(slots.end).map_err(|_| Error::new(Errno::EOVERFLOW))?;
            if let Err(error) = self.install_data_range_owner(entry, slots) {
                for installed in &entries[..owners_installed] {
                    let _ = self.remove_data_range_owner(installed);
                }
                self.release_entry_publication_pins(&prepared);
                return Err(error);
            }
            owners_installed += 1;
        }

        let data_slots = prepared
            .iter()
            .map(|(page, _, _)| page.index())
            .collect::<Vec<_>>();
        let persist_result = match fault {
            RedirectEntryFault::Point(fault) => {
                self.persist
                    .commit_redirect_extents(&records, &data_slots, fault)
            }
            #[cfg(ktest)]
            RedirectEntryFault::Counted(fault) => {
                self.persist
                    .commit_redirect_extents_counted(&records, &data_slots, fault)
            }
        };
        if let Err(error) = persist_result {
            self.release_entry_publication_pins(&prepared);
            if !self.persist.publication_uncertain() {
                for entry in &entries {
                    let _ = self.remove_data_range_owner(entry);
                }
            }
            return Err(error);
        }

        for (page, key, _) in &prepared {
            let mut objects = self.objects.lock(page.index());
            let record = &mut objects.records[page.index()];
            record.pins -= 1;
            record.persisting = false;
            record.committed = true;
            record.key = Some(*key);
            record.version = 0;
            self.account_unpin();
        }
        if let Err(error) = self.redirects.publish_entries(&entries) {
            self.persist.latch_publication_uncertain();
            return Err(error);
        }
        batch_metrics::count(batch_metrics::Event::RedirectPublishBatches, 1);
        batch_metrics::count(batch_metrics::Event::RedirectPublishedPages, prepared.len());
        batch_metrics::elapsed(batch_metrics::Phase::RedirectPublish, metrics_started);
        Ok(entries)
    }

    fn release_entry_publication_pins(&self, prepared: &[(NvPageId, PageKey, u64)]) {
        for (page, _, _) in prepared {
            let mut objects = self.objects.lock(page.index());
            let record = &mut objects.records[page.index()];
            record.pins -= 1;
            record.persisting = false;
            self.account_unpin();
        }
        if !prepared.is_empty() {
            note_data_page_lease_change();
        }
    }

    fn release_redirect_publication_pins(
        &self,
        prepared: &[(NvPageId, PageKey, u64, DescriptorHandle)],
    ) {
        for (page, _, _, _) in prepared {
            let mut objects = self.objects.lock(page.index());
            let record = &mut objects.records[page.index()];
            record.pins -= 1;
            record.persisting = false;
            self.account_unpin();
        }
        if !prepared.is_empty() {
            note_data_page_lease_change();
        }
    }

    pub(crate) fn pin_redirect(
        self: &Arc<Self>,
        key: PageKey,
        expected: DescriptorHandle,
    ) -> Result<PinnedDataPage> {
        if self.redirects.lookup(key) != Some(expected) {
            return_errno!(Errno::ESTALE);
        }
        let pin = self.pin_persistent_data_page(expected.reference)?;
        {
            let objects = self.objects.lock(pin.id.index());
            let record = &objects.records[pin.id.index()];
            if record.key != Some(key)
                || record.version != expected.logical_version
                || !record.committed
            {
                return_errno!(Errno::ESTALE);
            }
        }
        if self.redirects.lookup(key) != Some(expected) {
            return_errno!(Errno::ESTALE);
        }
        Ok(pin)
    }

    pub(crate) fn transition_redirects(
        &self,
        redirects: &[(PageKey, DescriptorHandle)],
        next: NvObjectState,
        fault: Option<FaultPoint>,
    ) -> Result<Vec<DescriptorHandle>> {
        if redirects.len() > 128 {
            return_errno!(Errno::EINVAL);
        }
        let mut updates = Vec::new();
        let mut slots = BTreeSet::new();
        for (key, handle) in redirects {
            if !handle.state.can_transition_to(next) {
                self.release_transition_pins(&updates);
                return_errno!(Errno::EINVAL);
            }
            if self.redirects.lookup(*key) != Some(*handle) {
                self.release_transition_pins(&updates);
                return_errno!(Errno::ESTALE);
            }
            let slot = handle
                .reference
                .slot()
                .and_then(|slot| usize::try_from(slot).ok())
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            if !slots.insert(slot) {
                self.release_transition_pins(&updates);
                return_errno!(Errno::EINVAL);
            }
            let mut objects = self.objects.lock_checked(slot)?;
            let record = &objects.records[slot];
            if !record.committed
                || record.alloc_seq != handle.reference.allocation_seq()
                || record.key != Some(*key)
                || record.version != handle.logical_version
                || record.persisting
                || record.pins != 0
            {
                drop(objects);
                self.release_transition_pins(&updates);
                return_errno!(Errno::ESTALE);
            }
            if let Err(error) = self.account_pin() {
                drop(objects);
                self.release_transition_pins(&updates);
                return Err(error);
            }
            objects.records[slot].pins = 1;
            objects.records[slot].persisting = true;
            drop(objects);
            let descriptor = if handle.migration_target
                || (handle.state == NvObjectState::NvWriteback && next == NvObjectState::NvDirty)
            {
                self.persist
                    .read_entry(slot)?
                    .filter(|descriptor| {
                        descriptor.allocation_seq() == handle.reference.allocation_seq()
                            && descriptor.recovery_id == handle.logical_version
                    })
                    .ok_or_else(|| Error::new(Errno::ESTALE))
                    .and_then(|mut descriptor| {
                        descriptor.transition_to(next)?;
                        Ok(descriptor)
                    })
            } else {
                self.descriptor_for(
                    PoolClass::Data,
                    *key,
                    handle.logical_version,
                    slot,
                    Some(handle.ssd_target),
                )
                .and_then(|mut descriptor| {
                    descriptor.transition_to(next)?;
                    Ok(descriptor)
                })
            };
            let descriptor = match descriptor {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    self.release_one_redirect_pin(slot);
                    self.release_transition_pins(&updates);
                    return Err(error);
                }
            };
            updates.push((slot, *key, *handle, descriptor));
        }
        let persisted: Vec<_> = updates
            .iter()
            .map(|(slot, _, _, descriptor)| (*slot, *descriptor))
            .collect();
        if let Err(error) = self.persist.update_descriptors(&persisted, fault) {
            self.release_transition_pins(&updates);
            return Err(error);
        }
        let runtime = Self::runtime_state_for(next)?;
        let mut transitioned = Vec::with_capacity(updates.len());
        for (slot, key, old, _) in updates {
            let new = DescriptorHandle { state: next, ..old };
            let mut objects = self.objects.lock(slot);
            objects.records[slot].kind = ObjectKind::Data(runtime);
            objects.records[slot].pins = 0;
            objects.records[slot].persisting = false;
            self.account_unpin();
            drop(objects);
            self.redirects.compare_update(key, old, new);
            transitioned.push(new);
        }
        if !transitioned.is_empty() {
            note_data_page_lease_change();
        }
        Ok(transitioned)
    }

    #[cfg(ktest)]
    pub(crate) fn dirty_redirect_ranges(&self, fs_id: u64, budget: usize) -> Vec<RedirectRange> {
        self.redirects
            .dirty_ranges(budget)
            .into_iter()
            .filter(|range| range.fs_id == fs_id)
            .collect()
    }

    #[cfg(ktest)]
    pub(crate) fn claim_redirect_range(
        self: &Arc<Self>,
        snapshot: &RedirectRange,
        budget: usize,
    ) -> Result<Vec<ClaimedRedirectRange>> {
        let metrics_started = batch_metrics::now();
        let limit = snapshot.pages.len().min(budget).min(128);
        let mut candidates = Vec::new();
        let mut claimed_generations = BTreeMap::new();
        for (offset, handle) in snapshot.pages.iter().copied().take(limit).enumerate() {
            let page_index = snapshot
                .logical_start
                .checked_add(offset as u64)
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
            let key = PageKey::new(snapshot.fs_id, snapshot.inode, page_index);
            if self.redirects.lookup(key) == Some(handle)
                && let Some(generation) = self.baseline_generations.try_claim_migration(key)
            {
                candidates.push((key, handle));
                claimed_generations.insert(key, generation);
            }
        }

        let mut claimed = Vec::new();
        let mut cursor = 0;
        while cursor < candidates.len() {
            let start = cursor;
            cursor += 1;
            while cursor < candidates.len()
                && candidates[cursor].0.page_index == candidates[cursor - 1].0.page_index + 1
            {
                cursor += 1;
            }
            let group = &candidates[start..cursor];
            let transitioned =
                match self.transition_redirects(group, NvObjectState::NvWriteback, None) {
                    Ok(handles) => group
                        .iter()
                        .zip(handles)
                        .map(|((key, _), handle)| (*key, handle))
                        .collect(),
                    Err(error)
                        if error.error() == Errno::ESTALE || error.error() == Errno::EBUSY =>
                    {
                        let mut pages = Vec::new();
                        for item in group {
                            if let Ok(mut handles) = self.transition_redirects(
                                core::slice::from_ref(item),
                                NvObjectState::NvWriteback,
                                None,
                            ) {
                                pages.push((item.0, handles.remove(0)));
                            } else {
                                self.baseline_generations.release_migration(item.0);
                            }
                        }
                        pages
                    }
                    Err(error) => {
                        for (key, _) in group {
                            self.baseline_generations.release_migration(*key);
                        }
                        return Err(error);
                    }
                };
            if transitioned.is_empty() {
                continue;
            }

            let mut pages = Vec::new();
            for (key, handle) in transitioned {
                match self.pin_redirect(key, handle) {
                    Ok(pin) => pages.push(ClaimedRedirectPage {
                        key,
                        handle,
                        pin,
                        baseline_generation: claimed_generations[&key],
                    }),
                    Err(_) => {
                        self.baseline_generations.release_migration(key);
                        if let Ok(reclaimed) = self.clear_redirects_exact(&[(key, handle)], None) {
                            for page in reclaimed {
                                let _ = self.free_data_page(page);
                            }
                        }
                    }
                }
            }
            let mut page_cursor = 0;
            while page_cursor < pages.len() {
                let page_start = page_cursor;
                page_cursor += 1;
                while page_cursor < pages.len()
                    && pages[page_cursor].key.page_index
                        == pages[page_cursor - 1].key.page_index + 1
                {
                    page_cursor += 1;
                }
                let contiguous = pages.drain(page_start..page_cursor).collect::<Vec<_>>();
                page_cursor = page_start;
                let token_pages = contiguous
                    .iter()
                    .map(|page| {
                        (
                            page.key,
                            page.baseline_generation,
                            if page.handle.migration_target {
                                0
                            } else {
                                page.handle.ssd_target
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                let baseline_token = self.baseline_generations.claimed_token(&token_pages)?;
                claimed.push(ClaimedRedirectRange {
                    baseline_token,
                    pages: contiguous,
                });
            }
        }
        batch_metrics::count(batch_metrics::Event::CandidateRanges, claimed.len());
        batch_metrics::count(
            batch_metrics::Event::CandidatePages,
            claimed.iter().map(|range| range.pages.len()).sum(),
        );
        batch_metrics::elapsed(batch_metrics::Phase::RangeClaim, metrics_started);
        Ok(claimed)
    }

    pub(crate) fn validate_baseline_token(&self, token: &BaselineRangeToken) -> bool {
        self.baseline_generations.validate(token)
    }

    #[cfg(ktest)]
    pub(crate) fn reset_baseline_validation_calls_for_test(&self) {
        self.baseline_generations.reset_validation_calls();
    }

    #[cfg(ktest)]
    pub(crate) fn baseline_validation_calls_for_test(&self) -> usize {
        self.baseline_generations.validation_calls()
    }

    pub(crate) fn release_baseline_migration(&self, key: PageKey) {
        self.baseline_generations.release_migration(key);
    }

    pub(crate) fn begin_baseline_mutation(
        &self,
        keys: &[PageKey],
    ) -> Result<BaselineMutationGuard<'_>> {
        self.baseline_generations.begin_mutation(keys)
    }

    pub(crate) fn advance_claimed_baseline(&self, keys: &[PageKey]) -> Result<()> {
        self.baseline_generations.advance_claimed(keys)
    }

    #[cfg(ktest)]
    pub(crate) fn advance_baseline_generation(
        &self,
        fs_id: u64,
        inode: u64,
        logical_start: u64,
        pages: usize,
    ) -> Result<()> {
        self.baseline_generations
            .advance(fs_id, inode, logical_start, pages)
    }

    fn release_transition_pins(
        &self,
        updates: &[(usize, PageKey, DescriptorHandle, NvDescriptor)],
    ) {
        for (slot, _, _, _) in updates {
            let mut objects = self.objects.lock(*slot);
            objects.records[*slot].pins = 0;
            objects.records[*slot].persisting = false;
            self.account_unpin();
        }
        if !updates.is_empty() {
            note_data_page_lease_change();
        }
    }

    fn release_one_redirect_pin(&self, slot: usize) {
        let mut objects = self.objects.lock(slot);
        objects.records[slot].pins = 0;
        objects.records[slot].persisting = false;
        self.account_unpin();
        drop(objects);
        note_data_page_lease_change();
    }

    pub(crate) fn clear_redirects_exact(
        &self,
        redirects: &[(PageKey, DescriptorHandle)],
        fault: Option<FaultPoint>,
    ) -> Result<Vec<NvPageId>> {
        let metrics_started = batch_metrics::now();
        let mut prepared = Vec::new();
        let mut slots = BTreeSet::new();
        for (key, handle) in redirects {
            let slot = handle
                .reference
                .slot()
                .and_then(|slot| usize::try_from(slot).ok())
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            if self.objects.migration_range_contains(slot) {
                self.release_clear_pins(&prepared);
                return_errno_with_message!(Errno::EBUSY, "redirect range is migrating");
            }
            if !slots.insert(slot) {
                self.release_clear_pins(&prepared);
                return_errno!(Errno::EINVAL);
            }
            let mut objects = self.objects.lock_checked(slot)?;
            let record = &objects.records[slot];
            if !record.committed
                || !matches!(record.kind, ObjectKind::Data(_))
                || record.alloc_seq != handle.reference.allocation_seq()
                || record.key != Some(*key)
                || record.version != handle.logical_version
            {
                drop(objects);
                self.release_clear_pins(&prepared);
                return_errno!(Errno::ESTALE);
            }
            if record.persisting || record.pins != 0 {
                drop(objects);
                self.release_clear_pins(&prepared);
                return_errno_with_message!(Errno::EBUSY, "redirect still has an active pin");
            }
            if let Err(error) = self.account_pin() {
                drop(objects);
                self.release_clear_pins(&prepared);
                return Err(error);
            }
            objects.records[slot].pins = 1;
            objects.records[slot].persisting = true;
            prepared.push((slot, *key, *handle));
        }
        for chunk in prepared.chunks(128) {
            let slots: Vec<_> = chunk.iter().map(|(slot, _, _)| *slot).collect();
            if let Err(error) = self.persist.clear_frames(&slots, fault) {
                self.release_clear_pins(&prepared);
                return Err(error);
            }
            batch_metrics::count(batch_metrics::Event::DescriptorClearBatches, 1);
        }
        let mut reclaimed = Vec::with_capacity(prepared.len());
        for (slot, _, _) in &prepared {
            let page = NvPageId::try_from(*slot)?;
            let mut objects = self.objects.lock(*slot);
            let record = &mut objects.records[*slot];
            record.kind = ObjectKind::Data(DataPageState::Synced);
            record.pins = 0;
            record.persisting = false;
            record.committed = false;
            record.key = None;
            record.version = 0;
            self.account_unpin();
            drop(objects);
            reclaimed.push(page);
        }
        let exact = prepared
            .iter()
            .map(|(_, key, handle)| (*key, *handle))
            .collect::<Vec<_>>();
        self.redirects.compare_remove_batch(&exact);
        if !reclaimed.is_empty() {
            note_data_page_lease_change();
        }
        batch_metrics::elapsed(batch_metrics::Phase::DescriptorClear, metrics_started);
        Ok(reclaimed)
    }

    fn release_clear_pins(&self, prepared: &[(usize, PageKey, DescriptorHandle)]) {
        for (slot, _, _) in prepared {
            let mut objects = self.objects.lock(*slot);
            objects.records[*slot].pins = 0;
            objects.records[*slot].persisting = false;
            self.account_unpin();
        }
        if !prepared.is_empty() {
            note_data_page_lease_change();
        }
    }

    fn runtime_state_for(state: NvObjectState) -> Result<DataPageState> {
        match state {
            NvObjectState::NvDirty => Ok(DataPageState::NvDirty),
            NvObjectState::NvWriteback => Ok(DataPageState::InFlight),
            NvObjectState::NvReclaimable => Ok(DataPageState::Synced),
            NvObjectState::DramClean => return_errno!(Errno::EINVAL),
        }
    }
}
