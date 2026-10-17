// SPDX-License-Identifier: MPL-2.0

//! Native buffered writes reserve capacity; writeback owns physical allocation.

use super::{super::allocator::DelayedAccount, *};

// Conservative per-page bound: one data block and two edge conversions each
// potentially splitting/growing every level. This deliberately over-reserves
// fragmented writes; successful materialization releases the unused headroom.
const PROMISE_PER_BLOCK: u64 = extent_manager::DELALLOC_PROMISE_PER_BLOCK;
const MAP_BATCH: usize = 64;

impl Inode {
    pub(super) fn write_delayed_chunk(
        &self,
        fs: &Ext4,
        offset: usize,
        reader: &mut VmReader,
    ) -> Result<usize> {
        fs.ensure_not_shutdown()?;
        fs.ensure_writable()?;
        if fs.journal().is_some_and(|journal| journal.is_aborted()) {
            return_errno_with_message!(
                Errno::EROFS,
                "filesystem is read-only after a journal abort"
            );
        }
        let amount = reader.remain();
        let end = offset
            .checked_add(amount)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let start_block =
            Iblock::try_from(offset / BLOCK_SIZE).map_err(|_| Error::new(Errno::EFBIG))?;
        let end_block =
            Iblock::try_from(end.div_ceil(BLOCK_SIZE)).map_err(|_| Error::new(Errno::EFBIG))?;
        let mut inner = self.inner.write();
        inner.ensure_size_within_limit(fs, end)?;
        let old_size = inner.file_size();
        let em = inner.extent_manager()?.clone();
        let needs_mapping = !em.range_known_written(start_block, end_block);
        // Even an already-mapped last page must keep the new EOF private
        // until ordered writeback has registered the newly appended bytes.
        let mut delayed = needs_mapping || end > old_size;
        if delayed {
            let account = inner
                .delayed_account
                .get_or_insert_with(|| fs.delayed_account());
            if needs_mapping {
                let missing = em.delayed_missing(start_block, end_block);
                if let Err(error) = fs.reserve_delayed(account, missing * PROMISE_PER_BLOCK) {
                    // A cold/evicted mapping fact is not evidence of a hole.
                    // Full volumes still permit existing-block overwrites.
                    if em.debug_range_all_written(start_block, end_block)? {
                        delayed = end > old_size;
                    } else {
                        return Err(error);
                    }
                }
            }
        }
        if end > old_size {
            if let Err(error) = inner.resize_page_cache(end, old_size) {
                inner.retain_delayed_promise()?;
                return Err(error);
            }
        }
        let result = inner.page_cache()?.write(offset, reader);
        let copied = amount - reader.remain();
        if copied != 0 {
            if delayed {
                let copied_end = (offset + copied).div_ceil(BLOCK_SIZE) as Iblock;
                em.record_delayed(start_block, copied_end);
            }
            inner.set_file_size(old_size.max(offset + copied));
            inner.set_mtime_ctime(super::super::utils::now());
        }
        if copied < amount && end > old_size.max(offset + copied) {
            inner.resize_page_cache(old_size.max(offset + copied), end)?;
        }
        inner.retain_delayed_promise()?;
        match result {
            Ok(()) => Ok(copied),
            Err(_) if copied != 0 => Ok(copied),
            Err(error) => Err(error.into()),
        }
    }

    /// Resolves a bounded delayed prefix. The caller holds the inode write lock;
    /// admission/restart never runs under the extent tree lock.
    pub(super) fn resolve_delalloc_locked(
        &self,
        fs: &Ext4,
        inner: &mut InodeInner,
        limit: usize,
    ) -> Result<usize> {
        if self.type_ != InodeType::File
            || fs.rnvpc_mode() != crate::fs::rnvpc::mode::RnvpcMode::Native
        {
            return Ok(0);
        }
        let Ok(em) = inner.extent_manager().cloned() else {
            return Ok(0);
        };
        // mmap can dirty holes without going through write(). Allocate only
        // actual dirty pages, never every hole in a broad ordered range.
        for index in inner.page_cache()?.dirty_page_indices(limit) {
            let block = Iblock::try_from(index).map_err(|_| Error::new(Errno::EFBIG))?;
            if em.delayed_missing(block, block + 1) == 0 {
                continue;
            }
            if matches!(
                em.map_blocks(block)?,
                extent_manager::Mapping::Mapped { written: true, .. }
            ) {
                continue;
            }
            let account = inner
                .delayed_account
                .get_or_insert_with(|| fs.delayed_account());
            fs.reserve_delayed(account, PROMISE_PER_BLOCK)?;
            em.record_delayed(block, block + 1);
        }

        let Some(owner) = inner.delayed_account.as_ref().map(DelayedAccount::id) else {
            return Ok(0);
        };
        let mut completed = 0;
        while completed < limit {
            let Some((start, end)) = em.first_delayed() else {
                break;
            };
            let end = end.min(start.saturating_add(MAP_BATCH.min(limit - completed) as Iblock));
            let depth = em.root_depth();
            let mut op = fs.begin_op(fs.write_credits(depth))?;
            let reached = loop {
                // Every restarted handle needs its own ordered registration,
                // before any metadata can be exposed by that transaction.
                if let Some(handle) = op.get() {
                    handle.register_ordered_data(
                        self.ino,
                        self.self_weak.clone(),
                        inner.page_cache()?.clone(),
                        inner.file_size(),
                    )?;
                }
                let mapped = em.with_delayed_owner(owner, || {
                    let mut mappings = extent_manager::NewMappings::default();
                    let outcome = em.ensure_allocated_chunk(start, end, op.get(), &mut mappings)?;
                    let reached = match outcome {
                        extent_manager::HoleFill::Filled => end,
                        extent_manager::HoleFill::Stopped { reached, .. } => reached,
                    };
                    if reached > start {
                        em.mark_range_written(start, reached, op.get())?;
                    }
                    Ok((reached, outcome))
                });
                let (reached, outcome) = match mapped {
                    Ok(result) => result,
                    Err(error) => {
                        if let Some(handle) = op.get() {
                            handle.abort_journal_on_fs_error();
                        }
                        return Err(error);
                    }
                };
                if reached > start {
                    break reached;
                }
                let extent_manager::HoleFill::Stopped { need, .. } = outcome else {
                    return_errno!(Errno::EUCLEAN);
                };
                if fs
                    .journal()
                    .is_some_and(|journal| need > journal.max_credits())
                {
                    return_errno_with_message!(
                        Errno::EFBIG,
                        "one delayed extent exceeds journal capacity"
                    );
                }
                let handle = op.get_mut().ok_or_else(|| Error::new(Errno::EUCLEAN))?;
                // No tree/page/capacity lock is held across re-admission.
                journal::journal_restart(handle, need)?;
            };
            em.remove_delayed(start, reached);
            inner.retain_delayed_promise()?;
            // Mutating through Dirty marks the descriptor even when i_size did
            // not change (e.g. materializing a hole inside an existing file).
            let size = inner.file_size();
            inner.desc.set_size(size as u64);
            if op.get().is_some() {
                inner.write_back_inode_desc(fs, self.ino, op.get())?;
                inner.stamp_datasync_tid(op.get());
            }
            completed += (reached - start) as usize;
        }
        Ok(completed)
    }
}

impl InodeInner {
    pub(super) fn retain_delayed_promise(&self) -> Result<()> {
        if let Some(account) = &self.delayed_account {
            let pending = self.extent_manager()?.delayed_blocks() * PROMISE_PER_BLOCK;
            account.retain(pending.min(account.remaining()))?;
        }
        Ok(())
    }

    pub(super) fn cancel_delalloc(&self) -> Result<()> {
        if let Ok(em) = self.extent_manager() {
            em.remove_delayed(0, Iblock::MAX);
        }
        if let Some(account) = &self.delayed_account {
            account.retain(0)?;
        }
        Ok(())
    }

    /// Attributes may be captured while newer buffered data is still delayed.
    /// Preserve the last publishable size and never expose the pending suffix.
    pub(super) fn capturable_desc(&self) -> InodeDesc {
        let mut desc = (*self.desc).clone();
        if let Ok(em) = self.extent_manager() {
            if let Some((first, _)) = em.first_delayed() {
                let prefix = self.mapped_size.max(first as usize * BLOCK_SIZE);
                desc.set_size(self.file_size().min(prefix) as u64);
            }
        }
        desc
    }
}
