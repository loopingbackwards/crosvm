// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![deny(missing_docs)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Context;
use base::error;
use base::warn;
use base::Event;
use base::WorkerThread;
use cros_async::sync::RwLock;
use cros_async::EventAsync;
use cros_async::Executor;
use cros_async::ExecutorKind;
use disk::AsyncDisk;
use disk::DiskFile;
use futures::pin_mut;
use futures::stream::FuturesUnordered;
use futures::FutureExt;
use futures::StreamExt;
use remain::sorted;
use thiserror::Error as ThisError;
use virtio_sys::virtio_scsi::virtio_scsi_cmd_req;
use virtio_sys::virtio_scsi::virtio_scsi_cmd_resp;
use virtio_sys::virtio_scsi::virtio_scsi_config;
use virtio_sys::virtio_scsi::virtio_scsi_event;
use virtio_sys::virtio_scsi::VIRTIO_SCSI_CDB_DEFAULT_SIZE;
use virtio_sys::virtio_scsi::VIRTIO_SCSI_SENSE_DEFAULT_SIZE;
use virtio_sys::virtio_scsi::VIRTIO_SCSI_S_BAD_TARGET;
use virtio_sys::virtio_scsi::VIRTIO_SCSI_S_OK;
use vm_memory::GuestMemory;
use zerocopy::AsBytes;

use crate::virtio::async_utils;
use crate::virtio::block::sys::get_seg_max;
use crate::virtio::copy_config;
use crate::virtio::scsi::commands::Command;
use crate::virtio::scsi::constants::CHECK_CONDITION;
use crate::virtio::scsi::constants::GOOD;
use crate::virtio::scsi::constants::ILLEGAL_REQUEST;
use crate::virtio::scsi::constants::MEDIUM_ERROR;
use crate::virtio::DescriptorChain;
use crate::virtio::DeviceType as VirtioDeviceType;
use crate::virtio::Interrupt;
use crate::virtio::Queue;
use crate::virtio::Reader;
use crate::virtio::VirtioDevice;
use crate::virtio::Writer;

// The following values reflects the virtio v1.2 spec:
// <https://docs.oasis-open.org/virtio/virtio/v1.2/csd01/virtio-v1.2-csd01.html#x1-3470004>

// Should have one controlq, one eventq, and at least one request queue.
const MINIMUM_NUM_QUEUES: usize = 3;
// Max channel should be 0.
const DEFAULT_MAX_CHANNEL: u16 = 0;
// Max target should be less than or equal to 255.
const DEFAULT_MAX_TARGET: u16 = 255;
// Max lun should be less than or equal to 16383
const DEFAULT_MAX_LUN: u32 = 16383;

const DEFAULT_QUEUE_SIZE: u16 = 256;

// The maximum number of linked commands.
const MAX_CMD_PER_LUN: u32 = 128;
// We set the maximum transfer size hint to 0xffff: 2^16 * 512 ~ 34mb.
const MAX_SECTORS: u32 = 0xffff;

const fn virtio_scsi_cmd_resp_ok() -> virtio_scsi_cmd_resp {
    virtio_scsi_cmd_resp {
        sense_len: 0,
        resid: 0,
        status_qualifier: 0,
        status: GOOD,
        response: VIRTIO_SCSI_S_OK as u8,
        sense: [0; VIRTIO_SCSI_SENSE_DEFAULT_SIZE as usize],
    }
}

/// Errors that happen while handling scsi commands.
#[sorted]
#[derive(ThisError, Debug)]
pub enum ExecuteError {
    #[error("invalid cdb field")]
    InvalidField,
    #[error("{length} bytes from sector {sector} exceeds end of this device {max_lba}")]
    LbaOutOfRange {
        length: usize,
        sector: u64,
        max_lba: u64,
    },
    #[error("failed to read message: {0}")]
    Read(io::Error),
    #[error("failed to read command from cdb")]
    ReadCommand,
    #[error("io error {resid} bytes remained to be read: {desc_error}")]
    ReadIo {
        resid: usize,
        desc_error: disk::Error,
    },
    #[error("writing to a read only device")]
    ReadOnly,
    #[error("unsupported scsi command: {0}")]
    Unsupported(u8),
    #[error("failed to write message: {0}")]
    Write(io::Error),
    #[error("io error {resid} bytes remained to be written: {desc_error}")]
    WriteIo {
        resid: usize,
        desc_error: disk::Error,
    },
}

impl ExecuteError {
    // TODO(b/301011017): We would need to define something like
    // virtio_scsi_cmd_resp_header to cope with the configurable sense size.
    fn as_resp(&self) -> virtio_scsi_cmd_resp {
        let resp = virtio_scsi_cmd_resp_ok();
        // The asc and ascq assignments are taken from the t10 SPC spec.
        // cf) Table 28 of <https://www.t10.org/cgi-bin/ac.pl?t=f&f=spc3r23.pdf>
        let sense = match self {
            Self::Read(_) | Self::ReadCommand => {
                // UNRECOVERED READ ERROR
                Sense {
                    key: MEDIUM_ERROR,
                    asc: 0x11,
                    ascq: 0x00,
                }
            }
            Self::Write(_) => {
                // WRITE ERROR
                Sense {
                    key: MEDIUM_ERROR,
                    asc: 0x0c,
                    ascq: 0x00,
                }
            }
            Self::InvalidField => {
                // INVALID FIELD IN CDB
                Sense {
                    key: ILLEGAL_REQUEST,
                    asc: 0x24,
                    ascq: 0x00,
                }
            }
            Self::Unsupported(_) => {
                // INVALID COMMAND OPERATION CODE
                Sense {
                    key: ILLEGAL_REQUEST,
                    asc: 0x20,
                    ascq: 0x00,
                }
            }
            Self::ReadOnly | Self::LbaOutOfRange { .. } => {
                // LOGICAL BLOCK ADDRESS OUT OF RANGE
                Sense {
                    key: ILLEGAL_REQUEST,
                    asc: 0x21,
                    ascq: 0x00,
                }
            }
            // Ignore these errors.
            Self::ReadIo { resid, desc_error } | Self::WriteIo { resid, desc_error } => {
                warn!("error while performing I/O {}", desc_error);
                return virtio_scsi_cmd_resp {
                    resid: (*resid).try_into().unwrap_or(u32::MAX).to_be(),
                    ..resp
                };
            }
        };
        let (sense, sense_len) = sense.as_bytes(true);
        virtio_scsi_cmd_resp {
            sense_len,
            sense,
            status: CHECK_CONDITION,
            ..resp
        }
    }
}

/// Sense code representation
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Sense {
    /// Provides generic information describing an error or exception condition.
    pub key: u8,
    /// Additional Sense Code.
    /// Indicates further information related to the error or exception reported in the key field.
    pub asc: u8,
    /// Additional Sense Code Qualifier.
    /// Indicates further detailed information related to the additional sense code.
    pub ascq: u8,
}

impl Sense {
    // Converts to (sense bytes, actual size of the sense data)
    // There are two formats to convert sense data to bytes; fixed format and descriptor format.
    // Details are in SPC-3 t10 revision 23: <https://www.t10.org/cgi-bin/ac.pl?t=f&f=spc3r23.pdf>
    fn as_bytes(&self, fixed: bool) -> ([u8; VIRTIO_SCSI_SENSE_DEFAULT_SIZE as usize], u32) {
        let mut sense_data = [0u8; VIRTIO_SCSI_SENSE_DEFAULT_SIZE as usize];
        if fixed {
            // Fixed format sense data has response code:
            // 1) 0x70 for current errors
            // 2) 0x71 for deferred errors
            sense_data[0] = 0x70;
            // sense_data[1]: Obsolete
            // Sense key
            sense_data[2] = self.key;
            // sense_data[3..7]: Information field, which we do not support.
            // Additional length. The data is 18 bytes, and this byte is 8th.
            sense_data[7] = 10;
            // sense_data[8..12]: Command specific information, which we do not support.
            // Additional sense code
            sense_data[12] = self.asc;
            // Additional sense code qualifier
            sense_data[13] = self.ascq;
            // sense_data[14]: Field replaceable unit code, which we do not support.
            // sense_data[15..18]: Field replaceable unit code, which we do not support.
            (sense_data, 18)
        } else {
            // Descriptor format sense data has response code:
            // 1) 0x72 for current errors
            // 2) 0x73 for deferred errors
            sense_data[0] = 0x72;
            // Sense key
            sense_data[1] = self.key;
            // Additional sense code
            sense_data[2] = self.asc;
            // Additional sense code qualifier
            sense_data[3] = self.ascq;
            // sense_data[4..7]: Reserved
            // sense_data[7]: Additional sense length, which is 0 in this case.
            (sense_data, 8)
        }
    }
}

/// Describes each SCSI device.
#[derive(Copy, Clone)]
pub struct LogicalUnit {
    /// The maximum logical block address of the target device.
    pub max_lba: u64,
    /// Block size of the target device.
    pub block_size: u32,
    pub read_only: bool,
}

/// Vitio device for exposing SCSI command operations on a host file.
pub struct Device {
    // Bitmap of virtio-scsi feature bits.
    avail_features: u64,
    // Represents the image on disk.
    disk_image: Option<Box<dyn DiskFile>>,
    // Sizes for the virtqueue.
    queue_sizes: Vec<u16>,
    // The maximum number of segments that can be in a command.
    seg_max: u32,
    // The size of the sense data.
    sense_size: u32,
    // The byte size of the CDB that the driver will write.
    cdb_size: u32,
    executor_kind: ExecutorKind,
    worker_threads: Vec<WorkerThread<()>>,
    // TODO(b/300586438): Make this a BTreeMap<_> to enable this Device struct to manage multiple
    // LogicalUnit. That is, when user passes multiple --scsi-block options, we will have a single
    // instance of Device which has multiple LogicalUnit.
    #[allow(dead_code)]
    target: Arc<RwLock<LogicalUnit>>,
}

impl Device {
    /// Creates a virtio-scsi device.
    pub fn new(
        disk_image: Box<dyn DiskFile>,
        base_features: u64,
        block_size: u32,
        read_only: bool,
    ) -> anyhow::Result<Self> {
        let target = LogicalUnit {
            max_lba: disk_image
                .get_len()
                .context("Failed to get the length of the disk image")?,
            block_size,
            read_only,
        };
        // b/300560198: Support feature bits in virtio-scsi.
        Ok(Self {
            avail_features: base_features,
            disk_image: Some(disk_image),
            queue_sizes: vec![DEFAULT_QUEUE_SIZE; MINIMUM_NUM_QUEUES],
            seg_max: get_seg_max(DEFAULT_QUEUE_SIZE),
            sense_size: VIRTIO_SCSI_SENSE_DEFAULT_SIZE,
            cdb_size: VIRTIO_SCSI_CDB_DEFAULT_SIZE,
            executor_kind: ExecutorKind::default(),
            worker_threads: vec![],
            target: Arc::new(RwLock::new(target)),
        })
    }

    fn build_config_space(&self) -> virtio_scsi_config {
        virtio_scsi_config {
            // num_queues is the number of request queues only so we subtract 2 for the control
            // queue and the event queue.
            num_queues: self.queue_sizes.len() as u32 - 2,
            seg_max: self.seg_max,
            max_sectors: MAX_SECTORS,
            cmd_per_lun: MAX_CMD_PER_LUN,
            event_info_size: std::mem::size_of::<virtio_scsi_event>() as u32,
            sense_size: self.sense_size,
            cdb_size: self.cdb_size,
            max_channel: DEFAULT_MAX_CHANNEL,
            max_target: DEFAULT_MAX_TARGET,
            max_lun: DEFAULT_MAX_LUN,
        }
    }

    async fn execute_request(
        reader: &mut Reader,
        resp_writer: &mut Writer,
        data_writer: &mut Writer,
        disk_image: &dyn AsyncDisk,
        dev: &Arc<RwLock<LogicalUnit>>,
    ) -> Result<usize, ExecuteError> {
        // TODO(b/301011017): Cope with the configurable cdb size. We would need to define
        // something like virtio_scsi_cmd_req_header.
        let req_header = reader
            .read_obj::<virtio_scsi_cmd_req>()
            .map_err(ExecuteError::Read)?;
        let resp = if Self::is_lun0(req_header.lun) {
            let command = Command::new(&req_header.cdb)?;
            match command
                .execute(reader, data_writer, Arc::clone(dev), disk_image)
                .await
            {
                Ok(()) => virtio_scsi_cmd_resp {
                    sense_len: 0,
                    resid: 0,
                    status_qualifier: 0,
                    status: GOOD,
                    response: VIRTIO_SCSI_S_OK as u8,
                    sense: [0; VIRTIO_SCSI_SENSE_DEFAULT_SIZE as usize],
                },
                Err(err) => {
                    error!("error while executing a scsi request: {err}");
                    err.as_resp()
                }
            }
        } else {
            virtio_scsi_cmd_resp {
                response: VIRTIO_SCSI_S_BAD_TARGET as u8,
                ..Default::default()
            }
        };
        resp_writer
            .write_all(resp.as_bytes())
            .map_err(ExecuteError::Write)?;
        Ok(resp_writer.bytes_written())
    }

    // TODO(b/300586438): Once we alter Device to handle multiple LogicalUnit, we should update
    // the search strategy as well.
    fn is_lun0(lun: [u8; 8]) -> bool {
        // First byte should be 1.
        if lun[0] != 1 {
            return false;
        }
        let bus_id = lun[1];
        // General search strategy for scsi devices is as follows:
        // 1) Look for a device which has the same bus id and lun indicated by the given lun. If
        //    there is one, that is the target device.
        // 2) If we cannot find such device, then we return the first device that has the same bus
        //    id.
        // Since we only support LUN0 for now, we only need to compare the bus id.
        bus_id == 0
    }
}

impl VirtioDevice for Device {
    fn keep_rds(&self) -> Vec<base::RawDescriptor> {
        self.disk_image
            .as_ref()
            .map(|i| i.as_raw_descriptors())
            .unwrap_or_default()
    }

    fn features(&self) -> u64 {
        self.avail_features
    }

    fn device_type(&self) -> VirtioDeviceType {
        VirtioDeviceType::Scsi
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let config_space = self.build_config_space();
        copy_config(data, 0, config_space.as_bytes(), offset);
    }

    // TODO(b/301011017): implement the write_config method to make spec values writable from the
    // guest driver.

    fn activate(
        &mut self,
        _mem: GuestMemory,
        interrupt: Interrupt,
        queues: BTreeMap<usize, Queue>,
    ) -> anyhow::Result<()> {
        let executor_kind = self.executor_kind;
        let dev = Arc::clone(&self.target);
        let disk_image = self
            .disk_image
            .take()
            .context("Failed to take a disk image")?;
        let worker_thread = WorkerThread::start("virtio_scsi", move |kill_evt| {
            let ex =
                Executor::with_executor_kind(executor_kind).expect("Failed to create an executor");
            let async_disk = match disk_image.to_async_disk(&ex) {
                Ok(d) => d,
                Err(e) => panic!("Failed to create async disk: {}", e),
            };
            if let Err(err) = ex
                .run_until(run_worker(
                    &ex, interrupt, queues, kill_evt, async_disk, dev,
                ))
                .expect("run_until failed")
            {
                error!("run_worker failed: {err}");
            }
        });
        self.worker_threads.push(worker_thread);
        Ok(())
    }
}

async fn run_worker(
    ex: &Executor,
    interrupt: Interrupt,
    mut queues: BTreeMap<usize, Queue>,
    kill_evt: Event,
    disk_image: Box<dyn AsyncDisk>,
    dev: Arc<RwLock<LogicalUnit>>,
) -> anyhow::Result<()> {
    let kill = async_utils::await_and_exit(ex, kill_evt).fuse();
    pin_mut!(kill);

    let resample = async_utils::handle_irq_resample(ex, interrupt.clone()).fuse();
    pin_mut!(resample);

    let request_queue = queues
        .remove(&2)
        .context("request queue should be present")?;
    let kick_evt = request_queue
        .event()
        .try_clone()
        .expect("Failed to clone queue event");
    let queue_handler = handle_queue(
        Rc::new(RefCell::new(request_queue)),
        EventAsync::new(kick_evt, ex).expect("Failed to create async event for queue"),
        interrupt.clone(),
        disk_image,
        dev,
    )
    .fuse();
    pin_mut!(queue_handler);

    futures::select! {
        _ = queue_handler => anyhow::bail!("queue handler exited unexpectedly"),
        r = resample => return r.context("failed to resample an irq value"),
        r = kill => return r.context("failed to wait on the kill event"),
    };
}

async fn handle_queue(
    queue: Rc<RefCell<Queue>>,
    evt: EventAsync,
    interrupt: Interrupt,
    disk_image: Box<dyn AsyncDisk>,
    dev: Arc<RwLock<LogicalUnit>>,
) {
    let mut background_tasks = FuturesUnordered::new();
    let evt_future = evt.next_val().fuse();
    pin_mut!(evt_future);
    loop {
        futures::select! {
            _ = background_tasks.next() => continue,
            res = evt_future => {
                evt_future.set(evt.next_val().fuse());
                if let Err(e) = res {
                    error!("Failed to read the next queue event: {e}");
                    continue;
                }
            }
        }
        while let Some(chain) = queue.borrow_mut().pop() {
            background_tasks.push(process_one_chain(
                &queue,
                chain,
                &interrupt,
                &*disk_image,
                &dev,
            ));
        }
    }
}

async fn process_one_chain(
    queue: &RefCell<Queue>,
    mut avail_desc: DescriptorChain,
    interrupt: &Interrupt,
    disk_image: &dyn AsyncDisk,
    dev: &Arc<RwLock<LogicalUnit>>,
) {
    let len = process_one_request(&mut avail_desc, disk_image, dev).await;
    let mut queue = queue.borrow_mut();
    queue.add_used(avail_desc, len as u32);
    queue.trigger_interrupt(interrupt);
}

async fn process_one_request(
    avail_desc: &mut DescriptorChain,
    disk_image: &dyn AsyncDisk,
    dev: &Arc<RwLock<LogicalUnit>>,
) -> usize {
    let reader = &mut avail_desc.reader;
    let resp_writer = &mut avail_desc.writer;
    let mut data_writer = resp_writer.split_at(std::mem::size_of::<virtio_scsi_cmd_resp>());
    if let Err(err) =
        Device::execute_request(reader, resp_writer, &mut data_writer, disk_image, dev).await
    {
        // If the write of the virtio_scsi_cmd_resp fails, there is nothing we can do to inform
        // the error to the guest driver (we usually propagate errors with sense field, which
        // is in the struct virtio_scsi_cmd_resp). The guest driver should have at least
        // sizeof(virtio_scsi_cmd_resp) bytes of device-writable part regions. For now we
        // simply emit an error message.
        if let Err(e) = resp_writer.write_all(err.as_resp().as_bytes()) {
            error!("failed to write response: {e}");
        }
    }
    resp_writer.bytes_written() + data_writer.bytes_written()
}
