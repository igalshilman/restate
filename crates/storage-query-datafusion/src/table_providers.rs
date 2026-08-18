// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.
use std::fmt::{self, Debug, Display, Formatter};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::common::{DataFusionError, Statistics};
use datafusion::datasource::{TableProvider, TableType};
use datafusion::execution::context::TaskContext;
use datafusion::logical_expr::{Expr, TableProviderFilterPushDown};
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::filter_pushdown::{
    FilterPushdownPhase, FilterPushdownPropagation, PushedDown,
};
use datafusion::physical_plan::metrics::{
    BaselineMetrics, ExecutionPlanMetricsSet, MetricsSet, Time,
};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PhysicalExpr, PlanProperties,
    SendableRecordBatchStream,
};
use datafusion::physical_plan::{empty::EmptyExec, union::UnionExec};
use futures::stream::{self, Stream, StreamExt, TryStreamExt};

use restate_types::NodeId;
use restate_types::identifiers::PartitionId;
use restate_types::partition_table::Partition;
use restate_types::sharding::KeyRange;

use crate::context::SelectPartitions;
use crate::filter::{FirstMatchingPartitionKeyExtractor, PointReadFanout};
use crate::partial_aggregation::PartialAggregateFragment;
use crate::table_util::{find_sort_columns, make_ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionLocation {
    Local,
    Remote { node_id: NodeId },
}

pub trait ScanPartition: Send + Sync + Debug + 'static {
    /// Resolves where a partition will be scanned while the physical plan is built.
    ///
    /// Implementations that only scan local data can use the default. Distributed
    /// scanners override this and must treat the returned location as fixed for the
    /// lifetime of the physical plan.
    fn partition_location(&self, _partition_id: PartitionId) -> anyhow::Result<PartitionLocation> {
        Ok(PartitionLocation::Local)
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_partition(
        &self,
        partition_id: PartitionId,
        range: KeyRange,
        projection: SchemaRef,
        predicate: Option<Arc<dyn PhysicalExpr>>,
        batch_size: usize,
        limit: Option<usize>,
        elapsed_compute: Time,
    ) -> anyhow::Result<SendableRecordBatchStream>;

    /// Scans a partition at the location selected by [`Self::partition_location`].
    ///
    /// The default is suitable for local-only scanners. A distributed scanner must
    /// override this method so execution cannot silently choose a different owner.
    #[allow(clippy::too_many_arguments)]
    fn scan_partition_at(
        &self,
        location: PartitionLocation,
        partition_id: PartitionId,
        range: KeyRange,
        projection: SchemaRef,
        predicate: Option<Arc<dyn PhysicalExpr>>,
        batch_size: usize,
        limit: Option<usize>,
        elapsed_compute: Time,
    ) -> anyhow::Result<SendableRecordBatchStream> {
        if location != PartitionLocation::Local {
            anyhow::bail!("local scanner cannot execute a remote partition");
        }
        self.scan_partition(
            partition_id,
            range,
            projection,
            predicate,
            batch_size,
            limit,
            elapsed_compute,
        )
    }

    /// Scans a partition and produces partial aggregate state. Distributed
    /// scanners override this to negotiate execution on the selected node. The
    /// default executes the same fragment locally over the raw scan stream.
    #[allow(clippy::too_many_arguments)]
    fn scan_partition_at_with_partial_aggregate(
        &self,
        location: PartitionLocation,
        partition_id: PartitionId,
        range: KeyRange,
        projection: SchemaRef,
        predicate: Option<Arc<dyn PhysicalExpr>>,
        batch_size: usize,
        limit: Option<usize>,
        elapsed_compute: Time,
        fragment: Arc<PartialAggregateFragment>,
        context: Arc<TaskContext>,
    ) -> anyhow::Result<SendableRecordBatchStream> {
        let stream = self.scan_partition_at(
            location,
            partition_id,
            range,
            projection,
            predicate,
            batch_size,
            limit,
            elapsed_compute,
        )?;
        fragment
            .execute_stream(stream, context)
            .map_err(anyhow::Error::from)
    }
}

#[derive(Debug)]
pub(crate) struct PartitionedTableProvider<S> {
    partition_selector: S,
    schema: SchemaRef,
    ordering: Vec<String>,
    partition_scanner: Arc<dyn ScanPartition>,
    partition_key_extractor: FirstMatchingPartitionKeyExtractor,
    statistics: Statistics,
}

impl<S> PartitionedTableProvider<S> {
    pub(crate) fn new<T: ScanPartition>(
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

#[derive(Debug, Clone)]
struct LogicalPartition {
    physical_partitions: Vec<(PartitionId, Partition)>,
}

impl LogicalPartition {
    fn new(physical_partitions: Vec<(PartitionId, Partition)>) -> Self {
        Self {
            physical_partitions,
        }
    }
}

fn physical_partitions_to_logical(
    physical_partitions: Vec<(PartitionId, Partition)>,
    target_partitions: usize,
) -> Vec<LogicalPartition> {
    if physical_partitions.len() <= target_partitions {
        // don't bother to coalesce physical partitions together, just
        // use them as-is.
        return physical_partitions
            .into_iter()
            .map(|p| LogicalPartition::new(vec![p]))
            .collect();
    }

    let mut logical_partitions = vec![LogicalPartition::new(Default::default()); target_partitions];
    let mut logical_index = 0;

    for partition in physical_partitions {
        logical_partitions[logical_index]
            .physical_partitions
            .push(partition);
        logical_index = (logical_index + 1) % target_partitions;
    }

    logical_partitions
}

#[derive(Debug)]
struct LocatedPartitions {
    location: PartitionLocation,
    physical_partitions: Vec<(PartitionId, Partition)>,
}

fn group_partitions_by_location(
    scanner: &dyn ScanPartition,
    physical_partitions: Vec<(PartitionId, Partition)>,
) -> anyhow::Result<Vec<LocatedPartitions>> {
    let mut groups: Vec<LocatedPartitions> = Vec::new();

    for physical_partition @ (partition_id, _) in physical_partitions {
        let location = scanner.partition_location(partition_id)?;
        if let Some(group) = groups.iter_mut().find(|group| group.location == location) {
            group.physical_partitions.push(physical_partition);
        } else {
            groups.push(LocatedPartitions {
                location,
                physical_partitions: vec![physical_partition],
            });
        }
    }

    Ok(groups)
}

fn allocate_logical_partitions(
    groups: Vec<LocatedPartitions>,
    target_partitions: usize,
) -> Vec<(PartitionLocation, Vec<LogicalPartition>)> {
    if groups.is_empty() {
        return Vec::new();
    }

    // Every placement needs at least one execution lane so a logical partition
    // never crosses an RPC boundary. Distribute the remaining session parallelism
    // across placements without exceeding the number of physical scans in a group.
    let desired_lanes = target_partitions.max(groups.len()).min(
        groups
            .iter()
            .map(|group| group.physical_partitions.len())
            .sum(),
    );
    let mut lane_counts = vec![1; groups.len()];
    let mut remaining = desired_lanes.saturating_sub(groups.len());
    while remaining > 0 {
        let mut allocated = false;
        for (group, lanes) in groups.iter().zip(&mut lane_counts) {
            if *lanes < group.physical_partitions.len() {
                *lanes += 1;
                remaining -= 1;
                allocated = true;
                if remaining == 0 {
                    break;
                }
            }
        }
        if !allocated {
            break;
        }
    }

    groups
        .into_iter()
        .zip(lane_counts)
        .map(|(group, lanes)| {
            (
                group.location,
                physical_partitions_to_logical(group.physical_partitions, lanes),
            )
        })
        .collect()
}

/// Combines the location-specific branches of one table scan.
///
/// This deliberately reports the table's original statistics instead of
/// summing its children: the children are disjoint placement fragments of the
/// same scan, but each child cannot derive an accurate share of a static table
/// estimate on its own.
#[derive(Debug, Clone)]
pub(crate) struct LocationAwareScanExec {
    inputs: Vec<Arc<dyn ExecutionPlan>>,
    union: Arc<dyn ExecutionPlan>,
    statistics: Arc<Statistics>,
}

impl LocationAwareScanExec {
    fn try_new(
        inputs: Vec<Arc<dyn ExecutionPlan>>,
        statistics: Arc<Statistics>,
    ) -> datafusion::common::Result<Arc<dyn ExecutionPlan>> {
        debug_assert!(inputs.len() > 1);
        let union = UnionExec::try_new(inputs.clone())?;
        Ok(Arc::new(Self {
            inputs,
            union,
            statistics,
        }))
    }

    pub(crate) fn supports_partial_aggregate(&self) -> bool {
        let mut has_remote_branch = false;
        let supported = self.inputs.iter().all(|input| {
            if let Some(scan) = input.downcast_ref::<PartitionScanExec>() {
                return scan.limit.is_none();
            }
            if let Some(remote) = input.downcast_ref::<RemoteNodeExec>() {
                has_remote_branch = true;
                return !remote.has_limit();
            }
            false
        });
        supported && has_remote_branch
    }

    pub(crate) fn with_partial_aggregate(
        &self,
        fragment: Arc<PartialAggregateFragment>,
    ) -> datafusion::common::Result<Arc<dyn ExecutionPlan>> {
        let inputs = self
            .inputs
            .iter()
            .map(|input| {
                if let Some(local) = input.downcast_ref::<PartitionScanExec>() {
                    fragment.create_partial_exec(Arc::new(local.clone()))
                } else if let Some(remote) = input.downcast_ref::<RemoteNodeExec>() {
                    remote.with_partial_aggregate(Arc::clone(&fragment))
                } else {
                    Err(DataFusionError::Internal(format!(
                        "unsupported location-aware scan branch {}",
                        input.name()
                    )))
                }
            })
            .collect::<datafusion::common::Result<Vec<_>>>()?;
        Self::try_new(
            inputs,
            Arc::new(Statistics::new_unknown(&fragment.output_schema())),
        )
    }
}

impl ExecutionPlan for LocationAwareScanExec {
    fn name(&self) -> &str {
        "LocationAwareScanExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        self.union.properties()
    }

    fn maintains_input_order(&self) -> Vec<bool> {
        self.union.maintains_input_order()
    }

    fn benefits_from_input_partitioning(&self) -> Vec<bool> {
        self.union.benefits_from_input_partitioning()
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        self.inputs.iter().collect()
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> datafusion::common::Result<Arc<dyn ExecutionPlan>> {
        Self::try_new(children, self.statistics.clone())
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> datafusion::common::Result<SendableRecordBatchStream> {
        self.union.execute(partition, context)
    }

    fn metrics(&self) -> Option<MetricsSet> {
        self.union.metrics()
    }

    fn partition_statistics(
        &self,
        partition: Option<usize>,
    ) -> datafusion::common::Result<Arc<Statistics>> {
        match partition {
            Some(partition) => self.union.partition_statistics(Some(partition)),
            None => Ok(self.statistics.clone()),
        }
    }
}

impl DisplayAs for LocationAwareScanExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "LocationAwareScanExec")
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
            Some(p) => SchemaRef::new(self.schema.project(p)?),
            None => self.schema.clone(),
        };

        // as we report our filter pushdown as inexact, all columns needed for the filters will be in the projection
        let filters: Vec<_> = filters
            .iter()
            .map(|p| {
                let p = datafusion::physical_expr::planner::logical2physical(p, &projected_schema);
                // The predicate *should* have the correct column indices but bugs in datafusion can create mixups.
                // Most datafusion table providers seem to use reassign_expr_columns so they are tolerant to this.
                // The column indices are not important as all columns should refer to fields in this table
                // and we don't have any duplicate field names.
                datafusion::physical_expr::utils::reassign_expr_columns(p, &projected_schema)
            })
            .collect::<datafusion::common::Result<_>>()?;

        let partition_key_selection = self
            .partition_key_extractor
            .try_extract_selection(&filters)
            .map_err(|e| DataFusionError::External(e.into()))?;

        let static_predicate = datafusion::physical_expr::conjunction_opt(filters);

        let physical_partitions: Vec<(PartitionId, Partition)> = self
            .partition_selector
            .get_live_partitions()
            .await
            .map_err(DataFusionError::External)?
            .into_iter()
            .flat_map(|(partition_id, partition)| {
                match &partition_key_selection {
                    // User requested a full scan of all partitions, return one physical partition per restate partition
                    None => itertools::Either::Left(Some((partition_id, partition)).into_iter()),
                    // Group selected keys into one physical scan per Restate partition if the number
                    // of keys is too large (to bound the number of concurrent scans) or if the fanout
                    // was set to per-partition.
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
                    // User requested a list of point reads
                    Some(selection) => {
                        itertools::Either::Right(
                            selection
                                .keys
                                // Find requested partition keys that are in this partition
                                .range(partition.key_range)
                                .cloned()
                                .map(move |partition_key| {
                                    // We create a 'physical partition' per partition key.
                                    // If the user provided a single point read (`id = 'inv_...'`),
                                    // then we will have 1 physical partition overall -> 1 logical partition.
                                    // If they provided N point reads (`id in ('inv_1', 'inv_2', ..)`),
                                    // we will have N physical partitions, perhaps even for a single restate partition.
                                    // Those will then be round-robined to the underlying logical partitions.
                                    // As a result, separate point reads on the same partition ID might end up
                                    // on separate logical partitions,but that's ok because they *can* be done
                                    // in parallel efficiently.
                                    (
                                        partition_id,
                                        Partition::new(
                                            partition_id,
                                            KeyRange::new(partition_key, partition_key),
                                        ),
                                    )
                                }),
                        )
                    }
                }
            })
            .collect();

        let located_partitions =
            group_partitions_by_location(self.partition_scanner.as_ref(), physical_partitions)
                .map_err(|error| DataFusionError::External(error.into()))?;
        let located_partitions =
            allocate_logical_partitions(located_partitions, state.config().target_partitions());

        if located_partitions.is_empty() {
            return Ok(Arc::new(EmptyExec::new(projected_schema)));
        }

        let sort_columns = find_sort_columns(&self.ordering, &projected_schema);

        let eq_properties = if sort_columns.is_empty() {
            EquivalenceProperties::new(projected_schema.clone())
        } else {
            let ordering = make_ordering(sort_columns.clone());
            EquivalenceProperties::new_with_orderings(projected_schema.clone(), [ordering])
        };

        let statistics = Arc::new(self.statistics.clone().project(projection));
        let branch_statistics = if located_partitions.len() == 1 {
            statistics.clone()
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

            let scan = PartitionScanExec {
                location,
                logical_partitions,
                projected_schema: projected_schema.clone(),
                limit,
                static_predicate: static_predicate.clone(),
                dynamic_predicate: None,
                scanner: Arc::clone(&self.partition_scanner),
                plan: Arc::new(plan),
                statistics: branch_statistics.clone(),
                metrics: ExecutionPlanMetricsSet::new(),
            };

            inputs.push(match location {
                PartitionLocation::Local => Arc::new(scan) as Arc<dyn ExecutionPlan>,
                PartitionLocation::Remote { .. } => {
                    Arc::new(RemoteNodeExec::new(scan)) as Arc<dyn ExecutionPlan>
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
        let res = filters
            .iter()
            // if we set this to exact, we might be able to remove a FilterExec higher up the plan.
            // however, it means that fields we filter on won't end up in our projection, meaning we
            // have to manage a projected schema and a filter schema - defer this complexity for
            // future optimization.
            .map(|_| TableProviderFilterPushDown::Inexact)
            .collect();

        Ok(res)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PartitionScanExec {
    location: PartitionLocation,
    logical_partitions: Vec<LogicalPartition>,
    projected_schema: SchemaRef,
    limit: Option<usize>,
    static_predicate: Option<Arc<dyn PhysicalExpr>>,
    dynamic_predicate: Option<Arc<dyn PhysicalExpr>>,
    scanner: Arc<dyn ScanPartition>,
    plan: Arc<PlanProperties>,
    statistics: Arc<Statistics>,
    metrics: ExecutionPlanMetricsSet,
}

impl ExecutionPlan for PartitionScanExec {
    fn name(&self) -> &str {
        "PartitionScanExec"
    }

    fn schema(&self) -> SchemaRef {
        self.projected_schema.clone()
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.plan
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        new_children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>, DataFusionError> {
        if !new_children.is_empty() {
            return Err(DataFusionError::Internal(
                "PartitionScanExec does not support children".to_owned(),
            ));
        }

        Ok(self)
    }

    fn partition_statistics(
        &self,
        _partition: Option<usize>,
    ) -> datafusion::common::Result<Arc<Statistics>> {
        Ok(self.statistics.clone())
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> datafusion::common::Result<SendableRecordBatchStream> {
        self.execute_with_partial_aggregate(partition, context, None)
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

    fn handle_child_pushdown_result(
        &self,
        phase: datafusion::physical_plan::filter_pushdown::FilterPushdownPhase,
        child_pushdown_result: datafusion::physical_plan::filter_pushdown::ChildPushdownResult,
        _config: &datafusion::config::ConfigOptions,
    ) -> datafusion::error::Result<
        datafusion::physical_plan::filter_pushdown::FilterPushdownPropagation<
            Arc<dyn ExecutionPlan>,
        >,
    > {
        if !matches!(phase, FilterPushdownPhase::Post) {
            return Ok(FilterPushdownPropagation::if_all(child_pushdown_result));
        }

        // As in the static case above, the predicate *should* have the correct column indices,
        // but bugs in datafusion can create mixups.
        let filters: Vec<_> = child_pushdown_result
            .parent_filters
            .iter()
            .map(|f| {
                datafusion::physical_expr::utils::reassign_expr_columns(
                    f.filter.clone(),
                    &self.projected_schema,
                )
            })
            .collect::<Result<_, _>>()?;

        let predicate = datafusion::physical_expr::conjunction(filters);
        let mut plan = self.clone();
        plan.dynamic_predicate = Some(predicate);

        Ok(FilterPushdownPropagation {
            // we report all filters as unsupported as we don't guarantee to apply them exactly as there can be a delay before new filters are used
            filters: child_pushdown_result
                .parent_filters
                .iter()
                .map(|_| PushedDown::No)
                .collect(),
            updated_node: Some(Arc::new(plan)),
        })
    }
}

impl PartitionScanExec {
    fn execute_with_partial_aggregate(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
        fragment: Option<Arc<PartialAggregateFragment>>,
    ) -> datafusion::common::Result<SendableRecordBatchStream> {
        let baseline_metrics = BaselineMetrics::new(&self.metrics, partition);

        let physical_partitions = self
            .logical_partitions
            .get(partition)
            .expect("partition exists")
            .physical_partitions
            .to_vec();

        let sequential_scanners_stream = stream::iter(physical_partitions)
            .map({
                let scanner = Arc::clone(&self.scanner);
                let schema = self.projected_schema.clone();
                let limit = self.limit;
                let predicate = datafusion::physical_expr::conjunction_opt(
                    [
                        self.static_predicate.clone(),
                        self.dynamic_predicate.clone(),
                    ]
                    .into_iter()
                    .flatten(),
                );
                let location = self.location;
                let batch_size = context.session_config().batch_size();
                let elapsed_compute = baseline_metrics.elapsed_compute().clone();
                let fragment = fragment.clone();
                let context = Arc::clone(&context);
                move |(partition_id, partition)| {
                    if let Some(fragment) = &fragment {
                        scanner.scan_partition_at_with_partial_aggregate(
                            location,
                            partition_id,
                            partition.key_range,
                            schema.clone(),
                            predicate.clone(),
                            batch_size,
                            limit,
                            elapsed_compute.clone(),
                            Arc::clone(fragment),
                            Arc::clone(&context),
                        )
                    } else {
                        scanner.scan_partition_at(
                            location,
                            partition_id,
                            partition.key_range,
                            schema.clone(),
                            predicate.clone(),
                            batch_size,
                            limit,
                            elapsed_compute.clone(),
                        )
                    }
                    .map_err(|e| DataFusionError::External(e.into()))
                }
            })
            .try_flatten();

        let metered = MeteredStream {
            inner: sequential_scanners_stream,
            baseline_metrics,
        };

        Ok(Box::pin(RecordBatchStreamAdapter::new(
            fragment
                .map(|fragment| fragment.output_schema())
                .unwrap_or_else(|| self.projected_schema.clone()),
            metered,
        )))
    }
}

/// An explicit physical-plan boundary for work assigned to another node.
///
/// The scan is deliberately opaque to DataFusion's default optimizer rules: an
/// operator must only appear inside this boundary once `RemoteNodeExec` knows how
/// to serialize and execute it remotely. Custom rules can match and enrich this
/// node without rediscovering placement.
#[derive(Debug, Clone)]
pub(crate) struct RemoteNodeExec {
    scan: PartitionScanExec,
    partial_aggregate: Option<Arc<PartialAggregateFragment>>,
    plan: Arc<PlanProperties>,
}

impl RemoteNodeExec {
    fn new(scan: PartitionScanExec) -> Self {
        debug_assert!(matches!(scan.location, PartitionLocation::Remote { .. }));
        let plan = scan.properties().clone();
        Self {
            scan,
            partial_aggregate: None,
            plan,
        }
    }

    fn target_node(&self) -> NodeId {
        let PartitionLocation::Remote { node_id } = self.scan.location else {
            unreachable!("RemoteNodeExec always contains a remote partition scan")
        };
        node_id
    }

    pub(crate) fn has_limit(&self) -> bool {
        self.scan.limit.is_some()
    }

    #[cfg(test)]
    pub(crate) fn has_partial_aggregate(&self) -> bool {
        self.partial_aggregate.is_some()
    }

    pub(crate) fn with_partial_aggregate(
        &self,
        fragment: Arc<PartialAggregateFragment>,
    ) -> datafusion::common::Result<Arc<dyn ExecutionPlan>> {
        let partial = fragment.create_partial_exec(Arc::new(self.scan.clone()))?;
        Ok(Arc::new(Self {
            scan: self.scan.clone(),
            partial_aggregate: Some(fragment),
            plan: partial.properties().clone(),
        }))
    }
}

impl ExecutionPlan for RemoteNodeExec {
    fn name(&self) -> &str {
        "RemoteNodeExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.plan
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        new_children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>, DataFusionError> {
        if !new_children.is_empty() {
            return Err(DataFusionError::Internal(format!(
                "RemoteNodeExec does not support children, got {}",
                new_children.len()
            )));
        }
        Ok(self)
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> datafusion::common::Result<SendableRecordBatchStream> {
        self.scan
            .execute_with_partial_aggregate(partition, context, self.partial_aggregate.clone())
    }

    fn partition_statistics(
        &self,
        partition: Option<usize>,
    ) -> datafusion::common::Result<Arc<Statistics>> {
        if self.partial_aggregate.is_some() {
            Ok(Arc::new(Statistics::new_unknown(&self.schema())))
        } else {
            self.scan.partition_statistics(partition)
        }
    }

    fn metrics(&self) -> Option<MetricsSet> {
        self.scan.metrics()
    }

    fn handle_child_pushdown_result(
        &self,
        phase: FilterPushdownPhase,
        child_pushdown_result: datafusion::physical_plan::filter_pushdown::ChildPushdownResult,
        config: &datafusion::config::ConfigOptions,
    ) -> datafusion::common::Result<FilterPushdownPropagation<Arc<dyn ExecutionPlan>>> {
        if self.partial_aggregate.is_some() {
            return Ok(FilterPushdownPropagation {
                filters: child_pushdown_result
                    .parent_filters
                    .iter()
                    .map(|_| PushedDown::No)
                    .collect(),
                updated_node: None,
            });
        }
        let propagation =
            self.scan
                .handle_child_pushdown_result(phase, child_pushdown_result, config)?;
        let updated_node = propagation.updated_node.map(|updated_scan| {
            let updated_scan = updated_scan
                .downcast_ref::<PartitionScanExec>()
                .expect("PartitionScanExec updates preserve their type")
                .clone();
            Arc::new(Self::new(updated_scan)) as Arc<dyn ExecutionPlan>
        });

        Ok(FilterPushdownPropagation {
            filters: propagation.filters,
            updated_node,
        })
    }
}

impl DisplayAs for RemoteNodeExec {
    fn fmt_as(&self, t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        match t {
            DisplayFormatType::Default | DisplayFormatType::Verbose => {
                write!(f, "RemoteNodeExec: target_node={}", self.target_node())?;
                if self.partial_aggregate.is_some() {
                    write!(f, ", fragment=PartialAggregate")?;
                }
                Ok(())
            }
            DisplayFormatType::TreeRender => {
                writeln!(f, "target_node={}", self.target_node())?;
                if self.partial_aggregate.is_some() {
                    writeln!(f, "fragment=PartialAggregate")?;
                }
                Ok(())
            }
        }
    }
}

impl DisplayAs for PartitionScanExec {
    fn fmt_as(&self, t: DisplayFormatType, f: &mut Formatter) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default | DisplayFormatType::Verbose => {
                write!(
                    f,
                    "PartitionScanExec: location={:?}, scanner={:?}, partitions={}, projection=[{}]",
                    self.location,
                    self.scanner,
                    self.logical_partitions.len(),
                    ProjectedColumns(&self.projected_schema),
                )?;
                if let Some(predicate) = &self.static_predicate {
                    write!(f, ", static_predicate={predicate}")?;
                }
                if let Some(predicate) = &self.dynamic_predicate {
                    write!(f, ", dynamic_predicate={predicate}")?;
                }
                if let Some(limit) = self.limit {
                    write!(f, ", limit={limit}")?;
                }
                Ok(())
            }
            DisplayFormatType::TreeRender => {
                writeln!(f, "location={:?}", self.location)?;
                writeln!(f, "scanner={:?}", self.scanner)?;
                writeln!(f, "partitions={}", self.logical_partitions.len())?;
                writeln!(
                    f,
                    "projection=[{}]",
                    ProjectedColumns(&self.projected_schema)
                )?;
                if let Some(predicate) = &self.static_predicate {
                    writeln!(f, "static_predicate={predicate}")?;
                }
                if let Some(predicate) = &self.dynamic_predicate {
                    writeln!(f, "dynamic_predicate={predicate}")?;
                }
                if let Some(limit) = self.limit {
                    writeln!(f, "limit={limit}")?;
                }
                Ok(())
            }
        }
    }
}

// Generic-based table provider that provides node-level or global data rather than
// partition-keyed data.
pub trait Scan: Debug + Send + Sync + 'static {
    fn scan(
        &self,
        projection: SchemaRef,
        filters: &[Expr],
        batch_size: usize,
        limit: Option<usize>,
    ) -> SendableRecordBatchStream;
}

pub(crate) type ScannerRef = Arc<dyn Scan>;

#[derive(Debug)]
pub(crate) struct GenericTableProvider {
    schema: SchemaRef,
    scanner: ScannerRef,
    statistics: Statistics,
}

impl GenericTableProvider {
    pub(crate) fn new(schema: SchemaRef, scanner: ScannerRef) -> Self {
        let statistics = Statistics::new_unknown(&schema);
        Self {
            schema,
            scanner,
            statistics,
        }
    }

    pub(crate) fn with_statistics(self, statistics: Statistics) -> Self {
        Self { statistics, ..self }
    }
}

#[async_trait]
impl TableProvider for GenericTableProvider {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        _state: &dyn datafusion::catalog::Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> datafusion::common::Result<Arc<dyn ExecutionPlan>> {
        let projected_schema = match projection {
            Some(p) => SchemaRef::new(self.schema.project(p)?),
            None => self.schema.clone(),
        };

        Ok(Arc::new(GenericExecutionPlan::new(
            projected_schema,
            filters,
            limit,
            Arc::clone(&self.scanner),
            self.statistics.clone().project(projection),
        )))
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> datafusion::common::Result<Vec<TableProviderFilterPushDown>> {
        let res = filters
            .iter()
            .map(|_| TableProviderFilterPushDown::Inexact)
            .collect();

        Ok(res)
    }
}

#[derive(Debug, Clone)]
struct GenericExecutionPlan {
    projected_schema: SchemaRef,
    scanner: ScannerRef,
    limit: Option<usize>,
    filters: Vec<Expr>,
    plan_properties: Arc<PlanProperties>,
    statistics: Arc<Statistics>,
    metrics: ExecutionPlanMetricsSet,
}

impl GenericExecutionPlan {
    fn new(
        projected_schema: SchemaRef,
        filters: &[Expr],
        limit: Option<usize>,
        scanner: ScannerRef,
        statistics: Statistics,
    ) -> Self {
        let eq_properties = EquivalenceProperties::new(projected_schema.clone());

        let plan_properties = PlanProperties::new(
            eq_properties,
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        );

        Self {
            projected_schema,
            scanner,
            limit,
            filters: filters.to_vec(),
            plan_properties: Arc::new(plan_properties),
            statistics: Arc::new(statistics),
            metrics: ExecutionPlanMetricsSet::new(),
        }
    }
}

impl ExecutionPlan for GenericExecutionPlan {
    fn name(&self) -> &str {
        "GenericExecutionPlan"
    }

    fn schema(&self) -> SchemaRef {
        self.projected_schema.clone()
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.plan_properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        new_children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>, DataFusionError> {
        if !new_children.is_empty() {
            return Err(DataFusionError::Internal(
                "GenericExecutionPlan does not support children".to_owned(),
            ));
        }

        Ok(self)
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> datafusion::common::Result<SendableRecordBatchStream> {
        let baseline_metrics = BaselineMetrics::new(&self.metrics, partition);

        let inner = self.scanner.scan(
            self.projected_schema.clone(),
            &self.filters,
            context.session_config().batch_size(),
            self.limit,
        );

        let metered = MeteredStream {
            inner,
            baseline_metrics,
        };

        Ok(Box::pin(RecordBatchStreamAdapter::new(
            self.projected_schema.clone(),
            metered,
        )))
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

    fn partition_statistics(&self, _: Option<usize>) -> datafusion::error::Result<Arc<Statistics>> {
        Ok(self.statistics.clone())
    }
}

impl DisplayAs for GenericExecutionPlan {
    fn fmt_as(&self, t: DisplayFormatType, f: &mut Formatter) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default | DisplayFormatType::Verbose => {
                write!(
                    f,
                    "GenericExecutionPlan: scanner={:?}, projection=[{}]",
                    self.scanner,
                    ProjectedColumns(&self.projected_schema),
                )?;
                if !self.filters.is_empty() {
                    write!(f, ", filters=[{}]", ExprList(&self.filters))?;
                }
                if let Some(limit) = self.limit {
                    write!(f, ", limit={limit}")?;
                }
                Ok(())
            }
            DisplayFormatType::TreeRender => {
                writeln!(f, "scanner={:?}", self.scanner)?;
                writeln!(
                    f,
                    "projection=[{}]",
                    ProjectedColumns(&self.projected_schema)
                )?;
                if !self.filters.is_empty() {
                    writeln!(f, "filters=[{}]", ExprList(&self.filters))?;
                }
                if let Some(limit) = self.limit {
                    writeln!(f, "limit={limit}")?;
                }
                Ok(())
            }
        }
    }
}

/// Display helper: comma-separated column names from a schema.
pub(crate) struct ProjectedColumns<'a>(pub(crate) &'a SchemaRef);

impl Display for ProjectedColumns<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for field in self.0.fields() {
            if !first {
                write!(f, ", ")?;
            }
            write!(f, "{}", field.name())?;
            first = false;
        }
        Ok(())
    }
}

/// Display helper: comma-separated logical expressions.
struct ExprList<'a>(&'a [Expr]);

impl Display for ExprList<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for expr in self.0 {
            if !first {
                write!(f, ", ")?;
            }
            write!(f, "{expr}")?;
            first = false;
        }
        Ok(())
    }
}

/// Stream wrapper that records [`BaselineMetrics`] using [`BaselineMetrics::record_poll`].
pub(crate) struct MeteredStream<S> {
    pub(crate) inner: S,
    pub(crate) baseline_metrics: BaselineMetrics,
}

impl<S> Stream for MeteredStream<S>
where
    S: Stream<Item = datafusion::common::Result<datafusion::arrow::record_batch::RecordBatch>>
        + Unpin,
{
    type Item = datafusion::common::Result<datafusion::arrow::record_batch::RecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let poll = self.inner.poll_next_unpin(cx);
        self.baseline_metrics.record_poll(poll)
    }
}

#[cfg(test)]
mod tests {
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::common::stats::Precision;
    use datafusion::config::ConfigOptions;
    use datafusion::execution::context::SessionContext;
    use datafusion::physical_expr::PhysicalSortExpr;
    use datafusion::physical_optimizer::PhysicalOptimizerRule;
    use datafusion::physical_optimizer::filter_pushdown::FilterPushdown;
    use datafusion::physical_plan::expressions::Column;
    use datafusion::physical_plan::sorts::sort::SortExec;
    use restate_types::GenerationalNodeId;
    use restate_types::errors::GenericError;

    use super::*;

    fn physical_partition(id: u16) -> (PartitionId, Partition) {
        let partition_id = PartitionId::new_unchecked(id);
        (partition_id, Partition::new(partition_id, KeyRange::FULL))
    }

    #[test]
    fn logical_partitions_never_cross_planned_locations() {
        let remote_one = PartitionLocation::Remote {
            node_id: GenerationalNodeId::new(2, 1).into(),
        };
        let remote_two = PartitionLocation::Remote {
            node_id: GenerationalNodeId::new(3, 1).into(),
        };
        let groups = vec![
            LocatedPartitions {
                location: PartitionLocation::Local,
                physical_partitions: (0..6).map(physical_partition).collect(),
            },
            LocatedPartitions {
                location: remote_one,
                physical_partitions: (6..9).map(physical_partition).collect(),
            },
            LocatedPartitions {
                location: remote_two,
                physical_partitions: vec![physical_partition(9)],
            },
        ];

        let allocated = allocate_logical_partitions(groups, 4);
        assert_eq!(allocated.len(), 3);
        assert_eq!(
            allocated
                .iter()
                .map(|(_, partitions)| partitions.len())
                .sum::<usize>(),
            4
        );
        assert_eq!(allocated[0].1.len(), 2);
        assert_eq!(allocated[1].1.len(), 1);
        assert_eq!(allocated[2].1.len(), 1);

        let mut partition_ids = allocated
            .iter()
            .flat_map(|(_, logical)| logical)
            .flat_map(|logical| &logical.physical_partitions)
            .map(|(partition_id, _)| *partition_id)
            .collect::<Vec<_>>();
        partition_ids.sort_unstable();
        assert_eq!(
            partition_ids,
            (0..10).map(PartitionId::new_unchecked).collect::<Vec<_>>()
        );
    }

    #[derive(Debug, Clone)]
    struct TestPartitionSelector;

    #[async_trait]
    impl SelectPartitions for TestPartitionSelector {
        async fn get_live_partitions(&self) -> Result<Vec<(PartitionId, Partition)>, GenericError> {
            Ok((0..4).map(physical_partition).collect())
        }
    }

    #[derive(Debug, Clone)]
    struct TestPartitionScanner;

    impl ScanPartition for TestPartitionScanner {
        fn partition_location(
            &self,
            partition_id: PartitionId,
        ) -> anyhow::Result<PartitionLocation> {
            if partition_id == PartitionId::MIN {
                Ok(PartitionLocation::Local)
            } else {
                Ok(PartitionLocation::Remote {
                    node_id: GenerationalNodeId::new(2, 1).into(),
                })
            }
        }

        fn scan_partition(
            &self,
            _partition_id: PartitionId,
            _range: KeyRange,
            _projection: SchemaRef,
            _predicate: Option<Arc<dyn PhysicalExpr>>,
            _batch_size: usize,
            _limit: Option<usize>,
            _elapsed_compute: Time,
        ) -> anyhow::Result<SendableRecordBatchStream> {
            unreachable!("plan-shape test does not execute the scan")
        }
    }

    #[tokio::test]
    async fn physical_plan_has_an_explicit_remote_node() {
        let schema = Arc::new(Schema::empty());
        let provider = PartitionedTableProvider::new(
            TestPartitionSelector,
            schema.clone(),
            Vec::new(),
            TestPartitionScanner,
            FirstMatchingPartitionKeyExtractor::default(),
        )
        .with_statistics(Statistics::new_unknown(&schema).with_num_rows(Precision::Inexact(1024)));
        let context = SessionContext::new();

        let plan = provider
            .scan(&context.state(), None, &[], None)
            .await
            .expect("physical plan should build");
        let scan = plan
            .downcast_ref::<LocationAwareScanExec>()
            .expect("local and remote placements should form one scan");
        assert_eq!(scan.children().len(), 2);
        assert_eq!(
            scan.partition_statistics(None)
                .expect("statistics should be available")
                .num_rows,
            Precision::Inexact(1024)
        );

        let remote = scan
            .children()
            .into_iter()
            .find_map(|child| child.downcast_ref::<RemoteNodeExec>())
            .expect("remote placement should have an explicit boundary");
        assert_eq!(
            remote.target_node(),
            NodeId::from(GenerationalNodeId::new(2, 1))
        );
        assert!(remote.children().is_empty());
        assert_eq!(remote.scan.name(), "PartitionScanExec");
    }

    #[tokio::test]
    async fn topk_dynamic_filter_reaches_the_remote_scan() {
        let provider = PartitionedTableProvider::new(
            TestPartitionSelector,
            Arc::new(Schema::new(vec![Field::new(
                "value",
                DataType::Int64,
                false,
            )])),
            Vec::new(),
            TestPartitionScanner,
            FirstMatchingPartitionKeyExtractor::default(),
        );
        let context = SessionContext::new();
        let scan = provider
            .scan(&context.state(), None, &[], None)
            .await
            .expect("physical plan should build");
        let sort = Arc::new(
            SortExec::new(
                [PhysicalSortExpr::new_default(Arc::new(Column::new(
                    "value", 0,
                )))]
                .into(),
                scan,
            )
            .with_fetch(Some(10)),
        );
        let mut config = ConfigOptions::new();
        config.optimizer.enable_topk_dynamic_filter_pushdown = true;

        let optimized = FilterPushdown::new_post_optimization()
            .optimize(sort, &config)
            .expect("TopK filter pushdown should succeed");
        let scan = optimized.children()[0]
            .downcast_ref::<LocationAwareScanExec>()
            .expect("sort input should remain a location-aware scan");
        let remote = scan
            .children()
            .into_iter()
            .find_map(|child| child.downcast_ref::<RemoteNodeExec>())
            .expect("remote boundary should remain explicit");

        assert!(remote.scan.static_predicate.is_none());
        assert!(remote.scan.dynamic_predicate.is_some());
    }
}
