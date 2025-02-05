// Copyright 2021 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use virtio_sys::virtio_net;
use vmm_vhost::message::VhostUserProtocolFeatures;

use crate::virtio::vhost::user::vmm::Connection;
use crate::virtio::vhost::user::vmm::Result;
use crate::virtio::vhost::user::vmm::VhostUserVirtioDevice;
use crate::virtio::DeviceType;

impl VhostUserVirtioDevice {
    pub fn new_net(
        base_features: u64,
        connection: Connection,
        max_queue_size: Option<u16>,
    ) -> Result<VhostUserVirtioDevice> {
        // 3 = rx, tx, ctrl
        let default_queues = 3;

        let allow_features = 1 << virtio_net::VIRTIO_NET_F_CSUM
            | 1 << virtio_net::VIRTIO_NET_F_CTRL_VQ
            | 1 << virtio_net::VIRTIO_NET_F_CTRL_GUEST_OFFLOADS
            | 1 << virtio_net::VIRTIO_NET_F_GUEST_CSUM
            | 1 << virtio_net::VIRTIO_NET_F_GUEST_TSO4
            | 1 << virtio_net::VIRTIO_NET_F_GUEST_UFO
            | 1 << virtio_net::VIRTIO_NET_F_HOST_TSO4
            | 1 << virtio_net::VIRTIO_NET_F_HOST_UFO
            | 1 << virtio_net::VIRTIO_NET_F_MAC
            | 1 << virtio_net::VIRTIO_NET_F_MQ
            | 1 << virtio_net::VIRTIO_NET_F_MTU;

        let allow_protocol_features =
            VhostUserProtocolFeatures::MQ | VhostUserProtocolFeatures::CONFIG;

        VhostUserVirtioDevice::new(
            connection,
            DeviceType::Net,
            default_queues,
            max_queue_size,
            allow_features,
            allow_protocol_features,
            base_features,
            None,
            false,
        )
    }
}
