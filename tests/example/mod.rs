// Copyright 2021 Ant Group. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

#[cfg(all(feature = "fusedev", not(feature = "virtiofs")))]
pub(crate) mod passthroughfs;

#[cfg(feature = "macfuse")]
pub(crate) mod macfuse;
