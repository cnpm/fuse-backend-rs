// Copyright (C) 2020 Alibaba Cloud. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Fuse Application Binary Interfaces(ABI).

/// Linux Fuse Application Binary Interfaces.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod linux_abi;

#[cfg(feature = "virtiofs")]
pub mod virtio_fs;
