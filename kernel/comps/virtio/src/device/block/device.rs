// SPDX-License-Identifier: MPL-2.0

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    fmt::Debug,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use aster_block::{
    BlockDeviceMeta, EXTENDED_DEVICE_ID_ALLOCATOR, PartitionInfo, PartitionNode,
    bio::{
        BioEnqueueError, BioQueueAffinity, BioStatus, BioType, SubmittedBio, bio_segment_pool_init,
    },
    request_queue::{BioRequest, BioRequestSingleQueue},
};
use aster_util::mem_obj_slice::Slice;
use device_id::{DeviceId, MinorId};
use ostd::{
    arch::trap::TrapFrame,
    cpu::CpuId,
    debug, info,
    mm::{PAGE_SIZE, VmIo, dma::DmaStream},
    sync::{LocalIrqDisabled, SpinLock, SpinLockGuard, WaitQueue},
    util::id_set::Id,
};
use smallvec::SmallVec;

use super::{
    BlockFeatures, VirtioBlockConfig,
    segment_merge::{DmaSpan, append_data},
};
use crate::{
    VIRTIO_BLOCK_MAJOR_ID,
    device::{
        VirtioDeviceError,
        block::{ReqType, RespStatus},
    },
    id_alloc::SyncIdAlloc,
    queue::VirtQueue,
    transport::{ConfigManager, VirtioTransport},
};

/// The number of minor device numbers allocated for each virtio disk,
/// including the whole disk and its partitions. If a disk has more than
/// 16 partitions, then allocate a device ID via `EXTENDED_DEVICE_ID_ALLOCATOR`.
const VIRTIO_DEVICE_MINORS: u32 = 16;

/// The number of virtio block devices, used to assign minor device numbers.
static NR_BLOCK_DEVICE: AtomicU32 = AtomicU32::new(0);

const DRIVER_QUEUE_LIMIT: u16 = 16;

fn optional_config_queues(
    features: BlockFeatures,
    legacy: bool,
    read: impl FnOnce() -> Result<u16, VirtioDeviceError>,
) -> Option<u16> {
    if !features.contains(BlockFeatures::MQ) || legacy {
        None
    } else {
        read().ok()
    }
}

fn active_queue_count(
    features: BlockFeatures,
    config_queues: Option<u16>,
    transport_queues: u16,
) -> Result<u16, VirtioDeviceError> {
    if transport_queues == 0 {
        return Err(VirtioDeviceError::InvalidQueueArgs);
    }
    if !features.contains(BlockFeatures::MQ) {
        return Ok(1);
    }
    Ok(config_queues.filter(|count| *count > 0).map_or(1, |count| {
        count.min(transport_queues).min(DRIVER_QUEUE_LIMIT)
    }))
}

fn preflight_queue_sizes(
    count: u16,
    mut max_queue_size: impl FnMut(u16) -> Result<u16, VirtioDeviceError>,
) -> Result<Vec<u16>, VirtioDeviceError> {
    if count == 0 {
        return Err(VirtioDeviceError::InvalidQueueArgs);
    }
    let mut sizes = Vec::with_capacity(count as usize);
    for index in 0..count {
        let size = match max_queue_size(index) {
            Ok(max) => DeviceInner::QUEUE_SIZE.min(max),
            Err(error) if index == 0 => return Err(error),
            Err(_) => return Ok(sizes[..1].to_vec()),
        };
        if size < 4 || !size.is_power_of_two() {
            if index == 0 {
                return Err(VirtioDeviceError::InvalidQueueArgs);
            }
            return Ok(sizes[..1].to_vec());
        }
        sizes.push(size);
    }
    Ok(sizes)
}

#[derive(Debug)]
pub struct BlockDevice {
    device: Arc<DeviceInner>,
    id: DeviceId,
    name: String,
    partitions: SpinLock<Option<Vec<Arc<PartitionNode>>>>,
    weak_self: Weak<Self>,
}

impl BlockDevice {
    /// Returns the formatted device name.
    ///
    /// The device name starts at "vda". The 26th device is "vdz" and the 27th is "vdaa".
    /// The last one for two lettered suffix is "vdzz" which is followed by "vdaaa".
    fn formatted_device_name(mut index: u32) -> String {
        const VIRTIO_DISK_PREFIX: &str = "vd";

        let mut suffix = Vec::new();
        loop {
            suffix.push((b'a' + (index % 26) as u8) as char);
            index /= 26;
            if index == 0 {
                break;
            }
            index -= 1;
        }
        suffix.reverse();
        let mut name = String::from(VIRTIO_DISK_PREFIX);
        name.extend(suffix);
        name
    }

    /// Creates a new VirtIO-Block driver and registers it.
    pub(crate) fn init(transport: Box<dyn VirtioTransport>) -> Result<(), VirtioDeviceError> {
        let device = DeviceInner::init(transport)?;

        let index = NR_BLOCK_DEVICE.fetch_add(1, Ordering::Relaxed);
        let id = DeviceId::new(
            VIRTIO_BLOCK_MAJOR_ID.get().unwrap().get(),
            MinorId::new(index * VIRTIO_DEVICE_MINORS),
        );
        let name = Self::formatted_device_name(index);

        let block_device = Arc::new_cyclic(|weak_self| BlockDevice {
            device,
            id,
            name,
            partitions: SpinLock::new(None),
            weak_self: weak_self.clone(),
        });

        aster_block::register(block_device).unwrap();

        bio_segment_pool_init();
        Ok(())
    }

    /// Dequeues and processes one request from the selected staging queue.
    ///
    /// Each queue has one device-lifetime forwarding thread.
    pub fn handle_requests(&self, queue_id: u16) {
        let queue = &self.device.queues[usize::from(queue_id)];
        let _forwarder = ForwardingGuard::new(&queue.counters);
        let request = queue.staging.dequeue();
        submit_request(queue_id, queue, self.device.features, request);
    }

    /// Selects whether callers submit requests directly to the virtqueue.
    pub fn set_direct_submit(&self, enabled: bool) {
        self.device.direct_submit.store(enabled, Ordering::Release);
    }

    /// Enables sampled queue-stage profiling for this device.
    pub fn set_profile_enabled(&self, enabled: bool) {
        for queue in &self.device.queues {
            queue.set_profile_enabled(enabled);
        }
    }

    /// Returns the number of initialized submission queues.
    pub fn nr_submission_queues(&self) -> u16 {
        self.device.queues.len() as u16
    }

    /// Returns a diagnostic snapshot for a submission queue.
    ///
    /// Cumulative counters are monotonic; depth and activity gauges can decrease.
    /// The snapshot is not atomic across fields.
    pub fn queue_diagnostics(&self, queue_id: u16) -> VirtioBlockQueueDiagnostics {
        self.device.queues[usize::from(queue_id)].diagnostics()
    }

    /// Negotiate features for the device specified bits 0~23
    pub(crate) fn negotiate_features(device_features: u64) -> u64 {
        BlockFeatures::negotiated_with_device(device_features).bits()
    }
}

impl aster_block::BlockDevice for BlockDevice {
    fn enqueue(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        bio.begin_virtio_e2e_profile();
        if self.device.direct_submit.load(Ordering::Acquire) {
            return submit_direct_on_queue(
                &self.device.queues,
                self.device.features,
                bio,
                CpuId::current_racy().as_usize(),
            );
        }
        enqueue_on_queue(&self.device.queues, bio, CpuId::current_racy().as_usize())
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: self.device.max_nr_segments_per_bio,
            nr_sectors: self.device.cached_nr_sectors.load(Ordering::Acquire),
            nr_submission_queues: self.nr_submission_queues(),
        }
    }

    fn queue_diagnostics(&self, queue_id: u16) -> Option<aster_block::BlockQueueDiagnostics> {
        self.device
            .queues
            .get(usize::from(queue_id))
            .map(|queue| queue.diagnostics())
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn id(&self) -> DeviceId {
        self.id
    }

    fn set_partitions(&self, infos: Vec<Option<PartitionInfo>>) {
        let mut partitions = self.partitions.lock();
        if let Some(old_partitions) = partitions.take() {
            for partition in old_partitions {
                let _ = aster_block::unregister(partition.id());
            }
        }

        let mut new_partitions = Vec::new();
        for (index, info_opt) in infos.iter().enumerate() {
            let Some(info) = info_opt else {
                continue;
            };

            let index = index as u32 + 1;
            let id = if index < VIRTIO_DEVICE_MINORS {
                DeviceId::new(self.id.major(), MinorId::new(self.id.minor().get() + index))
            } else {
                EXTENDED_DEVICE_ID_ALLOCATOR.get().unwrap().allocate()
            };
            let name = format!("{}{}", self.name(), index);
            let device = self.weak_self.upgrade().unwrap();

            let partition = Arc::new(PartitionNode::new(id, name, device, *info));
            new_partitions.push(partition);
        }

        for partition in new_partitions.iter() {
            let _ = aster_block::register(partition.clone());
        }

        *partitions = Some(new_partitions);
    }

    fn partitions(&self) -> Option<Vec<Arc<dyn aster_block::BlockDevice>>> {
        let partitions = self.partitions.lock();
        let devices = partitions
            .as_ref()?
            .iter()
            .map(|p| p.clone() as Arc<dyn aster_block::BlockDevice>)
            .collect();
        Some(devices)
    }
}

#[derive(Debug)]
struct DeviceInner {
    config_manager: ConfigManager<VirtioBlockConfig>,
    cached_nr_sectors: AtomicUsize,
    features: BlockFeatures,
    queues: Vec<Arc<VirtioBlockQueue>>,
    direct_submit: AtomicBool,
    max_nr_segments_per_bio: usize,
    transport: SpinLock<Box<dyn VirtioTransport>>,
}

impl DeviceInner {
    const QUEUE_SIZE: u16 = 1024;

    /// Creates and inits the device.
    fn init(mut transport: Box<dyn VirtioTransport>) -> Result<Arc<Self>, VirtioDeviceError> {
        let config_manager = VirtioBlockConfig::new_manager(transport.as_ref());

        let config = config_manager.read_config();
        debug!("virio_blk_config = {:?}", config);

        let features = BlockFeatures::negotiated_with_device(transport.read_device_features());

        let block_size = if features.contains(BlockFeatures::BLK_SIZE) {
            config_manager.block_size()
        } else {
            // Fall back to standard sector size SECTOR_SIZE (512 bytes)
            // Reference:
            // <https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html#x1-2790004>
            VirtioBlockConfig::sector_size()
        };
        if block_size != VirtioBlockConfig::sector_size() {
            ostd::error!("block size {} is not supported yet", block_size);
            return Err(VirtioDeviceError::UnsupportedConfig);
        }

        let config_queues = optional_config_queues(features, transport.is_legacy_version(), || {
            config_manager.num_queues()
        });
        let queue_count = active_queue_count(features, config_queues, transport.num_queues())?;
        let queue_sizes = preflight_queue_sizes(queue_count, |index| {
            transport.max_queue_size(index).map_err(Into::into)
        })?;
        let mut queues = Vec::with_capacity(queue_sizes.len());
        for (index, size) in queue_sizes.into_iter().enumerate() {
            queues.push(Arc::new(VirtioBlockQueue::new(
                index as u16,
                size,
                transport.as_mut(),
            )?));
        }

        let max_nr_segments_per_bio = queues
            .iter()
            .map(|queue| queue.staging.max_nr_segments_per_bio())
            .min()
            .unwrap();
        let device = Arc::new(Self {
            config_manager,
            cached_nr_sectors: AtomicUsize::new(config.capacity as usize),
            features,
            queues,
            direct_submit: AtomicBool::new(false),
            max_nr_segments_per_bio,
            transport: SpinLock::new(transport),
        });

        let cloned_device = device.clone();
        let handle_config_change = move |_: &TrapFrame| {
            cloned_device.handle_config_change();
        };

        {
            let mut transport = device.transport.lock();
            transport.register_cfg_callback(Box::new(handle_config_change))?;
            for (index, queue) in device.queues.iter().enumerate() {
                let queue = queue.clone();
                transport.register_queue_callback(
                    index as u16,
                    Box::new(move |_: &TrapFrame| {
                        queue.handle_irq();
                    }),
                    true,
                )?;
            }
            transport.finish_init();
        }

        Ok(device)
    }

    fn handle_config_change(&self) {
        self.cached_nr_sectors
            .store(self.config_manager.capacity_sectors(), Ordering::Release);
        info!("block device config space change");
    }
}

fn select_queue(
    type_: BioType,
    affinity: BioQueueAffinity,
    cpu: usize,
    count: u16,
) -> Result<usize, BioEnqueueError> {
    if let BioQueueAffinity::Queue(index) = affinity
        && index >= count
    {
        return Err(BioEnqueueError::InvalidQueue);
    }
    if type_ == BioType::Flush {
        return Ok(0);
    }
    Ok(match affinity {
        BioQueueAffinity::Any => cpu % usize::from(count),
        BioQueueAffinity::Queue(index) => usize::from(index),
    })
}

fn enqueue_on_queue(
    queues: &[Arc<VirtioBlockQueue>],
    bio: SubmittedBio,
    cpu: usize,
) -> Result<(), BioEnqueueError> {
    let index = select_queue(bio.type_(), bio.queue_affinity(), cpu, queues.len() as u16)?;
    let queue = &queues[index];
    let pages = bio
        .segments()
        .iter()
        .map(|segment| segment.nblocks())
        .sum::<usize>();
    queue.staging.enqueue(bio)?;
    queue.counters.queued_bios.fetch_add(1, Ordering::Relaxed);
    queue
        .counters
        .queued_pages
        .fetch_add(pages, Ordering::Relaxed);
    queue
        .counters
        .max_sampled_staging_depth
        .fetch_max(queue.staging.num_requests(), Ordering::Relaxed);
    Ok(())
}

fn submit_direct_on_queue(
    queues: &[Arc<VirtioBlockQueue>],
    features: BlockFeatures,
    bio: SubmittedBio,
    cpu: usize,
) -> Result<(), BioEnqueueError> {
    let index = select_queue(bio.type_(), bio.queue_affinity(), cpu, queues.len() as u16)?;
    let queue = &queues[index];
    if bio.segments().len() > queue.staging.max_nr_segments_per_bio() {
        return Err(BioEnqueueError::TooBig);
    }
    let _submitter = ForwardingGuard::new(&queue.counters);
    submit_request(index as u16, queue, features, BioRequest::from(bio));
    Ok(())
}

fn submit_request(
    queue_id: u16,
    queue: &VirtioBlockQueue,
    features: BlockFeatures,
    request: BioRequest,
) {
    match request.type_() {
        BioType::Read => queue.read(request),
        BioType::Write => queue.write(request),
        BioType::Flush if features.contains(BlockFeatures::FLUSH) => {
            debug_assert_eq!(queue_id, 0);
            queue.flush(request);
        }
        BioType::Flush => request.into_bios().for_each(|bio| {
            bio.complete(BioStatus::Complete);
        }),
    }
}

/// Per-queue counters for diagnosing submission balance and backpressure.
pub use aster_block::BlockQueueDiagnostics as VirtioBlockQueueDiagnostics;

#[derive(Debug, Default)]
struct QueueCounters {
    queued_bios: AtomicUsize,
    queued_pages: AtomicUsize,
    submitted_bios: AtomicUsize,
    submitted_pages: AtomicUsize,
    max_sampled_staging_depth: AtomicUsize,
    descriptor_wait_count: AtomicUsize,
    descriptor_wait_ticks: AtomicU64,
    active_forwarders: AtomicUsize,
    peak_forwarders: AtomicUsize,
    irq_completions: AtomicUsize,
    in_flight_requests: AtomicUsize,
}

const PROFILE_SAMPLE_INTERVAL: usize = 1024;
const PROFILE_REPORT_INTERVAL: usize = 8192;
const IRQ_ENTRY_MARKER_INTERVAL: usize = 257;

#[derive(Clone, Copy, Debug, Default)]
struct QueueProfileSnapshot {
    submit_samples: u64,
    submit_id_alloc_ticks: u64,
    submit_dma_prepare_ticks: u64,
    submit_vq_acquire_ticks: u64,
    submit_vq_add_ticks: u64,
    submit_owner_publish_ticks: u64,
    submit_should_notify_ticks: u64,
    submit_device_notify_ticks: u64,
    submit_publish_notify_ticks: u64,
    submit_total_ticks: u64,
    completion_samples: u64,
    completion_wait_ticks: u64,
    completion_pop_owner_ticks: u64,
    completion_descriptor_wake_ticks: u64,
    completion_response_id_ticks: u64,
    completion_bio_complete_ticks: u64,
    completion_total_ticks: u64,
}

#[derive(Debug, Default)]
struct QueueProfileCounters {
    enabled: AtomicBool,
    submit_sequence: AtomicUsize,
    irq_entry_sequence: AtomicUsize,
    submit_samples: AtomicU64,
    submit_id_alloc_ticks: AtomicU64,
    submit_dma_prepare_ticks: AtomicU64,
    submit_vq_acquire_ticks: AtomicU64,
    submit_vq_add_ticks: AtomicU64,
    submit_owner_publish_ticks: AtomicU64,
    submit_should_notify_ticks: AtomicU64,
    submit_device_notify_ticks: AtomicU64,
    submit_publish_notify_ticks: AtomicU64,
    submit_total_ticks: AtomicU64,
    completion_samples: AtomicU64,
    completion_wait_ticks: AtomicU64,
    completion_pop_owner_ticks: AtomicU64,
    completion_descriptor_wake_ticks: AtomicU64,
    completion_response_id_ticks: AtomicU64,
    completion_bio_complete_ticks: AtomicU64,
    completion_total_ticks: AtomicU64,
}

impl QueueProfileCounters {
    fn mark_irq_entry(&self, queue_index: u16) {
        if self.enabled.load(Ordering::Relaxed)
            && self
                .irq_entry_sequence
                .fetch_add(1, Ordering::Relaxed)
                .is_multiple_of(IRQ_ENTRY_MARKER_INTERVAL)
        {
            ostd::arch::emit_qemu_profile_marker(queue_index as u8);
        }
    }

    fn should_sample_submit(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
            && self.submit_sequence.fetch_add(1, Ordering::Relaxed) % PROFILE_SAMPLE_INTERVAL == 0
    }

    fn record_submit(&self, profile: WriteRequestProfile) {
        self.submit_samples.fetch_add(1, Ordering::Relaxed);
        self.submit_id_alloc_ticks
            .fetch_add(profile.id_alloc_ticks, Ordering::Relaxed);
        self.submit_dma_prepare_ticks
            .fetch_add(profile.dma_prepare_ticks, Ordering::Relaxed);
        self.submit_vq_acquire_ticks
            .fetch_add(profile.vq_acquire_ticks, Ordering::Relaxed);
        self.submit_vq_add_ticks
            .fetch_add(profile.vq_add_ticks, Ordering::Relaxed);
        self.submit_owner_publish_ticks
            .fetch_add(profile.owner_publish_ticks, Ordering::Relaxed);
        self.submit_should_notify_ticks
            .fetch_add(profile.should_notify_ticks, Ordering::Relaxed);
        self.submit_device_notify_ticks
            .fetch_add(profile.device_notify_ticks, Ordering::Relaxed);
        self.submit_publish_notify_ticks
            .fetch_add(profile.publish_notify_ticks, Ordering::Relaxed);
        self.submit_total_ticks.fetch_add(
            ostd::arch::read_tsc().wrapping_sub(profile.submit_started),
            Ordering::Relaxed,
        );
    }

    fn record_completion(
        &self,
        profile: WriteRequestProfile,
        started: u64,
        pop_owner_done: u64,
        descriptor_wake_done: u64,
        response_id_done: u64,
        bio_complete_done: u64,
    ) {
        self.completion_samples.fetch_add(1, Ordering::Relaxed);
        self.completion_wait_ticks.fetch_add(
            started.wrapping_sub(profile.completion_wait_started),
            Ordering::Relaxed,
        );
        self.completion_pop_owner_ticks
            .fetch_add(pop_owner_done.wrapping_sub(started), Ordering::Relaxed);
        self.completion_descriptor_wake_ticks.fetch_add(
            descriptor_wake_done.wrapping_sub(pop_owner_done),
            Ordering::Relaxed,
        );
        self.completion_response_id_ticks.fetch_add(
            response_id_done.wrapping_sub(descriptor_wake_done),
            Ordering::Relaxed,
        );
        self.completion_bio_complete_ticks.fetch_add(
            bio_complete_done.wrapping_sub(response_id_done),
            Ordering::Relaxed,
        );
        self.completion_total_ticks
            .fetch_add(bio_complete_done.wrapping_sub(started), Ordering::Relaxed);
    }

    fn snapshot(&self) -> QueueProfileSnapshot {
        QueueProfileSnapshot {
            submit_samples: self.submit_samples.load(Ordering::Relaxed),
            submit_id_alloc_ticks: self.submit_id_alloc_ticks.load(Ordering::Relaxed),
            submit_dma_prepare_ticks: self.submit_dma_prepare_ticks.load(Ordering::Relaxed),
            submit_vq_acquire_ticks: self.submit_vq_acquire_ticks.load(Ordering::Relaxed),
            submit_vq_add_ticks: self.submit_vq_add_ticks.load(Ordering::Relaxed),
            submit_owner_publish_ticks: self.submit_owner_publish_ticks.load(Ordering::Relaxed),
            submit_should_notify_ticks: self.submit_should_notify_ticks.load(Ordering::Relaxed),
            submit_device_notify_ticks: self.submit_device_notify_ticks.load(Ordering::Relaxed),
            submit_publish_notify_ticks: self.submit_publish_notify_ticks.load(Ordering::Relaxed),
            submit_total_ticks: self.submit_total_ticks.load(Ordering::Relaxed),
            completion_samples: self.completion_samples.load(Ordering::Relaxed),
            completion_wait_ticks: self.completion_wait_ticks.load(Ordering::Relaxed),
            completion_pop_owner_ticks: self.completion_pop_owner_ticks.load(Ordering::Relaxed),
            completion_descriptor_wake_ticks: self
                .completion_descriptor_wake_ticks
                .load(Ordering::Relaxed),
            completion_response_id_ticks: self.completion_response_id_ticks.load(Ordering::Relaxed),
            completion_bio_complete_ticks: self
                .completion_bio_complete_ticks
                .load(Ordering::Relaxed),
            completion_total_ticks: self.completion_total_ticks.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct WriteRequestProfile {
    submit_started: u64,
    completion_wait_started: u64,
    id_alloc_ticks: u64,
    dma_prepare_ticks: u64,
    vq_acquire_ticks: u64,
    vq_add_ticks: u64,
    owner_publish_ticks: u64,
    should_notify_ticks: u64,
    device_notify_ticks: u64,
    publish_notify_ticks: u64,
}

struct ForwardingGuard<'a>(&'a QueueCounters);

impl<'a> ForwardingGuard<'a> {
    fn new(counters: &'a QueueCounters) -> Self {
        let active = counters.active_forwarders.fetch_add(1, Ordering::Relaxed) + 1;
        counters
            .peak_forwarders
            .fetch_max(active, Ordering::Relaxed);
        Self(counters)
    }
}

impl Drop for ForwardingGuard<'_> {
    fn drop(&mut self) {
        self.0.active_forwarders.fetch_sub(1, Ordering::Relaxed);
    }
}

struct VirtioBlockQueue {
    index: u16,
    staging: BioRequestSingleQueue,
    // Lock order is always virtqueue -> submitted_requests. Both locks disable
    // local IRQs, so completion cannot interrupt a local submitter holding either.
    // No queue or completion-map lock is shared by different submission queues.
    queue: SpinLock<VirtQueue, LocalIrqDisabled>,
    queue_size: u16,
    block_requests: Arc<DmaStream>,
    block_responses: Arc<DmaStream>,
    id_allocator: SyncIdAlloc,
    submitted_requests: SpinLock<BTreeMap<u16, SubmittedRequest>, LocalIrqDisabled>,
    descriptor_waiters: WaitQueue,
    counters: QueueCounters,
    profile: QueueProfileCounters,
}

impl Debug for VirtioBlockQueue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtioBlockQueue")
            .field("queue_size", &self.queue_size)
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

impl VirtioBlockQueue {
    fn new(
        index: u16,
        queue_size: u16,
        transport: &mut dyn VirtioTransport,
    ) -> Result<Self, VirtioDeviceError> {
        let queue = VirtQueue::new(index, queue_size, transport)?;
        // Every queue owns its request IDs and DMA slots until completion.
        let block_requests = Arc::new(
            DmaStream::alloc(
                (usize::from(queue_size) * REQ_SIZE).div_ceil(PAGE_SIZE),
                false,
            )
            .map_err(VirtioDeviceError::ResourceAlloc)?,
        );
        let block_responses = Arc::new(
            DmaStream::alloc(
                (usize::from(queue_size) * RESP_SIZE).div_ceil(PAGE_SIZE),
                false,
            )
            .map_err(VirtioDeviceError::ResourceAlloc)?,
        );
        Ok(Self {
            index,
            // Keep the aggregation bound independent of queue depth; reserve
            // header/status descriptors even for fully fragmented data DMA.
            staging: BioRequestSingleQueue::with_max_nr_segments_per_bio(
                usize::from(queue_size - 2).min(62),
            ),
            queue: SpinLock::new(queue),
            queue_size,
            block_requests,
            block_responses,
            id_allocator: SyncIdAlloc::with_capacity(usize::from(queue_size)),
            submitted_requests: SpinLock::new(BTreeMap::new()),
            descriptor_waiters: WaitQueue::new(),
            counters: QueueCounters::default(),
            profile: QueueProfileCounters::default(),
        })
    }

    fn set_profile_enabled(&self, enabled: bool) {
        self.profile.enabled.store(enabled, Ordering::Release);
    }

    fn profile_snapshot(&self) -> QueueProfileSnapshot {
        self.profile.snapshot()
    }

    fn maybe_report_profile(&self, completed: usize) {
        if !self.profile.enabled.load(Ordering::Relaxed)
            || (completed != 1 && !completed.is_multiple_of(PROFILE_REPORT_INTERVAL))
        {
            return;
        }
        let profile = self.profile.snapshot();
        let queue = self.diagnostics();
        let resume = aster_block::bio_resume_profile_snapshot();
        ostd::error!(
            "[VIRTIO_PROFILE] queue={} irq_cpu={} tsc_hz={} completions={} submitted_bios={} descriptor_wait_count={} descriptor_wait_ticks={} max_staging_depth={} submit_samples={} submit_id_alloc_ticks={} submit_dma_prepare_ticks={} submit_vq_acquire_ticks={} submit_vq_add_ticks={} submit_owner_publish_ticks={} submit_should_notify_ticks={} submit_device_notify_ticks={} submit_publish_notify_ticks={} submit_total_ticks={} completion_samples={} completion_wait_ticks={} completion_pop_owner_ticks={} completion_descriptor_wake_ticks={} completion_response_id_ticks={} completion_bio_complete_ticks={} completion_total_ticks={} resume_samples={} resume_ticks={}",
            self.index,
            CpuId::current_racy().as_usize(),
            ostd::arch::tsc_freq(),
            completed,
            queue.submitted_bios,
            queue.descriptor_wait_count,
            queue.descriptor_wait_ticks,
            queue.max_sampled_staging_depth,
            profile.submit_samples,
            profile.submit_id_alloc_ticks,
            profile.submit_dma_prepare_ticks,
            profile.submit_vq_acquire_ticks,
            profile.submit_vq_add_ticks,
            profile.submit_owner_publish_ticks,
            profile.submit_should_notify_ticks,
            profile.submit_device_notify_ticks,
            profile.submit_publish_notify_ticks,
            profile.submit_total_ticks,
            profile.completion_samples,
            profile.completion_wait_ticks,
            profile.completion_pop_owner_ticks,
            profile.completion_descriptor_wake_ticks,
            profile.completion_response_id_ticks,
            profile.completion_bio_complete_ticks,
            profile.completion_total_ticks,
            resume.samples,
            resume.ticks,
        );
    }

    fn diagnostics(&self) -> VirtioBlockQueueDiagnostics {
        VirtioBlockQueueDiagnostics {
            queued_bios: self.counters.queued_bios.load(Ordering::Relaxed),
            queued_pages: self.counters.queued_pages.load(Ordering::Relaxed),
            submitted_bios: self.counters.submitted_bios.load(Ordering::Relaxed),
            submitted_pages: self.counters.submitted_pages.load(Ordering::Relaxed),
            staging_depth: self.staging.num_requests(),
            max_sampled_staging_depth: self
                .counters
                .max_sampled_staging_depth
                .load(Ordering::Relaxed),
            descriptor_wait_count: self.counters.descriptor_wait_count.load(Ordering::Relaxed),
            descriptor_wait_ticks: self.counters.descriptor_wait_ticks.load(Ordering::Relaxed),
            active_forwarders: self.counters.active_forwarders.load(Ordering::Relaxed),
            peak_forwarders: self.counters.peak_forwarders.load(Ordering::Relaxed),
            irq_completions: self.counters.irq_completions.load(Ordering::Relaxed),
            in_flight_requests: self.counters.in_flight_requests.load(Ordering::Relaxed),
        }
    }

    fn wait_for_descriptors(&self, count: usize) -> SpinLockGuard<'_, VirtQueue, LocalIrqDisabled> {
        assert!(
            count <= usize::from(self.queue_size),
            "The request size surpasses the queue size"
        );
        let mut wait_started = None;
        let queue = self.descriptor_waiters.wait_until(|| {
            let queue = self.queue.lock();
            if count <= queue.available_desc() {
                return Some(queue);
            }
            // Release the lock and restore IRQs before WaitQueue can park.
            drop(queue);
            if wait_started.is_none() {
                self.counters
                    .descriptor_wait_count
                    .fetch_add(1, Ordering::Relaxed);
                wait_started = Some(ostd::arch::read_tsc());
            }
            None
        });
        if let Some(start) = wait_started {
            self.counters.descriptor_wait_ticks.fetch_add(
                ostd::arch::read_tsc().wrapping_sub(start),
                Ordering::Relaxed,
            );
        }
        queue
    }

    /// Installs completion ownership before exposing the hardware notification.
    fn finish_submit(
        &self,
        queue: &mut VirtQueue,
        token: u16,
        mut request: SubmittedRequest,
        notify_fn: impl FnOnce(&mut VirtQueue),
    ) {
        let bios = request.bio_request.bios().count();
        let pages = request
            .bio_request
            .bios()
            .flat_map(|bio| bio.segments())
            .map(|segment| segment.nblocks())
            .sum::<usize>();
        let publish_started = request.profile.map(|_| ostd::arch::read_tsc());
        if let Some(profile) = request.profile.as_mut() {
            profile.completion_wait_started = publish_started.unwrap();
        }
        let request_profile = request.profile;
        {
            let mut submitted = self.submitted_requests.lock();
            let previous = submitted.insert(token, request);
            debug_assert!(previous.is_none());
            if previous.is_none() {
                self.counters
                    .in_flight_requests
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        self.counters
            .submitted_bios
            .fetch_add(bios, Ordering::Relaxed);
        self.counters
            .submitted_pages
            .fetch_add(pages, Ordering::Relaxed);
        let owner_publish_done = request_profile.map(|_| ostd::arch::read_tsc());
        let should_notify = queue.should_notify();
        let should_notify_done = request_profile.map(|_| ostd::arch::read_tsc());
        if should_notify {
            notify_fn(queue);
        }
        let device_notify_done = request_profile.map(|_| ostd::arch::read_tsc());
        if let Some(mut profile) = request_profile {
            let publish_started = publish_started.expect("profile start must exist");
            let owner_publish_done = owner_publish_done.expect("profile end must exist");
            let should_notify_done = should_notify_done.expect("profile end must exist");
            let device_notify_done = device_notify_done.expect("profile end must exist");
            profile.owner_publish_ticks = owner_publish_done.wrapping_sub(publish_started);
            profile.should_notify_ticks = should_notify_done.wrapping_sub(owner_publish_done);
            profile.device_notify_ticks = device_notify_done.wrapping_sub(should_notify_done);
            profile.publish_notify_ticks = device_notify_done.wrapping_sub(publish_started);
            self.profile.record_submit(profile);
        }
    }

    /// Handles the IRQ issued from the device.
    fn handle_irq(&self) {
        self.profile.mark_irq_entry(self.index);
        info!("block device handle IRQ");
        // When we enter the IRQs handling function,
        // IRQs have already been disabled,
        // so there is no need to call `disable_irq`.
        loop {
            let completion_started = ostd::arch::read_tsc();
            // Pops the complete request
            let complete_request = {
                let mut queue = self.queue.lock();
                let Ok((token, _)) = queue.pop_used_with_min_bytes(RESP_SIZE) else {
                    return;
                };
                let mut submitted = self.submitted_requests.lock();
                let request = submitted.remove(&token).unwrap();
                // Gauge writers hold this queue's virtqueue and ownership locks.
                // Check before storing so an invariant violation cannot wrap it.
                let in_flight = self
                    .counters
                    .in_flight_requests
                    .load(Ordering::Relaxed)
                    .checked_sub(1)
                    .expect("completed request must be in flight");
                self.counters
                    .in_flight_requests
                    .store(in_flight, Ordering::Relaxed);
                request
            };
            let pop_owner_done = ostd::arch::read_tsc();
            let request_profile = complete_request.profile;

            self.descriptor_waiters.wake_all();
            let completed = self
                .counters
                .irq_completions
                .fetch_add(1, Ordering::Relaxed)
                + 1;
            let descriptor_wake_done = ostd::arch::read_tsc();

            // Handles the response
            let id = complete_request.id as usize;
            let resp_slice =
                Slice::new(&self.block_responses, id * RESP_SIZE..(id + 1) * RESP_SIZE);
            resp_slice.sync_from_device().unwrap();
            let resp: BlockResp = resp_slice.read_val(0).unwrap();
            self.id_allocator.dealloc(id);
            let response_id_done = ostd::arch::read_tsc();
            match RespStatus::try_from(resp.status) {
                Ok(RespStatus::Ok) => {}
                _ => {
                    // Completes the bio request with an error
                    complete_request.bio_request.into_bios().for_each(|bio| {
                        bio.complete(BioStatus::IoError);
                    });
                    let bio_complete_done = ostd::arch::read_tsc();
                    if let Some(profile) = request_profile {
                        self.profile.record_completion(
                            profile,
                            completion_started,
                            pop_owner_done,
                            descriptor_wake_done,
                            response_id_done,
                            bio_complete_done,
                        );
                    }
                    self.maybe_report_profile(completed);
                    continue;
                }
            };

            // Synchronize DMA mapping if read from the device
            if let BioType::Read = complete_request.bio_request.type_() {
                complete_request
                    .bio_request
                    .bios()
                    .flat_map(|bio| {
                        bio.segments()
                            .iter()
                            .map(|segment| segment.inner_dma_slice())
                    })
                    .for_each(|dma_slice| dma_slice.sync_from_device().unwrap());
            }

            // Completes the bio request
            complete_request.bio_request.into_bios().for_each(|bio| {
                bio.complete(BioStatus::Complete);
            });
            let bio_complete_done = ostd::arch::read_tsc();
            if let Some(profile) = request_profile {
                self.profile.record_completion(
                    profile,
                    completion_started,
                    pop_owner_done,
                    descriptor_wake_done,
                    response_id_done,
                    bio_complete_done,
                );
            }
            self.maybe_report_profile(completed);
        }
    }

    /// Submits a read, waiting for queue-local capacity if needed.
    fn read(&self, bio_request: BioRequest) {
        let id = self.id_allocator.alloc();
        let req_slice = {
            let req_slice = Slice::new(
                self.block_requests.clone(),
                id * REQ_SIZE..(id + 1) * REQ_SIZE,
            );
            let req = BlockReq {
                type_: ReqType::In as _,
                reserved: 0,
                sector: bio_request.sid_range().start.to_raw(),
            };
            req_slice.write_val(0, &req).unwrap();
            req_slice.sync_to_device().unwrap();
            req_slice
        };

        let resp_slice = {
            let resp_slice = Slice::new(
                self.block_responses.clone(),
                id * RESP_SIZE..(id + 1) * RESP_SIZE,
            );
            resp_slice.write_val(0, &BlockResp::default()).unwrap();
            resp_slice.sync_to_device().unwrap();
            resp_slice
        };

        let outputs = {
            let mut outputs = Vec::with_capacity(bio_request.num_segments() + 1);
            let dma_slices_iter = bio_request.bios().flat_map(|bio| {
                bio.segments()
                    .iter()
                    .map(|segment| segment.inner_dma_slice())
            });
            for dma_slice in dma_slices_iter {
                append_data(&mut outputs, dma_slice, u32::MAX as usize - 1);
            }
            // The status byte must stay separate from data, even if adjacent.
            outputs.push(DmaSpan::single(&resp_slice));
            outputs
        };

        let output_refs: SmallVec<[_; 64]> = outputs.iter().collect();
        let mut queue = self.wait_for_descriptors(outputs.len() + 1);
        let token = queue
            .add_dma_bufs(&[&req_slice], output_refs.as_slice())
            .expect("add queue failed");
        drop(output_refs);
        drop(outputs);
        self.finish_submit(
            &mut queue,
            token,
            SubmittedRequest::new(id as u16, bio_request),
            VirtQueue::notify,
        );
    }

    /// Submits a write, waiting for queue-local capacity if needed.
    fn write(&self, bio_request: BioRequest) {
        let mut profile = self
            .profile
            .should_sample_submit()
            .then(|| WriteRequestProfile {
                submit_started: ostd::arch::read_tsc(),
                ..WriteRequestProfile::default()
            });
        if profile.is_some() {
            bio_request
                .bios()
                .for_each(|bio| bio.enable_resume_profile());
        }
        let phase_started = profile.map(|_| ostd::arch::read_tsc());
        let id = self.id_allocator.alloc();
        if let Some(profile) = profile.as_mut() {
            profile.id_alloc_ticks = ostd::arch::read_tsc()
                .wrapping_sub(phase_started.expect("profile start must exist"));
        }
        let phase_started = profile.map(|_| ostd::arch::read_tsc());
        let req_slice = {
            let req_slice = Slice::new(
                self.block_requests.clone(),
                id * REQ_SIZE..(id + 1) * REQ_SIZE,
            );
            let req = BlockReq {
                type_: ReqType::Out as _,
                reserved: 0,
                sector: bio_request.sid_range().start.to_raw(),
            };
            req_slice.write_val(0, &req).unwrap();
            req_slice.sync_to_device().unwrap();
            req_slice
        };

        let resp_slice = {
            let resp_slice = Slice::new(
                self.block_responses.clone(),
                id * RESP_SIZE..(id + 1) * RESP_SIZE,
            );
            resp_slice.write_val(0, &BlockResp::default()).unwrap();
            resp_slice.sync_to_device().unwrap();
            resp_slice
        };

        let inputs = {
            let mut inputs = Vec::with_capacity(bio_request.num_segments() + 1);
            let dma_slices_iter = bio_request.bios().flat_map(|bio| {
                bio.segments()
                    .iter()
                    .map(|segment| segment.inner_dma_slice())
            });
            for dma_slice in dma_slices_iter {
                dma_slice.sync_to_device().unwrap();
                append_data(&mut inputs, dma_slice, u32::MAX as usize - 1);
            }
            // Keep the protocol header distinct from the data descriptor(s).
            inputs.insert(0, DmaSpan::single(&req_slice));
            inputs
        };

        let input_refs: SmallVec<[_; 64]> = inputs.iter().collect();
        if let Some(profile) = profile.as_mut() {
            profile.dma_prepare_ticks = ostd::arch::read_tsc()
                .wrapping_sub(phase_started.expect("profile start must exist"));
        }
        let phase_started = profile.map(|_| ostd::arch::read_tsc());
        let mut queue = self.wait_for_descriptors(inputs.len() + 1);
        if let Some(profile) = profile.as_mut() {
            profile.vq_acquire_ticks = ostd::arch::read_tsc()
                .wrapping_sub(phase_started.expect("profile start must exist"));
        }
        let phase_started = profile.map(|_| ostd::arch::read_tsc());
        let token = queue
            .add_dma_bufs(input_refs.as_slice(), &[&resp_slice])
            .expect("add queue failed");
        if let Some(profile) = profile.as_mut() {
            profile.vq_add_ticks = ostd::arch::read_tsc()
                .wrapping_sub(phase_started.expect("profile start must exist"));
        }
        drop(input_refs);
        drop(inputs);
        self.finish_submit(
            &mut queue,
            token,
            SubmittedRequest::new_profiled(id as u16, bio_request, profile),
            VirtQueue::notify,
        );
    }

    /// Flushes any cached data from the guest to the persistent storage on the host.
    fn flush(&self, bio_request: BioRequest) {
        let id = self.id_allocator.alloc();
        let req_slice = {
            let req_slice = Slice::new(&self.block_requests, id * REQ_SIZE..(id + 1) * REQ_SIZE);
            let req = BlockReq {
                type_: ReqType::Flush as _,
                reserved: 0,
                sector: bio_request.sid_range().start.to_raw(),
            };
            req_slice.write_val(0, &req).unwrap();
            req_slice.sync_to_device().unwrap();
            req_slice
        };

        let resp_slice = {
            let resp_slice =
                Slice::new(&self.block_responses, id * RESP_SIZE..(id + 1) * RESP_SIZE);
            resp_slice.write_val(0, &BlockResp::default()).unwrap();
            resp_slice.sync_to_device().unwrap();
            resp_slice
        };

        let mut queue = self.wait_for_descriptors(2);
        let token = queue
            .add_dma_bufs(&[&req_slice], &[&resp_slice])
            .expect("add queue failed");
        self.finish_submit(
            &mut queue,
            token,
            SubmittedRequest::new(id as u16, bio_request),
            VirtQueue::notify,
        );
    }
}

#[cfg(ktest)]
mod tests {
    use aster_block::{
        bio::{Bio, BioDirection, BioSegment},
        id::Sid,
    };
    use aster_pci::cfg_space::BarAccess;
    use aster_util::safe_ptr::SafePtr;
    use device_id::MajorId;
    use io_util::batch::IoBatch;
    use ostd::{
        io::IoMem,
        irq::IrqCallbackFunction,
        mm::dma::DmaCoherent,
        prelude::ktest,
        sync::Waiter,
        task::{Task, TaskOptions},
    };

    use super::*;
    use crate::{
        VirtioDeviceType,
        queue::{AvailRing, Descriptor, UsedRing},
        transport::{DeviceStatus, VirtioTransportError, queue_interrupt_cpu},
    };

    // Only the device boundary is simulated. The driver uses real staging,
    // virtqueues, DMA slots, request IDs, completion maps, and wait queues.
    #[derive(Debug, Default)]
    struct TestTransport {
        rings: Vec<(
            SafePtr<AvailRing, Arc<DmaCoherent>>,
            SafePtr<UsedRing, Arc<DmaCoherent>>,
        )>,
    }

    impl TestTransport {
        fn complete(&self, queue_id: usize, token: u16, index: u16) {
            let used = &self.rings[queue_id].1;
            let mut ptr = used.borrow_vm();
            ptr.byte_add(4 + usize::from(index % 4) * 8);
            let mut ptr = ptr.cast::<u32>();
            ptr.write_once(&u32::from(token)).unwrap();
            ptr.add(1);
            ptr.write_once(&1).unwrap();
            let mut ptr = used.borrow_vm().cast::<u16>();
            ptr.add(1);
            ptr.write_once(&(index + 1)).unwrap();
        }
    }

    impl VirtioTransport for TestTransport {
        fn device_type(&self) -> VirtioDeviceType {
            VirtioDeviceType::Block
        }
        fn read_device_features(&self) -> u64 {
            0
        }
        fn write_driver_features(&mut self, _: u64) -> Result<(), VirtioTransportError> {
            Ok(())
        }
        fn read_device_status(&self) -> DeviceStatus {
            DeviceStatus::empty()
        }
        fn write_device_status(&mut self, _: DeviceStatus) -> Result<(), VirtioTransportError> {
            Ok(())
        }
        fn device_config_mem(&self) -> Option<IoMem> {
            None
        }
        fn device_config_bar(&self) -> Option<(BarAccess, usize)> {
            None
        }
        fn num_queues(&self) -> u16 {
            3
        }
        fn set_queue(
            &mut self,
            index: u16,
            _: u16,
            _: &SafePtr<Descriptor, Arc<DmaCoherent>>,
            avail: &SafePtr<AvailRing, Arc<DmaCoherent>>,
            used: &SafePtr<UsedRing, Arc<DmaCoherent>>,
        ) -> Result<(), VirtioTransportError> {
            assert_eq!(usize::from(index), self.rings.len());
            // Ordinary tests do not need a physical notification register.
            used.borrow_vm().cast::<u16>().write_once(&1).unwrap();
            self.rings.push((avail.clone(), used.clone()));
            Ok(())
        }
        fn max_queue_size(&self, _: u16) -> Result<u16, VirtioTransportError> {
            Ok(64)
        }
        fn notify_config(&self, _: usize) -> ConfigManager<u32> {
            ConfigManager::new(None, None)
        }
        fn is_legacy_version(&self) -> bool {
            false
        }
        fn register_queue_callback(
            &mut self,
            _: u16,
            _: Box<IrqCallbackFunction>,
            _: bool,
        ) -> Result<(), VirtioTransportError> {
            Ok(())
        }
        fn register_cfg_callback(
            &mut self,
            _: Box<IrqCallbackFunction>,
        ) -> Result<(), VirtioTransportError> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct QueueDevice {
        queues: Vec<Arc<VirtioBlockQueue>>,
        cpu: usize,
    }

    impl QueueDevice {
        fn new(count: u16, size: u16) -> (Self, TestTransport) {
            let mut transport = TestTransport::default();
            let queues = (0..count)
                .map(|index| Arc::new(VirtioBlockQueue::new(index, size, &mut transport).unwrap()))
                .collect();
            (Self { queues, cpu: 0 }, transport)
        }

        fn enqueue_bio(&self, type_: BioType, affinity: BioQueueAffinity) -> IoBatch {
            let mut batch = IoBatch::new();
            Bio::new_on_queue(type_, Sid::new(0), Vec::new(), None, affinity)
                .submit(self, &mut batch)
                .unwrap();
            batch
        }
    }

    impl aster_block::BlockDevice for QueueDevice {
        fn enqueue(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError> {
            enqueue_on_queue(&self.queues, bio, self.cpu)
        }
        fn metadata(&self) -> BlockDeviceMeta {
            BlockDeviceMeta {
                nr_submission_queues: self.queues.len() as u16,
                nr_sectors: 4096,
                max_nr_segments_per_bio: 2,
            }
        }
        fn name(&self) -> &str {
            "queue-runtime-test"
        }
        fn id(&self) -> DeviceId {
            DeviceId::new(MajorId::new(1), MinorId::new(0))
        }
    }

    #[derive(Debug)]
    struct DirectQueueDevice {
        queues: Vec<Arc<VirtioBlockQueue>>,
        cpu: usize,
    }

    impl aster_block::BlockDevice for DirectQueueDevice {
        fn enqueue(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError> {
            submit_direct_on_queue(&self.queues, BlockFeatures::empty(), bio, self.cpu)
        }

        fn metadata(&self) -> BlockDeviceMeta {
            BlockDeviceMeta {
                nr_submission_queues: self.queues.len() as u16,
                nr_sectors: 4096,
                max_nr_segments_per_bio: 2,
            }
        }

        fn name(&self) -> &str {
            "direct-queue-runtime-test"
        }

        fn id(&self) -> DeviceId {
            DeviceId::new(MajorId::new(1), MinorId::new(1))
        }
    }

    fn finish_staged(queue: &VirtioBlockQueue) {
        for bio in queue.staging.dequeue().into_bios() {
            bio.complete(BioStatus::Complete);
        }
    }

    #[ktest]
    fn strict_bios_enter_only_the_selected_staging_queue() {
        let (device, _) = QueueDevice::new(3, 4);
        let _batch = device.enqueue_bio(BioType::Write, BioQueueAffinity::Queue(2));
        assert_eq!(device.queues[0].staging.num_requests(), 0);
        assert_eq!(device.queues[1].staging.num_requests(), 0);
        assert_eq!(device.queues[2].staging.num_requests(), 1);
        let stats = device.queues[2].diagnostics();
        assert_eq!(stats.queued_bios, 1);
        assert_eq!(stats.staging_depth, 1);
        assert_eq!(stats.max_sampled_staging_depth, 1);
        assert_eq!(device.queues[0].diagnostics().queued_bios, 0);
        finish_staged(&device.queues[2]);
        assert_eq!(device.queues[2].diagnostics().staging_depth, 0);

        let _batch = device.enqueue_bio(BioType::Flush, BioQueueAffinity::Queue(2));
        assert_eq!(device.queues[0].staging.num_requests(), 1);
        assert_eq!(device.queues[2].staging.num_requests(), 0);
        finish_staged(&device.queues[0]);
        assert!(matches!(
            select_queue(BioType::Write, BioQueueAffinity::Queue(3), 0, 3),
            Err(BioEnqueueError::InvalidQueue)
        ));
    }

    #[ktest]
    fn any_bios_select_a_valid_cpu_local_queue() {
        let (mut device, _) = QueueDevice::new(3, 4);
        for (cpu, expected) in [(0, 0), (1, 1), (2, 2), (3, 0), (17, 2)] {
            device.cpu = cpu;
            let _batch = device.enqueue_bio(BioType::Read, BioQueueAffinity::Any);
            for (index, queue) in device.queues.iter().enumerate() {
                assert_eq!(queue.staging.num_requests(), usize::from(index == expected));
            }
            finish_staged(&device.queues[expected]);
        }
        assert_eq!(
            select_queue(BioType::Flush, BioQueueAffinity::Any, 17, 3).unwrap(),
            0
        );
    }

    #[ktest]
    fn direct_submit_bypasses_staging_and_preserves_completion() {
        let (queue_device, transport) = QueueDevice::new(2, 8);
        let device = DirectQueueDevice {
            queues: queue_device.queues,
            cpu: 1,
        };
        let mut batch = IoBatch::new();
        Bio::new_on_queue(
            BioType::Write,
            Sid::new(0),
            alloc::vec![BioSegment::alloc(1, BioDirection::ToDevice)],
            None,
            BioQueueAffinity::Any,
        )
        .submit(&device, &mut batch)
        .unwrap();

        assert_eq!(device.queues[0].staging.num_requests(), 0);
        assert_eq!(device.queues[1].staging.num_requests(), 0);
        assert_eq!(device.queues[1].submitted_requests.lock().len(), 1);
        Slice::new(&device.queues[1].block_responses, 0..RESP_SIZE)
            .write_val(0, &BlockResp { status: 0 })
            .unwrap();
        transport.complete(1, 0, 0);
        device.queues[1].handle_irq();

        assert!(batch.wait_all().is_ok());
        assert!(device.queues[1].submitted_requests.lock().is_empty());
        assert_eq!(device.queues[1].diagnostics().irq_completions, 1);
    }

    #[ktest]
    fn queue_interrupts_are_distributed_across_cpus() {
        let cpu_count = ostd::cpu::num_cpus();
        for queue in 0..8 {
            assert_eq!(
                queue_interrupt_cpu(queue, cpu_count).as_usize(),
                usize::from(queue) % cpu_count
            );
        }
    }

    #[ktest]
    fn queue_completion_maps_are_independent() {
        let (device, transport) = QueueDevice::new(2, 4);
        let mut batches = Vec::new();
        for index in 0..2 {
            batches.push(device.enqueue_bio(BioType::Write, BioQueueAffinity::Queue(index)));
            let queue = &device.queues[index as usize];
            queue.write(queue.staging.dequeue());
            assert!(queue.submitted_requests.lock().contains_key(&0));
            Slice::new(&queue.block_responses, 0..RESP_SIZE)
                .write_val(0, &BlockResp { status: 0 })
                .unwrap();
            transport.complete(index as usize, 0, 0);
        }
        device.queues[0].handle_irq();
        assert!(device.queues[0].submitted_requests.lock().is_empty());
        assert_eq!(device.queues[1].submitted_requests.lock().len(), 1);
        let first = device.queues[0].diagnostics();
        let second = device.queues[1].diagnostics();
        assert_eq!(first.submitted_bios, 1);
        assert_eq!(first.irq_completions, 1);
        assert_eq!(first.in_flight_requests, 0);
        assert_eq!(second.submitted_bios, 1);
        assert_eq!(second.irq_completions, 0);
        assert_eq!(second.in_flight_requests, 1);
        device.queues[1].handle_irq();
        assert!(device.queues[1].submitted_requests.lock().is_empty());
        for batch in batches {
            assert!(batch.wait_all().is_ok());
        }
    }

    #[ktest]
    fn common_queue_diagnostics_report_selected_queue_and_invalid_ids() {
        let (queue_device, transport) = QueueDevice::new(2, 4);
        let mut batch = IoBatch::new();
        (&queue_device as &dyn aster_block::BlockDevice)
            .write_segments_on_queue_async(
                aster_block::id::Bid::new(0),
                alloc::vec![BioSegment::alloc(2, BioDirection::ToDevice)],
                BioQueueAffinity::Queue(1),
                None,
                &mut batch,
            )
            .unwrap();
        let device = BlockDevice {
            device: Arc::new(DeviceInner {
                config_manager: ConfigManager::new(None, None),
                cached_nr_sectors: AtomicUsize::new(4096),
                features: BlockFeatures::empty(),
                queues: queue_device.queues,
                direct_submit: AtomicBool::new(false),
                max_nr_segments_per_bio: 2,
                transport: SpinLock::new(Box::new(transport)),
            }),
            id: DeviceId::new(MajorId::new(1), MinorId::new(0)),
            name: String::from("common-diagnostic-test"),
            partitions: SpinLock::new(None),
            weak_self: Weak::new(),
        };
        let common: &dyn aster_block::BlockDevice = &device;
        assert_eq!(common.queue_diagnostics(2), None);
        let stats = common.queue_diagnostics(1).unwrap();
        assert_eq!(stats.queued_bios, 1);
        assert_eq!(stats.queued_pages, 2);
        assert_eq!(stats.staging_depth, 1);
        assert_eq!(common.queue_diagnostics(0).unwrap().queued_bios, 0);
        finish_staged(&device.device.queues[1]);
        assert_eq!(common.queue_diagnostics(1).unwrap().staging_depth, 0);
        assert!(batch.wait_all().is_ok());
    }

    #[ktest]
    fn block_metadata_does_not_reread_device_config() {
        let (queue_device, transport) = QueueDevice::new(2, 4);
        let device = BlockDevice {
            device: Arc::new(DeviceInner {
                config_manager: ConfigManager::new(None, None),
                cached_nr_sectors: AtomicUsize::new(4096),
                features: BlockFeatures::empty(),
                queues: queue_device.queues,
                direct_submit: AtomicBool::new(false),
                max_nr_segments_per_bio: 2,
                transport: SpinLock::new(Box::new(transport)),
            }),
            id: DeviceId::new(MajorId::new(1), MinorId::new(0)),
            name: String::from("cached-metadata-test"),
            partitions: SpinLock::new(None),
            weak_self: Weak::new(),
        };

        let metadata = aster_block::BlockDevice::metadata(&device);
        assert_eq!(metadata.nr_sectors, 4096);
        assert_eq!(metadata.nr_submission_queues, 2);
        assert_eq!(metadata.max_nr_segments_per_bio, 2);
    }

    #[ktest]
    fn queue_diagnostics_count_pages_and_forwarders() {
        let (device, transport) = QueueDevice::new(1, 4);
        let queue = &device.queues[0];
        // Snapshots must remain available while completion ownership is locked.
        let owners = queue.submitted_requests.lock();
        assert_eq!(queue.diagnostics().in_flight_requests, 0);
        drop(owners);
        queue.handle_irq();
        assert_eq!(queue.diagnostics().in_flight_requests, 0);
        let mut batch = IoBatch::new();
        Bio::new_on_queue(
            BioType::Write,
            Sid::new(0),
            alloc::vec![BioSegment::alloc(2, BioDirection::ToDevice)],
            None,
            BioQueueAffinity::Queue(0),
        )
        .submit(&device, &mut batch)
        .unwrap();
        assert_eq!(queue.diagnostics().queued_pages, 2);
        queue.write(queue.staging.dequeue());
        let owners = queue.submitted_requests.lock();
        let stats = queue.diagnostics();
        assert_eq!(queue.counters.in_flight_requests.load(Ordering::Relaxed), 1);
        drop(owners);
        assert_eq!(stats.submitted_pages, 2);
        assert_eq!(stats.in_flight_requests, 1);
        let forwarder = ForwardingGuard::new(&queue.counters);
        assert_eq!(queue.diagnostics().active_forwarders, 1);
        assert_eq!(queue.diagnostics().peak_forwarders, 1);
        drop(forwarder);
        assert_eq!(queue.diagnostics().active_forwarders, 0);
        assert_eq!(queue.diagnostics().peak_forwarders, 1);
        Slice::new(&queue.block_responses, 0..RESP_SIZE)
            .write_val(0, &BlockResp { status: 0 })
            .unwrap();
        transport.complete(0, 0, 0);
        queue.handle_irq();
        assert_eq!(queue.diagnostics().in_flight_requests, 0);
        assert!(batch.wait_all().is_ok());
        queue.handle_irq();
        assert_eq!(queue.diagnostics().in_flight_requests, 0);

        let failed_batch = device.enqueue_bio(BioType::Write, BioQueueAffinity::Queue(0));
        queue.write(queue.staging.dequeue());
        assert_eq!(queue.diagnostics().in_flight_requests, 1);
        Slice::new(&queue.block_responses, 0..RESP_SIZE)
            .write_val(
                0,
                &BlockResp {
                    status: RespStatus::IoErr as u8,
                },
            )
            .unwrap();
        transport.complete(0, 0, 1);
        queue.handle_irq();
        assert!(failed_batch.wait_all().is_err());
        assert_eq!(queue.diagnostics().in_flight_requests, 0);
        queue.handle_irq();
        assert_eq!(queue.diagnostics().in_flight_requests, 0);
    }

    #[ktest]
    fn sampled_profile_records_submit_and_completion_stages() {
        let (device, transport) = QueueDevice::new(1, 8);
        let queue = &device.queues[0];
        queue.set_profile_enabled(true);
        let resume_before = aster_block::bio_resume_profile_snapshot();
        let mut batch = IoBatch::new();
        Bio::new_on_queue(
            BioType::Write,
            Sid::new(0),
            alloc::vec![BioSegment::alloc(1, BioDirection::ToDevice)],
            None,
            BioQueueAffinity::Queue(0),
        )
        .submit(&device, &mut batch)
        .unwrap();
        queue.write(queue.staging.dequeue());
        let submitted = queue.profile_snapshot();
        assert_eq!(submitted.submit_samples, 1);
        assert!(submitted.submit_total_ticks > 0);
        assert!(submitted.submit_owner_publish_ticks > 0);
        assert!(submitted.submit_should_notify_ticks > 0);
        assert!(submitted.submit_device_notify_ticks > 0);

        Slice::new(&queue.block_responses, 0..RESP_SIZE)
            .write_val(0, &BlockResp { status: 0 })
            .unwrap();
        transport.complete(0, 0, 0);
        queue.handle_irq();
        let completed = queue.profile_snapshot();
        assert_eq!(completed.completion_samples, 1);
        assert!(completed.completion_total_ticks > 0);
        assert!(batch.wait_all().is_ok());
        let resume_after = aster_block::bio_resume_profile_snapshot();
        assert_eq!(resume_after.samples, resume_before.samples + 1);
        assert!(resume_after.ticks > resume_before.ticks);
    }

    #[ktest]
    fn completion_owner_is_installed_before_notify() {
        let (device, transport) = QueueDevice::new(1, 4);
        let _batch = device.enqueue_bio(BioType::Write, BioQueueAffinity::Queue(0));
        let queue = &device.queues[0];
        let request = queue.staging.dequeue();
        let id = queue.id_allocator.alloc();
        let input = Slice::new(&queue.block_requests, 0..REQ_SIZE);
        let output = Slice::new(&queue.block_responses, 0..RESP_SIZE);
        transport.rings[0]
            .1
            .borrow_vm()
            .cast::<u16>()
            .write_once(&0)
            .unwrap();
        let notifications = AtomicUsize::new(0);
        let mut virtqueue = queue.wait_for_descriptors(2);
        let token = virtqueue.add_dma_bufs(&[&input], &[&output]).unwrap();
        queue.finish_submit(
            &mut virtqueue,
            token,
            SubmittedRequest::new(id as u16, request),
            |_| {
                // The simulated device observes the first published chain at notify.
                let mut ptr = transport.rings[0].0.borrow_vm().cast::<u16>();
                ptr.add(2);
                let token = ptr.read_once().unwrap();
                let owners = queue.submitted_requests.lock();
                assert!(owners.contains_key(&token));
                assert_eq!(queue.diagnostics().in_flight_requests, 1);
                drop(owners);
                notifications.fetch_add(1, Ordering::Relaxed);
            },
        );
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        drop(virtqueue);
        output.write_val(0, &BlockResp { status: 0 }).unwrap();
        transport.complete(0, 0, 0);
        queue.handle_irq();
    }

    #[ktest]
    fn descriptor_wait_is_woken_by_its_queue_irq() {
        let (device, transport) = QueueDevice::new(2, 4);
        let queue = &device.queues[0];
        let mut batches = Vec::new();
        for _ in 0..2 {
            batches.push(device.enqueue_bio(BioType::Write, BioQueueAffinity::Queue(0)));
            queue.write(queue.staging.dequeue());
        }
        assert_eq!(queue.queue.lock().available_desc(), 0);
        assert_eq!(queue.diagnostics().in_flight_requests, 2);
        let capacity_available = Arc::new(WaitQueue::new());
        let capacity_ready = Arc::new(AtomicUsize::new(0));
        let capacity_queue = queue.clone();
        let capacity_available_clone = capacity_available.clone();
        let capacity_ready_clone = capacity_ready.clone();
        TaskOptions::new(move || {
            let capacity = capacity_queue.wait_for_descriptors(2);
            drop(capacity);
            capacity_ready_clone.store(1, Ordering::Release);
            capacity_available_clone.wake_all();
        })
        .data(())
        .spawn()
        .unwrap();
        while queue.counters.descriptor_wait_count.load(Ordering::Relaxed) == 0 {
            Task::yield_now();
        }
        assert_eq!(capacity_ready.load(Ordering::Acquire), 0);
        // Another queue continues to submit while queue 0 is full.
        let other_batch = device.enqueue_bio(BioType::Write, BioQueueAffinity::Queue(1));
        let other_queue = &device.queues[1];
        other_queue.write(other_queue.staging.dequeue());
        assert_eq!(other_queue.diagnostics().submitted_bios, 1);
        let (_waiter0, waker0) = Waiter::new_pair();
        let (_waiter1, waker1) = Waiter::new_pair();
        queue.descriptor_waiters.enqueue(waker0.clone());
        device.queues[1].descriptor_waiters.enqueue(waker1.clone());
        queue.handle_irq();
        assert_eq!(queue.diagnostics().in_flight_requests, 2);
        assert!(waker0.wake_up(), "empty IRQ must not wake waiters");
        // Re-arm after checking the empty IRQ.
        let (_waiter0, waker0) = Waiter::new_pair();
        queue.descriptor_waiters.enqueue(waker0.clone());
        Slice::new(&queue.block_responses, 0..RESP_SIZE)
            .write_val(0, &BlockResp { status: 0 })
            .unwrap();
        transport.complete(0, 0, 0);
        queue.handle_irq();
        assert!(
            !waker0.wake_up(),
            "the completing queue must wake its descriptor waiter"
        );
        assert!(
            waker1.wake_up(),
            "another queue's descriptor waiter must remain asleep"
        );
        capacity_available
            .wait_until(|| (capacity_ready.load(Ordering::Acquire) == 1).then_some(()));
        assert_eq!(queue.diagnostics().descriptor_wait_count, 1);
        assert!(queue.diagnostics().descriptor_wait_ticks > 0);
        assert_eq!(queue.queue.lock().available_desc(), 2);
        assert_eq!(queue.diagnostics().in_flight_requests, 1);
        Slice::new(&other_queue.block_responses, 0..RESP_SIZE)
            .write_val(0, &BlockResp { status: 0 })
            .unwrap();
        transport.complete(1, 0, 0);
        other_queue.handle_irq();
        assert!(other_batch.wait_all().is_ok());
        Slice::new(&queue.block_responses, RESP_SIZE..2 * RESP_SIZE)
            .write_val(0, &BlockResp { status: 0 })
            .unwrap();
        transport.complete(0, 2, 1);
        queue.handle_irq();
        assert_eq!(queue.diagnostics().in_flight_requests, 0);
        for batch in batches {
            assert!(batch.wait_all().is_ok());
        }
    }

    #[ktest]
    fn virtio_blk_uses_one_queue_without_mq() {
        assert_eq!(
            optional_config_queues(BlockFeatures::MQ, true, || panic!("legacy config read")),
            None
        );
        assert_eq!(
            optional_config_queues(BlockFeatures::empty(), false, || panic!("MQ config read")),
            None
        );
        assert_eq!(
            optional_config_queues(BlockFeatures::MQ, false, || Ok(8)),
            Some(8)
        );
        assert_eq!(
            active_queue_count(BlockFeatures::empty(), Some(8), 8).unwrap(),
            1
        );
        assert_eq!(active_queue_count(BlockFeatures::MQ, None, 8).unwrap(), 1);
        assert_eq!(
            active_queue_count(BlockFeatures::MQ, Some(0), 8).unwrap(),
            1
        );
        assert_eq!(
            active_queue_count(BlockFeatures::MQ, Some(8), 1).unwrap(),
            1
        );
        assert!(active_queue_count(BlockFeatures::MQ, Some(8), 0).is_err());
    }

    #[ktest]
    fn virtio_blk_caps_queue_count_at_sixteen() {
        assert_eq!(
            active_queue_count(BlockFeatures::MQ, Some(64), 64).unwrap(),
            16
        );
        assert_eq!(
            active_queue_count(BlockFeatures::MQ, Some(3), 64).unwrap(),
            3
        );
        assert_eq!(
            active_queue_count(BlockFeatures::MQ, Some(64), 2).unwrap(),
            2
        );
    }

    #[ktest]
    fn virtio_blk_falls_back_before_setup_when_mq_preflight_is_unusable() {
        let mut checked = Vec::new();
        let sizes = preflight_queue_sizes(3, |index| {
            checked.push(index);
            Ok(if index == 1 { 2 } else { 128 })
        })
        .unwrap();
        assert_eq!(checked, [0, 1]);
        assert_eq!(sizes, [128]);

        let sizes = preflight_queue_sizes(3, |index| Ok(if index == 1 { 5 } else { 128 })).unwrap();
        assert_eq!(sizes, [128]);

        let sizes = preflight_queue_sizes(3, |index| {
            if index == 1 {
                Err(VirtioDeviceError::InvalidQueueArgs)
            } else {
                Ok(128)
            }
        })
        .unwrap();
        assert_eq!(sizes, [128]);

        let result = preflight_queue_sizes(3, |index| {
            if index == 0 {
                Err(VirtioDeviceError::InvalidQueueArgs)
            } else {
                Ok(128)
            }
        });
        assert!(matches!(result, Err(VirtioDeviceError::InvalidQueueArgs)));
        assert!(matches!(
            preflight_queue_sizes(3, |_| Ok(5)),
            Err(VirtioDeviceError::InvalidQueueArgs)
        ));
    }
}

/// A submitted bio request for callback.
#[derive(Debug)]
struct SubmittedRequest {
    id: u16,
    bio_request: BioRequest,
    profile: Option<WriteRequestProfile>,
}

impl SubmittedRequest {
    pub fn new(id: u16, bio_request: BioRequest) -> Self {
        Self {
            id,
            bio_request,
            profile: None,
        }
    }

    fn new_profiled(
        id: u16,
        bio_request: BioRequest,
        profile: Option<WriteRequestProfile>,
    ) -> Self {
        Self {
            id,
            bio_request,
            profile,
        }
    }
}

/// VirtIOBlock request.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
struct BlockReq {
    pub type_: u32,
    pub reserved: u32,
    pub sector: u64,
}

const REQ_SIZE: usize = size_of::<BlockReq>();

/// Response of a VirtIOBlock request.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
struct BlockResp {
    pub status: u8,
}

const RESP_SIZE: usize = size_of::<BlockResp>();

impl Default for BlockResp {
    fn default() -> Self {
        Self {
            status: RespStatus::_NotReady as _,
        }
    }
}
