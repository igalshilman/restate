// Copyright (c) 2024 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::bail;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::execution::SendableRecordBatchStream;

use restate_core::metadata;
use restate_core::partitions::PartitionNodeResolver;
use restate_types::identifiers::{PartitionId, PartitionKey};
use restate_types::{GenerationalNodeId, NodeId};

use crate::remote_query_scanner_client::{remote_scan_as_datafusion_stream, RemoteScannerService};
use crate::table_providers::ScanPartition;

/// LocalPartitionScannerRegistry is a mapping between a datafusion registered table name
/// (i.e. sys_inbox, sys_status, etc.) to an implementation of a ScanPartition.
/// This registry is populated when we register all the partitioned tables, and it is accessed
/// by the RemoteQueryScannerServer.
#[derive(Clone, Debug, Default)]
struct LocalPartitionScannerRegistry {
    local_store_scanners: Arc<Mutex<BTreeMap<String, Arc<dyn ScanPartition>>>>,
}

impl LocalPartitionScannerRegistry {
    pub fn get(&self, table_name: &str) -> Option<Arc<dyn ScanPartition>> {
        let guard = self
            .local_store_scanners
            .lock()
            .expect("something isn't right");
        guard.get(table_name).cloned()
    }

    fn register(&self, table_name: impl Into<String>, scanner: Arc<dyn ScanPartition>) {
        let mut guard = self
            .local_store_scanners
            .lock()
            .expect("something isn't right");
        guard.insert(table_name.into(), scanner);
    }
}

#[derive(Clone)]
pub struct RemoteScannerManager {
    my_node_id: OnceLock<GenerationalNodeId>,
    partition_node_resolver: PartitionNodeResolver,
    remote_scanner: Arc<dyn RemoteScannerService>,
    local_store_scanners: LocalPartitionScannerRegistry,
}

impl Debug for RemoteScannerManager {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemoteScannerManager")
    }
}

pub enum PartitionLocation {
    Local { scanner: Arc<dyn ScanPartition> },
    Remote { node_id: NodeId },
}

impl RemoteScannerManager {
    pub fn new(
        partition_node_resolver: PartitionNodeResolver,
        remote_scanner: Arc<dyn RemoteScannerService>,
    ) -> Self {
        Self {
            my_node_id: OnceLock::new(),
            partition_node_resolver,
            remote_scanner,
            local_store_scanners: LocalPartitionScannerRegistry::default(),
        }
    }

    #[cfg(test)]
    pub fn with_local_node_id(
        my_node_id: GenerationalNodeId,
        partition_node_resolver: PartitionNodeResolver,
        remote_scanner: Arc<dyn RemoteScannerService>,
    ) -> Self {
        Self {
            my_node_id: OnceLock::from(my_node_id),
            partition_node_resolver,
            remote_scanner,
            local_store_scanners: LocalPartitionScannerRegistry::default(),
        }
    }

    /// Combines the local partition scanner for the given table, with an RPC based partition scanner
    /// this is able to both scan partition hosted at the current node, and remote partitions hosted on
    /// other nodes via RPC.
    pub fn create_distributed_scanner(
        &self,
        table_name: impl Into<String>,
        local_scanner: Option<Arc<dyn ScanPartition>>,
    ) -> impl ScanPartition + Clone {
        let name = table_name.into();

        if let Some(local_scanner) = local_scanner {
            // make the local scanner available to serve a remote RPC.
            // see usages of [[local_partition_scanner]]
            // we use the table_name to associate a remote scanner with its local counterpart.
            self.local_store_scanners
                .register(name.clone(), local_scanner.clone());
        }

        RemotePartitionsScanner::new(self.clone(), name)
    }

    pub fn local_partition_scanner(&self, table: &str) -> Option<Arc<dyn ScanPartition>> {
        self.local_store_scanners.get(table)
    }

    pub fn get_partition_target_node(
        &self,
        table: &str,
        partition_id: PartitionId,
    ) -> anyhow::Result<PartitionLocation> {
        let my_node_id = self.my_node_id();

        match self
            .partition_node_resolver
            .get_partition_location(partition_id)
        {
            None => {
                self.partition_node_resolver.request_refresh();
                bail!("node lookup for partition {} failed", partition_id)
            }
            Some(node_id)
                if node_id
                    .as_generational()
                    .is_some_and(|id| id == *my_node_id) =>
            {
                Ok(PartitionLocation::Local {
                    scanner: self
                        .local_partition_scanner(table)
                        .expect("should be present"),
                })
            }
            Some(node_id) => Ok(PartitionLocation::Remote { node_id }),
        }
    }

    // Note: we initialize my_node_id lazily as it is not available at construction time. We should
    // be careful since the call to metadata().my_node_id() will panic if the system configuration
    // isn't fully initialized yet, but we shouldn't be able to accept queries until that is in fact
    // the case. For testing, set the node id explicitly with new_with_fixed_node_id.
    #[inline]
    fn my_node_id(&self) -> &GenerationalNodeId {
        self.my_node_id.get_or_init(|| metadata().my_node_id())
    }
}

// ----- remote partition scanner -----

#[derive(Clone, Debug)]
pub struct RemotePartitionsScanner {
    manager: RemoteScannerManager,
    table_name: String,
}

impl RemotePartitionsScanner {
    pub fn new(manager: RemoteScannerManager, table: impl Into<String>) -> Self {
        Self {
            manager,
            table_name: table.into(),
        }
    }
}

impl ScanPartition for RemotePartitionsScanner {
    fn scan_partition(
        &self,
        partition_id: PartitionId,
        range: RangeInclusive<PartitionKey>,
        projection: SchemaRef,
    ) -> anyhow::Result<SendableRecordBatchStream> {
        match self
            .manager
            .get_partition_target_node(&self.table_name, partition_id)?
        {
            PartitionLocation::Local { scanner } => {
                Ok(scanner.scan_partition(partition_id, range, projection)?)
            }
            PartitionLocation::Remote { node_id } => Ok(remote_scan_as_datafusion_stream(
                self.manager.remote_scanner.clone(),
                node_id,
                partition_id,
                range,
                self.table_name.clone(),
                projection,
            )),
        }
    }
}
