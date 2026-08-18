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
use datafusion::physical_optimizer::PhysicalOptimizerRule;
use datafusion::physical_plan::aggregates::{AggregateExec, AggregateMode, PhysicalGroupBy};
use datafusion::physical_plan::empty::EmptyExec;
use datafusion::physical_plan::expressions::Column;
use datafusion::physical_plan::repartition::RepartitionExec;
use datafusion::physical_plan::streaming::{PartitionStream, StreamingTableExec};
use datafusion::physical_plan::{ExecutionPlan, Partitioning};
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
    input_schema: SchemaRef,
    output_schema: SchemaRef,
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
            .field("input_schema", &self.input_schema)
            .field("output_schema", &self.output_schema)
            .finish()
    }
}

impl PartialAggregateFragment {
    /// Returns `None` when the aggregate is outside the allowlist or its
    /// accumulator state cannot be safely reduced and encoded for a peer.
    pub(crate) fn from_aggregate(aggregate: &AggregateExec) -> Option<Self> {
        let fragment = Self::from_supported_aggregate(aggregate)?;

        // Prove at planning time that every grouping and aggregate argument is
        // representable by the physical protobuf codec used on the wire.
        fragment.to_wire().ok()?;
        Some(fragment)
    }

    fn from_supported_aggregate(aggregate: &AggregateExec) -> Option<Self> {
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
            input_schema: aggregate.input_schema(),
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
        if input.schema() != self.input_schema {
            return internal_err!(
                "partial aggregate input schema mismatch: expected {:?}, got {:?}",
                self.input_schema,
                input.schema()
            );
        }

        let aggregate = AggregateExec::try_new(
            AggregateMode::Partial,
            self.group_by.clone(),
            self.aggregate.clone(),
            vec![None; self.aggregate.len()],
            input,
            Arc::clone(&self.input_schema),
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
            Arc::clone(&self.input_schema),
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
            schema: Arc::clone(&self.input_schema),
            stream: Mutex::new(Some(stream)),
        });
        let input = Arc::new(StreamingTableExec::try_new(
            Arc::clone(&self.input_schema),
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
            Arc::new(EmptyExec::new(Arc::clone(&self.input_schema))) as Arc<dyn ExecutionPlan>;
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
        input_schema: &SchemaRef,
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
        if aggregate.input().downcast_ref::<EmptyExec>().is_none() {
            return Ok(None);
        }
        let Some(fragment) = Self::from_supported_aggregate(aggregate) else {
            return Ok(None);
        };
        if fragment.input_schema != *input_schema
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

    let scan = if let Some(repartition) = aggregate.input().downcast_ref::<RepartitionExec>() {
        if !matches!(repartition.partitioning(), Partitioning::RoundRobinBatch(_)) {
            return Ok(Transformed::no(plan));
        }
        Arc::clone(repartition.input())
    } else {
        Arc::clone(aggregate.input())
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

    let Some(fragment) = PartialAggregateFragment::from_aggregate(aggregate) else {
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
            _projection: SchemaRef,
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
            Ok(Box::pin(MemoryStream::try_new(
                vec![batch],
                Arc::clone(&self.schema),
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

    #[tokio::test]
    async fn pushes_partial_aggregate_into_local_and_remote_branches() {
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
            scan,
            Arc::clone(&schema),
        )
        .expect("partial aggregate");
        assert!(
            PartialAggregateFragment::from_aggregate(
                &partial
                    .clone()
                    .with_limit_options(Some(LimitOptions::new_with_order(10, true)))
            )
            .is_none(),
            "aggregate TopK must retain its existing coordinator plan"
        );
        let partial = Arc::new(partial) as Arc<dyn ExecutionPlan>;

        let fragment = PartialAggregateFragment::from_aggregate(
            partial
                .downcast_ref::<AggregateExec>()
                .expect("aggregate plan"),
        )
        .expect("supported fragment");
        let wire = fragment.to_wire().expect("fragment serialization");
        let decoded = PartialAggregateFragment::from_wire(&wire, &context.task_ctx(), &schema)
            .expect("fragment deserialization")
            .expect("compatible fragment");
        assert_eq!(decoded.output_schema(), fragment.output_schema());

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
                .is_some_and(|aggregate| aggregate.mode() == &AggregateMode::Partial)
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
        assert!((result["a"].0 - 33.0).abs() < f64::EPSILON);
        assert_eq!(result["a"].1, 3);
        assert!((result["a"].2 - 3.0).abs() < f64::EPSILON);
        assert!((result["a"].3 - 20.0).abs() < f64::EPSILON);
        assert!((result["a"].4 - 11.0).abs() < f64::EPSILON);
        assert!((result["b"].0 - 20.0).abs() < f64::EPSILON);
        assert_eq!(result["b"].1, 3);
        assert!((result["b"].2 - 5.0).abs() < f64::EPSILON);
        assert!((result["b"].3 - 8.0).abs() < f64::EPSILON);
        assert!((result["b"].4 - (20.0 / 3.0)).abs() < f64::EPSILON);
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

        assert!(PartialAggregateFragment::from_aggregate(&aggregate).is_none());
    }
}
