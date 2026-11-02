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
    sync::atomic::{AtomicU32, Ordering},
};

use aster_block::{
    BlockDeviceMeta, EXTENDED_DEVICE_ID_ALLOCATOR, PartitionInfo, PartitionNode,
    bio::{BioEnqueueError, BioStatus, BioType, SubmittedBio, bio_segment_pool_init},
    request_queue::{BioRequest, BioRequestSingleQueue},
};
use aster_util::mem_obj_slice::Slice;
use device_id::{DeviceId, MinorId};
use ostd::{
    arch::trap::TrapFrame,
    debug, info,
    mm::{PAGE_SIZE, VmIo, dma::DmaStream},
    sync::SpinLock,
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
    /// The software staging queue.
    queue: BioRequestSingleQueue,
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

        let max_segments = usize::from(device.queue_size - 2).min(62);
        let block_device = Arc::new_cyclic(|weak_self| BlockDevice {
            device,
            // Keep the existing BIO aggregation bound independent of queue depth.
            // Reserve a header and status descriptor even for fully fragmented DMA.
            queue: BioRequestSingleQueue::with_max_nr_segments_per_bio(max_segments),
            id,
            name,
            partitions: SpinLock::new(None),
            weak_self: weak_self.clone(),
        });

        aster_block::register(block_device).unwrap();

        bio_segment_pool_init();
        Ok(())
    }

    /// Dequeues a `BioRequest` from the software staging queue and
    /// processes the request.
    pub fn handle_requests(&self) {
        let request = self.queue.dequeue();
        info!("Handle Request: {:?}", request);
        match request.type_() {
            BioType::Read => self.device.read(request),
            BioType::Write => self.device.write(request),
            BioType::Flush => self.device.flush(request),
        }
    }

    /// Negotiate features for the device specified bits 0~23
    pub(crate) fn negotiate_features(device_features: u64) -> u64 {
        BlockFeatures::negotiated_with_device(device_features).bits()
    }
}

impl aster_block::BlockDevice for BlockDevice {
    fn enqueue(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        self.queue.enqueue(bio)
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: self.queue.max_nr_segments_per_bio(),
            nr_sectors: self.device.config_manager.capacity_sectors(),
            nr_submission_queues: 1,
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
    queue: SpinLock<VirtQueue>,
    queue_size: u16,
    transport: SpinLock<Box<dyn VirtioTransport>>,
    block_requests: Arc<DmaStream>,
    block_responses: Arc<DmaStream>,
    id_allocator: SyncIdAlloc,
    submitted_requests: SpinLock<BTreeMap<u16, SubmittedRequest>>,
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
        let queue_size = queue_sizes[0];
        let queue = VirtQueue::new(0, queue_size, transport.as_mut())?;

        // Every request ID needs its own header and response until completion.
        let block_requests = Arc::new(
            DmaStream::alloc((queue_size as usize * REQ_SIZE).div_ceil(PAGE_SIZE), false)
                .map_err(VirtioDeviceError::ResourceAlloc)?,
        );
        let block_responses = Arc::new(
            DmaStream::alloc((queue_size as usize * RESP_SIZE).div_ceil(PAGE_SIZE), false)
                .map_err(VirtioDeviceError::ResourceAlloc)?,
        );

        let device = Arc::new(Self {
            config_manager,
            features,
            queue: SpinLock::new(queue),
            queue_size,
            transport: SpinLock::new(transport),
            block_requests,
            block_responses,
            id_allocator: SyncIdAlloc::with_capacity(queue_size as usize),
            submitted_requests: SpinLock::new(BTreeMap::new()),
        });

        let cloned_device = device.clone();
        let handle_irq = move |_: &TrapFrame| {
            cloned_device.handle_irq();
        };

        let cloned_device = device.clone();
        let handle_config_change = move |_: &TrapFrame| {
            cloned_device.handle_config_change();
        };

        {
            let mut transport = device.transport.lock();
            transport.register_cfg_callback(Box::new(handle_config_change))?;
            transport.register_queue_callback(0, Box::new(handle_irq), false)?;
            transport.finish_init();
        }

        Ok(device)
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

    fn handle_config_change(&self) {
        info!("block device config space change");
    }

    /// Reads data from the device, this function is non-blocking.
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
        let num_used_descs = outputs.len() + 1;
        // FIXME: Split the request if it is too big
        if num_used_descs > self.queue_size as usize {
            panic!("The request size surpasses the queue size");
        }

        loop {
            let mut queue = self.queue.disable_irq().lock();
            if num_used_descs > queue.available_desc() {
                continue;
            }
            let token = queue
                .add_dma_bufs(&[&req_slice], output_refs.as_slice())
                .expect("add queue failed");
            drop(output_refs);
            drop(outputs);
            if queue.should_notify() {
                queue.notify();
            }

            // Records the submitted request
            let submitted_request = SubmittedRequest::new(id as u16, bio_request);
            self.submitted_requests
                .disable_irq()
                .lock()
                .insert(token, submitted_request);
            return;
        }
    }

    /// Writes data to the device, this function is non-blocking.
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
        let num_used_descs = inputs.len() + 1;
        // FIXME: Split the request if it is too big
        if num_used_descs > self.queue_size as usize {
            panic!("The request size surpasses the queue size");
        }
        loop {
            let mut queue = self.queue.disable_irq().lock();
            if num_used_descs > queue.available_desc() {
                continue;
            }
            let token = queue
                .add_dma_bufs(input_refs.as_slice(), &[&resp_slice])
                .expect("add queue failed");
            drop(input_refs);
            drop(inputs);
            if queue.should_notify() {
                queue.notify();
            }

            // Records the submitted request
            let submitted_request = SubmittedRequest::new(id as u16, bio_request);
            self.submitted_requests
                .disable_irq()
                .lock()
                .insert(token, submitted_request);
            return;
        }
    }

    /// Flushes any cached data from the guest to the persistent storage on the host.
    /// This will be ignored if the device doesn't support the `VIRTIO_BLK_F_FLUSH` feature.
    fn flush(&self, bio_request: BioRequest) {
        if !self.features.contains(BlockFeatures::FLUSH) {
            bio_request.into_bios().for_each(|bio| {
                bio.complete(BioStatus::Complete);
            });
            return;
        }

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

        // One descriptor for the input `req_slice`, one for the output `resp_slice`.
        let num_used_descs = 2;
        loop {
            let mut queue = self.queue.disable_irq().lock();
            if num_used_descs > queue.available_desc() {
                continue;
            }
            let token = queue
                .add_dma_bufs(&[&req_slice], &[&resp_slice])
                .expect("add queue failed");
            if queue.should_notify() {
                queue.notify();
            }

            // Records the submitted request
            let submitted_request = SubmittedRequest::new(id as u16, bio_request);
            self.submitted_requests
                .disable_irq()
                .lock()
                .insert(token, submitted_request);
            return;
        }
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

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
