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

use crate::table_providers::{LocationAwareScanExec, PartitionScanExec, RemoteNodeExec};
use crate::{decode_schema, encode_schema};

/// Version of the accumulator-state contract exchanged by partial aggregates.
///
/// Increment this whenever a supported aggregate's state representation can no
/// longer be safely consumed by the peer's DataFusion runtime.
pub(crate) const PARTIAL_AGGREGATE_STATE_ABI: u32 = 1;

/// A validated, deliberately narrow aggregate fragment that consumes raw scan
/// rows and produces DataFusion accumulator state.
#[derive(Clone)]
pub struct PartialAggregateFragment {
    group_by: PhysicalGroupBy,
    aggregate: Vec<Arc<datafusion::physical_plan::udaf::AggregateFunctionExpr>>,
    input_schema: SchemaRef,
    output_schema: SchemaRef,
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
    /// Returns `None` for an aggregate shape that is intentionally outside the
    /// first implementation's allowlist.
    pub(crate) fn from_aggregate(aggregate: &AggregateExec) -> Result<Option<Self>> {
        let group_by = aggregate.group_expr();
        let ordinary_grouping = group_by.is_true_no_grouping()
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
            return Ok(None);
        }

        for expression in aggregate.aggr_expr() {
            if !matches!(
                expression.fun().name().to_ascii_lowercase().as_str(),
                "count" | "sum" | "min" | "max" | "avg"
            ) || expression.is_distinct()
                || expression.ignore_nulls()
                || expression.is_reversed()
                || !expression.order_bys().is_empty()
            {
                return Ok(None);
            }
        }

        let fragment = Self {
            group_by: aggregate.group_expr().clone(),
            aggregate: aggregate.aggr_expr().to_vec(),
            input_schema: aggregate.input_schema(),
            output_schema: aggregate.schema(),
        };

        // Prove at planning time that every grouping and aggregate argument is
        // representable by the physical protobuf codec used on the wire.
        fragment.to_wire()?;
        Ok(Some(fragment))
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
        let Some(fragment) = Self::from_aggregate(aggregate)? else {
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
    let Some(fragment) = PartialAggregateFragment::from_aggregate(aggregate)? else {
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

    let rewritten_scan = if let Some(scan) = scan.downcast_ref::<LocationAwareScanExec>() {
        if !scan.can_push_partial_aggregate() || !scan.has_remote_branch() {
            return Ok(Transformed::no(plan));
        }
        scan.with_partial_aggregate(Arc::new(fragment.clone()))?
    } else if let Some(remote) = scan.downcast_ref::<RemoteNodeExec>() {
        if remote.has_limit() {
            return Ok(Transformed::no(plan));
        }
        remote.with_partial_aggregate(Arc::new(fragment.clone()))?
    } else if scan.downcast_ref::<PartitionScanExec>().is_some() {
        // A local-only branch already executes the partial aggregate on the
        // coordinator and gains nothing from this distributed rewrite.
        return Ok(Transformed::no(plan));
    } else {
        return Ok(Transformed::no(plan));
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
    use datafusion::arrow::array::{Int64Array, StringArray};
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
                Ok(PartitionLocation::Remote {
                    node_id: GenerationalNodeId::new(2, 1).into(),
                })
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
            let (groups, values): (&[&str], &[i64]) = if partition_id == PartitionId::MIN {
                (&["a", "a", "b"], &[10, 20, 5])
            } else {
                (&["a", "b", "b"], &[3, 7, 8])
            };
            let batch = RecordBatch::try_new(
                Arc::clone(&self.schema),
                vec![
                    Arc::new(StringArray::from(groups.to_vec())),
                    Arc::new(Int64Array::from(values.to_vec())),
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
        ) -> anyhow::Result<SendableRecordBatchStream> {
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
    }

    #[tokio::test]
    async fn pushes_partial_aggregate_into_local_and_remote_branches() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("group", DataType::Utf8, false),
            Field::new("value", DataType::Int64, false),
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
        let sum = Arc::new(
            AggregateExprBuilder::new(sum_udaf(), vec![Arc::new(Column::new("value", 1))])
                .schema(Arc::clone(&schema))
                .alias("sum(value)")
                .build()
                .expect("sum expression"),
        );
        let partial = AggregateExec::try_new(
            AggregateMode::Partial,
            group_by.clone(),
            vec![sum.clone()],
            vec![None],
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
            .expect("limited aggregate validation")
            .is_none(),
            "aggregate TopK must retain its existing coordinator plan"
        );
        let partial = Arc::new(partial) as Arc<dyn ExecutionPlan>;

        let fragment = PartialAggregateFragment::from_aggregate(
            partial
                .downcast_ref::<AggregateExec>()
                .expect("aggregate plan"),
        )
        .expect("fragment validation")
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
                vec![sum],
                vec![None],
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
            let values = batch
                .column(1)
                .as_any()
                .downcast_ref::<Int64Array>()
                .expect("sum values");
            for row in 0..batch.num_rows() {
                result.insert(groups.value(row).to_owned(), values.value(row));
            }
        }
        assert_eq!(
            result,
            BTreeMap::from([("a".to_owned(), 33), ("b".to_owned(), 20)])
        );
    }

    #[tokio::test]
    async fn pushes_global_partial_aggregate_and_preserves_single_result() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("group", DataType::Utf8, false),
            Field::new("value", DataType::Int64, false),
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
    fn all_allowlisted_aggregates_round_trip() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "value",
            DataType::Int64,
            true,
        )]));
        let context = SessionContext::new();

        for aggregate_function in [count_udaf(), sum_udaf(), min_udaf(), max_udaf(), avg_udaf()] {
            let name = aggregate_function.name().to_owned();
            let expression = Arc::new(
                AggregateExprBuilder::new(
                    aggregate_function,
                    vec![Arc::new(Column::new("value", 0))],
                )
                .schema(Arc::clone(&schema))
                .alias(name.clone())
                .build()
                .expect("allowlisted aggregate expression"),
            );
            let aggregate = AggregateExec::try_new(
                AggregateMode::Partial,
                PhysicalGroupBy::default(),
                vec![expression],
                vec![None],
                Arc::new(EmptyExec::new(Arc::clone(&schema))),
                Arc::clone(&schema),
            )
            .expect("allowlisted aggregate plan");
            let fragment = PartialAggregateFragment::from_aggregate(&aggregate)
                .expect("allowlisted aggregate validation")
                .unwrap_or_else(|| panic!("{name} should be allowlisted"));
            let wire = fragment.to_wire().expect("allowlisted aggregate encoding");
            let decoded = PartialAggregateFragment::from_wire(&wire, &context.task_ctx(), &schema)
                .expect("allowlisted aggregate decoding")
                .unwrap_or_else(|| panic!("{name} should decode"));
            assert_eq!(decoded.output_schema(), fragment.output_schema());
        }
    }
}
