// SPDX-License-Identifier: MPL-2.0

//! Native EXT4 buffered-I/O pressure and clean-page reclamation.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ostd::sync::WaitQueue;

use super::{fs::Ext4, inode::Inode, prelude::*};
use crate::vm::page_cache::PageCacheAccounting;

pub(super) const IO_CHUNK: usize = 256 * 1024;
const WRITEBACK_BATCH: usize = 1024;
const RECLAIM_BATCH: usize = 4096;

/// Per-filesystem controller. Reservations cover worst-case cache growth until
/// the syscall chunk completes. They are acquired before inode locks/handles.
pub(super) struct CachePressure {
    pub(super) accounting: Arc<PageCacheAccounting>,
    reservations: Mutex<usize>,
    inodes: Mutex<BTreeMap<Ext4Ino, Weak<Inode>>>,
    next_inode: AtomicUsize,
    resident_high: AtomicUsize,
    dirty_high: AtomicUsize,
    background: AtomicUsize,
    trigger: WaitQueue,
    progress: WaitQueue,
    progress_epoch: AtomicUsize,
    waiting: AtomicUsize,
    stalled_rounds: AtomicUsize,
    failed: AtomicBool,
    started: AtomicBool,
    pub(super) throttles: AtomicUsize,
}

impl CachePressure {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            accounting: Arc::new(PageCacheAccounting::default()),
            reservations: Mutex::new(0),
            inodes: Mutex::new(BTreeMap::new()),
            next_inode: AtomicUsize::new(0),
            resident_high: AtomicUsize::new(512 * 1024 * 1024 / PAGE_SIZE),
            dirty_high: AtomicUsize::new(128 * 1024 * 1024 / PAGE_SIZE),
            background: AtomicUsize::new(64 * 1024 * 1024 / PAGE_SIZE),
            trigger: WaitQueue::new(),
            progress: WaitQueue::new(),
            progress_epoch: AtomicUsize::new(0),
            waiting: AtomicUsize::new(0),
            stalled_rounds: AtomicUsize::new(0),
            failed: AtomicBool::new(false),
            started: AtomicBool::new(false),
            throttles: AtomicUsize::new(0),
        })
    }

    pub(super) fn register(&self, inode: &Arc<Inode>, ino: Ext4Ino) {
        self.inodes.lock().insert(ino, Arc::downgrade(inode));
    }

    pub(super) fn prefetch_limit(&self) -> usize {
        (self.dirty_high.load(Ordering::Relaxed) / 4).max(1) * PAGE_SIZE
    }

    fn dirty_pressure(&self) -> usize {
        self.accounting
            .dirty_pages()
            .saturating_add(self.accounting.writeback_pages())
    }

    fn resident_low(&self) -> usize {
        self.resident_high.load(Ordering::Relaxed) * 3 / 4
    }

    fn needs_work(&self) -> bool {
        self.waiting.load(Ordering::Acquire) != 0
            || self.dirty_pressure() >= self.background.load(Ordering::Relaxed)
            || self.accounting.resident_pages() > self.resident_low()
    }

    pub(super) fn reserve(self: &Arc<Self>, fs: &Ext4, pages: usize) -> Result<CacheReservation> {
        if pages > self.resident_high.load(Ordering::Relaxed)
            || pages > self.dirty_high.load(Ordering::Relaxed)
        {
            return_errno!(Errno::ENOMEM);
        }
        let mut waiting = false;
        let result = loop {
            if fs.is_shutdown() || self.failed.load(Ordering::Acquire) {
                break Err(Error::with_message(
                    Errno::EIO,
                    "EXT4 cache writeback failed",
                ));
            }
            let mut reserved = self.reservations.lock();
            if self
                .accounting
                .resident_pages()
                .saturating_add(*reserved)
                .saturating_add(pages)
                <= self.resident_high.load(Ordering::Relaxed)
                && self
                    .dirty_pressure()
                    .saturating_add(*reserved)
                    .saturating_add(pages)
                    <= self.dirty_high.load(Ordering::Relaxed)
            {
                *reserved += pages;
                break Ok(CacheReservation {
                    pressure: self.clone(),
                    pages,
                });
            }
            drop(reserved);
            if !waiting {
                self.waiting.fetch_add(1, Ordering::AcqRel);
                self.throttles.fetch_add(1, Ordering::Relaxed);
                waiting = true;
            }
            // A complete series of rounds with no writable/reclaimable pages
            // means externally pinned frames dominate. Do not drop live mappings
            // or strand the caller indefinitely. RCU-delayed frees get time to run.
            if self.stalled_rounds.load(Ordering::Acquire) >= 100 {
                break Err(Error::with_message(
                    Errno::ENOMEM,
                    "EXT4 cache frames remain pinned",
                ));
            }
            let epoch = self.progress_epoch.load(Ordering::Acquire);
            self.trigger.wake_all();
            let _ = self.progress.wait_until_or_timeout(
                || {
                    (self.failed.load(Ordering::Acquire)
                        || self.progress_epoch.load(Ordering::Acquire) != epoch)
                        .then_some(())
                },
                &Duration::from_millis(10),
            );
        };
        if waiting {
            self.waiting.fetch_sub(1, Ordering::Release);
        }
        result
    }

    #[cfg(ktest)]
    fn set_limits_for_test(&self, resident: usize, dirty: usize, background: usize) {
        self.resident_high.store(resident, Ordering::Relaxed);
        self.dirty_high.store(dirty, Ordering::Relaxed);
        self.background.store(background, Ordering::Relaxed);
    }
}

pub(super) struct CacheReservation {
    pressure: Arc<CachePressure>,
    pages: usize,
}

impl Drop for CacheReservation {
    fn drop(&mut self) {
        *self.pressure.reservations.lock() -= self.pages;
        self.pressure.progress_epoch.fetch_add(1, Ordering::Release);
        self.pressure.progress.wake_all();
        if self.pressure.needs_work() {
            self.pressure.trigger.wake_all();
        }
    }
}

impl Ext4 {
    pub(super) fn start_cache_writeback(self: &Arc<Self>) {
        if self.rnvpc_mode() != crate::fs::rnvpc::mode::RnvpcMode::Native
            || self.cache_pressure.started.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let weak = Arc::downgrade(self);
        let pressure = self.cache_pressure.clone();
        crate::thread::kernel_thread::ThreadOptions::new(move || {
            loop {
                let busy = {
                    let Some(fs) = weak.upgrade() else {
                        break;
                    };
                    if fs.is_shutdown() {
                        break;
                    }
                    if let Err(error) = fs.writeback_cache_round() {
                        warn!("native cache writeback failed: {error:?}");
                        pressure.failed.store(true, Ordering::Release);
                        pressure.progress.wake_all();
                        break;
                    }
                    pressure.needs_work()
                };
                // Always yield between rounds, including pressure rounds. This
                // permits RCU grace periods to release removed array entries.
                let timeout = if busy {
                    Duration::from_millis(10)
                } else {
                    Duration::from_secs(1)
                };
                // The OSDK unit runner has no timer/idle servicing; exercise
                // the same worker cooperatively in that environment.
                #[cfg(ktest)]
                {
                    let _ = timeout;
                    crate::thread::Thread::yield_now();
                }
                #[cfg(not(ktest))]
                let _ = pressure.trigger.wait_until_or_timeout(
                    || (!busy && pressure.needs_work()).then_some(()),
                    &timeout,
                );
            }
        })
        .spawn();
    }

    pub(super) fn writeback_cache_round(&self) -> Result<()> {
        self.ensure_not_shutdown()?;
        let pressure = &self.cache_pressure;
        let start = pressure.next_inode.load(Ordering::Relaxed) as Ext4Ino;
        let inodes: Vec<(Ext4Ino, Weak<Inode>)> = {
            let mut registry = pressure.inodes.lock();
            registry.retain(|_, inode| inode.strong_count() != 0);
            registry
                .range(start..)
                .chain(registry.range(..start))
                .map(|(&ino, inode)| (ino, inode.clone()))
                .collect()
        };
        let mut written = 0;
        let mut reclaimed = 0;
        for (ino, inode) in inodes {
            let Some(inode) = inode.upgrade() else {
                continue;
            };
            let result = if self.is_rdonly() {
                Ok(0)
            } else {
                inode.writeback_cache_pages(WRITEBACK_BATCH - written)
            };
            let pages = match result {
                Ok(pages) => pages,
                Err(error) => {
                    pressure.failed.store(true, Ordering::Release);
                    pressure.progress.wake_all();
                    return Err(error);
                }
            };
            written += pages;
            if pressure.accounting.resident_pages() > pressure.resident_low()
                || pressure.waiting.load(Ordering::Acquire) != 0
            {
                if let Some(cache) = inode.page_cache() {
                    reclaimed += cache.reclaim_clean_pages(RECLAIM_BATCH - reclaimed);
                }
            }
            pressure
                .next_inode
                .store(ino as usize + 1, Ordering::Relaxed);
            if written >= WRITEBACK_BATCH || reclaimed >= RECLAIM_BATCH {
                break;
            }
        }
        if written == 0 && reclaimed == 0 && pressure.waiting.load(Ordering::Acquire) != 0 {
            pressure.stalled_rounds.fetch_add(1, Ordering::AcqRel);
        } else {
            pressure.stalled_rounds.store(0, Ordering::Release);
        }
        pressure.progress_epoch.fetch_add(1, Ordering::Release);
        pressure.progress.wake_all();
        Ok(())
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;
    use crate::fs::fs_impls::ext4::test_utils::{Ext4FixtureBuilder, make_empty_file_inode};

    #[ktest]
    fn ext4_native_cache_pressure() {
        crate::fs::fs_impls::ext4::test_utils::scenarios!(
            native_pressure_bounds_cache_and_preserves_two_files,
            native_pressure_concurrent_writers_preserve_all_files,
            native_pressure_reports_writeback_failure_without_losing_dirty_pages,
        );
    }

    fn native_pressure_bounds_cache_and_preserves_two_files() {
        crate::time::clocks::init_for_ktest();
        let fixture = Ext4FixtureBuilder::new(4096, 256, 4096)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        fixture
            .ext4
            .cache_pressure
            .set_limits_for_test(256, 128, 64);
        fixture.write_raw_inode(11, &make_empty_file_inode());
        fixture.write_raw_inode(12, &make_empty_file_inode());
        let files = [
            fixture.ext4.read_inode(11).unwrap(),
            fixture.ext4.read_inode(12).unwrap(),
        ];
        fixture.ext4.start_cache_writeback();
        let bytes = vec![0x71; 32 * PAGE_SIZE];
        for chunk in 0..32 {
            for file in &files {
                assert_eq!(
                    file.write_at(
                        chunk * bytes.len(),
                        &mut VmReader::from(bytes.as_slice()).to_fallible()
                    )
                    .unwrap(),
                    bytes.len()
                );
            }
        }
        assert!(
            fixture
                .ext4
                .cache_pressure
                .throttles
                .load(Ordering::Relaxed)
                > 0
        );
        assert!(fixture.ext4.cache_pressure.accounting.resident_pages() <= 256);
        let mut output = vec![0; bytes.len()];
        for chunk in 0..32 {
            for file in &files {
                assert_eq!(
                    file.read_at(
                        chunk * bytes.len(),
                        &mut VmWriter::from(output.as_mut_slice()).to_fallible()
                    )
                    .unwrap(),
                    bytes.len()
                );
                assert_eq!(output, bytes);
            }
        }
    }

    fn native_pressure_concurrent_writers_preserve_all_files() {
        crate::time::clocks::init_for_ktest();
        let fixture = Ext4FixtureBuilder::new(4096, 256, 4096)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        fixture
            .ext4
            .cache_pressure
            .set_limits_for_test(256, 128, 64);
        let mut files = Vec::new();
        let mut workers = Vec::new();
        for index in 0..8 {
            let ino = 11 + index;
            fixture.write_raw_inode(ino, &make_empty_file_inode());
            let file = fixture.ext4.read_inode(ino).unwrap();
            files.push(file.clone());
            workers.push(
                crate::thread::kernel_thread::ThreadOptions::new(move || {
                    let input = vec![0x40 + index as u8; 32 * PAGE_SIZE];
                    for chunk in 0..8 {
                        assert_eq!(
                            file.write_at(
                                chunk * input.len(),
                                &mut VmReader::from(input.as_slice()).to_fallible(),
                            )
                            .unwrap(),
                            input.len(),
                        );
                        crate::thread::Thread::yield_now();
                    }
                })
                .spawn(),
            );
        }
        fixture.ext4.start_cache_writeback();
        for worker in workers {
            worker.join();
        }
        assert!(
            fixture
                .ext4
                .cache_pressure
                .throttles
                .load(Ordering::Relaxed)
                > 0
        );
        assert!(fixture.ext4.cache_pressure.accounting.resident_pages() <= 256);
        let mut output = vec![0; 32 * PAGE_SIZE];
        for (index, file) in files.iter().enumerate() {
            for chunk in 0..8 {
                assert_eq!(
                    file.read_at(
                        chunk * output.len(),
                        &mut VmWriter::from(output.as_mut_slice()).to_fallible(),
                    )
                    .unwrap(),
                    output.len(),
                );
                assert!(output.iter().all(|&byte| byte == 0x40 + index as u8));
            }
        }
    }

    fn native_pressure_reports_writeback_failure_without_losing_dirty_pages() {
        crate::time::clocks::init_for_ktest();
        let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        fixture.write_raw_inode(11, &make_empty_file_inode());
        let inode = fixture.ext4.read_inode(11).unwrap();
        inode
            .write_at(
                0,
                &mut VmReader::from([0x57; PAGE_SIZE].as_slice()).to_fallible(),
            )
            .unwrap();
        fixture.disk.set_fail_writes(true);
        assert!(fixture.ext4.writeback_cache_round().is_err());
        assert_eq!(fixture.ext4.cache_pressure.accounting.dirty_pages(), 1);
        assert_eq!(fixture.ext4.cache_pressure.accounting.writeback_pages(), 0);
        assert_eq!(
            inode
                .write_at(PAGE_SIZE, &mut VmReader::from([1].as_slice()).to_fallible())
                .unwrap_err()
                .error(),
            Errno::EIO
        );
        fixture.disk.set_fail_writes(false);
        inode
            .sync_data_and_meta(super::super::inode::SyncScope::Full)
            .unwrap();
    }
}

#[cfg(ktest)]
mod read_regressions {
    use ostd::prelude::ktest;

    use super::{
        super::test_utils::{Ext4FixtureBuilder, make_empty_file_inode},
        *,
    };

    #[ktest]
    fn ext4_native_read_and_clean_reclaim() {
        crate::fs::fs_impls::ext4::test_utils::scenarios!(
            native_short_read_advances_only_copied_bytes,
            native_readonly_stream_reclaims_clean_pages,
        );
    }

    fn native_short_read_advances_only_copied_bytes() {
        let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        fixture.write_raw_inode(11, &make_empty_file_inode());
        let file = fixture.ext4.read_inode(11).unwrap();
        file.write_at(0, &mut VmReader::from([0x41; 17].as_slice()).to_fallible())
            .unwrap();
        let mut output = vec![0x9a; IO_CHUNK];
        let mut writer = VmWriter::from(output.as_mut_slice()).to_fallible();
        assert_eq!(file.read_at(0, &mut writer).unwrap(), 17);
        assert_eq!(writer.avail(), IO_CHUNK - 17);
        drop(writer);
        assert_eq!(&output[..17], &[0x41; 17]);
        assert!(output[17..].iter().all(|byte| *byte == 0x9a));
    }

    fn native_readonly_stream_reclaims_clean_pages() {
        crate::time::clocks::init_for_ktest();
        let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        fixture.write_raw_inode(11, &make_empty_file_inode());
        let file = fixture.ext4.read_inode(11).unwrap();
        let input = vec![0x68; 512 * PAGE_SIZE];
        file.write_at(0, &mut VmReader::from(input.as_slice()).to_fallible())
            .unwrap();
        file.sync_data_and_meta(super::super::inode::SyncScope::Full)
            .unwrap();
        file.page_cache().unwrap().reclaim_clean_pages(512);
        fixture.ext4.set_read_only(true).unwrap();
        fixture
            .ext4
            .cache_pressure
            .set_limits_for_test(128, 128, 64);
        fixture.ext4.start_cache_writeback();
        let mut output = vec![0; 32 * PAGE_SIZE];
        for offset in (0..input.len()).step_by(output.len()) {
            assert_eq!(
                file.read_at(
                    offset,
                    &mut VmWriter::from(output.as_mut_slice()).to_fallible()
                )
                .unwrap(),
                output.len()
            );
            assert!(output.iter().all(|byte| *byte == 0x68));
        }
    }
}
