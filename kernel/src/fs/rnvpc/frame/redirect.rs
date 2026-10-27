// SPDX-License-Identifier: MPL-2.0

//! Exact descriptor publication and retirement for rNVPCe redirects.

use super::*;
use crate::fs::rnvpc::batch_metrics;

impl Rnvpc {
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

        let mut updates = Vec::new();
        if updates.try_reserve_exact(prepared.len()).is_err() {
            self.release_redirect_publication_pins(&prepared);
            self.redirects.abort_reserved(&reservation)?;
            return_errno!(Errno::ENOMEM);
        }
        for (page, key, ssd_target, handle) in &prepared {
            let descriptor = match self.descriptor_for(
                PoolClass::Data,
                *key,
                handle.logical_version,
                page.index(),
                Some(*ssd_target),
            ) {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    self.release_redirect_publication_pins(&prepared);
                    self.redirects.abort_reserved(&reservation)?;
                    return Err(error);
                }
            };
            updates.push((page.index(), descriptor));
        }
        if let Err(error) = self.persist.commit_frames(&updates, fault) {
            self.release_redirect_publication_pins(&prepared);
            self.redirects.abort_reserved(&reservation)?;
            if !self.persist.publication_uncertain() {
                for (page, _, _, _) in &prepared {
                    let _ = self.abort_data_page(*page);
                }
            }
            return Err(error);
        }
        batch_metrics::count(batch_metrics::Event::RedirectPublishBatches, 1);
        batch_metrics::count(batch_metrics::Event::RedirectPublishedPages, updates.len());

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
        self.redirects
            .commit_reserved(&reservation, &redirect_updates)?;

        let mut legacy = self.state.lock();
        for (page, key, ssd_target, handle) in &prepared {
            if self.redirects.lookup(*key) == Some(*handle) {
                legacy.index.insert(
                    *key,
                    PageEntry {
                        residency: Residency::Nv,
                        handle: Some(RuntimeHandle::Nv(*page)),
                        reference: handle.reference,
                        logical_version: handle.logical_version,
                        ssd_backing_block: *ssd_target,
                        state: handle.state,
                    },
                );
            }
        }
        let handles = prepared
            .into_iter()
            .map(|(_, _, _, handle)| handle)
            .collect();
        batch_metrics::elapsed(batch_metrics::Phase::RedirectPublish, metrics_started);
        Ok(handles)
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
            let mut legacy = self.state.lock();
            if let Some(entry) = legacy.index.lookup_mut(key)
                && entry.reference == old.reference
                && entry.logical_version == old.logical_version
            {
                entry.state = next;
                entry.ssd_backing_block = new.ssd_target;
            }
            transitioned.push(new);
        }
        if !transitioned.is_empty() {
            note_data_page_lease_change();
        }
        Ok(transitioned)
    }

    pub(crate) fn dirty_redirect_ranges(&self, fs_id: u64, budget: usize) -> Vec<RedirectRange> {
        self.redirects
            .dirty_ranges(budget)
            .into_iter()
            .filter(|range| range.fs_id == fs_id)
            .collect()
    }

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
        for (slot, key, handle) in prepared {
            let page = NvPageId::try_from(slot)?;
            let mut objects = self.objects.lock(slot);
            let record = &mut objects.records[slot];
            record.kind = ObjectKind::Data(DataPageState::Synced);
            record.pins = 0;
            record.persisting = false;
            record.committed = false;
            record.key = None;
            record.version = 0;
            self.account_unpin();
            drop(objects);
            self.redirects.compare_remove(key, handle);
            let mut legacy = self.state.lock();
            if legacy.index.lookup(key).is_some_and(|entry| {
                entry.reference == handle.reference
                    && entry.logical_version == handle.logical_version
            }) {
                legacy.index.remove(key);
            }
            reclaimed.push(page);
        }
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
