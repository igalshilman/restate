// Copyright (c) 2023 - 2026 Restate Software Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::fmt::{Debug, Formatter};
use std::sync::Arc;

use datafusion::arrow::datatypes::SchemaRef;
use datafusion::common::tree_node::{Transformed, TransformedResult, TreeNode};
use datafusion::common::{DataFusionError, Result, internal_err};
use datafusion::config::ConfigOptions;
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::physical_expr::projection::ProjectionRef;
use datafusion::physical_expr_common::physical_expr::is_volatile;
use datafusion::physical_optimizer::PhysicalOptimizerRule;
use datafusion::physical_plan::aggregates::{AggregateExec, AggregateMode, PhysicalGroupBy};
use datafusion::physical_plan::empty::EmptyExec;
use datafusion::physical_plan::expressions::Column;
use datafusion::physical_plan::filter::{FilterExec, FilterExecBuilder};
use datafusion::physical_plan::repartition::RepartitionExec;
use datafusion::physical_plan::streaming::{PartitionStream, StreamingTableExec};
use datafusion::physical_plan::{ExecutionPlan, Partitioning, PhysicalExpr};
use datafusion_proto::physical_plan::{AsExecutionPlan, DefaultPhysicalExtensionCodec};
use parking_lot::Mutex;
use prost::Message;

use restate_types::net::remote_query_scanner::RemoteQueryScannerPartialAggregate;

use crate::table_providers::{LocationAwareScanExec, RemoteNodeExec};
use crate::{decode_schema, encode_schema};

/// Version of the accumulator-state contract exchanged by partial aggregates.
///
/// Increment this whenever a supported aggregate's state representation can no
/// longer be safely consumed by the peer's DataFusion runtime.
pub(crate) const PARTIAL_AGGREGATE_STATE_ABI: u32 = 1;

/// A validated, deliberately narrow aggregate fragment that consumes raw scan
/// rows and produces DataFusion accumulator state.
#[derive(Clone)]
pub(crate) struct PartialAggregateFragment {
    group_by: PhysicalGroupBy,
    aggregate: Vec<Arc<datafusion::physical_plan::udaf::AggregateFunctionExpr>>,
    filter: Option<PartialAggregateFilter>,
    scan_schema: SchemaRef,
    aggregate_input_schema: SchemaRef,
    output_schema: SchemaRef,
}

#[derive(Clone, Debug)]
struct PartialAggregateFilter {
    predicate: Arc<dyn PhysicalExpr>,
    projection: Option<ProjectionRef>,
    default_selectivity: u8,
    batch_size: usize,
}

impl PartialAggregateFilter {
    fn from_exec(filter: &FilterExec, expected_aggregate_input_schema: &SchemaRef) -> Option<Self> {
        if filter.fetch().is_some()
            || is_volatile(filter.predicate())
            || filter.schema() != *expected_aggregate_input_schema
        {
            return None;
        }
        Some(Self {
            predicate: Arc::clone(filter.predicate()),
            projection: filter.projection().clone(),
            default_selectivity: filter.default_selectivity(),
            batch_size: filter.batch_size(),
        })
    }

    fn create_exec(&self, input: Arc<dyn ExecutionPlan>) -> Result<Arc<dyn ExecutionPlan>> {
        let filter = FilterExecBuilder::new(Arc::clone(&self.predicate), input)
            .with_default_selectivity(self.default_selectivity)
            .with_batch_size(self.batch_size)
            .apply_projection_by_ref(self.projection.as_ref())?
            .build()?;
        Ok(Arc::new(filter))
    }
}

#[derive(Clone)]
pub(crate) struct PartialAggregateExecution {
    fragment: Arc<PartialAggregateFragment>,
    context: Arc<TaskContext>,
}

impl PartialAggregateExecution {
    pub(crate) fn new(fragment: Arc<PartialAggregateFragment>, context: Arc<TaskContext>) -> Self {
        Self { fragment, context }
    }

    pub(crate) fn output_schema(&self) -> SchemaRef {
        self.fragment.output_schema()
    }

    pub(crate) fn to_wire(&self) -> Result<RemoteQueryScannerPartialAggregate> {
        self.fragment.to_wire()
    }

    pub(crate) fn execute(
        self,
        stream: SendableRecordBatchStream,
    ) -> Result<SendableRecordBatchStream> {
        self.fragment.execute_stream(stream, self.context)
    }
}

pub(crate) fn execute_partial_aggregate(
    partial: Option<PartialAggregateExecution>,
    stream: SendableRecordBatchStream,
) -> Result<SendableRecordBatchStream> {
    match partial {
        Some(partial) => partial.execute(stream),
        None => Ok(stream),
    }
}

impl Debug for PartialAggregateFragment {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PartialAggregateFragment")
            .field("group_by", &self.group_by)
            .field(
                "aggregate",
                &self
                    .aggregate
                    .iter()
                    .map(|aggregate| aggregate.name())
                    .collect::<Vec<_>>(),
            )
            .field("filter", &self.filter)
            .field("scan_schema", &self.scan_schema)
            .field("aggregate_input_schema", &self.aggregate_input_schema)
            .field("output_schema", &self.output_schema)
            .finish()
    }
}

impl PartialAggregateFragment {
    /// Returns `None` when the aggregate is outside the allowlist or its
    /// accumulator state cannot be safely reduced and encoded for a peer.
    pub(crate) fn from_aggregate(
        aggregate: &AggregateExec,
        filter: Option<&FilterExec>,
    ) -> Option<Self> {
        let aggregate_input_schema = aggregate.input_schema();
        let (filter, scan_schema) = match filter {
            Some(filter) => (
                Some(PartialAggregateFilter::from_exec(
                    filter,
                    &aggregate_input_schema,
                )?),
                filter.input().schema(),
            ),
            None => (None, Arc::clone(&aggregate_input_schema)),
        };
        let fragment = Self::from_supported_aggregate(aggregate, filter, scan_schema)?;

        // Prove at planning time that the filter, grouping, and aggregate
        // expressions are representable by the physical protobuf codec.
        fragment.to_wire().ok()?;
        Some(fragment)
    }

    fn from_supported_aggregate(
        aggregate: &AggregateExec,
        filter: Option<PartialAggregateFilter>,
        scan_schema: SchemaRef,
    ) -> Option<Self> {
        let group_by = aggregate.group_expr();
        let global_grouping = group_by.is_true_no_grouping();
        let ordinary_grouping = global_grouping
            || (group_by.groups().len() == 1
                && group_by
                    .groups()
                    .first()
                    .is_some_and(|group| group.iter().all(|is_null| !*is_null)));
        if aggregate.mode() != &AggregateMode::Partial
            || aggregate.aggr_expr().is_empty()
            || aggregate.limit_options().is_some()
            || group_by.has_grouping_set()
            || !ordinary_grouping
            || !group_by.null_expr().is_empty()
            || aggregate.filter_expr().iter().any(Option::is_some)
        {
            return None;
        }

        for expression in aggregate.aggr_expr() {
            if !matches!(
                expression.fun().name(),
                "count" | "sum" | "min" | "max" | "avg"
            ) || expression.is_distinct()
                || expression.ignore_nulls()
                || expression.is_reversed()
                || !expression.order_bys().is_empty()
            {
                return None;
            }

            // Preflight the accumulator implementation used by PartialReduce.
            // Some type combinations are accepted while the physical expression
            // is built but fail when DataFusion constructs the accumulator.
            let accumulator_supported =
                if global_grouping || !expression.groups_accumulator_supported() {
                    expression.create_accumulator().is_ok()
                } else {
                    expression.create_groups_accumulator().is_ok()
                };
            if !accumulator_supported {
                return None;
            }
        }

        Some(Self {
            group_by: aggregate.group_expr().clone(),
            aggregate: aggregate.aggr_expr().to_vec(),
            filter,
            scan_schema,
            aggregate_input_schema: aggregate.input_schema(),
            output_schema: aggregate.schema(),
        })
    }

    pub(crate) fn output_schema(&self) -> SchemaRef {
        Arc::clone(&self.output_schema)
    }

    pub(crate) fn create_partial_exec(
        &self,
        input: Arc<dyn ExecutionPlan>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if input.schema() != self.scan_schema {
            return internal_err!(
                "partial aggregate input schema mismatch: expected {:?}, got {:?}",
                self.scan_schema,
                input.schema()
            );
        }

        let input = match &self.filter {
            Some(filter) => filter.create_exec(input)?,
            None => input,
        };
        if input.schema() != self.aggregate_input_schema {
            return internal_err!(
                "partial aggregate filtered input schema mismatch: expected {:?}, got {:?}",
                self.aggregate_input_schema,
                input.schema()
            );
        }
        let aggregate = AggregateExec::try_new(
            AggregateMode::Partial,
            self.group_by.clone(),
            self.aggregate.clone(),
            vec![None; self.aggregate.len()],
            input,
            Arc::clone(&self.aggregate_input_schema),
        )?;
        if aggregate.schema() != self.output_schema {
            return internal_err!(
                "partial aggregate output schema mismatch: expected {:?}, got {:?}",
                self.output_schema,
                aggregate.schema()
            );
        }
        Ok(Arc::new(aggregate))
    }

    fn create_partial_reduce_exec(
        &self,
        input: Arc<dyn ExecutionPlan>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if input.schema() != self.output_schema {
            return internal_err!(
                "partial reduce input schema mismatch: expected {:?}, got {:?}",
                self.output_schema,
                input.schema()
            );
        }

        let group_by = PhysicalGroupBy::new_single(
            self.group_by
                .expr()
                .iter()
                .enumerate()
                .map(|(index, (_, name))| {
                    (
                        Arc::new(Column::new(name, index))
                            as Arc<dyn datafusion::physical_plan::PhysicalExpr>,
                        name.clone(),
                    )
                })
                .collect(),
        );
        let aggregate = AggregateExec::try_new(
            AggregateMode::PartialReduce,
            group_by,
            self.aggregate.clone(),
            vec![None; self.aggregate.len()],
            input,
            Arc::clone(&self.aggregate_input_schema),
        )?;
        if aggregate.schema() != self.output_schema {
            return internal_err!(
                "partial reduce output schema mismatch: expected {:?}, got {:?}",
                self.output_schema,
                aggregate.schema()
            );
        }
        Ok(Arc::new(aggregate))
    }

    pub(crate) fn execute_stream(
        &self,
        stream: SendableRecordBatchStream,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let partition = Arc::new(OneShotPartitionStream {
            schema: Arc::clone(&self.scan_schema),
            stream: Mutex::new(Some(stream)),
        });
        let input = Arc::new(StreamingTableExec::try_new(
            Arc::clone(&self.scan_schema),
            vec![partition],
            None,
            [],
            false,
            None,
        )?) as Arc<dyn ExecutionPlan>;
        self.create_partial_exec(input)?.execute(0, context)
    }

    pub(crate) fn to_wire(&self) -> Result<RemoteQueryScannerPartialAggregate> {
        let placeholder =
            Arc::new(EmptyExec::new(Arc::clone(&self.scan_schema))) as Arc<dyn ExecutionPlan>;
        let aggregate = self.create_partial_exec(placeholder)?;
        let plan = datafusion_proto::protobuf::PhysicalPlanNode::try_from_physical_plan(
            aggregate,
            &DefaultPhysicalExtensionCodec {},
        )?;

        Ok(RemoteQueryScannerPartialAggregate {
            state_abi: PARTIAL_AGGREGATE_STATE_ABI,
            serialized_plan: plan.encode_to_vec(),
            output_schema_bytes: encode_schema(&self.output_schema),
        })
    }

    pub(crate) fn from_wire(
        wire: &RemoteQueryScannerPartialAggregate,
        context: &TaskContext,
        expected_scan_schema: &SchemaRef,
    ) -> Result<Option<Self>> {
        if wire.state_abi != PARTIAL_AGGREGATE_STATE_ABI {
            return Ok(None);
        }

        let expected_output_schema = Arc::new(
            decode_schema(&wire.output_schema_bytes)
                .map_err(|error| DataFusionError::External(error.into()))?,
        );
        let proto =
            datafusion_proto::protobuf::PhysicalPlanNode::decode(wire.serialized_plan.as_slice())
                .map_err(|error| DataFusionError::External(error.into()))?;
        let plan = proto.try_into_physical_plan(context, &DefaultPhysicalExtensionCodec {})?;
        let Some(aggregate) = plan.downcast_ref::<AggregateExec>() else {
            return Ok(None);
        };
        let (filter, decoded_scan_schema) =
            if let Some(filter) = aggregate.input().downcast_ref::<FilterExec>() {
                let Some(filter_fragment) =
                    PartialAggregateFilter::from_exec(filter, &aggregate.input_schema())
                else {
                    return Ok(None);
                };
                if filter.input().downcast_ref::<EmptyExec>().is_none() {
                    return Ok(None);
                }
                (Some(filter_fragment), filter.input().schema())
            } else {
                if aggregate.input().downcast_ref::<EmptyExec>().is_none() {
                    return Ok(None);
                }
                (None, aggregate.input().schema())
            };
        let Some(fragment) = Self::from_supported_aggregate(aggregate, filter, decoded_scan_schema)
        else {
            return Ok(None);
        };
        if fragment.scan_schema != *expected_scan_schema
            || fragment.output_schema != expected_output_schema
        {
            return Ok(None);
        }
        Ok(Some(fragment))
    }
}

struct OneShotPartitionStream {
    schema: SchemaRef,
    stream: Mutex<Option<SendableRecordBatchStream>>,
}

impl Debug for OneShotPartitionStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneShotPartitionStream")
            .field("schema", &self.schema)
            .finish_non_exhaustive()
    }
}

impl PartitionStream for OneShotPartitionStream {
    fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    fn execute(&self, _context: Arc<TaskContext>) -> SendableRecordBatchStream {
        self.stream
            .lock()
            .take()
            .expect("one-shot aggregate input must only be executed once")
    }
}

/// Pushes DataFusion's partial aggregate into each placement branch and leaves
/// a `PartialReduce` at the coordinator boundary.
#[derive(Debug, Default)]
pub(crate) struct PartialAggregationPushdown;

impl PhysicalOptimizerRule for PartialAggregationPushdown {
    fn optimize(
        &self,
        plan: Arc<dyn ExecutionPlan>,
        _config: &ConfigOptions,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        plan.transform_up(rewrite_partial_aggregate).data()
    }

    fn name(&self) -> &str {
        "PartialAggregationPushdown"
    }

    fn schema_check(&self) -> bool {
        true
    }
}

fn rewrite_partial_aggregate(
    plan: Arc<dyn ExecutionPlan>,
) -> Result<Transformed<Arc<dyn ExecutionPlan>>> {
    let Some(aggregate) = plan.downcast_ref::<AggregateExec>() else {
        return Ok(Transformed::no(plan));
    };

    let Some((scan, filter)) = extract_fragment_input(aggregate) else {
        return Ok(Transformed::no(plan));
    };

    if let Some(scan) = scan.downcast_ref::<LocationAwareScanExec>() {
        if !scan.supports_partial_aggregate() {
            return Ok(Transformed::no(plan));
        }
    } else if let Some(remote) = scan.downcast_ref::<RemoteNodeExec>() {
        if remote.has_limit() {
            return Ok(Transformed::no(plan));
        }
    } else {
        // Local-only and unrelated inputs gain nothing from this rewrite.
        return Ok(Transformed::no(plan));
    }

    let Some(fragment) = PartialAggregateFragment::from_aggregate(aggregate, filter.as_ref())
    else {
        return Ok(Transformed::no(plan));
    };
    let fragment = Arc::new(fragment);
    let rewritten_scan = if let Some(scan) = scan.downcast_ref::<LocationAwareScanExec>() {
        scan.with_partial_aggregate(Arc::clone(&fragment))?
    } else if let Some(remote) = scan.downcast_ref::<RemoteNodeExec>() {
        remote.with_partial_aggregate(Arc::clone(&fragment))?
    } else {
        unreachable!("remote placement was validated above")
    };

    let reduced = fragment.create_partial_reduce_exec(rewritten_scan)?;
    if reduced.schema() != plan.schema() {
        return internal_err!(
            "partial aggregation pushdown changed the plan schema: expected {:?}, got {:?}",
            plan.schema(),
            reduced.schema()
        );
    }
    Ok(Transformed::yes(reduced))
}

fn extract_fragment_input(
    aggregate: &AggregateExec,
) -> Option<(Arc<dyn ExecutionPlan>, Option<FilterExec>)> {
    let mut scan = Arc::clone(aggregate.input());
    let mut filter = None;
    let mut has_repartition = false;
    loop {
        if let Some(candidate) = scan.downcast_ref::<FilterExec>() {
            if filter.is_some() {
                return None;
            }
            filter = Some(candidate.clone());
            scan = Arc::clone(candidate.input());
        } else if let Some(repartition) = scan.downcast_ref::<RepartitionExec>() {
            if has_repartition
                || !matches!(repartition.partitioning(), Partitioning::RoundRobinBatch(_))
            {
                return None;
            }
            has_repartition = true;
            scan = Arc::clone(repartition.input());
        } else {
            break;
        }
    }
    Some((scan, filter))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use datafusion::arrow::array::{Float64Array, Int64Array, StringArray};
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::arrow::record_batch::RecordBatch;
    use datafusion::datasource::TableProvider;
    use datafusion::execution::context::SessionContext;
    use datafusion::functions_aggregate::average::avg_udaf;
    use datafusion::functions_aggregate::count::count_udaf;
    use datafusion::functions_aggregate::min_max::{max_udaf, min_udaf};
    use datafusion::functions_aggregate::sum::sum_udaf;
    use datafusion::physical_expr::aggregate::AggregateExprBuilder;
    use datafusion::physical_plan::aggregates::LimitOptions;
    use datafusion::physical_plan::coalesce_partitions::CoalescePartitionsExec;
    use datafusion::physical_plan::expressions::Literal;
    use datafusion::physical_plan::memory::MemoryStream;
    use datafusion::physical_plan::metrics::Time;
    use datafusion::prelude::{col, lit};
    use datafusion::scalar::ScalarValue;

    use restate_types::GenerationalNodeId;
    use restate_types::errors::GenericError;
    use restate_types::identifiers::PartitionId;
    use restate_types::partition_table::Partition;
    use restate_types::sharding::KeyRange;

    use super::*;
    use crate::context::SelectPartitions;
    use crate::filter::FirstMatchingPartitionKeyExtractor;
    use crate::table_providers::{PartitionLocation, PartitionedTableProvider, ScanPartition};

    #[derive(Debug, Clone)]
    struct TwoPartitions;

    #[async_trait]
    impl SelectPartitions for TwoPartitions {
        async fn get_live_partitions(
            &self,
        ) -> std::result::Result<Vec<(PartitionId, Partition)>, GenericError> {
            Ok((0..2)
                .map(|id| {
                    let id = PartitionId::new_unchecked(id);
                    (id, Partition::new(id, KeyRange::FULL))
                })
                .collect())
        }
    }

    #[derive(Debug, Clone)]
    struct LocatedTestScanner {
        schema: SchemaRef,
    }

    impl ScanPartition for LocatedTestScanner {
        fn partition_location(
            &self,
            partition_id: PartitionId,
        ) -> anyhow::Result<PartitionLocation> {
            if partition_id == PartitionId::MIN {
                Ok(PartitionLocation::Local)
            } else {
                Ok(PartitionLocation::Remote(
                    GenerationalNodeId::new(2, 1).into(),
                ))
            }
        }

        fn scan_partition(
            &self,
            partition_id: PartitionId,
            _range: KeyRange,
            projection: SchemaRef,
            _predicate: Option<Arc<dyn datafusion::physical_plan::PhysicalExpr>>,
            _batch_size: usize,
            _limit: Option<usize>,
            _elapsed_compute: Time,
        ) -> anyhow::Result<SendableRecordBatchStream> {
            let (groups, values): (&[&str], &[f64]) = if partition_id == PartitionId::MIN {
                (&["a", "a", "b"], &[10.0, 20.0, 5.0])
            } else {
                (&["a", "b", "b"], &[3.0, 7.0, 8.0])
            };
            let batch = RecordBatch::try_new(
                Arc::clone(&self.schema),
                vec![
                    Arc::new(StringArray::from(groups.to_vec())),
                    Arc::new(Float64Array::from(values.to_vec())),
                ],
            )?;
            let indices = projection
                .fields()
                .iter()
                .map(|field| self.schema.index_of(field.name()))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let batch = batch.project(&indices)?;
            Ok(Box::pin(MemoryStream::try_new(
                vec![batch],
                projection,
                None,
            )?))
        }

        fn scan_partition_at(
            &self,
            _location: PartitionLocation,
            partition_id: PartitionId,
            range: KeyRange,
            projection: SchemaRef,
            predicate: Option<Arc<dyn datafusion::physical_plan::PhysicalExpr>>,
            batch_size: usize,
            limit: Option<usize>,
            elapsed_compute: Time,
            partial_aggregate: Option<PartialAggregateExecution>,
        ) -> anyhow::Result<SendableRecordBatchStream> {
            let stream = self.scan_partition(
                partition_id,
                range,
                projection,
                predicate,
                batch_size,
                limit,
                elapsed_compute,
            )?;
            execute_partial_aggregate(partial_aggregate, stream).map_err(anyhow::Error::from)
        }
    }

    fn int64_column(batch: &RecordBatch, index: usize) -> &Int64Array {
        batch
            .column(index)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("integer aggregate values")
    }

    fn float64_column(batch: &RecordBatch, index: usize) -> &Float64Array {
        batch
            .column(index)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("floating-point aggregate values")
    }

    fn find_partial_aggregate(plan: &Arc<dyn ExecutionPlan>) -> Option<AggregateExec> {
        if let Some(aggregate) = plan.downcast_ref::<AggregateExec>()
            && aggregate.mode() == &AggregateMode::Partial
        {
            return Some(aggregate.clone());
        }
        plan.children().into_iter().find_map(find_partial_aggregate)
    }

    fn aggregation_test_context() -> SessionContext {
        let schema = Arc::new(Schema::new(vec![
            Field::new("group", DataType::Utf8, false),
            Field::new("value", DataType::Float64, false),
        ]));
        let provider = PartitionedTableProvider::new(
            TwoPartitions,
            Arc::clone(&schema),
            Vec::new(),
            LocatedTestScanner { schema },
            FirstMatchingPartitionKeyExtractor::default(),
        );
        let context = SessionContext::new();
        context
            .register_table("test_values", Arc::new(provider))
            .expect("register test table");
        context
    }

    #[tokio::test]
    async fn pushes_filtered_partial_aggregate_into_local_and_remote_branches() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("group", DataType::Utf8, false),
            Field::new("value", DataType::Float64, false),
        ]));
        let provider = PartitionedTableProvider::new(
            TwoPartitions,
            Arc::clone(&schema),
            Vec::new(),
            LocatedTestScanner {
                schema: Arc::clone(&schema),
            },
            FirstMatchingPartitionKeyExtractor::default(),
        );
        let context = SessionContext::new();
        let logical_filter = col("value").gt(lit(6.0));
        let scan = provider
            .scan(
                &context.state(),
                None,
                std::slice::from_ref(&logical_filter),
                None,
            )
            .await
            .expect("scan plan");
        let predicate =
            datafusion::physical_expr::planner::logical2physical(&logical_filter, &schema);
        let filtered = Arc::new(
            FilterExecBuilder::new(predicate, scan)
                .apply_projection(Some(vec![0, 1]))
                .expect("filter projection")
                .build()
                .expect("residual filter"),
        ) as Arc<dyn ExecutionPlan>;
        let repartitioned = Arc::new(
            RepartitionExec::try_new(filtered, Partitioning::RoundRobinBatch(2))
                .expect("round-robin repartition"),
        ) as Arc<dyn ExecutionPlan>;
        let group_by = PhysicalGroupBy::new_single(vec![(
            Arc::new(Column::new("group", 0)) as _,
            "group".to_owned(),
        )]);
        let aggregates = [sum_udaf(), count_udaf(), min_udaf(), max_udaf(), avg_udaf()]
            .into_iter()
            .map(|function| {
                let name = function.name().to_owned();
                Arc::new(
                    AggregateExprBuilder::new(function, vec![Arc::new(Column::new("value", 1))])
                        .schema(Arc::clone(&schema))
                        .alias(name)
                        .build()
                        .expect("aggregate expression"),
                )
            })
            .collect::<Vec<_>>();
        let partial = AggregateExec::try_new(
            AggregateMode::Partial,
            group_by.clone(),
            aggregates.clone(),
            vec![None; aggregates.len()],
            repartitioned,
            Arc::clone(&schema),
        )
        .expect("partial aggregate");
        let residual_filter = partial
            .input()
            .downcast_ref::<RepartitionExec>()
            .expect("round-robin repartition below partial aggregate")
            .input()
            .downcast_ref::<FilterExec>()
            .expect("residual filter below partial aggregate");
        assert!(
            PartialAggregateFragment::from_aggregate(
                &partial
                    .clone()
                    .with_limit_options(Some(LimitOptions::new_with_order(10, true))),
                Some(residual_filter),
            )
            .is_none(),
            "aggregate TopK must retain its existing coordinator plan"
        );

        let fragment = PartialAggregateFragment::from_aggregate(&partial, Some(residual_filter))
            .expect("supported fragment");
        let wire = fragment.to_wire().expect("fragment serialization");
        let decoded = PartialAggregateFragment::from_wire(&wire, &context.task_ctx(), &schema)
            .expect("fragment deserialization")
            .expect("compatible fragment");
        assert_eq!(decoded.output_schema(), fragment.output_schema());
        assert!(decoded.filter.is_some());

        let partial = Arc::new(partial) as Arc<dyn ExecutionPlan>;
        let optimized = PartialAggregationPushdown
            .optimize(partial, &ConfigOptions::new())
            .expect("partial aggregation pushdown");
        let reduce = optimized
            .downcast_ref::<AggregateExec>()
            .expect("partial reduce");
        assert_eq!(reduce.mode(), &AggregateMode::PartialReduce);
        let located = reduce
            .input()
            .downcast_ref::<LocationAwareScanExec>()
            .expect("location-aware state union");
        assert!(located.children().iter().any(|branch| {
            branch
                .downcast_ref::<AggregateExec>()
                .is_some_and(|aggregate| {
                    aggregate.mode() == &AggregateMode::Partial
                        && aggregate.input().downcast_ref::<FilterExec>().is_some()
                })
        }));
        assert!(located.children().iter().any(|branch| {
            branch
                .downcast_ref::<RemoteNodeExec>()
                .is_some_and(RemoteNodeExec::has_partial_aggregate)
        }));

        let coalesced = Arc::new(CoalescePartitionsExec::new(optimized)) as Arc<dyn ExecutionPlan>;
        let final_aggregate = Arc::new(
            AggregateExec::try_new(
                AggregateMode::Final,
                PhysicalGroupBy::new_single(vec![(
                    Arc::new(Column::new("group", 0)) as _,
                    "group".to_owned(),
                )]),
                aggregates.clone(),
                vec![None; aggregates.len()],
                coalesced,
                schema,
            )
            .expect("final aggregate"),
        ) as Arc<dyn ExecutionPlan>;
        let batches = datafusion::physical_plan::collect(final_aggregate, context.task_ctx())
            .await
            .expect("aggregate result");
        let mut result = BTreeMap::new();
        for batch in batches {
            let groups = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("group strings");
            for row in 0..batch.num_rows() {
                result.insert(
                    groups.value(row).to_owned(),
                    (
                        float64_column(&batch, 1).value(row),
                        int64_column(&batch, 2).value(row),
                        float64_column(&batch, 3).value(row),
                        float64_column(&batch, 4).value(row),
                        float64_column(&batch, 5).value(row),
                    ),
                );
            }
        }
        assert!((result["a"].0 - 30.0).abs() < f64::EPSILON);
        assert_eq!(result["a"].1, 2);
        assert!((result["a"].2 - 10.0).abs() < f64::EPSILON);
        assert!((result["a"].3 - 20.0).abs() < f64::EPSILON);
        assert!((result["a"].4 - 15.0).abs() < f64::EPSILON);
        assert!((result["b"].0 - 15.0).abs() < f64::EPSILON);
        assert_eq!(result["b"].1, 2);
        assert!((result["b"].2 - 7.0).abs() < f64::EPSILON);
        assert!((result["b"].3 - 8.0).abs() < f64::EPSILON);
        assert!((result["b"].4 - 7.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn sql_filter_is_applied_before_partial_aggregation() {
        let context = aggregation_test_context();

        let plan = context
            .sql(
                r#"SELECT "group", SUM(value) AS total
                   FROM test_values
                   WHERE value > 6
                   GROUP BY "group""#,
            )
            .await
            .expect("filtered aggregate query")
            .create_physical_plan()
            .await
            .expect("filtered aggregate plan");
        let optimized = PartialAggregationPushdown
            .optimize(plan, &ConfigOptions::new())
            .expect("filtered partial aggregation pushdown");
        let display = datafusion::physical_plan::displayable(optimized.as_ref())
            .indent(true)
            .to_string();
        assert!(display.contains("mode=PartialReduce"), "{display}");
        assert!(
            display.contains("RemoteNodeExec: target_node=N2:1, fragment=PartialAggregate"),
            "{display}"
        );

        let batches = datafusion::physical_plan::collect(optimized, context.task_ctx())
            .await
            .expect("filtered aggregate result");
        let mut result = BTreeMap::new();
        for batch in batches {
            let groups = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("group strings");
            let totals = float64_column(&batch, 1);
            for row in 0..batch.num_rows() {
                result.insert(groups.value(row).to_owned(), totals.value(row));
            }
        }
        assert_eq!(
            result,
            BTreeMap::from([("a".to_owned(), 30.0), ("b".to_owned(), 15.0)])
        );

        let count_plan = context
            .sql("SELECT COUNT(*) AS total FROM test_values WHERE value > 6")
            .await
            .expect("filtered count query")
            .create_physical_plan()
            .await
            .expect("filtered count plan");
        let partial = find_partial_aggregate(&count_plan).expect("partial count aggregate");
        let (scan, filter) = extract_fragment_input(&partial).expect("count fragment input");
        let fragment = PartialAggregateFragment::from_aggregate(&partial, filter.as_ref())
            .expect("filtered count fragment");
        let wire = fragment.to_wire().expect("filtered count serialization");
        let decoded =
            PartialAggregateFragment::from_wire(&wire, &context.task_ctx(), &scan.schema())
                .expect("filtered count deserialization")
                .expect("compatible filtered count fragment");
        assert_eq!(
            decoded
                .filter
                .as_ref()
                .and_then(|filter| filter.projection.as_deref()),
            Some([].as_slice())
        );
        let count_plan = PartialAggregationPushdown
            .optimize(count_plan, &ConfigOptions::new())
            .expect("filtered count pushdown");
        let display = datafusion::physical_plan::displayable(count_plan.as_ref())
            .indent(true)
            .to_string();
        assert!(display.contains("mode=PartialReduce"), "{display}");
        let batches = datafusion::physical_plan::collect(count_plan, context.task_ctx())
            .await
            .expect("filtered count result");
        assert_eq!(int64_column(&batches[0], 0).value(0), 4);
    }

    #[tokio::test]
    async fn volatile_filter_is_not_pushed_into_partial_aggregation() {
        let context = aggregation_test_context();
        let volatile_plan = context
            .sql("SELECT COUNT(*) FROM test_values WHERE random() > 0.5")
            .await
            .expect("volatile filtered query")
            .create_physical_plan()
            .await
            .expect("volatile filtered plan");
        let partial = find_partial_aggregate(&volatile_plan).expect("partial aggregate");
        let (_, filter) = extract_fragment_input(&partial).expect("volatile fragment input");
        assert!(
            filter
                .as_ref()
                .is_some_and(|filter| is_volatile(filter.predicate()))
        );
        let volatile_plan = PartialAggregationPushdown
            .optimize(volatile_plan, &ConfigOptions::new())
            .expect("volatile filter remains unchanged");
        let display = datafusion::physical_plan::displayable(volatile_plan.as_ref())
            .indent(true)
            .to_string();
        assert!(!display.contains("mode=PartialReduce"), "{display}");
        assert!(!display.contains("fragment=PartialAggregate"), "{display}");
    }

    #[tokio::test]
    async fn pushes_global_partial_aggregate_and_preserves_single_result() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("group", DataType::Utf8, false),
            Field::new("value", DataType::Float64, false),
        ]));
        let provider = PartitionedTableProvider::new(
            TwoPartitions,
            Arc::clone(&schema),
            Vec::new(),
            LocatedTestScanner {
                schema: Arc::clone(&schema),
            },
            FirstMatchingPartitionKeyExtractor::default(),
        );
        let context = SessionContext::new();
        let scan = provider
            .scan(&context.state(), None, &[], None)
            .await
            .expect("scan plan");
        let count = Arc::new(
            AggregateExprBuilder::new(
                count_udaf(),
                vec![Arc::new(Literal::new(ScalarValue::Int64(Some(1))))],
            )
            .schema(Arc::clone(&schema))
            .alias("count(*)")
            .build()
            .expect("count expression"),
        );
        let partial = Arc::new(
            AggregateExec::try_new(
                AggregateMode::Partial,
                PhysicalGroupBy::default(),
                vec![count.clone()],
                vec![None],
                scan,
                Arc::clone(&schema),
            )
            .expect("global partial aggregate"),
        ) as Arc<dyn ExecutionPlan>;

        let optimized = PartialAggregationPushdown
            .optimize(partial, &ConfigOptions::new())
            .expect("global partial aggregation pushdown");
        assert_eq!(
            optimized
                .downcast_ref::<AggregateExec>()
                .expect("partial reduce")
                .mode(),
            &AggregateMode::PartialReduce
        );

        let final_aggregate = Arc::new(
            AggregateExec::try_new(
                AggregateMode::Final,
                PhysicalGroupBy::default(),
                vec![count],
                vec![None],
                Arc::new(CoalescePartitionsExec::new(optimized)),
                schema,
            )
            .expect("global final aggregate"),
        ) as Arc<dyn ExecutionPlan>;
        let batches = datafusion::physical_plan::collect(final_aggregate, context.task_ctx())
            .await
            .expect("global aggregate result");
        assert_eq!(batches.iter().map(RecordBatch::num_rows).sum::<usize>(), 1);
        let count = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("count result");
        assert_eq!(count.value(0), 6);
    }

    #[test]
    fn skips_unconstructable_partial_reduce_accumulator() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("group", DataType::Utf8, false),
            Field::new("value", DataType::Int64, false),
        ]));
        let average = Arc::new(
            AggregateExprBuilder::new(avg_udaf(), vec![Arc::new(Column::new("value", 1))])
                .schema(Arc::clone(&schema))
                .alias("avg")
                .build()
                .expect("average expression"),
        );
        let aggregate = AggregateExec::try_new(
            AggregateMode::Partial,
            PhysicalGroupBy::new_single(vec![(
                Arc::new(Column::new("group", 0)) as _,
                "group".to_owned(),
            )]),
            vec![average],
            vec![None],
            Arc::new(EmptyExec::new(Arc::clone(&schema))),
            schema,
        )
        .expect("partial average");

        assert!(PartialAggregateFragment::from_aggregate(&aggregate, None).is_none());
    }
}
