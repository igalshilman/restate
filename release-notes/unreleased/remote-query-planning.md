# Plan remote storage queries against fixed partition owners

## Behavioral Change

### What Changed

Distributed storage-query physical plans now record local and remote partition placement
explicitly. If partition ownership changes after planning, the query fails instead of
silently rerouting part of the already planned query. Remote query cancellation also
interrupts a scanner while it is actively producing the next batch.

### Why This Matters

One physical query now uses a single, internally consistent partition-placement
decision. Pull-driven remote cursors also avoid doing work ahead of downstream demand
and stop promptly when the query is cancelled.

### Impact on Users

Queries keep the same results and require no configuration changes. A query that races
with partition movement can now return a transient error and should be retried.

### Migration Guidance

No migration is required. Clients that already retry transient query failures need no
changes.
