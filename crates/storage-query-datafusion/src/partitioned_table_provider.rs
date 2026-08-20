// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Physical planning for partition-routed DataFusion tables.

use std::sync::Arc;

use async_trait::async_trait;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::common::{DataFusionError, Statistics};
use datafusion::datasource::{TableProvider, TableType};
use datafusion::logical_expr::{Expr, TableProviderFilterPushDown};
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_plan::empty::EmptyExec;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::{ExecutionPlan, Partitioning, PlanProperties};

use restate_types::identifiers::PartitionId;
use restate_types::partition_table::Partition;
use restate_types::sharding::KeyRange;

use crate::context::SelectPartitions;
use crate::filter::{FirstMatchingPartitionKeyExtractor, PointReadFanout};
use crate::partition_planning::{PartitionLocation, plan_partitions_by_location};
use crate::partitioned_scan::{LocationAwareScanExec, PartitionScanExec, RemoteNodeExec};
use crate::table_providers::DistributedPartitionScanner;
use crate::table_util::{find_sort_columns, make_ordering};

/// Builds a physical scan whose branches encode each selected partition's
/// planned local or remote placement.
#[derive(Debug)]
pub(crate) struct PartitionedTableProvider<S> {
    partition_selector: S,
    schema: SchemaRef,
    ordering: Vec<String>,
    partition_scanner: Arc<dyn DistributedPartitionScanner>,
    partition_key_extractor: FirstMatchingPartitionKeyExtractor,
    statistics: Statistics,
}

impl<S> PartitionedTableProvider<S> {
    pub(crate) fn new<T: DistributedPartitionScanner>(
        partition_selector: S,
        schema: SchemaRef,
        ordering: Vec<String>,
        partition_scanner: T,
        partition_key_extractor: FirstMatchingPartitionKeyExtractor,
    ) -> Self {
        let statistics = Statistics::new_unknown(&schema);
        Self {
            partition_selector,
            schema,
            ordering,
            partition_scanner: Arc::new(partition_scanner),
            partition_key_extractor,
            statistics,
        }
    }

    pub(crate) fn with_statistics(self, statistics: Statistics) -> Self {
        Self { statistics, ..self }
    }
}

#[async_trait]
impl<S> TableProvider for PartitionedTableProvider<S>
where
    S: SelectPartitions,
{
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn datafusion::catalog::Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> datafusion::common::Result<Arc<dyn ExecutionPlan>> {
        let projected_schema = match projection {
            Some(projection) => SchemaRef::new(self.schema.project(projection)?),
            None => self.schema.clone(),
        };

        // Inexact filter pushdown keeps every filter column in the projected
        // schema, so all filters can be planned against that single schema.
        let filters = filters
            .iter()
            .map(|filter| {
                let filter =
                    datafusion::physical_expr::planner::logical2physical(filter, &projected_schema);
                // Column indices should already be correct, but DataFusion can
                // produce stale indices. Names are unambiguous in these tables.
                datafusion::physical_expr::utils::reassign_expr_columns(filter, &projected_schema)
            })
            .collect::<datafusion::common::Result<Vec<_>>>()?;

        let partition_key_selection = self
            .partition_key_extractor
            .try_extract_selection(&filters)
            .map_err(|error| DataFusionError::External(error.into()))?;
        let predicate = datafusion::physical_expr::conjunction_opt(filters);

        let physical_partitions =
            self.partition_selector
                .get_live_partitions()
                .await
                .map_err(DataFusionError::External)?
                .into_iter()
                .flat_map(|(partition_id, partition)| match &partition_key_selection {
                    None => itertools::Either::Left(Some((partition_id, partition)).into_iter()),
                    // Bound fan-out by grouping point reads into one key range per
                    // Restate partition when requested or when the set is large.
                    Some(selection)
                        if selection.fanout == PointReadFanout::PerPartition
                            || selection.keys.len() > 4096 =>
                    {
                        let mut keys = selection.keys.range(partition.key_range).copied();
                        let selected = keys.next().map(|first| {
                            let last = keys.next_back().unwrap_or(first);
                            (
                                partition_id,
                                Partition::new(partition_id, KeyRange::new(first, last)),
                            )
                        });
                        itertools::Either::Left(selected.into_iter())
                    }
                    // Keep smaller point-read sets independent so DataFusion can
                    // distribute them across execution lanes.
                    Some(selection) => itertools::Either::Right(
                        selection.keys.range(partition.key_range).copied().map(
                            move |partition_key| {
                                (
                                    partition_id,
                                    Partition::new(
                                        partition_id,
                                        KeyRange::new(partition_key, partition_key),
                                    ),
                                )
                            },
                        ),
                    ),
                })
                .collect::<Vec<(PartitionId, Partition)>>();

        let located_partitions = plan_partitions_by_location(
            physical_partitions,
            state.config().target_partitions(),
            |partition_id| self.partition_scanner.partition_location(partition_id),
        )
        .map_err(|error| DataFusionError::External(error.into()))?;

        if located_partitions.is_empty() {
            return Ok(Arc::new(EmptyExec::new(projected_schema)));
        }

        let sort_columns = find_sort_columns(&self.ordering, &projected_schema);
        let eq_properties = if sort_columns.is_empty() {
            EquivalenceProperties::new(projected_schema.clone())
        } else {
            EquivalenceProperties::new_with_orderings(
                projected_schema.clone(),
                [make_ordering(sort_columns)],
            )
        };

        let statistics = Arc::new(self.statistics.clone().project(projection));
        let branch_statistics = if located_partitions.len() == 1 {
            Arc::clone(&statistics)
        } else {
            Arc::new(Statistics::new_unknown(&projected_schema))
        };
        let mut inputs = Vec::with_capacity(located_partitions.len());
        for (location, logical_partitions) in located_partitions {
            let plan = PlanProperties::new(
                eq_properties.clone(),
                Partitioning::UnknownPartitioning(logical_partitions.len()),
                EmissionType::Incremental,
                Boundedness::Bounded,
            )
            .with_scheduling_type(
                datafusion::physical_plan::execution_plan::SchedulingType::Cooperative,
            );
            let scan = PartitionScanExec::new(
                logical_partitions,
                projected_schema.clone(),
                limit,
                predicate.clone(),
                Arc::clone(&self.partition_scanner),
                plan,
                Arc::clone(&branch_statistics),
            );

            inputs.push(match location {
                PartitionLocation::Local => Arc::new(scan) as Arc<dyn ExecutionPlan>,
                PartitionLocation::Remote(node_id) => {
                    Arc::new(RemoteNodeExec::new(node_id, scan)) as Arc<dyn ExecutionPlan>
                }
            });
        }

        match inputs.len() {
            1 => Ok(inputs.pop().expect("one scan input")),
            _ => LocationAwareScanExec::try_new(inputs, statistics),
        }
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> datafusion::common::Result<Vec<TableProviderFilterPushDown>> {
        // Inexact pushdown retains a coordinator FilterExec and ensures its
        // columns remain available in the scan projection.
        Ok(filters
            .iter()
            .map(|_| TableProviderFilterPushDown::Inexact)
            .collect())
    }
}
