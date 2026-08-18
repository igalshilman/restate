# Design: location-aware physical execution for storage queries

- **Status**: location-aware scanning and partial-aggregation pushdown implemented
- **Date**: 2026-08-17
- **Scope**: `crates/storage-query-datafusion` and the remote scanner protocol in
  `crates/types`

## 1. Summary

Distributed storage queries should represent placement in the physical plan. A scan of
partitioned state is therefore planned as explicit local and remote execution branches,
not as one location-agnostic leaf that decides where to run while it is being scanned.

The physical plan has this shape when a query spans the coordinator and two remote
nodes:

```text
LocationAwareScanExec
├── PartitionScanExec
├── RemoteNodeExec: target_node=N2:7
└── RemoteNodeExec: target_node=N3:4
```

`RemoteNodeExec` is the boundary for work assigned to one remote node. It is deliberately
opaque to DataFusion's general-purpose physical optimizer. A Restate-specific optimizer
may enrich it with a typed remote fragment, but ordinary rules cannot place a local
operator beneath a node that claims remote execution.

Partition ownership is resolved while the physical plan is built. Execution uses the
planned owner and only validates that the decision is still valid. It never silently
reroutes a branch. An ownership change fails the query, allowing the caller to retry and
receive a newly planned, internally consistent execution.

Remote cursors remain pull-driven. One downstream poll can create at most one `Next`
RPC, dropping a cursor sends `Close`, and `Close` cancels a server task even while it is
polling a pipeline-breaking operator. This contract also preserves DataFusion's TopK
dynamic-filter pushdown: the latest filter generation is sampled immediately before
each pull.

Partial aggregation is the first implemented extension of this boundary. A dedicated
physical optimizer turns each eligible local branch into a local partial aggregate and
attaches the equivalent typed fragment to each `RemoteNodeExec`. The coordinator merges
the returned accumulator states with `PartialReduce` before the existing final
aggregation.

## 2. Goals

1. Make local versus remote execution visible and stable in the physical plan.
2. Resolve partition ownership during planning, not while choosing an execution path.
3. Fail the whole query if a planned owner is no longer valid.
4. Keep the remote scanner a true pull interface with bounded in-flight work.
5. Make query cancellation interrupt both ordinary scans and future pipeline-breaking
   remote operators.
6. Preserve the existing global TopK implementation and its dynamic filters.
7. Provide a narrow, reviewable boundary for pushing partial aggregation to partition
   owners without committing to arbitrary distributed DataFusion plan execution.
8. Keep rolling upgrades safe through additive protocol fields and explicit fragment
   acceptance.

## 3. Non-goals

- This design does not turn every `ExecutionPlan` into a remotely serializable plan.
- It does not introduce a distributed scheduler or move coordinator ownership of the
  query.
- It does not reroute an executing branch after ownership changes.
- It does not push a separate TopK operator to workers. The coordinator continues to own
  the global TopK; only its dynamic threshold is pushed into scans.
- It does not push joins, window functions, arbitrary UDAFs, or arbitrary expression
  trees in the first partial-aggregation version.
- It does not guarantee transparent recovery from a failure after a remote fragment has
  begun consuming input. Such failures fail the query.

## 4. Physical-plan model

### 4.1 Placement is part of the scan contract

`PartitionLocation` has two states: `Local` and `Remote(node_id)`.
`ScanPartition::partition_location` resolves that state while the table provider builds
the physical plan. `scan_partition_at` subsequently receives the fixed location selected
by the plan (`crates/storage-query-datafusion/src/table_providers.rs:49-108`).

The table provider performs these steps:

1. select the physical Restate partitions and key ranges required by the query;
2. resolve a `PartitionLocation` for every physical partition;
3. group physical partitions by location;
4. distribute the session's target parallelism across those groups, giving every
   location at least one execution lane;
5. build one `PartitionScanExec` per group;
6. wrap remote groups in `RemoteNodeExec`;
7. combine multiple groups in `LocationAwareScanExec`.

The construction is implemented in
`crates/storage-query-datafusion/src/table_providers.rs:487-547`.

Logical execution lanes never contain physical partitions from different locations. A
single lane therefore never changes from local iteration to remote RPC midway through
its stream. When the number of locations exceeds `target_partitions`, the plan uses at
least one lane per location rather than violating this boundary.

### 4.2 Node responsibilities

| Node | Responsibility | Ordinary children |
|---|---|---|
| `LocationAwareScanExec` | Concatenate location-specific partitions and retain the table's one global statistics estimate. | The local and remote placement branches. |
| `PartitionScanExec` | Scan one set of logical partitions locally and hold static and dynamic predicates separately. | None. |
| `RemoteNodeExec` | Identify one target node and own the remote scan/fragment contract for that target. | None; its scan is intentionally opaque. |

`LocationAwareScanExec` delegates execution and physical properties to an internal
`UnionExec`, but reports the original table statistics rather than summing an identical
estimate once per placement group
(`crates/storage-query-datafusion/src/table_providers.rs:260-380`).

A separate `LocalNodeExec` is unnecessary. `PartitionScanExec` is already the concrete
local execution leaf, whereas a remote branch needs an additional semantic boundary for
transport, validation, fragment negotiation, and cancellation. Adding a no-op local
wrapper would not make an otherwise illegal state unrepresentable.

### 4.3 Why `RemoteNodeExec` is opaque

`RemoteNodeExec::children()` returns no children even though the node privately owns its
`PartitionScanExec` (`crates/storage-query-datafusion/src/table_providers.rs:747-831`).
This is intentional.

If the scan were exposed as an ordinary child, a default DataFusion rule could insert a
`RepartitionExec`, `CoalesceBatchesExec`, filter, or other locally implemented operator
under `RemoteNodeExec`. The displayed plan would then claim that the operator runs
remotely even though `RemoteNodeExec::execute` could only execute it locally.

Only a Restate rule that understands the wire contract may enrich the remote boundary.
The first such enrichment is the typed partial-aggregate fragment. If Restate later
implements a general remote plan interpreter, `RemoteNodeExec` can expose a normal child
subtree at that point.

### 4.4 Physical optimizer ordering

The location nodes are created by `TableProvider::scan`, which is part of physical-plan
construction. They are therefore visible to subsequent physical optimizer passes.
Restate starts with DataFusion's default physical rules and inserts
`PartialAggregationPushdown` immediately before the post-optimization
`FilterPushdown`. The custom rule therefore sees the DataFusion aggregate split, the
explicit placement nodes, and the round-robin repartition below the original partial
aggregate. DataFusion's final dynamic-filter repair and `SanityCheckPlan` still run
after the Restate rewrite (`crates/storage-query-datafusion/src/context.rs:634-641`).

The custom rule must operate on `RemoteNodeExec` through a dedicated API. It must not
rely on generic child rewriting to cross the opaque boundary.

## 5. Ownership semantics

### 5.1 Planning

The coordinator resolves all selected partitions before returning the scan plan. The
selected `NodeId`, including its generation when available, becomes immutable state of
the physical plan. Repeated point reads for one Restate partition are grouped using that
planned location.

Planning fails if the routing metadata cannot identify an owner. This is preferable to
constructing a plan whose execution topology is incomplete.

### 5.2 Execution-time validation

Execution validates a planning decision but does not make a new routing decision:

- a planned local branch checks that the partition still resolves as local before it
  opens the local scanner;
- a planned remote branch connects to the `NodeId` stored in `RemoteNodeExec` without
  consulting routing again;
- the remote `Open` carries `expected_partition_owner` when the target is generational;
- the server checks both its own generational identity and current local ownership before
  it creates the scanner.

The coordinator-side validation is implemented in
`crates/storage-query-datafusion/src/remote_query_scanner_manager.rs:226-252`; the server
invokes it before scanner construction in
`crates/storage-query-datafusion/src/scanner_task.rs:78-91`.

Any mismatch returns an execution error. DataFusion then fails the query as a whole. No
branch is redirected, and no result combines data read under two independently resolved
ownership snapshots.

### 5.3 Wire representation

`RemoteQueryScannerOpen.expected_partition_owner` is optional bilrost tag 9 and is
omitted from flexbuffers when unset
(`crates/types/src/net/remote_query_scanner.rs:65-73`). It is unset for node-level scans,
which use the scanner protocol but are not partition-routed.

The field is additive. Compatibility coverage verifies that an old-shape bilrost message
can be decoded by the new type, that the old type skips tag 9, and that the flexbuffers
encoding is unchanged when the field is absent
(`crates/types/src/net/remote_query_scanner.rs:339-423`).

Additive decoding alone is not sufficient for ownership fencing: an older worker can
ignore tag 9 and return the legacy `Success`. A partition-routed client therefore
accepts only `SuccessWithOwnerValidation` or `SuccessWithPartialAggregate`, both of
which positively acknowledge that the worker understood and validated the planned
owner. A legacy `Success` fails an owner-fenced query and closes the opened scanner.
New-shape failures carry the server's diagnostic so an ownership mismatch reports the
partition and validation result rather than a generic open error
(`crates/storage-query-datafusion/src/remote_query_scanner_client.rs:559-595`).

## 6. Pull execution and backpressure

The client exposes a `RecordBatchStream` backed by a `try_unfold` cursor:

```text
Opening ──success──► Ready ──downstream poll / await Next──► Ready
   │                   │
   │                   ├── declined fragment ──► Fallback
   │                   └── EOF/error ──────────► Done
   └──failure──────────────────────────────────► Done
```

The implementation is in
`crates/storage-query-datafusion/src/remote_query_scanner_client.rs:218-458`.

The invariants are:

1. Normal batch pulling waits for `Open` to complete before creating a `Next`.
2. At most one `Next` RPC is outstanding for a cursor.
3. The cursor awaits `Next` inside the future created for a downstream `poll_next`.
4. After returning a batch, the cursor returns to `Ready`; it does not prefetch the next
   batch into a channel.
5. The cursor owns the `RemoteScanner` in every live state after `Open` reaches the wire.
6. Dropping any live state drops the `RemoteScanner`, whose guard sends `Close`; a
   cancellation may therefore send `Close` while the `Open` reply is still pending.
7. EOF and terminal server failures disarm the close guard because the server has already
   removed the cursor.

This is a pull model at the RPC boundary, not merely a bounded producer model. It limits
remote work to what the downstream plan is actively requesting and gives dynamic filters
a chance to change between batches.

## 7. Cancellation

The client-side drop guard converts DataFusion stream cancellation into a remote `Close`.
On the server, each scanner-map entry contains both the request channel and a watch-based
cancellation signal (`crates/storage-query-datafusion/src/scanner_task.rs:40-72`).

When `Close` arrives, the server removes the handle and sends the cancellation signal
before replying (`crates/storage-query-datafusion/src/remote_query_scanner_server.rs:88-97`).
`ScannerTask` selects cancellation and peer death both while idle and while polling
`stream.next()` (`crates/storage-query-datafusion/src/scanner_task.rs:170-229`). This is
important for partial aggregation and any other pipeline-breaking fragment because the
first output batch may require consuming the entire input.

The 60-second scanner expiration remains an idle-cursor timeout. An active-execution
deadline is a separate policy decision and should not be conflated with cancellation.

## 8. TopK and dynamic filters

For `ORDER BY ... LIMIT K`, DataFusion's coordinator-side TopK owns a shared
`DynamicFilterPhysicalExpr`. As its threshold improves, the scan can discard rows that
cannot enter the global top K.

The remote boundary preserves this mechanism in two places:

1. `RemoteNodeExec` implements the post-optimization filter-pushdown hook and forwards
   the result to its private `PartitionScanExec`, without exposing that scan as an
   ordinary child (`crates/storage-query-datafusion/src/table_providers.rs:842-873`).
2. The cursor snapshots the predicate generation immediately before every `Next` and
   piggybacks a changed predicate on that pull
   (`crates/storage-query-datafusion/src/remote_query_scanner_client.rs:322-340` and
   `:380-400`).

Because there is no read-ahead, the parent TopK has processed the preceding batch before
the next generation snapshot. The server applies an updated predicate before polling its
next batch (`crates/storage-query-datafusion/src/scanner_task.rs:205-229`).

The plan-shape test runs the rules in session order and proves that DataFusion's TopK
filter reaches the opaque remote scan after the partial-aggregation rule at
`crates/storage-query-datafusion/src/table_providers.rs:1366-1413`. Generation-update
coverage verifies that a changed predicate is serialized once for the next pull at
`crates/storage-query-datafusion/src/remote_query_scanner_client.rs:682-737`.

Partial aggregation does not replace this mechanism. A remote fragment must keep the
dynamic predicate below the aggregate so filtering still applies to raw rows. Queries
whose TopK expression cannot soundly filter raw input remain coordinator-only, as they
are today.

## 9. Partial-aggregation extension

### 9.1 Physical plan rewrite

DataFusion 54 is pinned in `Cargo.toml:150-159` and provides `PartialReduce`, which
consumes partial accumulator state and produces partial accumulator state. That makes it
possible to push the original `Partial` stage into every placement branch while leaving
the final aggregate and its hash repartitioning intact.

For an eligible grouped aggregation, the rewrite is:

```text
Before
------
AggregateExec: FinalPartitioned
└── RepartitionExec: Hash(group keys)
    └── AggregateExec: Partial
        └── FilterExec: residual SQL predicate (optional)
            └── RepartitionExec: RoundRobinBatch (optional)
                └── LocationAwareScanExec
                    ├── PartitionScanExec: Local
                    └── RemoteNodeExec: N2

After
-----
AggregateExec: FinalPartitioned
└── RepartitionExec: Hash(group keys)
    └── AggregateExec: PartialReduce
        └── LocationAwareScanExec: output=accumulator state
            ├── AggregateExec: Partial
            │   └── FilterExec: residual SQL predicate (optional)
            │       └── PartitionScanExec: Local
            └── RemoteNodeExec: N2, fragment=PartialAggregate
```

Every child of the rewritten `LocationAwareScanExec` has the same accumulator-state
schema. The root is rebuilt from those children so its physical properties reflect the
new schema, and its statistics are reset to unknown rather than reusing raw-row table
statistics.

### 9.2 Rule eligibility

The optimizer rule is deliberately narrow. It rewrites only when all of
the following are true:

- the candidate is a DataFusion `AggregateExec` in `Partial` mode;
- its input reaches a `LocationAwareScanExec` containing a remote branch, or a direct
  `RemoteNodeExec`, through at most one residual `FilterExec` and the round-robin
  `RepartitionExec` inserted by DataFusion, in either order;
- a residual filter has no fetch limit, its predicate is non-volatile and serializable,
  and any embedded projection produces exactly the schema expected by the partial
  aggregate;
- the scan has no pushed limit;
- there is one ordinary grouping set;
- aggregate functions are from the initial built-in allowlist: `count`, `sum`, `min`,
  `max`, and `avg`;
- DataFusion can construct the row or grouped accumulator that `PartialReduce` will use
  for the aggregate's concrete argument and result types;
- there is no `DISTINCT`, aggregate `FILTER`, aggregate ordering, reversed aggregate, or
  unsupported null treatment;
- every grouping and argument expression can be serialized using the already supported
  physical-expression codec;
- the partial output schema can be computed and validated before execution.

Unsupported shapes remain unchanged and execute with the ordinary scan plan. Eligibility
is an optimization decision, not a new query-validity rule. Local-only scans are also
left unchanged because moving their existing partial aggregate provides no distributed
execution benefit. The rule and eligibility checks are implemented at
`crates/storage-query-datafusion/src/partial_aggregation.rs:148-238` and `:452-540`.

`PartitionScanExec` continues to store the provider's static predicate separately from
later dynamic predicates (`crates/storage-query-datafusion/src/table_providers.rs:568-577`
and `:698-705`). The scan-level copy remains useful for pruning, but it is not treated as
an exact replacement: some `ScanPartition` implementations ignore predicates. The rule
therefore clones the residual `FilterExec` into every local and remote fragment, before
the partial aggregate. This preserves DataFusion's exact filtering semantics even when a
scanner applies no pushed predicate. Filters outside the validated shape still skip the
rewrite.

### 9.3 Remote fragment model

`RemoteNodeExec` holds an optional `PartialAggregateFragment` rather than exposing a
general remotely executable child plan:

```rust
struct PartialAggregateFragment {
    group_by: PhysicalGroupBy,
    aggregate: Vec<Arc<AggregateFunctionExpr>>,
    filter: Option<PartialAggregateFilter>,
    scan_schema: SchemaRef,
    aggregate_input_schema: SchemaRef,
    output_schema: SchemaRef,
    wire: OnceLock<RemoteQueryScannerPartialAggregate>,
}
```

The in-memory fragment is implemented in
`crates/storage-query-datafusion/src/partial_aggregation.rs`. Its semantic contract is:

- the filter predicate is evaluated against the raw projected scan schema, while
  grouping and aggregate expressions use the post-filter projection schema;
- an optional exact residual filter, including its embedded projection, runs before the
  partial aggregate;
- output is accumulator state, not final values;
- the output schema is known at planning time;
- the server reconstructs only allowlisted aggregates;
- the server validates the reconstructed schema against the requested output schema;
- accumulator state is accepted only when both peers advertise the same state ABI.

The node remains opaque. `with_partial_aggregate` validates the fragment and returns a
new `RemoteNodeExec` with updated schema and plan properties. Generic
`with_new_children` continues to reject children.

For transport, the fragment constructs a supported `AggregateExec::Partial` over either
an `EmptyExec` or one validated `FilterExec` over that `EmptyExec`, carrying the raw input
schema, and serializes the plan with DataFusion's physical protobuf codec. The server
accepts only those two shapes, reruns the aggregate and filter validation after decoding,
and verifies the raw input, post-filter aggregate input, and accumulator output schemas.
This uses DataFusion's expression and built-in aggregate codecs without granting
permission to execute an arbitrary serialized physical plan.

### 9.4 Wire negotiation and fallback

`RemoteQueryScannerOpen.partial_aggregate` is optional bilrost tag 10. It carries the
state ABI, serialized partial plan, and expected accumulator-state schema. Acceptance is
reported with `RemoteQueryScannerOpened::SuccessWithPartialAggregate`; the original
`SuccessWithOwnerValidation` means that an owner-fenced scanner opened but the fragment
was not applied. The original `Success` remains only for requests without owner fencing
(`crates/types/src/net/remote_query_scanner.rs:74-148`).

Execution then proceeds as follows:

1. the server validates planned ownership;
2. it validates the fragment ABI, allowlist, expression decoding, and output schema
   before consuming input;
3. if accepted, it returns `SuccessWithPartialAggregate` and streams accumulator-state
   batches;
4. if unsupported before execution, it acknowledges owner validation and streams raw
   batches;
5. the client applies the same residual filter and partial aggregate locally to an
   unapplied raw stream, so the outward `RemoteNodeExec` schema remains accumulator state;
6. an error after `applied=true` fails the query because raw input may already have been
   consumed.

An old server ignores both optional request fields and returns the existing success
response. A new client must reject that response when owner fencing was requested; this
can transiently fail partition-routed queries from an upgraded coordinator to an older
worker, but cannot silently execute against an unvalidated owner. An old client never
sends either new field and therefore receives only the legacy response variants.
Flexbuffers fields must use `serde(default, skip_serializing_if = "Option::is_none")`,
and both bilrost directions must be covered by old-shape compatibility tests.

The client cursor does not expose raw batches through its state-schema stream while
fallback is being selected. `Open` produces an internal negotiated cursor; the
`RemoteNodeExec` execution adapter then either exposes the applied remote stream or wraps
the raw stream once with a local `AggregateExec::Partial`.

### 9.5 Serving-side execution and admission

The server constructs the ordinary local partition scan first, installs the static and
dynamic predicate below the fragment, and then wraps the stream with the reconstructed
exact filter and partial aggregate. The exact filter can duplicate scan-level pruning,
which is intentional because scan predicate handling is not part of the `ScanPartition`
correctness contract. The fragment uses the server's query `TaskContext`, so allocations
are charged to its DataFusion memory pool.

Fragment construction preflights the protobuf encoding once and caches that validated
wire representation. Opening multiple physical partitions clones the encoded payload;
it does not rebuild and re-encode the same DataFusion fragment for every partition.

Explicit fragment admission accounting remains an operational follow-up. Today fragment
memory is governed by the server query context's DataFusion memory pool, but there is no
separate concurrent-fragment permit. Adding one must use a non-blocking acquisition: a
request that cannot obtain admission must decline before consuming input and stream raw
rows instead of waiting while holding a cursor and ownership snapshot.

Cancellation uses the existing active-poll select and requires no fragment-specific
side channel.

## 10. Correctness invariants

The implementation and subsequent optimizer extensions must preserve these invariants:

1. **Fixed placement**: every physical partition has one planned location for the life
   of a physical plan.
2. **No rerouting**: execution either uses that location or fails.
3. **One boundary, one target**: every `RemoteNodeExec` contains partitions for exactly
   its displayed target node.
4. **No cross-location lane**: one output partition never switches RPC targets.
5. **Opaque remote work**: only Restate-aware code may add work to a remote boundary.
6. **Uniform branch schema**: all children of `LocationAwareScanExec` have identical
   schemas, including after fragment pushdown or fallback.
7. **State compatibility**: partial accumulator state is merged only when its ABI is
   positively compatible.
8. **No post-accept fallback**: once remote execution consumes input, a failure fails the
   query.
9. **Pull boundedness**: a cursor has no more than one outstanding `Next`.
10. **Cancellation reachability**: dropping the client stream can interrupt the server
    while it is producing the current batch.
11. **Dynamic-filter freshness**: every new pull observes the latest available predicate
    generation.
12. **Statistics are not multiplied**: splitting one table scan by placement does not
    multiply its global row estimate.

## 11. Implementation plan

### Phase 1: location-aware scanner foundation — implemented

- planning-time `PartitionLocation` resolution;
- location-specific execution lanes;
- `LocationAwareScanExec`, local `PartitionScanExec`, and opaque `RemoteNodeExec`;
- fixed-location execution and owner validation through wire tag 9;
- direct pull `try_unfold` cursor;
- active server cancellation;
- separate static and dynamic scan predicates;
- TopK pushdown forwarding through `RemoteNodeExec`;
- plan-shape, statistics, TopK, and protocol compatibility tests.

### Phase 2: local fragment model — implemented

- add the typed in-memory `PartialAggregateFragment`;
- add validated fragment state and output properties to `RemoteNodeExec`;
- insert the physical optimizer before DataFusion's final dynamic-filter and sanity
  rules;
- rewrite local branches and execute declined remote fragments locally;
- add plan-shape and result-equivalence tests.

### Phase 3: remote fragment execution — implemented

- add optional wire tag 10 and explicit acceptance response;
- rebuild and validate allowlisted partial aggregates on the server;
- execute accepted fragments and locally wrap declined fragments;
- preserve active-poll cancellation for pipeline-breaking remote aggregation.

The remaining operational work from this phase is fragment-specific admission control
and accepted/declined metrics.

### Phase 4: rollout

- run mixed-version tests in both directions;
- compare result equivalence across the supported aggregate matrix;
- measure wire bytes, time to first batch, total latency, coordinator CPU, worker CPU,
  and peak worker memory;
- test low- and high-cardinality groups, empty input, global aggregates, ownership
  movement, fragment decline, cancellation, and TopK queries;
- retain or narrow the default-on allowlist based on those measurements.

## 12. Test strategy

The foundation has high-signal tests for placement isolation, the explicit remote plan
shape, global statistics, TopK filter propagation, tag-9 compatibility, positive owner
acknowledgement, detailed open failures, client-side dynamic-filter serialization,
server-side dynamic-filter application, and cancellation of a blocked active pull.
Transport-level cursor tests additionally prove that pulls do not prefetch, changed
dynamic predicates travel on the following `Next`, and dropping a blocked pull closes
the remote scanner.

The implemented partial-aggregation coverage includes a mixed-placement plan-shape and
result-equivalence test for every allowlisted aggregate, fragment codec round-trip
coverage, filtered grouped aggregation, filtered `COUNT(*)` with an embedded filter
projection, a scanner that ignores pushed predicates, rejection of unconstructable reduce
accumulators, global aggregation coverage, TopK propagation coverage, and protocol
compatibility tests. A declined-fragment transport test proves that raw batches are
consumed through the local fallback aggregate and never escape through the fragment's
state-schema stream. The implementation was also verified manually with three-node
`EXPLAIN ANALYZE`. The remaining matrix should add:

- identical result tests for local-only, remote-only, and mixed placement;
- empty-input tests, especially global aggregation's single output row;
- supported null behavior across the aggregate allowlist;
- explicit non-rewrite tests for every eligibility rejection;
- post-accept remote failure tests;
- ownership movement between planning and both local and remote execution.

## 13. Operational visibility

The following node-level metrics should accompany remote fragments:

- active ordinary and fragment cursors;
- accepted and declined fragments by low-cardinality reason;
- raw rows/bytes read and result rows/bytes sent;
- fragment execution time and time to first batch;
- cancellation count and cancellation latency;
- ownership-validation failures;
- server-side DataFusion memory consumption or reservation failures.

Expected fragment declines due to an unsupported ABI, allowlist, or admission pressure
should be debug events and counters, not one warning per scanner. Missing ownership
acknowledgement, ownership mismatch, and post-accept execution failure remain query errors
and should carry the partition, planned target, and current validation result without
including user row data.
