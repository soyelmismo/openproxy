# Usage persistence and shutdown

## Pipeline admission

The pipeline submits attempts, predictive skips, no-healthy-targets errors and
winner updates through one bounded FIFO queue (capacity 1024). Producers await
capacity instead of discarding jobs when the queue is full. No detached overflow
tasks or unbounded buffers are created.

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

The shared writer returns a usage row for publication only after commit. Callers
release the connection lock before publishing the dashboard event.

Successful audio, embeddings, image and System One requests await the same
transactional writer through `DbPool::spawn_write`. They no longer discard usage
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

This queue is in memory, not a durable journal. An abrupt crash, SIGKILL, forced
container termination or power loss can lose queued attempts. SQLite errors
that remain after the bounded retries are reported, not replayed indefinitely.
Admission awaits are cancellable: cancellation before admission does not commit
the job. Crash recovery, durable pending jobs, replay idempotency and operational
queue metrics remain separate work.

Shutdown does not impose a new hard deadline on active responses: existing
request timeouts still apply. Supervisors may impose their own termination grace
period, but a forced exit forfeits the drain guarantee. Detached producers must
be stopped by their owner before closing the worker.

The broader migration of synchronous repository methods and admin handlers to
blocking-safe async interfaces is not completed by these usage changes.
