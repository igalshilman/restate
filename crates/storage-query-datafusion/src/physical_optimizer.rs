// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use datafusion::common::tree_node::{Transformed, TreeNode};
use datafusion::config::ConfigOptions;
use datafusion::physical_optimizer::PhysicalOptimizerRule;
use datafusion::physical_plan::coalesce_batches::CoalesceBatchesExec;
use datafusion::physical_plan::joins::{
    HashJoinExec, StreamJoinPartitionMode, SymmetricHashJoinExec,
};
use datafusion::physical_plan::repartition::RepartitionExec;
use datafusion::physical_plan::{ExecutionPlan, Partitioning};
use std::sync::Arc;

pub(crate) struct JoinRewrite;

impl JoinRewrite {
    pub(crate) fn new() -> Self {
        Self {}
    }
}

impl PhysicalOptimizerRule for JoinRewrite {
    fn optimize(
        &self,
        plan: Arc<dyn ExecutionPlan>,
        _config: &ConfigOptions,
    ) -> datafusion::common::Result<Arc<dyn ExecutionPlan>> {
        plan.transform_up(&|plan| {
            let Some(hash_join) = plan.as_any().downcast_ref::<HashJoinExec>() else {
                return Ok(Transformed::No(plan));
            };

            let has_partition_key = hash_join
                .on()
                .iter()
                .any(|(l, r)| l.name() == "partition_key" || r.name() == "partition_key");

            if !has_partition_key {
                return Ok(Transformed::No(plan));
            }

            let left_partition_count = hash_join.left().output_partitioning().partition_count();
            let right_partition_count = hash_join.right().output_partitioning().partition_count();

            let mut left: Arc<dyn ExecutionPlan> = hash_join.left().clone();
            let mut right: Arc<dyn ExecutionPlan> = hash_join.right().clone();

            if left_partition_count != right_partition_count {
                if left_partition_count != _config.execution.target_partitions {
                    left = configure_repartition(
                        _config.execution.target_partitions,
                        _config.execution.batch_size,
                        hash_join.left(),
                    )?;
                }
                if right_partition_count != _config.execution.target_partitions {
                    right = configure_repartition(
                        _config.execution.target_partitions,
                        _config.execution.batch_size,
                        hash_join.right(),
                    )?;
                }
            }

            let Ok(new_plan) = SymmetricHashJoinExec::try_new(
                left,
                right,
                hash_join.on().to_vec(),
                hash_join.filter().cloned(),
                hash_join.join_type(),
                hash_join.null_equals_null(),
                None,
                None,
                StreamJoinPartitionMode::Partitioned,
            ) else {
                return Ok(Transformed::No(plan));
            };

            Ok(Transformed::Yes(Arc::new(new_plan)))
        })
    }

    fn name(&self) -> &str {
        "join_rewrite"
    }

    fn schema_check(&self) -> bool {
        true
    }
}

fn configure_repartition(
    required_partition_count: usize,
    batch_size: usize,
    input: &Arc<dyn ExecutionPlan>,
) -> datafusion::common::Result<Arc<dyn ExecutionPlan>> {
    let repartitioned = Arc::new(RepartitionExec::try_new(
        input.clone(),
        fixup_partition(required_partition_count, &input.output_partitioning()),
    )?);

   let plan = Arc::new(CoalesceBatchesExec::new(repartitioned, batch_size));
   Ok(plan)
}

fn fixup_partition(new_partition_count: usize, partition: &Partitioning) -> Partitioning {
    match partition {
        Partitioning::RoundRobinBatch(_) => Partitioning::RoundRobinBatch(new_partition_count),
        Partitioning::Hash(plan, _) => Partitioning::Hash(plan.to_vec(), new_partition_count),
        Partitioning::UnknownPartitioning(_) => {
            Partitioning::UnknownPartitioning(new_partition_count)
        }
    }
}
