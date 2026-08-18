# Plan remote storage queries against fixed partition owners

## Behavioral Change

### What Changed

Distributed storage-query physical plans now record local and remote partition placement
explicitly. If partition ownership changes after planning, the query fails instead of
silently rerouting part of the already planned query. Remote query cancellation also
interrupts a scanner while it is actively producing the next batch. Eligible `count`,
`sum`, `min`, `max`, and `avg` partial aggregates on supported argument types now execute
at partition owners and send accumulator state instead of raw input rows to the query
coordinator.

### Why This Matters

One physical query now uses a single, internally consistent partition-placement
decision. Pull-driven remote cursors also avoid doing work ahead of downstream demand
and stop promptly when the query is cancelled. Partial aggregation can substantially
reduce network traffic and coordinator work for low-cardinality grouped aggregates.

### Impact on Users

Queries keep the same results and require no configuration changes. Unsupported
aggregate shapes continue to execute on the coordinator. During rolling upgrades, a new
querying node falls back to applying the partial aggregate locally when an older worker
does not accept the fragment. A query that races with partition movement can return a
transient error and should be retried.

### Migration Guidance

No migration is required. Clients that already retry transient query failures need no
changes.
