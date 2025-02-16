// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

mod header_map;
#[cfg(feature = "proto")]
mod proto;
mod uuid;

pub mod default;

pub use crate::uuid::{SerdeableUuid, UuidAsStringOrBytesOrBase64};
pub use header_map::SerdeableHeaderHashMap;
#[cfg(feature = "proto")]
pub use proto::ProtobufEncoded;
