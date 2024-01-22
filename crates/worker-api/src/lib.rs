// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use restate_schema_api::subscription::{Subscription, SubscriptionValidator};
use restate_types::invocation::InvocationTermination;
use restate_types::state_mut::ExternalStateMutation;
use std::future::Future;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("worker is unreachable")]
    Unreachable,
}

// This is just an interface to isolate the interaction between meta and subscription controller.
// Depending on how we evolve the Kafka ingress deployment, this might end up living in a separate process.
pub trait SubscriptionController: SubscriptionValidator {
    fn start_subscription(
        &self,
        subscription: Subscription,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    fn stop_subscription(
        &self,
        subscription_id: String,
    ) -> impl Future<Output = Result<(), Error>> + Send;
}

pub trait Handle: Clone {
    type SubscriptionControllerHandle: SubscriptionController + Send + Sync;

    /// Send a command to terminate an invocation. This command is best-effort.
    fn terminate_invocation(
        &self,
        invocation_termination: InvocationTermination,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Send a command to mutate a state. This command is best-effort.
    fn external_state_mutation(
        &self,
        mutation: ExternalStateMutation,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    fn subscription_controller_handle(&self) -> Self::SubscriptionControllerHandle;
}
