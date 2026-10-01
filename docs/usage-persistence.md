# Usage persistence and shutdown

## Pipeline admission

The pipeline reserves bounded notification capacity (1024), then persists attempts,
predictive skips, no-healthy-targets errors and winner updates in `usage_journal`
before returning their usage tuple. Notifications wake the worker; SQLite journal
IDs define FIFO replay. No detached overflow tasks or unbounded buffers are created.
The journal rejects admission at 100,000 pending jobs instead of overwriting data.

An attempt is admitted before its usage tuple is returned. Its winner update
therefore follows its insert in the queue. If admission is closed, the builder
returns an error; response/error paths report telemetry failure without replacing
the upstream result.

Selection reputation is updated once at attempt completion, independently of
the SQLite worker. The worker does not count successes or failures again.

## SQLite writes

The worker performs synchronous SQLite operations on `spawn_blocking`. Each
attempt's usage row and persistent cooldown update share a transaction. A failed
cooldown update rolls back the usage insert as well; retrying does not leave a
partial attempt behind. Existing SQLite BUSY/LOCKED retry policy applies to the
whole transaction. Winner updates also use the BUSY/LOCKED retry policy.

Applying a job and deleting its journal entry share the same transaction. Replays
skip already acknowledged IDs, including competing replay workers; there is no
separate delete window that can duplicate a committed row after a crash.
The shared writer returns a usage row for publication only after commit. Callers
release the connection lock before publishing the dashboard event.

Successful audio, embeddings, image and System One requests admit and replay
through the same journal on blocking threads. They no longer discard usage
because the writer lock was unavailable for 100 ms. A blocking API is retained
for synchronous routing callers and existing library consumers.
Async unary execution also resolves its targets on a blocking thread, including
the not-found usage write. Image multipart routing uses the async routing helper.

## Graceful shutdown

SIGINT and, on Unix, SIGTERM stop TCP admission. Existing HTTP connections are
asked to finish gracefully; the server waits for their tasks before closing
usage admission. The worker drains accepted jobs and waits for its blocking
batch before returning. Persisting failures are logged and make worker shutdown
return an error rather than reporting success.

Library consumers using `AppState` can explicitly await
`shutdown_usage_worker()` after their own request producers have stopped.
`UsageRecordBuilder::record()` now returns a future and must be awaited; a
returned usage tuple means admission succeeded, not that the insert has already
committed. `spawn_worker()` returns a lifecycle handle instead of detaching the
worker without a shutdown path.

## Limits and remaining work

Committed admissions survive process crashes and SIGKILL and are replayed at
startup and periodically. Errors retain their entries for later retry and are
reported in pending/failed-batch metrics. Cancellation before admission does not
commit a job; cancellation after admission cannot remove its durable entry.
SQLite uses WAL with `synchronous=NORMAL`: power-loss durability is limited by
that SQLite policy, unlike process-crash recovery. Disk-full or journal-capacity
errors reject new admissions explicitly; they cannot promise accounting for a job
that was never successfully persisted.

Shutdown does not impose a new hard deadline on active responses: existing
request timeouts still apply. Supervisors may impose their own termination grace
period, but a forced exit forfeits the drain guarantee. Detached producers must
be stopped by their owner before closing the worker.

The broader migration of synchronous repository methods and admin handlers to
blocking-safe async interfaces is not completed by these usage changes.
