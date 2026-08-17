# Design: location-aware physical execution for storage queries

- **Status**: remote-scan foundation implemented; partial aggregation proposed
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
├── PartitionScanExec: location=Local
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

Partial aggregation is the first planned extension of this boundary. A dedicated
physical optimizer will turn each eligible local branch into a local partial aggregate
and attach the equivalent typed fragment to each `RemoteNodeExec`. The coordinator will
merge the returned accumulator states with `PartialReduce` before the existing final
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

`PartitionLocation` has two states: `Local` and `Remote { node_id }`.
`ScanPartition::partition_location` resolves that state while the table provider builds
the physical plan. `scan_partition_at` subsequently receives the fixed location selected
by the plan (`crates/storage-query-datafusion/src/table_providers.rs:47-89`).

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
`crates/storage-query-datafusion/src/table_providers.rs:442-503`.

Logical execution lanes never contain physical partitions from different locations. A
single lane therefore never changes from local iteration to remote RPC midway through
its stream. When the number of locations exceeds `target_partitions`, the plan uses at
least one lane per location rather than violating this boundary.

### 4.2 Node responsibilities

| Node | Responsibility | Ordinary children |
|---|---|---|
| `LocationAwareScanExec` | Concatenate location-specific partitions and retain the table's one global statistics estimate. | The local and remote placement branches. |
| `PartitionScanExec` | Scan one set of logical partitions at its already selected location; hold static and dynamic predicates separately. | None. |
| `RemoteNodeExec` | Identify one target node and own the remote scan/fragment contract for that target. | None; its scan is intentionally opaque. |

`LocationAwareScanExec` delegates execution and physical properties to an internal
`UnionExec`, but reports the original table statistics rather than summing an identical
estimate once per placement group
(`crates/storage-query-datafusion/src/table_providers.rs:253-335`).

A separate `LocalNodeExec` is unnecessary. `PartitionScanExec` is already the concrete
local execution leaf, whereas a remote branch needs an additional semantic boundary for
transport, validation, fragment negotiation, and cancellation. Adding a no-op local
wrapper would not make an otherwise illegal state unrepresentable.

### 4.3 Why `RemoteNodeExec` is opaque

`RemoteNodeExec::children()` returns no children even though the node privately owns its
`PartitionScanExec` (`crates/storage-query-datafusion/src/table_providers.rs:679-735`).
This is intentional.

If the scan were exposed as an ordinary child, a default DataFusion rule could insert a
`RepartitionExec`, `CoalesceBatchesExec`, filter, or other locally implemented operator
under `RemoteNodeExec`. The displayed plan would then claim that the operator runs
remotely even though `RemoteNodeExec::execute` could only execute it locally.

Only a Restate rule that understands the wire contract may enrich the remote boundary.
The first such enrichment will be a typed partial-aggregate fragment. If Restate later
implements a general remote plan interpreter, `RemoteNodeExec` can expose a normal child
subtree at that point.

### 4.4 Physical optimizer ordering

The location nodes are created by `TableProvider::scan`, which is part of physical-plan
construction. They are therefore visible to subsequent physical optimizer passes.
Restate currently builds its session with DataFusion's default features at
`crates/storage-query-datafusion/src/context.rs:631-635`. A partial-aggregation rule will
be appended with DataFusion's `with_physical_optimizer_rule`, so it sees both the
DataFusion aggregate split and the explicit placement nodes.

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
`crates/storage-query-datafusion/src/remote_query_scanner_manager.rs:229-255`; the server
invokes it before scanner construction in
`crates/storage-query-datafusion/src/scanner_task.rs:75-90`.

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
(`crates/types/src/net/remote_query_scanner.rs:290-332`).

## 6. Pull execution and backpressure

The client exposes a `RecordBatchStream` with this state machine:

```text
Opening ──success──► Ready ──downstream poll──► Pulling
   │                   ▲                           │
   │                   └──────── batch ───────────┘
   └──failure──► Done          EOF/error ───────► Done
```

The implementation is in
`crates/storage-query-datafusion/src/remote_query_scanner_client.rs:155-336`.

The invariants are:

1. Normal batch pulling waits for `Open` to complete before creating a `Next`.
2. At most one `Next` RPC is outstanding for a cursor.
3. A `Next` is created only while servicing a downstream `poll_next`.
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
cancellation signal (`crates/storage-query-datafusion/src/scanner_task.rs:39-70`).

When `Close` arrives, the server removes the handle and sends the cancellation signal
before replying (`crates/storage-query-datafusion/src/remote_query_scanner_server.rs:88-97`).
`ScannerTask` selects cancellation and peer death both while idle and while polling
`stream.next()` (`crates/storage-query-datafusion/src/scanner_task.rs:159-224`). This is
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
   ordinary child (`crates/storage-query-datafusion/src/table_providers.rs:748-769`).
2. The cursor snapshots the predicate generation immediately before every `Next` and
   piggybacks a changed predicate on that pull
   (`crates/storage-query-datafusion/src/remote_query_scanner_client.rs:249-268` and
   `:338-355`).

Because there is no read-ahead, the parent TopK has processed the preceding batch before
the next generation snapshot. The server applies an updated predicate before polling its
next batch (`crates/storage-query-datafusion/src/scanner_task.rs:185-224`).

The plan-shape test proves that DataFusion's TopK filter reaches the opaque remote scan at
`crates/storage-query-datafusion/src/table_providers.rs:1258-1303`.

Partial aggregation does not replace this mechanism. A remote fragment must keep the
dynamic predicate below the aggregate so filtering still applies to raw rows. Queries
whose TopK expression cannot soundly filter raw input remain coordinator-only, as they
are today.

## 9. Partial-aggregation extension

### 9.1 Target plan rewrite

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
            │   └── PartitionScanExec: Local
            └── RemoteNodeExec: N2, fragment=PartialAggregate
```

Every child of the rewritten `LocationAwareScanExec` has the same accumulator-state
schema. The root is rebuilt from those children so its physical properties reflect the
new schema, and its statistics are reset to unknown rather than reusing raw-row table
statistics.

### 9.2 Rule eligibility

The first optimizer rule should be deliberately narrow. It may rewrite only when all of
the following are true:

- the candidate is a DataFusion `AggregateExec` in `Partial` mode;
- its input reaches a `LocationAwareScanExec` through an explicitly supported set of
  transparent nodes;
- the scan has no pushed limit;
- there is one ordinary grouping set;
- aggregate functions are from the initial built-in allowlist: `count`, `sum`, `min`,
  `max`, and `avg`;
- there is no `DISTINCT`, aggregate `FILTER`, aggregate ordering, reversed aggregate, or
  unsupported null treatment;
- every grouping and argument expression can be serialized using the already supported
  physical-expression codec;
- the partial output schema can be computed and validated before execution.

Unsupported shapes remain unchanged and execute with the ordinary scan plan. Eligibility
is an optimization decision, not a new query-validity rule.

`PartitionScanExec` stores the provider's static predicate separately from later dynamic
predicates. This lets the optimizer reason about exact predicate provenance without
mistaking a best-effort dynamic filter for a removable static `FilterExec`
(`crates/storage-query-datafusion/src/table_providers.rs:479-489` and `:649-669`). If a
remaining filter cannot be proven redundant or safely cloned into every branch, the rule
must skip the rewrite.

### 9.3 Remote fragment model

`RemoteNodeExec` should gain a typed optional fragment rather than a general serialized
child plan:

```rust
enum RemoteFragment {
    PartialAggregate(PartialAggregateFragment),
}

struct PartialAggregateFragment {
    group_exprs: Vec<NamedPhysicalExpr>,
    aggregate_exprs: Vec<SupportedAggregateExpr>,
    input_schema: SchemaRef,
    output_schema: SchemaRef,
    state_abi: AggregateStateAbi,
}
```

The exact Rust field types may follow DataFusion's builders, but the semantic contract is
fixed:

- input expressions are evaluated against the raw projected scan schema;
- output is accumulator state, not final values;
- the output schema is known at planning time;
- the server reconstructs only allowlisted aggregates;
- the server validates the reconstructed schema against the requested output schema;
- accumulator state is accepted only when both peers advertise the same state ABI.

The node remains opaque. A method such as `with_partial_aggregate` validates the fragment
and returns a new `RemoteNodeExec` with updated schema and plan properties. Generic
`with_new_children` continues to reject children.

### 9.4 Wire negotiation and fallback

The wire protocol should add an optional partial-aggregate field at tag 10. `Open` also
needs a response that states whether the fragment was applied.

Execution then proceeds as follows:

1. the server validates planned ownership;
2. it validates the fragment ABI, allowlist, expression decoding, and output schema
   before consuming input;
3. if accepted, it returns `applied=true` and streams accumulator-state batches;
4. if unsupported before execution, it returns `applied=false` and streams raw batches;
5. the client applies the same partial aggregate locally to an unapplied raw stream, so
   the outward `RemoteNodeExec` schema remains accumulator state;
6. an error after `applied=true` fails the query because raw input may already have been
   consumed.

An old server will ignore the optional tag and return the existing success response. A
new client interprets that response as `applied=false`. An old client never sends the
fragment and therefore never receives the new response variant. Flexbuffers fields must
use `serde(default, skip_serializing_if = "Option::is_none")`, and both bilrost directions
must be covered by old-shape compatibility tests.

The client cursor must not expose raw batches through its state-schema stream while
fallback is being selected. `Open` should produce an internal negotiated cursor; the
`RemoteNodeExec` execution adapter then either exposes the applied remote stream or wraps
the raw stream once with a local `AggregateExec::Partial`.

### 9.5 Serving-side execution and admission

The server constructs the ordinary local partition scan first, installs the static and
dynamic predicate below the fragment, and then wraps the stream with the reconstructed
partial aggregate. It uses the server's query `TaskContext`, so allocations are charged
to its DataFusion memory pool.

Remote aggregation also needs explicit admission accounting. The first version should
bound concurrently active aggregation fragments per node. A request that cannot obtain
admission declines the fragment before consuming input and streams raw rows instead. It
must not queue indefinitely while holding a remote cursor and ownership snapshot.

Cancellation uses the existing active-poll select and requires no fragment-specific
side channel.

## 10. Correctness invariants

The implementation and future optimizer must preserve these invariants:

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
- direct pull cursor state machine;
- active server cancellation;
- separate static and dynamic scan predicates;
- TopK pushdown forwarding through `RemoteNodeExec`;
- plan-shape, statistics, TopK, and protocol compatibility tests.

### Phase 2: local fragment model

- add the typed in-memory `PartialAggregateFragment`;
- add validated fragment state and output properties to `RemoteNodeExec`;
- implement the physical optimizer behind a default-off option;
- rewrite local branches only and execute remote fallback locally;
- add plan-shape and result-equivalence tests.

### Phase 3: remote fragment execution

- add optional wire tag 10 and explicit acceptance response;
- rebuild and validate allowlisted partial aggregates on the server;
- execute accepted fragments and locally wrap declined fragments;
- add admission control, decline metrics, and loopback protocol tests;
- test slow pipeline-breaking cancellation.

### Phase 4: rollout

- run mixed-version tests in both directions;
- compare result equivalence across the supported aggregate matrix;
- measure wire bytes, time to first batch, total latency, coordinator CPU, worker CPU,
  and peak worker memory;
- test low- and high-cardinality groups, empty input, global aggregates, ownership
  movement, fragment decline, cancellation, and TopK queries;
- enable by default only after those measurements support it.

## 12. Test strategy

The foundation has high-signal tests for placement isolation, the explicit remote plan
shape, global statistics, TopK filter propagation, and tag-9 compatibility at
`crates/storage-query-datafusion/src/table_providers.rs:1140-1303` and
`crates/types/src/net/remote_query_scanner.rs:290-332`.

The partial-aggregation phases should add:

- plan-shape tests for global and grouped aggregation;
- identical result tests for local-only, remote-only, and mixed placement;
- empty-input tests, especially global aggregation's single output row;
- every allowlisted aggregate and supported null behavior;
- explicit non-rewrite tests for every eligibility rejection;
- old-client/new-server and new-client/old-server negotiation tests;
- server-decline fallback and post-accept failure tests;
- a fake-transport pull test proving that no second `Next` is sent before another poll;
- a dynamic-filter transport test proving that a changed TopK generation is serialized
  on the following `Next`;
- cancellation while a deliberately blocking stream is producing its first batch;
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

Expected declines during rolling upgrades or admission pressure should be debug events
and counters, not one warning per scanner. Ownership mismatch and post-accept execution
failure remain query errors and should carry the partition, planned target, and current
validation result without including user row data.
