# Usage persistence and shutdown

## Pipeline admission

The pipeline reserves bounded notification capacity (1024), then persists attempts,
predictive skips, no-healthy-targets errors and winner updates in `usage_journal`
before returning their usage tuple. Notifications wake the worker; SQLite journal
IDs define FIFO replay. No detached overflow tasks or unbounded buffers are created.
At 100,000 pending jobs, async producers wait for effective journal ACKs rather
than overwriting data or polling SQLite. The reservation bounds concurrent append
attempts by the channel's capacity. Worker closure unblocks waiting producers with
an error; synchronous append callers receive `JournalCapacityExhausted`.

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
batch before returning. Failures remain observable in batch statistics. A shutdown
that leaves durable entries pending, or encounters a task join failure, returns an
error; a transient failure successfully retried before drain does not make that
drain fail merely because the failure counter is nonzero.

`AppState` signals cancellation to discovery and the background supervisor before
joining their tasks, then closes/drains the usage worker. Quota, check-in and
Smart Warmup runners retain in-flight operations until completion and stop between
cycles/accounts. A cancelled caller waiting for shutdown does not take ownership
of, abort or detach the retained task handles; a subsequent caller resumes drain.

Library consumers using `AppState` can explicitly await
`shutdown_usage_worker()` after their own request producers have stopped.
`UsageRecordBuilder::record()` now returns a future and must be awaited; a
returned usage tuple means the journal append committed, not that the final usage
row has already been applied. `spawn_worker()` returns a lifecycle handle instead of detaching the
worker without a shutdown path.

## Limits and remaining work

Committed admissions survive process crashes and SIGKILL and are replayed at
startup and periodically. Errors retain their entries for later retry and are
reported in pending/failed-batch metrics. Cancellation while waiting for capacity
does not insert a job. Once `spawn_blocking` has been scheduled, cancelling its
await does not cancel a running SQLite commit: the job may still become durable
and be recovered by periodic replay or restart, even without its wake message.
Cancellation after admission cannot remove the durable entry.
SQLite uses WAL with `synchronous=FULL` by default. `[storage].synchronous="normal"`
explicitly selects weaker power-loss durability for greater throughput. `FULL`
still depends on storage honoring flushes. Disk-full/corruption errors reject new
admissions explicitly; no policy promises accounting for a job that never persisted.

Shutdown does not impose a new hard deadline on active responses: existing
request timeouts still apply. Supervisors may impose their own termination grace
period, but a forced exit forfeits the drain guarantee. Detached producers must
be stopped by their owner before closing the worker.

Legacy synchronous repository APIs remain available for blocking callers. Async
pipeline access uses `AsyncPipelineRepository`; pool and shared-connection OAuth
operations acquire SQLite locks only inside blocking closures. These boundaries
do not justify retaining a connection guard across an `.await` in new code.
