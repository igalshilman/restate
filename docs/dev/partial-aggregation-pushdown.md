# Design: remote physical-plan fragments for storage queries

- **Status**: generic fragment foundation, row-wise fragments, and partial aggregation implemented
- **Date**: 2026-08-20
- **Scope**: `crates/storage-query-datafusion` and the remote scanner protocol in
  `crates/types`

For the current implementation architecture and wire-level execution flow, see
[distributed-query-execution.md](distributed-query-execution.md).

## 1. Summary

Distributed storage queries represent placement explicitly in the physical plan. A scan
of partitioned state is planned as local and remote execution branches, rather than as
one location-agnostic leaf that chooses where to run while it is being scanned:

```text
LocationAwareScanExec
├── PartitionScanExec
├── RemoteNodeExec: target_node=N2:7
└── RemoteNodeExec: target_node=N3:4
```

`RemoteNodeExec` is an opaque boundary for work assigned to one remote node. It may hold
a generic `RemoteFragment`: a DataFusion physical-plan template with exactly one
`FragmentLeafExec`. At execution, that placeholder is bound to the partition scan on the
worker. The same template can instead be bound to the raw remote stream on the
coordinator when the worker declines the fragment.

This foundation deliberately comes before individual pushdown features. A
Restate-specific physical optimizer can construct any DataFusion-protobuf-serializable,
single-input, single-output-partition fragment after applying the semantic checks for
that feature. The transport, negotiation, local fallback, and execution paths are
feature-independent.

Two fragment-producing optimizers use the foundation. `ScanFragmentPushdown` moves stable
`FilterExec` and `ProjectionExec` chains, including computed projections, independently
of aggregation. `PartialAggregationPushdown` moves an eligible `AggregateExec::Partial`
and its stable row-wise input chain into each location branch. Local branches execute the
bound plan directly. Remote branches carry the same template in `RemoteNodeExec`, and the
coordinator combines partial accumulator state with `PartialReduce` before the existing
final aggregation.

Partition ownership is resolved during physical planning. Execution uses the planned
owner and validates that it is still valid; it never reroutes a branch. An ownership
change fails the entire query so a retry can build a new, internally consistent plan.

Remote cursors remain strictly pull-driven. One downstream poll creates at most one
`Next` RPC, dropping a cursor sends `Close`, and `Close` cancels a server task even while
it is polling a pipeline-breaking fragment. DataFusion TopK dynamic-filter pushdown is
preserved by sampling the latest filter immediately before every pull.

## 2. Goals

1. Make local versus remote execution visible and stable in the physical plan.
2. Resolve partition ownership during planning, not scanning.
3. Fail the entire query if a planned owner is no longer valid.
4. Keep the remote scanner a true pull interface with bounded in-flight work.
5. Make cancellation interrupt both raw scans and pipeline-breaking fragments.
6. Preserve coordinator-side TopK and its scan-level dynamic-filter pushdown.
7. Provide a generic physical-fragment foundation before layering individual pushdown
   optimizations on it.
8. Let every fragment-producing rule own its semantic eligibility and expression-safety
   checks.
9. Keep rolling upgrades safe through additive protocol fields, explicit format
   versioning, and positive fragment acceptance.

## 3. Non-goals

- This design does not introduce a distributed scheduler. The coordinator still owns
  the query and assigns each remote fragment to the already planned partition owner.
- It does not support multi-input remote fragments, distributed joins, exchanges between
  workers, or fragments with more than one output partition.
- It does not expose the private remote scan as an ordinary DataFusion child.
- It does not reroute an executing branch after ownership changes.
- It does not push a separate TopK operator to workers. The coordinator owns the global
  TopK; only its dynamic threshold is pushed into raw scans.
- It does not make every serializable physical plan automatically eligible for
  pushdown. A feature-specific optimizer must construct the fragment and prove that the
  rewrite preserves query semantics.
- It does not guarantee recovery after an accepted remote fragment has consumed input.
  Such a failure fails the query.

## 4. Physical-plan model

### 4.1 Placement is part of the scan contract

`PartitionLocation` has two states: `Local` and `Remote(node_id)`.
`DistributedPartitionScanner::partition_location` resolves that state while the table
provider builds the physical plan. The selected `PartitionScanExec` or `RemoteNodeExec`
subsequently uses its distinct local or remote scan entry point with the fixed location
selected by that plan.

`ScanPartition` is the narrower node-local storage interface used by local table scanners
and by the RPC server after ownership validation. Keeping it separate from
`DistributedPartitionScanner` prevents a raw local scanner from acquiring placement or
remote-transport responsibilities.

The table provider:

1. selects the physical Restate partitions and key ranges required by the query;
2. resolves a `PartitionLocation` for every selected partition;
3. groups physical partitions by location;
4. distributes the session's target parallelism across those groups, giving every
   location at least one execution lane;
5. builds one `PartitionScanExec` per group;
6. wraps remote groups in `RemoteNodeExec`;
7. combines multiple groups in `LocationAwareScanExec`.

The stateless placement grouping and physical-to-logical lane allocation live in the
`partition_planning` module. The table provider owns partition selection and plan-node
construction; the planning helper owns only placement isolation and lane allocation.

Logical execution lanes never contain physical partitions from different locations. A
single lane therefore never changes from local iteration to remote RPC midway through
its stream. If the number of locations exceeds `target_partitions`, the plan uses at
least one lane per location rather than violating this boundary.

### 4.2 Node responsibilities

| Node | Responsibility | Ordinary children |
|---|---|---|
| `LocationAwareScanExec` | Combine location-specific branches and retain the table's one global statistics estimate. | Local and remote placement branches. |
| `PartitionScanExec` | Scan one set of logical partitions locally and hold its current scan predicate. | None. |
| `RemoteNodeExec` | Identify one target node and own an optional generic fragment for that target. | None; its scan and fragment template are intentionally opaque. |

`LocationAwareScanExec` delegates execution and physical properties to an internal
`UnionExec`, but reports the original table statistics rather than summing an identical
estimate once per placement group.

A separate `LocalNodeExec` is unnecessary. `PartitionScanExec` is already the concrete
local execution leaf. A remote branch needs the extra boundary for transport,
negotiation, ownership validation, fallback, and cancellation.

### 4.3 Why `RemoteNodeExec` remains opaque

`RemoteNodeExec::children()` returns no children even though the node privately owns its
`PartitionScanExec` and may own a `RemoteFragment`. This prevents default DataFusion
rules from placing a locally implemented operator beneath a node that claims remote
execution.

This structural opacity does not hide remote work from `EXPLAIN`. The display renders
the fragment as an inline operator pipeline and renders the private scan, including its
current predicate. The scan and fragment remain absent from
`ExecutionPlan::children`, so displaying them does not make them generically rewritable.

Restate-specific rules cross the boundary through explicit APIs:

- `supports_fragment_pushdown()` checks that a mixed-placement scan contains only raw,
  unlimited branches that do not already own a fragment;
- `with_fragment()` binds the fragment to local scan branches and attaches it to remote
  branches, deriving schema and partitioning from the same template while discarding
  remote ordering that the worker's stream boundary cannot guarantee;
- `RemoteNodeExec::can_accept_fragment()` and `with_fragment()` prevent a later pass
  from silently replacing work selected by an earlier optimizer.

The generic fragment foundation does not require making the remote node's children
generically rewritable. This keeps placement and remote-execution claims consistent.

### 4.4 Physical optimizer ordering

The placement nodes are created by `TableProvider::scan`, so subsequent physical
optimizer passes can recognize them. Restate inserts `PartialAggregationPushdown` after
DataFusion has split aggregates and before the post-optimization `FilterPushdown`. It
then runs `ScanFragmentPushdown` after that final filter pass, so scan-level TopK dynamic
filters are in place before stable residual filters and projections are selected.
`SanityCheckPlan` validates the resulting tree. Fragment-producing optimizers must use
the explicit `RemoteNodeExec` API rather than generic child rewriting to cross the opaque
placement boundary.

## 5. Generic remote-fragment foundation

### 5.1 In-memory representation

`RemoteFragment` contains:

- a validated `Arc<dyn ExecutionPlan>` template;
- the schema required by its unique input placeholder;
- its pre-encoded `RemoteQueryScannerFragment` wire value.

The template represents a single-input physical plan. `FragmentLeafExec` marks where the
raw partition scan belongs. The placeholder contains the expected input schema, cannot
execute by itself, and is the only custom extension node understood by the fragment
codec.

For example, the row-wise optimizer constructs:

```text
ProjectionExec
└── FilterExec
    └── FragmentLeafExec
```

The foundation accepts arbitrary standard DataFusion physical nodes and expressions
that its protobuf codec can round-trip, subject to the structural contract below. It
does not decide whether a rewrite is semantically valid. That decision belongs to the
optimizer constructing the fragment.

### 5.2 Structural contract

Every fragment is validated before encoding and again after decoding. It must:

1. contain exactly one `FragmentLeafExec`;
2. contain no other leaf nodes;
3. contain only unary operators above that leaf;
4. produce exactly one output partition;
5. declare an input schema equal to the raw projected scan schema;
6. decode to an output schema equal to the schema declared on the wire.

These constraints make fragment binding deterministic and match one remote scanner
cursor to one output stream. Multi-input plans and plans containing an unbound ordinary
leaf are rejected.

`RemoteFragment::try_new` serializes the validated template once. Every physical
partition can clone the encoded bytes instead of rebuilding the same plan. On a worker,
`RemoteFragment::from_wire` decodes and validates the template against the concrete scan
schema. A format-version mismatch is a clean decline.

### 5.3 Binding and execution

Binding replaces the unique `FragmentLeafExec` with a concrete `ExecutionPlan` while
rebuilding the path to the root through `with_new_children`.

There are three uses of the same operation:

1. an optimizer binds the template to a local `PartitionScanExec`;
2. a remote worker binds it to the local scan stream through a one-shot
   `PartitionStream` adapter and DataFusion's standard `StreamingTableExec`;
3. the coordinator binds it to a raw remote stream when the worker declines it.

The fragment therefore has one implementation and one output schema regardless of
where it executes. The remote protocol only negotiates placement; it does not carry a
feature-specific alternative representation.

### 5.4 Feature-specific safety

Serialization is necessary but not sufficient for a sound optimizer rewrite. Each
fragment producer must additionally validate the operators and expressions it moves.
For example, the partial-aggregation producer rejects:

- volatile expressions;
- `DynamicFilterPhysicalExpr` state shared with coordinator-side operators;
- `UnKnownColumn`, whose runtime fallback cannot be reconstructed reliably;
- `CastExpr` values with non-default cast options, because the physical protobuf does
  not preserve those options;
- partial aggregates that exploit input ordering, use grouping sets, or cannot construct
  accumulator state that `PartialReduce` can merge.

This separation keeps the execution foundation reusable without weakening the semantic
requirements of individual optimizations.

## 6. Ownership semantics

### 6.1 Planning and execution

The coordinator resolves all selected partitions before returning the scan plan. The
selected `NodeId`, including its generation when available, becomes immutable state of
the physical plan. Planning fails if routing metadata cannot identify an owner.

Execution validates that decision but does not make a new routing decision:

- a planned local branch checks that the partition still resolves as local before
  opening the scanner;
- a planned remote branch connects directly to the `NodeId` stored in
  `RemoteNodeExec`;
- the remote `Open` carries `expected_partition_owner` for generational targets;
- the server validates both its identity and current local ownership before constructing
  the scan stream.

Any mismatch fails the DataFusion query. No branch is redirected, and one result cannot
combine data read under independently resolved ownership snapshots.

### 6.2 Wire acknowledgement

`RemoteQueryScannerOpen.expected_partition_owner` is optional bilrost tag 9. It is unset
for node-level scans, which use the scanner protocol but are not partition-routed.

An older worker can ignore tag 9 and return legacy `Success`. During a rolling upgrade,
the client accepts that response only when the nodes configuration positively identifies
the exact connected node generation as a pre-v1.8 binary. Missing, unknown, malformed,
or v1.8-and-newer versions remain strict and must return `SuccessWithOwnerValidation` or
`SuccessWithFragment`.

The narrow pre-v1.8 compatibility path cannot provide the new server-side ownership
fence because the old worker does not implement it. It is limited to the known old node
generation and disappears when that worker restarts on v1.8. Outside that rolling-upgrade
window, every partition-routed success positively acknowledges owner validation.

## 7. Fragment wire negotiation and fallback

`RemoteQueryScannerOpen.fragment` is optional bilrost tag 10. Its
`RemoteQueryScannerFragment` contains:

- `format_version`;
- the serialized physical plan;
- the declared output schema.

`REMOTE_FRAGMENT_FORMAT_VERSION` must change for every DataFusion upgrade or fragment
codec change. Compatibility depends on the exact physical-plan serialization contract,
not only the message shape.

The open handshake has three successful responses:

- `Success`: legacy open without positive ownership or fragment acknowledgement;
- `SuccessWithOwnerValidation`: the owner was validated, but the fragment was absent or
  declined before input was consumed;
- `SuccessWithFragment`: the owner was validated when requested and the fragment was
  accepted before input was consumed.

Execution proceeds as follows:

1. the server validates planned ownership;
2. it decodes and validates the optional fragment against the raw scan schema;
3. if accepted, it binds the fragment over the raw scan and replies
   `SuccessWithFragment`;
4. if unsupported, including a format-version mismatch, it replies with owner validation
   and streams raw batches;
5. the client applies the same in-memory fragment over that raw pull cursor before
   exposing any batches to the parent plan;
6. a failure after fragment acceptance fails the query because raw input may already
   have been consumed.

The outward `RemoteNodeExec` schema is always the fragment output schema. The client
checks decoded record batches against the expected raw or fragment schema, preventing a
decline or peer error from leaking raw batches into a fragment-output stream.
`RemoteNodeExec` does not advertise fragment-derived output ordering because the worker
binds the fragment to an unordered stream boundary; downstream operators must restore any
ordering they require.

The generic fallback is part of the cursor state machine, so future fragment types do
not need their own transport adapter.

## 8. Pull execution, cancellation, and dynamic filters

### 8.1 Pull contract

The client exposes a `RecordBatchStream` backed by a `try_unfold` cursor:

```text
Opening ──accepted──► Ready(fragment) ──poll / one Next──► Ready(fragment)
   │
   ├──declined──► Fallback(raw pull cursor + local fragment)
   └──failure───► Done
```

The invariants are:

1. batch pulling waits for `Open` to complete;
2. at most one `Next` RPC is outstanding per cursor;
3. `Next` is awaited inside the future created by downstream `poll_next`;
4. returning a batch restores `Ready` without channel prefetch;
5. every live state after `Open` reaches the wire owns the remote-scanner close guard;
6. EOF and terminal server failures disarm the guard because the server has already
   removed the cursor.

Fallback remains pull-driven because its input is another instance of the raw remote
cursor. A pipeline-breaking local fallback may consume several raw batches during one
outward poll, but each raw `Next` is still issued only when that fragment requests it.

### 8.2 Cancellation

Dropping the DataFusion stream drops the remote-scanner guard and sends `Close`. Each
server scanner-map entry contains both its request channel and a cancellation signal.
On `Close`, the server removes the handle and signals cancellation before replying.

`ScannerTask` selects cancellation and peer death both while idle and while polling
`stream.next()`. This is essential for aggregates and other pipeline-breaking fragments,
whose first output may require consuming the complete input.

### 8.3 TopK dynamic-filter pushdown

For `ORDER BY ... LIMIT K`, DataFusion's coordinator-side TopK owns a shared
`DynamicFilterPhysicalExpr`. As its threshold improves, the raw scan can discard rows
that cannot enter the global top K.

The placement nodes preserve this mechanism independently of fragment execution:

1. `LocationAwareScanExec` routes DataFusion's filter to every placement branch;
2. before a fragment is attached, `RemoteNodeExec` forwards that filter to its private
   `PartitionScanExec`;
3. the remote cursor samples the predicate generation immediately before every `Next`
   and piggybacks a changed predicate on that pull;
4. the server updates the predicate before polling the next batch.

Because there is no read-ahead, the parent TopK processes the preceding batch before the
next generation snapshot. The same cursor path is used when a fragment is accepted and
when it is declined, so dynamic updates remain active in both cases.

Once a fragment is attached, generic filter pushdown stops at `RemoteNodeExec`. This
prevents a later optimizer pass from moving a dynamic or ordinary filter across an
already validated fragment boundary. A feature-specific rule must deliberately place
any exact filter required by its semantics inside the fragment.

## 9. Fragment-producing feature layers

### 9.1 Stable filters and computed projections

`ScanFragmentPushdown` recognizes a maximal `[FilterExec | ProjectionExec]+` chain whose
input is a fragment-capable `LocationAwareScanExec` or `RemoteNodeExec`. It validates every
expression, rebuilds the chain over `FragmentLeafExec`, and replaces the coordinator-side
chain with location-specific execution of that template.

Filters with a fetch limit are rejected because applying the limit independently per
partition changes semantics. Volatile expressions, mutable dynamic filters, unresolved
columns, and casts whose options are lost by the protobuf codec are also rejected. When
an unsafe outer operator covers a safe suffix, the suffix remains independently eligible;
for example, a volatile projection stays at the coordinator while its stable filter can
still run at partition owners.

This feature makes residual filtering exact at the worker even when a storage scanner
only treats its scan predicate as a best-effort hint. Computed projections reduce both
bytes transferred and coordinator CPU. Simple column pruning still belongs to the table
scan's ordinary projection.

### 9.2 Partial aggregation

#### 9.2.1 Rewrite

DataFusion's `PartialReduce` consumes partial accumulator state and produces partial
accumulator state. This allows the original partial stage to move into each placement
branch while its hash repartitioning and final aggregate remain on the coordinator:

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
            └── RemoteNodeExec: N2,
                fragment=[AggregateExec: Partial -> FilterExec: residual SQL predicate],
                scan=[PartitionScanExec: predicate=storage predicate]
```

`PartialAggregateFragment` is now a feature-level planning object. It retains the group
and aggregate metadata required to construct `PartialReduce`, plus an
`Arc<RemoteFragment>` containing the exact physical template. It does not define a
separate wire format or execution path.

#### 9.2.2 Eligibility

The partial-aggregation optimizer rewrites only when all of these conditions hold:

- the candidate is a DataFusion `AggregateExec` in `Partial` mode;
- its input reaches a `LocationAwareScanExec` containing a remote branch, or a direct
  `RemoteNodeExec`, through stable `FilterExec`/`ProjectionExec` operators and at most one
  round-robin `RepartitionExec` inserted by DataFusion;
- row-wise operators produce the schema expected by the aggregate and pass the same
  safety checks as standalone scan fragments;
- the scan has no pushed limit;
- the partial aggregate uses linear input order and advertises no output ordering;
- there is one ordinary grouping set;
- DataFusion can construct the row or grouped accumulator used by `PartialReduce` for
  the concrete aggregate, argument, and result types, and the complete fragment can be
  serialized;
- there is no `DISTINCT`, aggregate ordering, reversed aggregate, or unsupported null
  treatment;
- grouping, aggregate-argument, aggregate `FILTER (WHERE ...)`, and row-wise expressions
  pass the remote expression-safety checks described above;
- the output accumulator-state schema can be computed before execution.

Unsupported shapes remain unchanged and run using the ordinary coordinator plan.
Eligibility is an optimization decision, not a query-validity rule. Local-only scans
also remain unchanged because moving their partial aggregate has no distributed benefit.

#### 9.2.3 Exact filtering

`PartitionScanExec` stores one scan predicate. It starts with the provider predicate and
later filter-pushdown passes conjoin any dynamic expressions into the same tree. The
expression-generation mechanism identifies whether that tree can change, so a separate
static-versus-dynamic representation is unnecessary. The scan predicate can prune data,
but it is not an exact semantic replacement because a `ScanPartition` implementation may
ignore pushed predicates.

The optimizer therefore clones each validated residual filter into both local and remote
fragment templates before the aggregate. This preserves SQL filtering semantics even
when the underlying scanner does not apply its pushed copy. Aggregate `FILTER` clauses
remain part of the cloned partial aggregate and are evaluated against raw input rows.

Dynamic TopK filters remain mutable components of the scan predicate and are not
serialized into the physical fragment.

## 10. Correctness invariants

The implementation and future fragment-producing optimizers must preserve:

1. **Fixed placement**: every physical partition has one planned location for the life
   of the plan.
2. **No rerouting**: execution uses that location or fails the query.
3. **One boundary, one target**: every `RemoteNodeExec` contains partitions for exactly
   its displayed target.
4. **No cross-location lane**: one output partition never switches RPC targets.
5. **Opaque remote work**: only Restate-aware code may attach a fragment.
6. **One fragment input**: a fragment has exactly one placeholder and no other leaves.
7. **One fragment output partition**: a scanner cursor exposes exactly one output
   stream.
8. **Schema agreement**: the raw input and declared output schemas are checked at every
   binding and wire boundary.
9. **Feature safety**: serializability never substitutes for optimizer-specific semantic
   validation.
10. **No silent replacement**: an optimizer cannot overwrite a fragment attached by an
    earlier pass or move a fragment below a scan-level limit.
11. **Uniform branch schema**: all children of `LocationAwareScanExec` agree after
    pushdown or fallback.
12. **No post-accept fallback**: a failure after remote consumption fails the query.
13. **Pull boundedness**: a cursor has no more than one outstanding `Next`.
14. **Cancellation reachability**: dropping the client stream can interrupt the worker
    while it is producing the current batch.
15. **Dynamic-filter freshness**: every new raw pull observes the latest available
    predicate generation.
16. **Statistics are not multiplied**: splitting one scan by placement does not
    multiply its global row estimate.
17. **Remote ordering is conservative**: a fragment boundary never advertises ordering
    that the worker-side stream adapter does not provide.

## 11. Implementation sequence

### Phase 1: location-aware scan foundation — implemented

- planning-time `PartitionLocation` resolution;
- location-specific execution lanes;
- `LocationAwareScanExec`, local `PartitionScanExec`, and opaque `RemoteNodeExec`;
- fixed-location execution and owner validation through wire tag 9;
- strict pull cursor and active server cancellation;
- one scan predicate with generation-based TopK updates;
- TopK dynamic-filter forwarding through the remote boundary.

### Phase 2: arbitrary single-input fragment foundation — implemented

- generic `RemoteFragment` and `FragmentLeafExec` template;
- DataFusion protobuf encoding with one custom leaf codec;
- structural and schema validation on both peers;
- optional tag-10 negotiation and `SuccessWithFragment` acknowledgement;
- generic worker execution and coordinator fallback over the same template;
- output-schema validation without exposing raw fallback batches;
- preservation of pull, cancellation, and dynamic-filter behavior.

### Phase 3: partial-aggregation layer — implemented

- feature-specific eligibility and expression-safety checks;
- stable row-wise input chain plus partial aggregate as a generic template;
- local branch binding and remote fragment attachment;
- `PartialReduce` merge on the coordinator;
- aggregate `FILTER` support and accumulator-based eligibility without a function-name
  allowlist;
- plan-shape, result-equivalence, fallback, and codec coverage.

### Phase 4: row-wise fragments — implemented

- maximal stable `FilterExec`/`ProjectionExec` chains independent of aggregation;
- computed projections and exact residual filters at partition owners;
- safe-suffix pushdown when an outer operator is ineligible;
- preservation of scan-level TopK dynamic filters.

Additional fragment-producing features should still be added one at a time, with their
own semantic proof and tests. Multi-input plans require a different execution and
scheduling model and remain outside this foundation.

### Phase 5: rollout and operations

- extend the wire-level compatibility tests with live mixed-version cluster coverage;
- measure bytes, time to first batch, total latency, coordinator and worker CPU, and
  peak worker memory;
- test ownership movement, fragment decline, cancellation, and TopK queries;
- add fragment admission control that declines before consuming input rather than
  waiting while holding a cursor;
- extend the accepted/declined execution-plan counters with low-cardinality decline
  reasons.

## 12. Test strategy

The foundation requires high-signal tests for:

- placement isolation, explicit plan shape, and global statistics;
- owner-protocol compatibility and positive acknowledgement;
- an arbitrary multi-operator unary fragment round trip and execution;
- rejection of version, structure, input-schema, and output-schema mismatches;
- applied and declined fragments through the transport;
- no prefetch and no more than one outstanding pull;
- dynamic-filter updates after fragment acceptance and decline;
- cancellation of a blocked active fragment pull.

The partial-aggregation layer additionally covers:

- mixed local and remote plan shape and result equivalence;
- grouped and global aggregation;
- aggregate `FILTER` clauses and an aggregate outside the original five-function
  allowlist;
- exact residual filters, including a scanner that ignores pushed predicates;
- expression-safety rejection, including volatile and lossy encodings;
- local fallback without exposing raw batches;
- unsupported accumulator and aggregate shapes remaining unmodified.

The remaining integration matrix should include live mixed-version clusters, empty input,
supported null behavior, post-accept remote failure, ownership movement between planning
and execution, and representative TopK queries against accepted and declined fragments.

## 13. Operational visibility

Every remote fragment exposes `remote_fragment_accepted` and
`remote_fragment_declined` execution-plan counters in `EXPLAIN ANALYZE`. Accept and
fallback decisions are also debug events. Wire tests verify that fragment-free requests
remain byte-identical to the legacy request and that both peers decode the additive
request and response shapes they can receive.

Additional remote-fragment metrics should include:

- active raw and fragment cursors;
- declined fragments by low-cardinality reason;
- raw rows and bytes read versus result rows and bytes sent;
- fragment execution time and time to first batch;
- cancellation count and latency;
- ownership-validation failures;
- DataFusion memory consumption or admission failures.

Expected declines caused by a version mismatch, unsupported plan, or admission pressure
should be debug events and counters rather than one warning per scanner. Ownership
mismatch, missing acknowledgement, and post-accept execution failure remain query errors
and should report the partition, planned target, and validation result without including
user row data.
