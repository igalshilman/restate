// Copyright (c) 2023 - 2026 Restate Software Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::sync::Arc;

use datafusion::arrow::datatypes::SchemaRef;
use datafusion::common::tree_node::{Transformed, TransformedResult, TreeNode};
use datafusion::common::{Result, internal_err};
use datafusion::config::ConfigOptions;
use datafusion::physical_optimizer::PhysicalOptimizerRule;
use datafusion::physical_plan::aggregates::{AggregateExec, AggregateMode, PhysicalGroupBy};
use datafusion::physical_plan::repartition::RepartitionExec;
use datafusion::physical_plan::udaf::AggregateFunctionExpr;
use datafusion::physical_plan::{ExecutionPlan, InputOrderMode, Partitioning};

use crate::partitioned_scan::{LocationAwareScanExec, RemoteNodeExec};
use crate::remote_fragment::{
    FragmentLeafExec, RemoteFragment, bind_unary_operators, is_remote_safe_expression,
    is_remote_safe_operator, is_row_wise_operator,
};

/// A validated partial aggregate that consumes raw scan rows and produces
/// DataFusion accumulator state that can be merged at the coordinator.
#[derive(Clone, Debug)]
struct PartialAggregateFragment {
    group_by: PhysicalGroupBy,
    aggregate: Vec<Arc<AggregateFunctionExpr>>,
    aggregate_input_schema: SchemaRef,
    remote_fragment: Arc<RemoteFragment>,
}

impl PartialAggregateFragment {
    /// Returns `None` when the operator chain is unsafe or the accumulator
    /// state cannot be safely reduced and encoded for a peer.
    fn from_aggregate(
        aggregate: &AggregateExec,
        operators: &[Arc<dyn ExecutionPlan>],
        fragment_input_schema: SchemaRef,
    ) -> Option<Self> {
        if !is_supported_partial_aggregate(aggregate)
            || !operators.iter().all(is_remote_safe_operator)
        {
            return None;
        }

        let aggregate_input_schema = aggregate.input_schema();
        let leaf = Arc::new(FragmentLeafExec::new(fragment_input_schema)) as Arc<dyn ExecutionPlan>;
        let mut template = bind_unary_operators(operators, leaf)?;
        if template.schema() != aggregate_input_schema {
            return None;
        }
        template = Arc::new(aggregate.clone())
            .with_new_children(vec![template])
            .ok()?;
        let remote_fragment = Arc::new(RemoteFragment::try_new(template).ok()?);

        Some(Self {
            group_by: aggregate.group_expr().clone(),
            aggregate: aggregate.aggr_expr().to_vec(),
            aggregate_input_schema,
            remote_fragment,
        })
    }

    fn fragment(&self) -> Arc<RemoteFragment> {
        Arc::clone(&self.remote_fragment)
    }

    /// Recombines the accumulator states emitted independently by placement
    /// branches without changing the schema expected by DataFusion's final
    /// aggregate stage.
    fn create_partial_reduce_exec(
        &self,
        input: Arc<dyn ExecutionPlan>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let output_schema = self.remote_fragment.output_schema();
        if input.schema() != output_schema {
            return internal_err!(
                "partial reduce input schema mismatch: expected {:?}, got {:?}",
                output_schema,
                input.schema()
            );
        }

        let aggregate = AggregateExec::try_new(
            AggregateMode::PartialReduce,
            self.group_by.as_final(),
            self.aggregate.clone(),
            vec![None; self.aggregate.len()],
            input,
            Arc::clone(&self.aggregate_input_schema),
        )?;
        if aggregate.schema() != output_schema {
            return internal_err!(
                "partial reduce output schema mismatch: expected {:?}, got {:?}",
                output_schema,
                aggregate.schema()
            );
        }
        Ok(Arc::new(aggregate))
    }
}

/// Checks the semantic restrictions that make a DataFusion partial aggregate
/// safe to clone into independent local and remote placement branches.
fn is_supported_partial_aggregate(aggregate: &AggregateExec) -> bool {
    let group_by = aggregate.group_expr();
    let global_grouping = group_by.is_true_no_grouping();
    if aggregate.mode() != &AggregateMode::Partial
        || aggregate.input_order_mode() != &InputOrderMode::Linear
        || aggregate.aggr_expr().is_empty()
        || aggregate.limit_options().is_some()
        || aggregate.properties().output_ordering().is_some()
        || group_by.has_grouping_set()
        || !is_ordinary_grouping(group_by)
        || !group_by.null_expr().is_empty()
        || aggregate
            .filter_expr()
            .iter()
            .flatten()
            .any(|expression| !is_remote_safe_expression(expression))
        || group_by
            .expr()
            .iter()
            .any(|(expression, _)| !is_remote_safe_expression(expression))
    {
        return false;
    }

    aggregate
        .aggr_expr()
        .iter()
        .all(|expression| is_supported_aggregate_expression(expression, global_grouping))
}

/// Accepts a normal GROUP BY (including no grouping), but rejects grouping
/// sets and the null-placeholder layouts they require.
fn is_ordinary_grouping(group_by: &PhysicalGroupBy) -> bool {
    group_by.is_true_no_grouping()
        || (group_by.groups().len() == 1 && group_by.groups()[0].iter().all(|is_null| !*is_null))
}

/// Checks whether an order-insensitive aggregate can construct the accumulator
/// implementation needed by `PartialReduce`. Wire support is preflighted when
/// the complete fragment is serialized.
fn is_supported_aggregate_expression(
    expression: &Arc<AggregateFunctionExpr>,
    global_grouping: bool,
) -> bool {
    if expression.is_distinct()
        || expression.ignore_nulls()
        || expression.is_reversed()
        || !expression.order_bys().is_empty()
        || expression
            .expressions()
            .iter()
            .any(|expression| !is_remote_safe_expression(expression))
    {
        return false;
    }

    if global_grouping || !expression.groups_accumulator_supported() {
        expression.create_accumulator().is_ok()
    } else {
        expression.create_groups_accumulator().is_ok()
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

/// Recognizes DataFusion's partial-aggregation shape and replaces only its scan
/// side with placement-specific copies of the generic remote fragment.
fn rewrite_partial_aggregate(
    plan: Arc<dyn ExecutionPlan>,
) -> Result<Transformed<Arc<dyn ExecutionPlan>>> {
    let Some(aggregate) = plan.downcast_ref::<AggregateExec>() else {
        return Ok(Transformed::no(plan));
    };

    let Some(input) = extract_fragment_input(aggregate) else {
        return Ok(Transformed::no(plan));
    };

    if let Some(scan) = input.scan.downcast_ref::<LocationAwareScanExec>() {
        if !scan.supports_fragment_pushdown() {
            return Ok(Transformed::no(plan));
        }
    } else if let Some(remote) = input.scan.downcast_ref::<RemoteNodeExec>() {
        if !remote.can_accept_fragment() {
            return Ok(Transformed::no(plan));
        }
    } else {
        // Local-only and unrelated inputs gain nothing from this rewrite.
        return Ok(Transformed::no(plan));
    }

    let Some(fragment) =
        PartialAggregateFragment::from_aggregate(aggregate, &input.operators, input.scan.schema())
    else {
        return Ok(Transformed::no(plan));
    };
    let rewritten_scan = if let Some(scan) = input.scan.downcast_ref::<LocationAwareScanExec>() {
        scan.with_fragment(fragment.fragment())?
    } else if let Some(remote) = input.scan.downcast_ref::<RemoteNodeExec>() {
        remote.with_fragment(fragment.fragment())?
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

/// The scan and row-wise operators that become one partial-aggregate fragment.
struct FragmentInput {
    scan: Arc<dyn ExecutionPlan>,
    operators: Vec<Arc<dyn ExecutionPlan>>,
}

/// Peels stable row-wise operators and the optional round-robin repartition
/// that DataFusion may place between a partial aggregate and its scan.
fn extract_fragment_input(aggregate: &AggregateExec) -> Option<FragmentInput> {
    let mut input = Arc::clone(aggregate.input());
    let mut operators = Vec::new();
    let mut has_repartition = false;
    loop {
        if is_row_wise_operator(&input) {
            operators.push(Arc::clone(&input));
            input = Arc::clone(input.children()[0]);
        } else if let Some(repartition) = input.downcast_ref::<RepartitionExec>() {
            if has_repartition
                || !matches!(repartition.partitioning(), Partitioning::RoundRobinBatch(_))
            {
                return None;
            }
            has_repartition = true;
            input = Arc::clone(repartition.input());
        } else {
            break;
        }
    }
    Some(FragmentInput {
        scan: input,
        operators,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use datafusion::arrow::array::{Float64Array, Int64Array, StringArray};
    use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
    use datafusion::arrow::record_batch::RecordBatch;
    use datafusion::datasource::TableProvider;
    use datafusion::execution::context::SessionContext;
    use datafusion::functions_aggregate::average::avg_udaf;
    use datafusion::functions_aggregate::count::count_udaf;
    use datafusion::functions_aggregate::min_max::{max_udaf, min_udaf};
    use datafusion::functions_aggregate::sum::sum_udaf;
    use datafusion::physical_expr::aggregate::AggregateExprBuilder;
    use datafusion::physical_expr_common::physical_expr::is_volatile;
    use datafusion::physical_plan::aggregates::LimitOptions;
    use datafusion::physical_plan::coalesce_partitions::CoalescePartitionsExec;
    use datafusion::physical_plan::empty::EmptyExec;
    use datafusion::physical_plan::expressions::{Column, Literal};
    use datafusion::physical_plan::filter::{FilterExec, FilterExecBuilder};
    use datafusion::prelude::{col, lit};
    use datafusion::scalar::ScalarValue;

    use super::*;
    use crate::filter::FirstMatchingPartitionKeyExtractor;
    use crate::mocks::{LocatedTestScanner, TwoPartitions};
    use crate::partitioned_table_provider::PartitionedTableProvider;
    use crate::remote_fragment::RemoteFragment;

    fn located_test_scanner(schema: &SchemaRef) -> LocatedTestScanner {
        let batch = |groups: &[&str], values: &[f64]| {
            RecordBatch::try_new(
                Arc::clone(schema),
                vec![
                    Arc::new(StringArray::from(groups.to_vec())),
                    Arc::new(Float64Array::from(values.to_vec())),
                ],
            )
            .expect("test batch")
        };
        LocatedTestScanner::new(
            batch(&["a", "a", "b"], &[10.0, 20.0, 5.0]),
            batch(&["a", "b", "b"], &[3.0, 7.0, 8.0]),
        )
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
            located_test_scanner(&schema),
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
            located_test_scanner(&schema),
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
        let fragment_input = extract_fragment_input(&partial).expect("partial fragment input");
        assert!(
            PartialAggregateFragment::from_aggregate(
                &partial
                    .clone()
                    .with_limit_options(Some(LimitOptions::new_with_order(10, true))),
                &fragment_input.operators,
                fragment_input.scan.schema(),
            )
            .is_none(),
            "aggregate TopK must retain its existing coordinator plan"
        );

        let fragment = PartialAggregateFragment::from_aggregate(
            &partial,
            &fragment_input.operators,
            fragment_input.scan.schema(),
        )
        .expect("supported fragment");
        let remote_fragment = fragment.fragment();
        let wire = remote_fragment.to_wire();
        let decoded = RemoteFragment::from_wire(&wire, &context.task_ctx(), &schema)
            .expect("fragment deserialization")
            .expect("compatible fragment");
        assert_eq!(decoded.output_schema(), fragment.fragment().output_schema());

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
                .is_some_and(|remote| !remote.can_accept_fragment())
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
            display.contains("RemoteNodeExec: target_node=N2:1, fragment=[AggregateExec"),
            "{display}"
        );
        assert!(display.contains(" -> FilterExec: value@1 > 6"), "{display}");
        assert!(display.contains("predicate=value@1 > 6"), "{display}");

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
        let input = extract_fragment_input(&partial).expect("count fragment input");
        let fragment = PartialAggregateFragment::from_aggregate(
            &partial,
            &input.operators,
            input.scan.schema(),
        )
        .expect("filtered count fragment");
        let wire = fragment.fragment().to_wire();
        let decoded = RemoteFragment::from_wire(&wire, &context.task_ctx(), &input.scan.schema())
            .expect("filtered count deserialization")
            .expect("compatible filtered count fragment");
        assert_eq!(decoded.output_schema(), fragment.fragment().output_schema());
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
    async fn pushes_aggregate_filters_and_non_allowlisted_accumulators() {
        let context = aggregation_test_context();
        let plan = context
            .sql(
                r#"SELECT "group",
                          COUNT(*) FILTER (WHERE value > 7) AS high_values,
                          STDDEV(value) FILTER (WHERE value > 6) AS spread
                   FROM test_values
                   GROUP BY "group""#,
            )
            .await
            .expect("filtered aggregate query")
            .create_physical_plan()
            .await
            .expect("filtered aggregate plan");
        let optimized = PartialAggregationPushdown
            .optimize(plan, &ConfigOptions::new())
            .expect("filtered aggregate pushdown");
        let display = datafusion::physical_plan::displayable(optimized.as_ref())
            .indent(true)
            .to_string();
        assert!(display.contains("mode=PartialReduce"), "{display}");
        assert!(display.contains("fragment=[AggregateExec"), "{display}");
        assert!(display.contains("stddev("), "{display}");

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
            for row in 0..batch.num_rows() {
                result.insert(
                    groups.value(row).to_owned(),
                    (
                        int64_column(&batch, 1).value(row),
                        float64_column(&batch, 2).value(row),
                    ),
                );
            }
        }
        assert_eq!(result["a"].0, 2);
        assert!((result["a"].1 - 50.0_f64.sqrt()).abs() < f64::EPSILON);
        assert_eq!(result["b"].0, 1);
        assert!((result["b"].1 - 0.5_f64.sqrt()).abs() < f64::EPSILON);
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
        let input = extract_fragment_input(&partial).expect("volatile fragment input");
        assert!(
            input
                .operators
                .iter()
                .find_map(|operator| operator.downcast_ref::<FilterExec>())
                .is_some_and(|filter| is_volatile(filter.predicate()))
        );
        let volatile_plan = PartialAggregationPushdown
            .optimize(volatile_plan, &ConfigOptions::new())
            .expect("volatile filter remains unchanged");
        let display = datafusion::physical_plan::displayable(volatile_plan.as_ref())
            .indent(true)
            .to_string();
        assert!(!display.contains("mode=PartialReduce"), "{display}");
        assert!(!display.contains("fragment=[AggregateExec"), "{display}");

        let volatile_aggregate = context
            .sql("SELECT SUM(random()) FROM test_values")
            .await
            .expect("volatile aggregate query")
            .create_physical_plan()
            .await
            .expect("volatile aggregate plan");
        let volatile_aggregate = PartialAggregationPushdown
            .optimize(volatile_aggregate, &ConfigOptions::new())
            .expect("volatile aggregate remains unchanged");
        let display = datafusion::physical_plan::displayable(volatile_aggregate.as_ref())
            .indent(true)
            .to_string();
        assert!(!display.contains("mode=PartialReduce"), "{display}");
        assert!(!display.contains("fragment=[AggregateExec"), "{display}");
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
            located_test_scanner(&schema),
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

        assert!(
            PartialAggregateFragment::from_aggregate(&aggregate, &[], aggregate.input_schema())
                .is_none()
        );
    }
}
