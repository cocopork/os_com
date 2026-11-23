// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use aster_block::{BLOCK_SIZE, BlockDevice, SECTOR_SIZE};
use aster_nvme::NvmeBlockDevice;
use aster_virtio::device::block::device::BlockDevice as VirtIoBlockDevice;
use device_id::DeviceId;
use ostd::mm::VmIo;

use crate::{
    context::current_userspace,
    device::{Device, DeviceType, DevtmpfsInodeMeta, add_node},
    events::IoEvents,
    fs::{
        file::{PerOpenFileOps, StatusFlags},
        vfs::{inode::FileOps, path::PathResolver},
    },
    prelude::*,
    process::{
        posix_thread::AsPosixThread,
        signal::{PollHandle, Pollable},
    },
    thread::kernel_thread::ThreadOptions,
    util::ioctl::{RawIoctl, dispatch_ioctl},
};

fn direct_submit_requested(command_line: &str) -> bool {
    command_line
        .split_ascii_whitespace()
        .any(|argument| argument == "virtio_blk_direct_submit=on")
}

fn virtio_profile_requested(command_line: &str) -> bool {
    command_line
        .split_ascii_whitespace()
        .any(|argument| argument == "virtio_blk_profile=on")
}

const MODULE_PROFILE_SAMPLE_INTERVAL: usize = 1024;
const MODULE_PROFILE_REPORT_SAMPLES: u64 = 64;
static RAW_PROFILE_ENABLED: AtomicBool = AtomicBool::new(false);
static RAW_PROFILE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
static RAW_PROFILE_SAMPLES: AtomicU64 = AtomicU64::new(0);
static RAW_FILE_PRE_TICKS: AtomicU64 = AtomicU64::new(0);
static RAW_BLOCK_CALL_TICKS: AtomicU64 = AtomicU64::new(0);
static RAW_FILE_POST_TICKS: AtomicU64 = AtomicU64::new(0);
static RAW_TOTAL_TICKS: AtomicU64 = AtomicU64::new(0);
static RAW_INTER_WRITE_GAP_TICKS: AtomicU64 = AtomicU64::new(0);
static RAW_INTER_WRITE_GAP_SAMPLES: AtomicU64 = AtomicU64::new(0);

const RAW_WRITE_GAP_SLOT_COUNT: usize = 4096;

struct RawWriteGapSlot(AtomicU64);

impl RawWriteGapSlot {
    const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    fn take_previous_return(&self, tid: u32, now: u64) -> Option<u64> {
        let expected_tid = u64::from(tid) << 32;
        let mut previous = self.0.load(Ordering::Acquire);
        loop {
            if previous >> 32 != u64::from(tid) || previous as u32 == 0 {
                return None;
            }
            match self.0.compare_exchange_weak(
                previous,
                expected_tid,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(u64::from((now as u32).wrapping_sub(previous as u32)));
                }
                Err(actual) => previous = actual,
            }
        }
    }

    fn record_return(&self, tid: u32, now: u64) {
        let timestamp = now as u32;
        let timestamp = if timestamp == 0 { 1 } else { timestamp };
        self.0.store(
            (u64::from(tid) << 32) | u64::from(timestamp),
            Ordering::Release,
        );
    }
}

static RAW_WRITE_GAP_SLOTS: [RawWriteGapSlot; RAW_WRITE_GAP_SLOT_COUNT] =
    [const { RawWriteGapSlot::new() }; RAW_WRITE_GAP_SLOT_COUNT];

fn current_raw_write_gap_slot() -> Option<(u32, &'static RawWriteGapSlot)> {
    let tid = current_thread!().as_posix_thread()?.tid();
    let slot = &RAW_WRITE_GAP_SLOTS[tid as usize % RAW_WRITE_GAP_SLOT_COUNT];
    Some((tid, slot))
}

fn should_sample_raw_profile() -> bool {
    RAW_PROFILE_ENABLED.load(Ordering::Relaxed)
        && RAW_PROFILE_SEQUENCE
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(MODULE_PROFILE_SAMPLE_INTERVAL)
}

fn record_raw_profile(
    started: u64,
    block_started: u64,
    block_done: u64,
    finished: u64,
    inter_write_gap: Option<u64>,
) {
    RAW_FILE_PRE_TICKS.fetch_add(block_started.wrapping_sub(started), Ordering::Relaxed);
    RAW_BLOCK_CALL_TICKS.fetch_add(block_done.wrapping_sub(block_started), Ordering::Relaxed);
    RAW_FILE_POST_TICKS.fetch_add(finished.wrapping_sub(block_done), Ordering::Relaxed);
    RAW_TOTAL_TICKS.fetch_add(finished.wrapping_sub(started), Ordering::Relaxed);
    if let Some(gap) = inter_write_gap {
        RAW_INTER_WRITE_GAP_TICKS.fetch_add(gap, Ordering::Relaxed);
        RAW_INTER_WRITE_GAP_SAMPLES.fetch_add(1, Ordering::Relaxed);
    }
    let samples = RAW_PROFILE_SAMPLES.fetch_add(1, Ordering::Relaxed) + 1;
    if !samples.is_multiple_of(MODULE_PROFILE_REPORT_SAMPLES) {
        return;
    }
    ostd::error!(
        "[RAW_BLOCK_PROFILE] tsc_hz={} samples={} file_pre_ticks={} block_call_ticks={} file_post_ticks={} total_ticks={} inter_write_gap_ticks={} inter_write_gap_samples={}",
        ostd::arch::tsc_freq(),
        samples,
        RAW_FILE_PRE_TICKS.load(Ordering::Relaxed),
        RAW_BLOCK_CALL_TICKS.load(Ordering::Relaxed),
        RAW_FILE_POST_TICKS.load(Ordering::Relaxed),
        RAW_TOTAL_TICKS.load(Ordering::Relaxed),
        RAW_INTER_WRITE_GAP_TICKS.load(Ordering::Relaxed),
        RAW_INTER_WRITE_GAP_SAMPLES.load(Ordering::Relaxed),
    );
}

pub(super) fn init_in_first_kthread() {
    let command_line = ostd::boot::boot_info().kernel_cmdline.as_str();
    let direct_submit = direct_submit_requested(command_line);
    let virtio_profile = virtio_profile_requested(command_line);
    RAW_PROFILE_ENABLED.store(virtio_profile, Ordering::Release);
    aster_block::set_module_profile_enabled(virtio_profile);
    for device in aster_block::collect_all() {
        if device.is_partition() {
            continue;
        }

        // Spawn threads for virtio block devices
        if let Some(virtio_device) = device.downcast_ref::<VirtIoBlockDevice>() {
            virtio_device.set_direct_submit(direct_submit);
            virtio_device.set_profile_enabled(virtio_profile);
            let queue_count = virtio_device.nr_submission_queues();
            for queue_id in 0..queue_count {
                let device_clone = device.clone();
                let task_fn = move || {
                    info!("spawn the virtio-block thread for queue {}", queue_id);
                    crate::thread::io_test_affinity::trace(
                        crate::thread::io_test_affinity::Role::Virtio,
                    );
                    let virtio_block_device =
                        device_clone.downcast_ref::<VirtIoBlockDevice>().unwrap();
                    loop {
                        virtio_block_device.handle_requests(queue_id);
                    }
                };
                ThreadOptions::new(task_fn)
                    .cpu_affinity(crate::thread::io_test_affinity::virtio_queue_affinity(
                        queue_id,
                        queue_count,
                    ))
                    .spawn();
            }
        }
        // Spawn threads for NVMe block devices
        else if device.downcast_ref::<NvmeBlockDevice>().is_some() {
            let device_clone = device.clone();
            let task_fn = move || {
                info!("spawn the nvme-block thread");
                let nvme_block_device = device_clone.downcast_ref::<NvmeBlockDevice>().unwrap();
                loop {
                    nvme_block_device.handle_requests();
                }
            };
            ThreadOptions::new(task_fn).spawn();
        }
    }
}

pub(super) fn init_in_first_process(path_resolver: &PathResolver) -> Result<()> {
    for device in aster_block::collect_all() {
        let device = Arc::new(BlockFile::new(device));
        if let Some(devtmpfs_meta) = device.devtmpfs_meta() {
            let dev_id = device.id().as_encoded_u64();
            add_node(DeviceType::Block, dev_id, &devtmpfs_meta, path_resolver)?;
        }
    }

    Ok(())
}

mod ioctl_defs {
    use crate::util::ioctl::{NoData, OutData, ioc};

    // Reference: <https://elixir.bootlin.com/linux/v6.18/source/include/uapi/linux/fs.h>

    /// Returns the device size in bytes.
    pub(super) type BlkGetSize64 = ioc!(BLKGETSIZE64, 0x12, 114, OutData<u64>);

    /// Returns the logical sector size of the block device.
    ///
    /// This is the smallest unit of I/O the device can address and,
    /// importantly, the minimum alignment required for `O_DIRECT` I/O on
    /// files backed by this device. Both buffer address and offset must be a
    /// multiple of this value.
    ///
    /// Benchmarks and filesystem tests (for example, `xfstests`, LTP
    /// `preadv03`/`pwritev03`) rely on this ioctl to size `O_DIRECT` buffers.
    /// If the effective alignment enforced by the filesystem layered on top
    /// is larger than the hardware sector, such as `ext2`'s 4 KiB block, this
    /// ioctl must return that larger value. Otherwise user programs will align
    /// correctly for the device but still hit `EINVAL` at the filesystem.
    pub(super) type BlkGetSectorSize = ioc!(BLKSSZGET, 0x12, 104, NoData);
}

/// Represents a block device inode in the filesystem.
//
// TODO: This type wraps an `Arc<dyn BlockDevice>` in another `Arc` just to implement the `Device`
// trait. It leads to redundant vtable dispatch, reference counting, and heap allocation. We should
// devise a better strategy to eliminate the unnecessary intermediate `Arc`.
#[derive(Debug)]
struct BlockFile(Arc<dyn BlockDevice>);

impl BlockFile {
    fn new(device: Arc<dyn BlockDevice>) -> Self {
        Self(device)
    }
}

impl Device for BlockFile {
    fn type_(&self) -> DeviceType {
        DeviceType::Block
    }

    fn id(&self) -> DeviceId {
        self.0.id()
    }

    fn devtmpfs_meta(&self) -> Option<DevtmpfsInodeMeta<'_>> {
        Some(DevtmpfsInodeMeta::new(self.0.name()))
    }

    fn open(&self) -> Result<Box<dyn PerOpenFileOps>> {
        Ok(Box::new(OpenBlockFile(self.0.clone())))
    }
}

/// Represents an opened block device file ready for I/O operations.
//
// TODO: This type wraps an `Arc<dyn BlockDevice>` in another `Box` just to implement the
// `PerOpenFileOps` trait. It leads to redundant vtable dispatch and heap allocation. We should
// devise a better strategy to eliminate the unnecessary intermediate `Box`.
struct OpenBlockFile(Arc<dyn BlockDevice>);

impl FileOps for OpenBlockFile {
    fn read_at(
        &self,
        offset: usize,
        writer: &mut VmWriter,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        let total = writer.avail();
        if total == 0 {
            return Ok(0);
        }

        let device_size = self.0.metadata().nr_sectors * SECTOR_SIZE;
        if offset >= device_size {
            return Ok(0);
        }

        let read_len = total.min(device_size - offset);
        {
            // `VmIo::read` does not allow short writes,
            // so the writer must be precisely limited here.
            let mut limited_writer = writer.clone_exclusive();
            limited_writer.limit(read_len);
            self.0.read(offset, &mut limited_writer)?;
        }
        writer.skip(read_len);
        Ok(read_len)
    }

    fn write_at(
        &self,
        offset: usize,
        reader: &mut VmReader,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        let gap_context = RAW_PROFILE_ENABLED
            .load(Ordering::Relaxed)
            .then(current_raw_write_gap_slot)
            .flatten();
        let write_entered = gap_context.map(|_| ostd::arch::read_tsc());
        let inter_write_gap = gap_context.and_then(|(tid, slot)| {
            slot.take_previous_return(tid, write_entered.expect("write entry must exist"))
        });
        let profile_started =
            should_sample_raw_profile().then(|| write_entered.unwrap_or_else(ostd::arch::read_tsc));
        let total = reader.remain();
        if total == 0 {
            return Ok(0);
        }

        let device_size = self.0.metadata().nr_sectors * SECTOR_SIZE;
        if offset >= device_size {
            return_errno_with_message!(
                Errno::ENOSPC,
                "the write offset is beyond the block device"
            );
        }

        let write_len = total.min(device_size - offset);
        let block_started = profile_started.map(|_| ostd::arch::read_tsc());
        let block_profile = {
            // `VmIo::write` does not allow short writes,
            // so the reader must be precisely limited here.
            let mut limited_reader = reader.clone();
            limited_reader.limit(write_len);
            if profile_started.is_some() {
                Some(
                    self.0
                        .write_with_module_profile(offset, &mut limited_reader)?,
                )
            } else {
                self.0.write(offset, &mut limited_reader)?;
                None
            }
        };
        let block_done = profile_started.map(|_| ostd::arch::read_tsc());
        reader.skip(write_len);
        let finished = profile_started.map(|_| ostd::arch::read_tsc());
        let returned = gap_context.map(|(tid, slot)| {
            let returned = finished.unwrap_or_else(ostd::arch::read_tsc);
            slot.record_return(tid, returned);
            returned
        });
        if let (Some(started), Some(block_started), Some(block_done)) =
            (profile_started, block_started, block_done)
        {
            record_raw_profile(
                started,
                block_started,
                block_done,
                finished.or(returned).expect("profile end must exist"),
                inter_write_gap,
            );
        }
        if let Some(profile) = block_profile {
            aster_block::record_block_module_profile(profile);
        }
        Ok(write_len)
    }
}

impl Pollable for OpenBlockFile {
    fn poll(&self, mask: IoEvents, _: Option<&mut PollHandle>) -> IoEvents {
        let events = IoEvents::IN | IoEvents::OUT;
        events & mask
    }
}

impl PerOpenFileOps for OpenBlockFile {
    fn check_seekable(&self) -> Result<()> {
        Ok(())
    }

    fn is_offset_aware(&self) -> bool {
        true
    }

    fn seek_end(&self) -> Result<Option<usize>> {
        Ok(Some(self.0.metadata().nr_sectors * SECTOR_SIZE))
    }

    fn ioctl(&self, raw_ioctl: RawIoctl) -> Result<i32> {
        use ioctl_defs::*;

        dispatch_ioctl!(match raw_ioctl {
            _cmd @ BlkGetSectorSize => {
                // TODO: Query the per-device logical block size once block device metadata
                // exposes it. For now, report the effective minimum I/O granularity enforced
                // by Asterinas filesystems so userspace can use `BLKSSZGET` for `O_DIRECT`
                // alignment.
                let sector_size = SECTOR_SIZE.max(BLOCK_SIZE) as i32;
                current_userspace!().write_val(raw_ioctl.arg(), &sector_size)?;
                Ok(0)
            }
            cmd @ BlkGetSize64 => {
                let size = (self.0.metadata().nr_sectors * SECTOR_SIZE) as u64;
                cmd.write(&size)?;
                Ok(0)
            }
            _ => return_errno_with_message!(
                Errno::ENOTTY,
                "the ioctl command is not supported by block devices"
            ),
        })
    }
}

pub(super) fn lookup(id: DeviceId) -> Option<Arc<dyn Device>> {
    let block_device = aster_block::lookup(id)?;

    let mut registry = DEVICE_REGISTRY.lock();
    let block_device_file = registry
        .entry(id.to_raw())
        .or_insert_with(move || Arc::new(BlockFile::new(block_device)))
        .clone();
    Some(block_device_file)
}

// TODO: Merge the two mapping tables, one is here and the other is in the block component.
// Maintaining two mapping tables is undesirable due to duplication and (potential) inconsistency.
static DEVICE_REGISTRY: Mutex<BTreeMap<u32, Arc<dyn Device>>> = Mutex::new(BTreeMap::new());

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn direct_submit_requires_explicit_on_value() {
        assert!(direct_submit_requested(
            "quiet virtio_blk_direct_submit=on console=ttyS0"
        ));
        assert!(!direct_submit_requested("virtio_blk_direct_submit=off"));
        assert!(!direct_submit_requested("quiet console=ttyS0"));
    }

    #[ktest]
    fn virtio_profile_requires_explicit_on_value() {
        assert!(virtio_profile_requested(
            "quiet virtio_blk_profile=on console=ttyS0"
        ));
        assert!(!virtio_profile_requested("virtio_blk_profile=off"));
        assert!(!virtio_profile_requested("quiet console=ttyS0"));
    }

    #[ktest]
    fn raw_write_gap_slot_tracks_only_the_same_thread() {
        let slot = RawWriteGapSlot::new();

        assert_eq!(slot.take_previous_return(41, 100), None);
        slot.record_return(41, 120);
        assert_eq!(slot.take_previous_return(41, 155), Some(35));
        assert_eq!(slot.take_previous_return(41, 160), None);

        slot.record_return(41, 180);
        assert_eq!(slot.take_previous_return(42, 200), None);
        slot.record_return(42, 220);
        assert_eq!(slot.take_previous_return(42, 250), Some(30));
    }
}
