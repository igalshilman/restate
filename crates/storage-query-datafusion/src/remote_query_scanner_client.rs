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
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::DataFusionError;
use datafusion::execution::{RecordBatchStream, SendableRecordBatchStream};
use datafusion::physical_expr_common::physical_expr::snapshot_generation;
use datafusion::physical_plan::PhysicalExpr;
use futures::future::BoxFuture;
use futures::stream::Stream;
use tracing::debug;

use restate_core::network::{Connection, NetworkSender, Networking, Swimlane, TransportConnect};
use restate_core::{TaskCenter, TaskCenterFutureExt, TaskKind, task_center};
use restate_types::identifiers::PartitionId;
use restate_types::net::remote_query_scanner::{
    RemoteQueryScannerClose, RemoteQueryScannerNext, RemoteQueryScannerNextResult,
    RemoteQueryScannerOpen, RemoteQueryScannerOpened, RemoteQueryScannerPartialAggregate,
    RemoteQueryScannerPredicate, ScannerBatch, ScannerFailure, ScannerId,
};
use restate_types::sharding::KeyRange;
use restate_types::{GenerationalNodeId, NodeId};

use crate::partial_aggregation::PartialAggregateFragment;
use crate::{decode_record_batch, encode_expr, encode_schema};

#[derive(derive_more::Debug)]
pub struct RemoteScanner {
    scanner_id: ScannerId,
    connection: Option<Connection>,
}

impl RemoteScanner {
    /// Constructs a scanner that owns `connection` for the purpose of sending
    /// `Close` on drop. Use this to install a drop-guard *before* sending
    /// `Open`: if the caller's future is cancelled (or the proxy returns
    /// `Err`) after `Open` reaches the wire, the existing `Drop` impl emits
    /// `Close` so the server doesn't keep an orphan scanner until TTL.
    pub fn new(scanner_id: ScannerId, connection: Connection) -> Self {
        Self {
            scanner_id,
            connection: Some(connection),
        }
    }

    async fn next_batch(
        &mut self,
        next_predicate: Option<RemoteQueryScannerPredicate>,
    ) -> Result<RemoteQueryScannerNextResult, DataFusionError> {
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
    pub fn forget(mut self) {
        self.connection.take();
    }
}

impl Drop for RemoteScanner {
    fn drop(&mut self) {
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

pub struct OpenedRemoteScanner {
    scanner: RemoteScanner,
    partial_aggregate_applied: bool,
}

impl OpenedRemoteScanner {
    pub fn new(scanner: RemoteScanner, partial_aggregate_applied: bool) -> Self {
        Self {
            scanner,
            partial_aggregate_applied,
        }
    }
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
/// `scanner_id` is allocated by the caller (typically via
/// [`RemoteScannerManager::allocate_scanner_id`]) so the server can adopt the
/// caller's id instead of minting its own.
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
    remote_scan_as_datafusion_stream_inner(
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn remote_scan_with_partial_aggregate(
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
    fragment: Arc<PartialAggregateFragment>,
    context: Arc<datafusion::execution::TaskContext>,
) -> SendableRecordBatchStream {
    remote_scan_as_datafusion_stream_inner(
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
        Some((fragment, context)),
    )
}

#[allow(clippy::too_many_arguments)]
fn remote_scan_as_datafusion_stream_inner(
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
    partial_aggregate: Option<(
        Arc<PartialAggregateFragment>,
        Arc<datafusion::execution::TaskContext>,
    )>,
) -> SendableRecordBatchStream {
    let predicate_generation = predicate.as_ref().map(snapshot_generation).unwrap_or(0);
    let initial_predicate = predicate
        .as_ref()
        .map(|predicate| {
            encode_expr(predicate).map(|serialized_physical_expression| {
                RemoteQueryScannerPredicate {
                    serialized_physical_expression,
                }
            })
        })
        .transpose();

    let wire_partial_aggregate: Result<Option<RemoteQueryScannerPartialAggregate>, _> =
        partial_aggregate
            .as_ref()
            .map(|(fragment, _)| fragment.to_wire())
            .transpose();
    let output_schema = partial_aggregate
        .as_ref()
        .map(|(fragment, _)| fragment.output_schema())
        .unwrap_or_else(|| Arc::clone(&projection_schema));

    let state = match (initial_predicate, wire_partial_aggregate) {
        (Ok(initial_predicate), Ok(partial_aggregate_request)) => {
            let open_request = RemoteQueryScannerOpen {
                scanner_id: Some(scanner_id),
                partition_id,
                range,
                table: table_name,
                projection_schema_bytes: encode_schema(&projection_schema),
                limit: limit.map(|limit| u64::try_from(limit).expect("limit to fit in a u64")),
                predicate: initial_predicate,
                batch_size: u64::try_from(batch_size).expect("batch_size to fit in a u64"),
                expected_partition_owner,
                partial_aggregate: partial_aggregate_request,
            };
            RemoteCursorState::Opening(Box::pin(async move {
                service.open(target_node_id, open_request).await
            }))
        }
        (Err(error), _) | (_, Err(error)) => RemoteCursorState::Failed(Some(error)),
    };

    Box::pin(RemoteCursorStream {
        schema: output_schema,
        raw_schema: projection_schema,
        predicate,
        predicate_generation,
        partial_aggregate,
        state,
    })
}

enum RemoteCursorState {
    Opening(BoxFuture<'static, Result<OpenedRemoteScanner, DataFusionError>>),
    Ready(RemoteScanner),
    Pulling(
        BoxFuture<
            'static,
            (
                RemoteScanner,
                Result<RemoteQueryScannerNextResult, DataFusionError>,
            ),
        >,
    ),
    Fallback(SendableRecordBatchStream),
    Failed(Option<DataFusionError>),
    Done,
}

struct RemoteCursorStream {
    schema: SchemaRef,
    raw_schema: SchemaRef,
    predicate: Option<Arc<dyn PhysicalExpr>>,
    predicate_generation: u64,
    partial_aggregate: Option<(
        Arc<PartialAggregateFragment>,
        Arc<datafusion::execution::TaskContext>,
    )>,
    state: RemoteCursorState,
}

impl Stream for RemoteCursorStream {
    type Item = Result<RecordBatch, DataFusionError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            match &mut this.state {
                RemoteCursorState::Opening(open) => match open.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(opened)) => {
                        let OpenedRemoteScanner {
                            scanner,
                            partial_aggregate_applied,
                        } = opened;
                        if let Some((fragment, context)) = this.partial_aggregate.take()
                            && !partial_aggregate_applied
                        {
                            let raw_cursor = Self {
                                schema: Arc::clone(&this.raw_schema),
                                raw_schema: Arc::clone(&this.raw_schema),
                                predicate: this.predicate.clone(),
                                predicate_generation: this.predicate_generation,
                                partial_aggregate: None,
                                state: RemoteCursorState::Ready(scanner),
                            };
                            match fragment.execute_stream(Box::pin(raw_cursor), context) {
                                Ok(stream) => {
                                    this.state = RemoteCursorState::Fallback(stream);
                                }
                                Err(error) => {
                                    this.state = RemoteCursorState::Done;
                                    return Poll::Ready(Some(Err(error)));
                                }
                            }
                        } else {
                            this.state = RemoteCursorState::Ready(scanner);
                        }
                    }
                    Poll::Ready(Err(error)) => {
                        this.state = RemoteCursorState::Done;
                        return Poll::Ready(Some(Err(error)));
                    }
                },
                RemoteCursorState::Ready(_) => {
                    let next_predicate = match next_predicate(
                        &mut this.predicate_generation,
                        this.predicate.as_ref(),
                    ) {
                        Ok(next_predicate) => next_predicate,
                        Err(error) => {
                            this.state = RemoteCursorState::Done;
                            return Poll::Ready(Some(Err(error)));
                        }
                    };
                    let RemoteCursorState::Ready(mut scanner) =
                        std::mem::replace(&mut this.state, RemoteCursorState::Done)
                    else {
                        unreachable!("matched ready cursor state")
                    };
                    this.state = RemoteCursorState::Pulling(Box::pin(async move {
                        let result = scanner.next_batch(next_predicate).await;
                        (scanner, result)
                    }));
                }
                RemoteCursorState::Pulling(pull) => {
                    let (scanner, result) = match pull.as_mut().poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(result) => result,
                    };

                    match result {
                        Ok(RemoteQueryScannerNextResult::NextBatch(ScannerBatch {
                            record_batch,
                            ..
                        })) => match decode_record_batch(&record_batch) {
                            Ok(batch) => {
                                this.state = RemoteCursorState::Ready(scanner);
                                return Poll::Ready(Some(Ok(batch)));
                            }
                            Err(error) => {
                                this.state = RemoteCursorState::Done;
                                return Poll::Ready(Some(Err(error)));
                            }
                        },
                        Ok(RemoteQueryScannerNextResult::NoMoreRecords(_)) => {
                            scanner.forget();
                            this.state = RemoteCursorState::Done;
                            return Poll::Ready(None);
                        }
                        Ok(RemoteQueryScannerNextResult::Failure(ScannerFailure {
                            message,
                            ..
                        })) => {
                            scanner.forget();
                            this.state = RemoteCursorState::Done;
                            return Poll::Ready(Some(Err(DataFusionError::Internal(message))));
                        }
                        Ok(RemoteQueryScannerNextResult::NoSuchScanner(_)) => {
                            scanner.forget();
                            this.state = RemoteCursorState::Done;
                            return Poll::Ready(Some(Err(DataFusionError::Internal(
                                "No such scanner. It could have expired due to a long period of inactivity."
                                    .to_string(),
                            ))));
                        }
                        Ok(RemoteQueryScannerNextResult::Unknown) => {
                            this.state = RemoteCursorState::Done;
                            return Poll::Ready(Some(Err(DataFusionError::Internal(
                                "Received unknown scanner result".to_owned(),
                            ))));
                        }
                        Err(error) => {
                            this.state = RemoteCursorState::Done;
                            return Poll::Ready(Some(Err(error)));
                        }
                    }
                }
                RemoteCursorState::Fallback(stream) => return stream.as_mut().poll_next(cx),
                RemoteCursorState::Failed(error) => {
                    return Poll::Ready(error.take().map(Err));
                }
                RemoteCursorState::Done => return Poll::Ready(None),
            }
        }
    }
}

impl RecordBatchStream for RemoteCursorStream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

fn next_predicate(
    predicate_generation: &mut u64,
    predicate: Option<&Arc<dyn PhysicalExpr>>,
) -> Result<Option<RemoteQueryScannerPredicate>, DataFusionError> {
    if *predicate_generation != 0 {
        // generation 0 means the predicate is static (or we never had one)
        let predicate = predicate.ok_or(DataFusionError::Internal(
            "Missing predicate despite non-zero predicate generation".into(),
        ))?;
        let current_predicate_generation = snapshot_generation(predicate);

        if current_predicate_generation != *predicate_generation {
            *predicate_generation = current_predicate_generation;
            Ok(Some(RemoteQueryScannerPredicate {
                serialized_physical_expression: encode_expr(predicate)?,
            }))
        } else {
            Ok(None)
        }
    } else {
        Ok(None)
    }
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

        match open_reply.await {
            Ok(RemoteQueryScannerOpened::Success { scanner_id }) => {
                // Server is running Restate <v1.7 so we need to respect
                // the returned scanner_id
                if remote_scanner.scanner_id != scanner_id {
                    remote_scanner.forget();
                    remote_scanner = RemoteScanner::new(scanner_id, connection.clone())
                }
                Ok(OpenedRemoteScanner::new(remote_scanner, false))
            }
            Ok(RemoteQueryScannerOpened::SuccessWithPartialAggregate { scanner_id }) => {
                if remote_scanner.scanner_id != scanner_id {
                    remote_scanner.forget();
                    remote_scanner = RemoteScanner::new(scanner_id, connection.clone())
                }
                Ok(OpenedRemoteScanner::new(remote_scanner, true))
            }
            Ok(RemoteQueryScannerOpened::Failure) => {
                remote_scanner.forget();
                Err(DataFusionError::Internal(
                    "Unable to open a remote scanner".to_string(),
                ))
            }
            Err(e) => Err(DataFusionError::External(e.into())),
        }
    }
}
