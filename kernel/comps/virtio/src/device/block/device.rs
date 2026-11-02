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
    sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
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
        match request.type_() {
            BioType::Read => queue.read(request),
            BioType::Write => queue.write(request),
            BioType::Flush if self.device.features.contains(BlockFeatures::FLUSH) => {
                debug_assert_eq!(queue_id, 0);
                queue.flush(request);
            }
            BioType::Flush => request.into_bios().for_each(|bio| {
                bio.complete(BioStatus::Complete);
            }),
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
        enqueue_on_queue(&self.device.queues, bio, CpuId::current_racy().as_usize())
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: self.device.max_nr_segments_per_bio,
            nr_sectors: self.device.config_manager.capacity_sectors(),
            nr_submission_queues: self.nr_submission_queues(),
        }
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
    features: BlockFeatures,
    queues: Vec<Arc<VirtioBlockQueue>>,
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
            features,
            queues,
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
                    false,
                )?;
            }
            transport.finish_init();
        }

        Ok(device)
    }

    fn handle_config_change(&self) {
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

/// Per-queue counters for diagnosing submission balance and backpressure.
#[derive(Clone, Copy, Debug)]
pub struct VirtioBlockQueueDiagnostics {
    pub queued_bios: usize,
    pub queued_pages: usize,
    pub submitted_bios: usize,
    pub submitted_pages: usize,
    pub staging_depth: usize,
    /// Maximum sampled staging depth; a concurrent dequeue may precede sampling.
    pub max_sampled_staging_depth: usize,
    pub descriptor_wait_count: usize,
    /// TSC ticks spent waiting for descriptors (convert with `ostd::arch::tsc_freq()`).
    pub descriptor_wait_ticks: u64,
    pub active_forwarders: usize,
    pub peak_forwarders: usize,
    pub irq_completions: usize,
    pub in_flight_requests: usize,
}

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
        })
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
            in_flight_requests: self.submitted_requests.lock().len(),
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
        request: SubmittedRequest,
        notify_fn: impl FnOnce(&mut VirtQueue),
    ) {
        let bios = request.bio_request.bios().count();
        let pages = request
            .bio_request
            .bios()
            .flat_map(|bio| bio.segments())
            .map(|segment| segment.nblocks())
            .sum::<usize>();
        let previous = self.submitted_requests.lock().insert(token, request);
        debug_assert!(previous.is_none());
        self.counters
            .submitted_bios
            .fetch_add(bios, Ordering::Relaxed);
        self.counters
            .submitted_pages
            .fetch_add(pages, Ordering::Relaxed);
        if queue.should_notify() {
            notify_fn(queue);
        }
    }

    /// Handles the IRQ issued from the device.
    fn handle_irq(&self) {
        info!("block device handle IRQ");
        // When we enter the IRQs handling function,
        // IRQs have already been disabled,
        // so there is no need to call `disable_irq`.
        loop {
            // Pops the complete request
            let complete_request = {
                let mut queue = self.queue.lock();
                let Ok((token, _)) = queue.pop_used_with_min_bytes(RESP_SIZE) else {
                    return;
                };
                self.submitted_requests.lock().remove(&token).unwrap()
            };

            self.descriptor_waiters.wake_all();
            self.counters
                .irq_completions
                .fetch_add(1, Ordering::Relaxed);

            // Handles the response
            let id = complete_request.id as usize;
            let resp_slice =
                Slice::new(&self.block_responses, id * RESP_SIZE..(id + 1) * RESP_SIZE);
            resp_slice.sync_from_device().unwrap();
            let resp: BlockResp = resp_slice.read_val(0).unwrap();
            self.id_allocator.dealloc(id);
            match RespStatus::try_from(resp.status) {
                Ok(RespStatus::Ok) => {}
                _ => {
                    // Completes the bio request with an error
                    complete_request.bio_request.into_bios().for_each(|bio| {
                        bio.complete(BioStatus::IoError);
                    });
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
        let id = self.id_allocator.alloc();
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
        let mut queue = self.wait_for_descriptors(inputs.len() + 1);
        let token = queue
            .add_dma_bufs(input_refs.as_slice(), &[&resp_slice])
            .expect("add queue failed");
        drop(input_refs);
        drop(inputs);
        self.finish_submit(
            &mut queue,
            token,
            SubmittedRequest::new(id as u16, bio_request),
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
        transport::{DeviceStatus, VirtioTransportError},
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
    fn queue_diagnostics_count_pages_and_forwarders() {
        let (device, transport) = QueueDevice::new(1, 4);
        let queue = &device.queues[0];
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
        let stats = queue.diagnostics();
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
                assert!(queue.submitted_requests.lock().contains_key(&token));
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
}

impl SubmittedRequest {
    pub fn new(id: u16, bio_request: BioRequest) -> Self {
        Self { id, bio_request }
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
