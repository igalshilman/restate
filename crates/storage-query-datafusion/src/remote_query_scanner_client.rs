// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
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

use async_trait::async_trait;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::DataFusionError;
use datafusion::execution::SendableRecordBatchStream;
use datafusion::physical_expr_common::physical_expr::snapshot_generation;
use datafusion::physical_plan::PhysicalExpr;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use futures::stream::{self, StreamExt};
use tracing::debug;

use restate_core::network::{Connection, NetworkSender, Networking, Swimlane, TransportConnect};
use restate_core::{Metadata, TaskCenter, TaskCenterFutureExt, TaskKind, task_center};
use restate_types::identifiers::PartitionId;
use restate_types::net::remote_query_scanner::{
    RemoteQueryScannerClose, RemoteQueryScannerNext, RemoteQueryScannerNextResult,
    RemoteQueryScannerOpen, RemoteQueryScannerOpened, RemoteQueryScannerPredicate, ScannerBatch,
    ScannerFailure, ScannerId,
};
use restate_types::sharding::KeyRange;
use restate_types::{
    GenerationalNodeId, NodeId, RESTATE_VERSION_1_8_0, RestateVersion, SemanticRestateVersion,
};

use crate::remote_fragment::RemoteFragmentExecution;
use crate::{decode_record_batch, encode_expr, encode_schema};

#[derive(derive_more::Debug)]
pub struct RemoteScanner {
    scanner_id: ScannerId,
    connection: Option<Connection>,
    #[cfg(test)]
    test_scanner: Option<TestRemoteScanner>,
}

#[cfg(test)]
#[derive(Debug)]
struct TestRemoteScanner {
    requests: tokio::sync::mpsc::UnboundedSender<Option<RemoteQueryScannerPredicate>>,
    responses: tokio::sync::mpsc::UnboundedReceiver<RemoteQueryScannerNextResult>,
    closed: tokio::sync::watch::Sender<bool>,
    close_on_drop: bool,
}

#[cfg(test)]
impl Drop for TestRemoteScanner {
    fn drop(&mut self) {
        if self.close_on_drop {
            let _ = self.closed.send(true);
        }
    }
}

impl RemoteScanner {
    /// Constructs a scanner that owns `connection` for the purpose of sending
    /// `Close` on drop. Use this to install a drop-guard *before* sending
    /// `Open`: if the caller's future is cancelled (or the proxy returns
    /// `Err`) after `Open` reaches the wire, the existing `Drop` impl emits
    /// `Close` so the server doesn't keep an orphan scanner until TTL.
    fn new(scanner_id: ScannerId, connection: Connection) -> Self {
        Self {
            scanner_id,
            connection: Some(connection),
            #[cfg(test)]
            test_scanner: None,
        }
    }

    #[cfg(test)]
    fn for_test(
        scanner_id: ScannerId,
        requests: tokio::sync::mpsc::UnboundedSender<Option<RemoteQueryScannerPredicate>>,
        responses: tokio::sync::mpsc::UnboundedReceiver<RemoteQueryScannerNextResult>,
        closed: tokio::sync::watch::Sender<bool>,
    ) -> Self {
        Self {
            scanner_id,
            connection: None,
            test_scanner: Some(TestRemoteScanner {
                requests,
                responses,
                closed,
                close_on_drop: true,
            }),
        }
    }

    async fn next_batch(
        &mut self,
        next_predicate: Option<RemoteQueryScannerPredicate>,
    ) -> Result<RemoteQueryScannerNextResult, DataFusionError> {
        #[cfg(test)]
        if let Some(scanner) = self.test_scanner.as_mut() {
            scanner.requests.send(next_predicate).map_err(|_| {
                DataFusionError::Internal("test scanner request receiver dropped".to_owned())
            })?;
            return scanner.responses.recv().await.ok_or_else(|| {
                DataFusionError::Internal("test scanner response sender dropped".to_owned())
            });
        }

        let Some(ref connection) = self.connection else {
            return Err(DataFusionError::Internal(
                "connection used after forget()".to_string(),
            ));
        };
        let peer = connection.peer();
        let permit = connection.reserve().await.ok_or_else(|| {
            DataFusionError::External(
                anyhow::anyhow!(
                    "remote scanner {} connection lost to {peer}",
                    self.scanner_id
                )
                .into(),
            )
        })?;

        let reply = permit
            .send_rpc(
                RemoteQueryScannerNext {
                    scanner_id: self.scanner_id,
                    next_predicate,
                },
                None,
            )
            .map_err(|e| DataFusionError::Internal(e.to_string()))?;

        reply.await.map_err(|e| DataFusionError::External(e.into()))
    }

    /// The scanner will not auto close the remote scanner on drop
    fn forget(mut self) {
        self.connection.take();
        #[cfg(test)]
        if let Some(scanner) = self.test_scanner.as_mut() {
            scanner.close_on_drop = false;
        }
    }
}

impl Drop for RemoteScanner {
    fn drop(&mut self) {
        #[cfg(test)]
        if self.test_scanner.is_some() {
            return;
        }

        let scanner_id = self.scanner_id;
        if let Some(connection) = self.connection.take() {
            tokio::spawn(async move {
                let Some(permit) = connection.reserve().await else {
                    return;
                };
                debug!(
                    "Closing remote scanner {scanner_id} remotely for {}",
                    connection.peer()
                );
                // Ideally, this should be a unary call, but to maintain compatibility
                // with previous version we keep this as rpc.
                // todo (lo-pri): migrate this to a unary call.
                let Ok(reply) = permit.send_rpc(RemoteQueryScannerClose { scanner_id }, None)
                else {
                    return;
                };

                let _ = reply.await;
            });
        }
    }
}

// ----- rpc service definition -----

#[async_trait]
pub trait RemoteScannerService: Send + Sync + Debug + 'static {
    async fn open(
        &self,
        peer: NodeId,
        req: RemoteQueryScannerOpen,
    ) -> Result<OpenedRemoteScanner, DataFusionError>;
}

/// Result of the open handshake, distinguishing the schema that subsequent
/// batches carry. A `Raw` result makes the client execute any requested
/// fragment locally; `Fragment` means the worker already applied it.
pub enum OpenedRemoteScanner {
    Raw(RemoteScanner),
    Fragment(RemoteScanner),
}

/// Wire-level open result kept separate from [`OpenedRemoteScanner`] so a
/// rejected request can disarm the speculative close guard safely.
#[derive(Debug)]
enum OpenResponse {
    Opened {
        scanner_id: ScannerId,
        fragment_applied: bool,
    },
    Rejected(String),
}

// ----- service proxy -----
pub fn create_remote_scanner_service<T: TransportConnect>(
    network: Networking<T>,
) -> Arc<dyn RemoteScannerService> {
    Arc::new(RemoteScannerServiceProxy::new(
        network,
        TaskCenter::current(),
    ))
}

// ----- datafusion remote scan -----

/// Given an implementation of a remote ScannerService, this function
/// creates a DataFusion [[SendableRecordBatchStream]] that transports
/// record batches via the RemoteScannerService API.
///
/// `scanner_id` is allocated by the caller so the server can adopt the caller's
/// id instead of minting its own.
#[allow(clippy::too_many_arguments)]
pub fn remote_scan_as_datafusion_stream(
    service: Arc<dyn RemoteScannerService>,
    target_node_id: NodeId,
    scanner_id: ScannerId,
    partition_id: PartitionId,
    range: KeyRange,
    table_name: String,
    projection_schema: SchemaRef,
    predicate: Option<Arc<dyn PhysicalExpr>>,
    batch_size: usize,
    limit: Option<usize>,
    expected_partition_owner: Option<GenerationalNodeId>,
) -> SendableRecordBatchStream {
    remote_scan(
        service,
        target_node_id,
        scanner_id,
        partition_id,
        range,
        table_name,
        projection_schema,
        predicate,
        batch_size,
        limit,
        expected_partition_owner,
        None,
    )
}

/// Builds a lazy remote cursor with optional fragment negotiation. If the peer
/// declines the fragment, the same cursor becomes its raw, local input.
#[allow(clippy::too_many_arguments)]
pub(crate) fn remote_scan(
    service: Arc<dyn RemoteScannerService>,
    target_node_id: NodeId,
    scanner_id: ScannerId,
    partition_id: PartitionId,
    range: KeyRange,
    table_name: String,
    projection_schema: SchemaRef,
    predicate: Option<Arc<dyn PhysicalExpr>>,
    batch_size: usize,
    limit: Option<usize>,
    expected_partition_owner: Option<GenerationalNodeId>,
    fragment: Option<RemoteFragmentExecution>,
) -> SendableRecordBatchStream {
    let predicate_generation = predicate.as_ref().map(snapshot_generation).unwrap_or(0);
    let output_schema = fragment
        .as_ref()
        .map(RemoteFragmentExecution::output_schema)
        .unwrap_or_else(|| Arc::clone(&projection_schema));

    let initial_predicate = match predicate.as_ref().map(encode_predicate).transpose() {
        Ok(predicate) => predicate,
        Err(error) => return error_stream(output_schema, error),
    };
    let state = RemoteCursorState::Opening {
        service,
        target_node_id,
        request: Box::new(RemoteQueryScannerOpen {
            scanner_id: Some(scanner_id),
            partition_id,
            range,
            table: table_name,
            projection_schema_bytes: encode_schema(&projection_schema),
            limit: limit.map(|limit| u64::try_from(limit).expect("limit to fit in a u64")),
            predicate: initial_predicate,
            batch_size: u64::try_from(batch_size).expect("batch_size to fit in a u64"),
            expected_partition_owner,
            fragment: fragment.as_ref().map(RemoteFragmentExecution::to_wire),
        }),
        raw_schema: projection_schema,
        output_schema: Arc::clone(&output_schema),
        fragment,
    };

    remote_cursor_stream(
        RemoteCursor {
            predicate,
            predicate_generation,
            state,
        },
        output_schema,
    )
}

/// Lifecycle states for a pull-driven remote scan. State-specific data lives in
/// the corresponding variant so, for example, a fragment can only be consumed
/// while the opening handshake is unresolved.
enum RemoteCursorState {
    Opening {
        service: Arc<dyn RemoteScannerService>,
        target_node_id: NodeId,
        request: Box<RemoteQueryScannerOpen>,
        raw_schema: SchemaRef,
        output_schema: SchemaRef,
        fragment: Option<RemoteFragmentExecution>,
    },
    Ready(RemoteScanner, SchemaRef),
    Fallback(SendableRecordBatchStream),
    Done,
}

/// Mutable client-side state carried between successive downstream pulls.
/// Nothing opens or advances the remote scanner until [`Self::next`] is called.
struct RemoteCursor {
    predicate: Option<Arc<dyn PhysicalExpr>>,
    predicate_generation: u64,
    state: RemoteCursorState,
}

impl RemoteCursor {
    /// Performs one downstream pull. A declined fragment transfers the raw
    /// scanner and dynamic-predicate state into a nested cursor, then exposes
    /// only batches produced by the local fragment wrapped around that cursor.
    async fn next(mut self) -> Result<Option<(RecordBatch, Self)>, DataFusionError> {
        loop {
            match std::mem::replace(&mut self.state, RemoteCursorState::Done) {
                RemoteCursorState::Opening {
                    service,
                    target_node_id,
                    request,
                    raw_schema,
                    output_schema,
                    fragment,
                } => match (fragment, service.open(target_node_id, *request).await?) {
                    (Some(fragment), OpenedRemoteScanner::Raw(scanner)) => {
                        fragment.record_remote_decline();
                        debug!(
                            %target_node_id,
                            "Remote scanner declined a physical fragment; executing it at the coordinator"
                        );
                        let raw_stream = remote_cursor_stream(
                            Self {
                                predicate: self.predicate.take(),
                                predicate_generation: self.predicate_generation,
                                state: RemoteCursorState::Ready(scanner, Arc::clone(&raw_schema)),
                            },
                            raw_schema,
                        );
                        self.state = RemoteCursorState::Fallback(fragment.execute(raw_stream)?);
                    }
                    (Some(fragment), OpenedRemoteScanner::Fragment(scanner)) => {
                        fragment.record_remote_acceptance();
                        debug!(%target_node_id, "Remote scanner accepted a physical fragment");
                        self.state = RemoteCursorState::Ready(scanner, output_schema);
                    }
                    (None, OpenedRemoteScanner::Raw(scanner)) => {
                        self.state = RemoteCursorState::Ready(scanner, output_schema);
                    }
                    (None, OpenedRemoteScanner::Fragment(_)) => {
                        return Err(DataFusionError::Internal(
                            "remote scanner applied an unrequested fragment".to_owned(),
                        ));
                    }
                },
                RemoteCursorState::Ready(mut scanner, expected_schema) => {
                    let next_predicate =
                        next_predicate(&mut self.predicate_generation, self.predicate.as_ref())?;
                    match scanner.next_batch(next_predicate).await? {
                        RemoteQueryScannerNextResult::NextBatch(ScannerBatch {
                            record_batch,
                            ..
                        }) => {
                            let batch = decode_record_batch(&record_batch)?;
                            if batch.schema() != expected_schema {
                                return Err(DataFusionError::Internal(format!(
                                    "remote scanner returned an unexpected schema: expected {:?}, got {:?}",
                                    expected_schema,
                                    batch.schema()
                                )));
                            }
                            self.state = RemoteCursorState::Ready(scanner, expected_schema);
                            return Ok(Some((batch, self)));
                        }
                        RemoteQueryScannerNextResult::NoMoreRecords(_) => {
                            scanner.forget();
                            return Ok(None);
                        }
                        RemoteQueryScannerNextResult::Failure(ScannerFailure {
                            message, ..
                        }) => {
                            scanner.forget();
                            return Err(DataFusionError::Internal(message));
                        }
                        RemoteQueryScannerNextResult::NoSuchScanner(_) => {
                            scanner.forget();
                            return Err(DataFusionError::Internal(
                                "No such scanner. It could have expired due to a long period of inactivity."
                                    .to_string(),
                            ));
                        }
                        RemoteQueryScannerNextResult::Unknown => {
                            return Err(DataFusionError::Internal(
                                "Received unknown scanner result".to_owned(),
                            ));
                        }
                    }
                }
                RemoteCursorState::Fallback(mut stream) => match stream.next().await.transpose()? {
                    Some(batch) => {
                        self.state = RemoteCursorState::Fallback(stream);
                        return Ok(Some((batch, self)));
                    }
                    None => return Ok(None),
                },
                RemoteCursorState::Done => return Ok(None),
            }
        }
    }
}

/// Wraps a cursor in DataFusion's stream interface without scheduling any work
/// ahead of the consumer's next poll.
fn remote_cursor_stream(cursor: RemoteCursor, schema: SchemaRef) -> SendableRecordBatchStream {
    let stream = stream::try_unfold(cursor, RemoteCursor::next);
    Box::pin(RecordBatchStreamAdapter::new(schema, stream))
}

fn error_stream(schema: SchemaRef, error: DataFusionError) -> SendableRecordBatchStream {
    let stream = stream::once(async move { Err(error) });
    Box::pin(RecordBatchStreamAdapter::new(schema, stream))
}

fn encode_predicate(
    predicate: &Arc<dyn PhysicalExpr>,
) -> Result<RemoteQueryScannerPredicate, DataFusionError> {
    Ok(RemoteQueryScannerPredicate {
        serialized_physical_expression: encode_expr(predicate)?,
    })
}

/// Serializes a dynamic predicate only when its generation changed since the
/// previous pull. Generation zero denotes a static predicate.
fn next_predicate(
    predicate_generation: &mut u64,
    predicate: Option<&Arc<dyn PhysicalExpr>>,
) -> Result<Option<RemoteQueryScannerPredicate>, DataFusionError> {
    // Generation zero means the predicate is static (or absent).
    if *predicate_generation == 0 {
        return Ok(None);
    }

    let predicate = predicate.ok_or(DataFusionError::Internal(
        "Missing predicate despite non-zero predicate generation".into(),
    ))?;
    let current_generation = snapshot_generation(predicate);
    if current_generation == *predicate_generation {
        return Ok(None);
    }

    *predicate_generation = current_generation;
    encode_predicate(predicate).map(Some)
}

// ----- everything below is the client side implementation details -----

#[derive(Clone)]
struct RemoteScannerServiceProxy<T> {
    networking: Networking<T>,
    task_center: task_center::Handle,
}

impl<T> Debug for RemoteScannerServiceProxy<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemoteScannerServiceProxy")
    }
}

impl<T: TransportConnect> RemoteScannerServiceProxy<T> {
    fn new(networking: Networking<T>, task_center: task_center::Handle) -> Self {
        Self {
            networking,
            task_center,
        }
    }
}

#[async_trait]
impl<T: TransportConnect> RemoteScannerService for RemoteScannerServiceProxy<T> {
    async fn open(
        &self,
        peer: NodeId,
        req: RemoteQueryScannerOpen,
    ) -> Result<OpenedRemoteScanner, DataFusionError> {
        let partition_id = req.partition_id;
        let expected_partition_owner = req.expected_partition_owner;
        let connection = self
            .networking
            .get_connection(peer, Swimlane::Datafusion)
            .in_tc_as_task(
                &self.task_center,
                TaskKind::InPlace,
                "RemoteScannerServiceProxy::open",
            )
            .await
            .map_err(|e| DataFusionError::External(e.into()))?;
        let allow_legacy_owner_response = expected_partition_owner.is_some_and(|expected_owner| {
            expected_owner == connection.peer() && peer_predates_owner_validation(expected_owner)
        });

        // We always set the client minted scanner-id
        let scanner_id = req.scanner_id.unwrap();

        // Reserve and send Open. `send_rpc` is synchronous after the permit
        // is in hand — by the time it returns the message is queued on the
        // egress and the server is committed to seeing it.
        let open_permit = connection.reserve().await.ok_or_else(|| {
            DataFusionError::External(
                anyhow::anyhow!("cannot open remote scanner; connection lost to {peer}").into(),
            )
        })?;
        let open_reply = open_permit
            .send_rpc(req, None)
            .map_err(|e| DataFusionError::Internal(e.to_string()))?;

        // From here on we must guarantee a `Close` reaches the server if we
        // don't hand a `RemoteScanner` back to the caller — otherwise the
        // scanner the server is about to create sits orphaned until TTL.
        // Pre-constructing the scanner installs its own `Drop` as the guard;
        // it fires `Close` on cancellation or any `Err` return below.
        // On `Failure` we disarm via `.forget()` so we don't accidentally close a scanner
        // that another caller holds under the same id.
        let mut remote_scanner = RemoteScanner::new(scanner_id, connection.clone());

        let response = open_reply
            .await
            .map_err(|e| DataFusionError::External(e.into()))?;
        let (scanner_id, fragment_applied) = match interpret_open_response(
            response,
            peer,
            partition_id,
            expected_partition_owner,
            allow_legacy_owner_response,
        )? {
            OpenResponse::Opened {
                scanner_id,
                fragment_applied,
            } => (scanner_id, fragment_applied),
            OpenResponse::Rejected(message) => {
                remote_scanner.forget();
                return Err(DataFusionError::Internal(message));
            }
        };

        // A pre-v1.7 server can return a different scanner id.
        if remote_scanner.scanner_id != scanner_id {
            remote_scanner.forget();
            remote_scanner = RemoteScanner::new(scanner_id, connection);
        }
        Ok(if fragment_applied {
            OpenedRemoteScanner::Fragment(remote_scanner)
        } else {
            OpenedRemoteScanner::Raw(remote_scanner)
        })
    }
}

/// Converts the versioned open acknowledgement into cursor state. A legacy
/// success is accepted for an owner-routed scan only from an exact peer
/// generation positively identified as predating the acknowledgement.
fn interpret_open_response(
    response: RemoteQueryScannerOpened,
    peer: NodeId,
    partition_id: PartitionId,
    expected_partition_owner: Option<GenerationalNodeId>,
    allow_legacy_owner_response: bool,
) -> Result<OpenResponse, DataFusionError> {
    match response {
        RemoteQueryScannerOpened::Success { scanner_id } => {
            if let Some(expected_owner) = expected_partition_owner {
                if !allow_legacy_owner_response {
                    return Err(DataFusionError::Internal(format!(
                        "remote node {peer} opened partition {partition_id} without acknowledging validation of planned owner {expected_owner}"
                    )));
                }
                debug!(
                    %peer,
                    %partition_id,
                    "Accepting legacy owner acknowledgement from a known pre-v1.8 peer"
                );
            }
            Ok(OpenResponse::Opened {
                scanner_id,
                fragment_applied: false,
            })
        }
        RemoteQueryScannerOpened::SuccessWithFragment { scanner_id } => Ok(OpenResponse::Opened {
            scanner_id,
            fragment_applied: true,
        }),
        RemoteQueryScannerOpened::SuccessWithOwnerValidation { scanner_id } => {
            Ok(OpenResponse::Opened {
                scanner_id,
                fragment_applied: false,
            })
        }
        RemoteQueryScannerOpened::Failure => Ok(OpenResponse::Rejected(
            "unable to open a remote scanner".to_owned(),
        )),
        RemoteQueryScannerOpened::FailureWithMessage { message } => {
            Ok(OpenResponse::Rejected(message))
        }
    }
}

/// Allows the rolling-upgrade compatibility path only when cluster metadata
/// identifies the exact connected node generation as a pre-v1.8 binary.
/// Missing, unknown, malformed, or current versions retain strict fencing.
fn peer_predates_owner_validation(peer: GenerationalNodeId) -> bool {
    Metadata::with_current(|metadata| {
        let nodes_config = metadata.nodes_config_ref();
        nodes_config
            .find_node_by_id(peer)
            .ok()
            .and_then(|node| node.binary_version.as_ref())
            .is_some_and(binary_version_predates_owner_validation)
    })
}

fn binary_version_predates_owner_validation(binary_version: &RestateVersion) -> bool {
    if binary_version.as_str() == RestateVersion::UNKNOWN_STR {
        return false;
    }
    SemanticRestateVersion::try_from(binary_version)
        .is_ok_and(|version| !version.is_equal_or_newer_than(&RESTATE_VERSION_1_8_0))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use datafusion::arrow::array::{BooleanArray, Int64Array};
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::arrow::record_batch::RecordBatch;
    use datafusion::execution::TaskContext;
    use datafusion::functions_aggregate::count::count_udaf;
    use datafusion::logical_expr::Operator;
    use datafusion::physical_expr::aggregate::AggregateExprBuilder;
    use datafusion::physical_expr::expressions::{
        BinaryExpr, Column, DynamicFilterPhysicalExpr, Literal,
    };
    use datafusion::physical_plan::aggregates::{AggregateExec, AggregateMode, PhysicalGroupBy};
    use datafusion::physical_plan::metrics::{ExecutionPlanMetricsSet, MetricValue};
    use datafusion::scalar::ScalarValue;

    use super::*;
    use crate::decode_expr;
    use crate::remote_fragment::{
        FragmentLeafExec, REMOTE_FRAGMENT_ACCEPTED_METRIC, REMOTE_FRAGMENT_DECLINED_METRIC,
        RemoteFragment, RemoteFragmentExecution,
    };

    #[derive(Debug)]
    struct TestScannerService {
        scanner: Mutex<Option<RemoteScanner>>,
        open_requests: tokio::sync::mpsc::UnboundedSender<RemoteQueryScannerOpen>,
        apply_fragments: bool,
    }

    #[async_trait]
    impl RemoteScannerService for TestScannerService {
        async fn open(
            &self,
            _peer: NodeId,
            request: RemoteQueryScannerOpen,
        ) -> Result<OpenedRemoteScanner, DataFusionError> {
            let fragment_requested = request.fragment.is_some();
            self.open_requests.send(request).map_err(|_| {
                DataFusionError::Internal("test scanner open receiver dropped".to_owned())
            })?;
            let scanner = self
                .scanner
                .lock()
                .expect("test scanner lock")
                .take()
                .ok_or_else(|| DataFusionError::Internal("test scanner reopened".to_owned()))?;
            Ok(if self.apply_fragments && fragment_requested {
                OpenedRemoteScanner::Fragment(scanner)
            } else {
                OpenedRemoteScanner::Raw(scanner)
            })
        }
    }

    struct TestScannerChannels {
        service: Arc<dyn RemoteScannerService>,
        open_requests: tokio::sync::mpsc::UnboundedReceiver<RemoteQueryScannerOpen>,
        next_requests: tokio::sync::mpsc::UnboundedReceiver<Option<RemoteQueryScannerPredicate>>,
        responses: tokio::sync::mpsc::UnboundedSender<RemoteQueryScannerNextResult>,
        closed: tokio::sync::watch::Receiver<bool>,
    }

    fn test_scanner_channels(scanner_id: ScannerId) -> TestScannerChannels {
        test_scanner_channels_with_fragment_support(scanner_id, false)
    }

    fn test_scanner_channels_with_fragment_support(
        scanner_id: ScannerId,
        apply_fragments: bool,
    ) -> TestScannerChannels {
        let (open_requests, open_request_rx) = tokio::sync::mpsc::unbounded_channel();
        let (next_requests, next_request_rx) = tokio::sync::mpsc::unbounded_channel();
        let (responses, response_rx) = tokio::sync::mpsc::unbounded_channel();
        let (closed, close_rx) = tokio::sync::watch::channel(false);
        let scanner = RemoteScanner::for_test(scanner_id, next_requests, response_rx, closed);
        TestScannerChannels {
            service: Arc::new(TestScannerService {
                scanner: Mutex::new(Some(scanner)),
                open_requests,
                apply_fragments,
            }),
            open_requests: open_request_rx,
            next_requests: next_request_rx,
            responses,
            closed: close_rx,
        }
    }

    fn greater_than(column: &Arc<dyn PhysicalExpr>, value: i64) -> Arc<dyn PhysicalExpr> {
        Arc::new(BinaryExpr::new(
            Arc::clone(column),
            Operator::Gt,
            Arc::new(Literal::new(ScalarValue::Int64(Some(value)))),
        ))
    }

    fn metric_value(metrics: &ExecutionPlanMetricsSet, name: &str) -> usize {
        match metrics.clone_inner().sum_by_name(name) {
            Some(MetricValue::Count { count, .. }) => count.value(),
            other => panic!("expected count metric {name}, got {other:?}"),
        }
    }

    #[test]
    fn dynamic_predicate_is_sent_after_generation_update() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "value",
            DataType::Int64,
            false,
        )]));
        let column = Arc::new(Column::new("value", 0)) as Arc<dyn PhysicalExpr>;
        let dynamic = Arc::new(DynamicFilterPhysicalExpr::new(
            vec![Arc::clone(&column)],
            greater_than(&column, 10),
        ));
        let predicate = Arc::clone(&dynamic) as Arc<dyn PhysicalExpr>;
        let mut generation = snapshot_generation(&predicate);
        assert_ne!(generation, 0);
        assert!(
            next_predicate(&mut generation, Some(&predicate))
                .expect("unchanged dynamic predicate")
                .is_none()
        );

        dynamic
            .update(greater_than(&column, 20))
            .expect("dynamic predicate update");
        let current_generation = snapshot_generation(&predicate);
        assert_ne!(current_generation, generation);
        let update = next_predicate(&mut generation, Some(&predicate))
            .expect("updated dynamic predicate")
            .expect("new generation must be sent");
        assert_eq!(generation, current_generation);

        let decoded = decode_expr(
            &TaskContext::default(),
            schema.as_ref(),
            &update.serialized_physical_expression,
        )
        .expect("dynamic predicate payload");
        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(vec![19, 20, 21]))])
                .expect("predicate input");
        let values = decoded
            .evaluate(&batch)
            .expect("decoded predicate evaluation")
            .into_array(batch.num_rows())
            .expect("decoded predicate values");
        let values = values
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("boolean predicate values");
        assert_eq!(values, &BooleanArray::from(vec![false, false, true]));
        assert!(
            next_predicate(&mut generation, Some(&predicate))
                .expect("already sent predicate")
                .is_none()
        );
    }

    #[test]
    fn planned_owner_requires_an_explicit_server_acknowledgement() {
        let owner = GenerationalNodeId::new(2, 3);
        let scanner_id = ScannerId(owner, 4);
        let peer = NodeId::from(owner);

        let error = interpret_open_response(
            RemoteQueryScannerOpened::Success { scanner_id },
            peer,
            PartitionId::MIN,
            Some(owner),
            false,
        )
        .expect_err("legacy success must not bypass ownership fencing");
        assert!(
            error
                .to_string()
                .contains("without acknowledging validation")
        );

        let legacy = interpret_open_response(
            RemoteQueryScannerOpened::Success { scanner_id },
            peer,
            PartitionId::MIN,
            Some(owner),
            true,
        )
        .expect("a known pre-v1.8 peer may use its legacy acknowledgement");
        assert!(matches!(
            legacy,
            OpenResponse::Opened {
                scanner_id: id,
                fragment_applied: false,
            } if id == scanner_id
        ));

        let acknowledged = interpret_open_response(
            RemoteQueryScannerOpened::SuccessWithOwnerValidation { scanner_id },
            peer,
            PartitionId::MIN,
            Some(owner),
            false,
        )
        .expect("new server acknowledged ownership validation");
        assert!(matches!(
            acknowledged,
            OpenResponse::Opened {
                scanner_id: id,
                fragment_applied: false,
            } if id == scanner_id
        ));

        let rejected = interpret_open_response(
            RemoteQueryScannerOpened::FailureWithMessage {
                message: "planned owner N2:3, current owner N2:4".to_owned(),
            },
            peer,
            PartitionId::MIN,
            Some(owner),
            false,
        )
        .expect("server rejection is a valid response");
        assert!(matches!(
            rejected,
            OpenResponse::Rejected(message) if message.contains("current owner")
        ));
    }

    #[test]
    fn only_known_pre_v1_8_versions_use_the_legacy_owner_acknowledgement() {
        assert!(binary_version_predates_owner_validation(
            &RestateVersion::new("1.7.4".to_owned())
        ));
        assert!(!binary_version_predates_owner_validation(
            &RestateVersion::new("1.8.0-dev".to_owned())
        ));
        assert!(!binary_version_predates_owner_validation(
            &RestateVersion::unknown()
        ));
        assert!(!binary_version_predates_owner_validation(
            &RestateVersion::new("not-semver".to_owned())
        ));
    }

    #[tokio::test]
    async fn remote_cursor_is_pull_driven_and_forwards_dynamic_predicates() {
        let owner = GenerationalNodeId::new(2, 3);
        let scanner_id = ScannerId(owner, 4);
        let schema = Arc::new(Schema::new(vec![Field::new(
            "value",
            DataType::Int64,
            false,
        )]));
        let column = Arc::new(Column::new("value", 0)) as Arc<dyn PhysicalExpr>;
        let dynamic = Arc::new(DynamicFilterPhysicalExpr::new(
            vec![Arc::clone(&column)],
            greater_than(&column, 10),
        ));
        let predicate = Arc::clone(&dynamic) as Arc<dyn PhysicalExpr>;
        let fragment = Arc::new(
            RemoteFragment::try_new(Arc::new(FragmentLeafExec::new(Arc::clone(&schema))))
                .expect("identity remote fragment"),
        );
        let metrics = ExecutionPlanMetricsSet::new();
        let fragment = RemoteFragmentExecution::new(fragment, Arc::new(TaskContext::default()))
            .with_metrics(&metrics, 0);
        let mut channels = test_scanner_channels_with_fragment_support(scanner_id, true);
        let stream = remote_scan(
            Arc::clone(&channels.service),
            NodeId::from(owner),
            scanner_id,
            PartitionId::MIN,
            KeyRange::FULL,
            "sys_test".to_owned(),
            Arc::clone(&schema),
            Some(predicate),
            128,
            None,
            Some(owner),
            Some(fragment),
        );
        assert!(matches!(
            channels.open_requests.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        let first_poll = tokio::spawn(async move {
            let mut stream = stream;
            let batch = stream.next().await;
            (batch, stream)
        });
        let open = channels
            .open_requests
            .recv()
            .await
            .expect("first poll opens the scanner");
        assert!(open.fragment.is_some());
        assert!(open.predicate.is_some());
        assert!(
            channels
                .next_requests
                .recv()
                .await
                .expect("first batch request")
                .is_none(),
            "the initial predicate travels on Open"
        );
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(Int64Array::from(vec![11, 12]))],
        )
        .expect("remote batch");
        channels
            .responses
            .send(RemoteQueryScannerNextResult::NextBatch(ScannerBatch {
                scanner_id,
                record_batch: crate::encode_record_batch(&schema, batch)
                    .expect("encode remote batch"),
            }))
            .expect("first batch receiver");
        let (batch, stream) = first_poll.await.expect("first poll task");
        assert_eq!(metric_value(&metrics, REMOTE_FRAGMENT_ACCEPTED_METRIC), 1);
        assert_eq!(metric_value(&metrics, REMOTE_FRAGMENT_DECLINED_METRIC), 0);
        assert_eq!(
            batch
                .expect("first batch result")
                .expect("successful first batch")
                .num_rows(),
            2
        );
        assert!(matches!(
            channels.next_requests.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        dynamic
            .update(greater_than(&column, 20))
            .expect("update TopK predicate");
        let second_poll = tokio::spawn(async move {
            let mut stream = stream;
            let batch = stream.next().await;
            (batch, stream)
        });
        assert!(
            channels
                .next_requests
                .recv()
                .await
                .expect("second batch request")
                .is_some(),
            "the changed predicate travels on the following pull"
        );
        channels
            .responses
            .send(RemoteQueryScannerNextResult::NoMoreRecords(scanner_id))
            .expect("end-of-stream receiver");
        let (batch, stream) = second_poll.await.expect("second poll task");
        assert!(batch.is_none());
        drop(stream);
        assert!(!*channels.closed.borrow());
    }

    #[tokio::test]
    async fn dropping_a_blocked_pull_closes_the_remote_scanner() {
        let owner = GenerationalNodeId::new(2, 3);
        let scanner_id = ScannerId(owner, 4);
        let schema = Arc::new(Schema::new(vec![Field::new(
            "value",
            DataType::Int64,
            false,
        )]));
        let mut channels = test_scanner_channels(scanner_id);
        let stream = remote_scan_as_datafusion_stream(
            Arc::clone(&channels.service),
            NodeId::from(owner),
            scanner_id,
            PartitionId::MIN,
            KeyRange::FULL,
            "sys_test".to_owned(),
            schema,
            None,
            128,
            None,
            Some(owner),
        );

        let blocked_pull = tokio::spawn(async move {
            let mut stream = stream;
            stream.next().await
        });
        channels
            .open_requests
            .recv()
            .await
            .expect("first poll opens the scanner");
        channels
            .next_requests
            .recv()
            .await
            .expect("scanner is blocked on its first batch");
        blocked_pull.abort();
        let _ = blocked_pull.await;

        tokio::time::timeout(Duration::from_secs(1), channels.closed.changed())
            .await
            .expect("close notification timeout")
            .expect("close sender remains live");
        assert!(*channels.closed.borrow());
    }

    #[tokio::test]
    async fn declined_fragment_falls_back_before_exposing_batches() {
        let owner = GenerationalNodeId::new(2, 3);
        let scanner_id = ScannerId(owner, 4);
        let schema = Arc::new(Schema::new(vec![Field::new(
            "value",
            DataType::Int64,
            false,
        )]));
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
        let aggregate = AggregateExec::try_new(
            AggregateMode::Partial,
            PhysicalGroupBy::default(),
            vec![count],
            vec![None],
            Arc::new(FragmentLeafExec::new(Arc::clone(&schema))),
            Arc::clone(&schema),
        )
        .expect("partial count");
        let fragment = Arc::new(
            RemoteFragment::try_new(Arc::new(aggregate)).expect("serializable count fragment"),
        );
        let metrics = ExecutionPlanMetricsSet::new();
        let fragment = RemoteFragmentExecution::new(fragment, Arc::new(TaskContext::default()))
            .with_metrics(&metrics, 0);
        let mut channels = test_scanner_channels(scanner_id);
        let stream = remote_scan(
            Arc::clone(&channels.service),
            NodeId::from(owner),
            scanner_id,
            PartitionId::MIN,
            KeyRange::FULL,
            "sys_test".to_owned(),
            Arc::clone(&schema),
            None,
            128,
            None,
            Some(owner),
            Some(fragment),
        );

        let poll = tokio::spawn(async move {
            let mut stream = stream;
            let batch = stream.next().await;
            (batch, stream)
        });
        let open = channels
            .open_requests
            .recv()
            .await
            .expect("first poll opens the scanner");
        assert!(open.fragment.is_some());
        channels
            .next_requests
            .recv()
            .await
            .expect("fallback requests raw input");
        let raw_batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(Int64Array::from(vec![10, 20]))],
        )
        .expect("raw remote batch");
        channels
            .responses
            .send(RemoteQueryScannerNextResult::NextBatch(ScannerBatch {
                scanner_id,
                record_batch: crate::encode_record_batch(&schema, raw_batch)
                    .expect("encode raw batch"),
            }))
            .expect("raw batch receiver");
        channels
            .next_requests
            .recv()
            .await
            .expect("partial aggregate consumes raw input to completion");
        channels
            .responses
            .send(RemoteQueryScannerNextResult::NoMoreRecords(scanner_id))
            .expect("end-of-stream receiver");

        let (batch, stream) = poll.await.expect("fallback poll task");
        assert_eq!(metric_value(&metrics, REMOTE_FRAGMENT_ACCEPTED_METRIC), 0);
        assert_eq!(metric_value(&metrics, REMOTE_FRAGMENT_DECLINED_METRIC), 1);
        let batch = batch
            .expect("partial aggregate output")
            .expect("successful local fallback");
        let count = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("count state");
        assert_eq!(count.value(0), 2);
        drop(stream);
        assert!(!*channels.closed.borrow());
    }
}
