//! Race execution: launch K targets in parallel, first valid response wins.
//! Per spec §5.5.

use crate::error::{CoreError, Result};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Copy)]
pub struct RaceConfig {
    pub race_size: u8,
    pub abort_grace_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaceOutcome {
    Winner,
    Loser,
}

/// A single "lane" in the race.
///
/// - `future`: the user-supplied async work that produces a `Result<T>`. `Ok` means a valid
///   response was obtained; `Err` means this lane failed.
///
/// - `handle`: the `JoinHandle` of the task that owns `future` (and any associated resources,
///   like an UpstreamClient response body). When the race is lost, `run` will `await` this handle for
///   up to `abort_grace_ms` to let the lane write its usage row and drop the body, then
///   hard-abort it if the grace window expires.
///
/// - `abort_signal`: the sender side of a oneshot that `run` fires with `AbortReason::Lost`
///   when this lane is a loser. The lane's task is expected to `select!` on the receiver
///   side and clean up (write usage row, drain body) within `abort_grace_ms`.
pub struct Lane<T> {
    pub future: std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send>>,
    pub handle: JoinHandle<()>,
    pub abort_signal: oneshot::Sender<AbortReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortReason {
    /// Another lane won the race.
    Lost,
    /// The client disconnected (pass-through cancellation; not used by `run` itself).
    ClientGone,
}

#[derive(Debug)]
pub enum RaceResult<T> {
    Won { value: T, lane_index: usize },
    AllFailed { last_error: CoreError },
}

/// Run `lanes` in parallel. The first lane whose future completes `Ok` wins; its value is
/// returned in `RaceResult::Won { value, lane_index }`. The remaining lanes are signalled
/// via `abort_signal` (`AbortReason::Lost`) and awaited for up to `abort_grace_ms`; any lane
/// that has not finished within the grace window is hard-aborted via its `JoinHandle`.
///
/// If every lane completes with `Err`, `RaceResult::AllFailed` is returned with the
/// highest-priority error per spec §5.5 C5
/// (timeout > 5xx transient > 429 > 4xx > network > parse).
///
/// `race_size == 1` is a fast path: no cancellation signals are emitted; the single lane's
/// outcome is returned directly. Callers distinguish a winner from total-failure by
/// inspecting the variant.
pub async fn run<T: 'static>(config: &RaceConfig, mut lanes: Vec<Lane<T>>) -> RaceResult<T> {
    if lanes.len() <= 1 {
        let lane = match lanes.pop() {
            Some(l) => l,
            None => {
                return RaceResult::AllFailed {
                    last_error: CoreError::Internal("race::run called with zero lanes".into()),
                };
            }
        };
        // No cancellation in the single-lane path; drop the sender so it's never fired.
        drop(lane.abort_signal);
        match lane.future.await {
            Ok(value) => RaceResult::Won {
                value,
                lane_index: 0,
            },
            Err(e) => RaceResult::AllFailed { last_error: e },
        }
    } else {
        run_parallel(config, lanes).await
    }
}

async fn run_parallel<T: 'static>(config: &RaceConfig, lanes: Vec<Lane<T>>) -> RaceResult<T> {
    debug_assert!(lanes.len() >= 2);

    // Broadcast channel: lets each lane learn the winner index. The first lane to produce
    // `Ok` sets it; the others observe `Some(_)` and short-circuit to `RaceLost`.
    let (winner_tx, winner_rx) = tokio::sync::watch::channel::<Option<usize>>(None);

    // Build the polling futures. Each combined future races the user future against the
    // winner-broadcast: if the broadcast says "already won", the lane returns `RaceLost`
    // without polling the user future further.
    let mut slots: Vec<LaneSlot<T>> = Vec::with_capacity(lanes.len());
    for (idx, lane) in lanes.into_iter().enumerate() {
        let user_future = lane.future;
        let mut my_winner_rx = winner_rx.clone();
        let combined = async move {
            if my_winner_rx.borrow_and_update().is_some() {
                return Err(CoreError::RaceLost);
            }
            user_future.await
        };
        slots.push(LaneSlot {
            index: idx,
            abort_signal: Some(lane.abort_signal),
            handle: lane.handle,
            combined: Some(Box::pin(combined)),
        });
    }

    // Drive all lanes concurrently. We move the boxed combined futures out of the slots and
    // into a `FuturesUnordered` that yields `(index, result)` for each completion.
    let mut stream = FuturesUnordered::new();
    for slot in slots.iter_mut() {
        let idx = slot.index;
        let fut = slot.combined.take().expect("combined future missing");
        let wrapped = async move {
            let res = fut.await;
            (idx, res)
        };
        stream.push(Box::pin(wrapped));
    }

    let mut winner: Option<(usize, T)> = None;
    let mut last_error: Option<CoreError> = None;

    // Pull completions. The first `Ok` declares the winner; we break out and let the
    // stream drop, which cancels the still-pending futures (their user futures are also
    // dropped; the lane tasks observe `abort_signal` in the cleanup phase and clean up).
    while let Some((idx, res)) = stream.next().await {
        match res {
            Ok(value) => {
                if winner.is_none() {
                    let _ = winner_tx.send(Some(idx));
                    winner = Some((idx, value));
                }
                // Late Ok from a lane that didn't observe the broadcast in time. We
                // discard the value; the lane is still a loser and gets signalled below.
            }
            Err(e) => {
                if winner.is_none() {
                    match &last_error {
                        Some(cur) => {
                            if error_priority(&e) < error_priority(cur) {
                                last_error = Some(e);
                            }
                        }
                        None => last_error = Some(e),
                    }
                }
            }
        }
        if winner.is_some() {
            break;
        }
    }

    // Drop the stream (cancels still-pending futures) and the winner sender.
    drop(stream);
    drop(winner_tx);

    // Cleanup: signal losers, wait for grace, then hard-abort.
    let grace = Duration::from_millis(config.abort_grace_ms);
    let winner_idx = winner.as_ref().map(|(i, _)| *i);

    for slot in slots.iter_mut() {
        if Some(slot.index) == winner_idx {
            // The winner's task owns the response stream that the caller will keep reading.
            // We do NOT abort it: aborting would cancel the very response we just won.
            // The caller's pipeline is expected to hold the corresponding `JoinHandle`
            // (or its equivalent) separately. From `run`'s point of view, we simply drop
            // ours.
            continue;
        }

        // Signal the lane to clean up. The sender is consumed (oneshot is single-use).
        if let Some(tx) = slot.abort_signal.take() {
            let _ = tx.send(AbortReason::Lost);
        }

        // Wait for the task to finish within the grace window. We reborrow `&mut *handle`
        // so that the `&mut JoinHandle` can be reused in the timeout branch.
        let handle = &mut slot.handle;
        match tokio::time::timeout(grace, &mut *handle).await {
            Ok(_) => {
                // Lane finished within grace.
            }
            Err(_) => {
                // Grace expired: hard-abort.
                handle.abort();
            }
        }
    }

    match winner {
        Some((lane_index, value)) => RaceResult::Won { value, lane_index },
        None => RaceResult::AllFailed {
            last_error: last_error.unwrap_or_else(|| {
                CoreError::Internal("race::run: all lanes failed but no error captured".into())
            }),
        },
    }
}

struct LaneSlot<T> {
    index: usize,
    /// Combined future (user future + winner-broadcast short-circuit). Moved into the
    /// `FuturesUnordered` stream; the slot keeps the rest of its state.
    combined: Option<std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send>>>,
    abort_signal: Option<oneshot::Sender<AbortReason>>,
    handle: JoinHandle<()>,
}

/// Error priority per spec §5.5 C5. Lower value = higher priority. When all lanes fail, the
/// `AllFailed` variant carries the error with the lowest priority value (i.e. highest
/// priority) seen.
pub fn error_priority(err: &CoreError) -> u8 {
    use crate::error::CoreError::*;
    match err {
        UpstreamTimeout { .. } => 0,
        UpstreamError { status, .. } if *status == 502 || *status == 503 || *status == 504 => 1,
        RateLimited { .. } => 2,
        UpstreamError { status, .. } if (400..500).contains(status) => 3,
        UpstreamConnection(_) => 4,
        UpstreamError { .. } => 5, // residual 1xx/3xx — shouldn't reach here
        Parse(_) => 5,
        RaceLost | ClientDisconnected => 6,
        _ => 6,
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CoreError;
    use std::sync::Arc;
    use std::time::Instant;

    /// Build a Lane whose `future` is the provided async block and whose `handle` is a no-op
    /// task that exits immediately. The `abort_signal` sender is returned in the `Lane` for
    /// `run` to fire.
    fn make_lane<T, F>(f: F) -> (Lane<T>, oneshot::Receiver<AbortReason>)
    where
        F: std::future::Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let (abort_tx, abort_rx) = oneshot::channel::<AbortReason>();
        let lane = Lane {
            future: Box::pin(f),
            handle: tokio::spawn(async {}),
            abort_signal: abort_tx,
        };
        (lane, abort_rx)
    }

    fn cfg(size: u8, grace_ms: u64) -> RaceConfig {
        RaceConfig {
            race_size: size,
            abort_grace_ms: grace_ms,
        }
    }

    #[tokio::test]
    async fn race_size_1_runs_sequentially() {
        let (lane, _rx) = make_lane(async { Ok::<i32, CoreError>(42) });
        let result = run(&cfg(1, 100), vec![lane]).await;
        match result {
            RaceResult::Won { value, lane_index } => {
                assert_eq!(value, 42);
                assert_eq!(lane_index, 0);
            }
            RaceResult::AllFailed { .. } => panic!("expected Won"),
        }
    }

    #[tokio::test]
    async fn race_size_1_propagates_error() {
        // Build a future that produces a known error. We can't `Clone` CoreError, so we
        // construct it inside the future.
        let (lane, _rx) = make_lane(async {
            Err::<i32, _>(CoreError::UpstreamTimeout {
                phase: "ttft".into(),
                ms: 500,
            })
        });
        let result = run(&cfg(1, 100), vec![lane]).await;
        match result {
            RaceResult::AllFailed { last_error } => {
                // Match on the variant rather than comparing the full value (CoreError has
                // no `PartialEq`).
                match last_error {
                    CoreError::UpstreamTimeout { phase, ms } => {
                        assert_eq!(phase, "ttft");
                        assert_eq!(ms, 500);
                    }
                    other => panic!("expected UpstreamTimeout, got {:?}", other),
                }
            }
            RaceResult::Won { .. } => panic!("expected AllFailed"),
        }
    }

    #[tokio::test]
    async fn race_size_3_first_wins() {
        // Lane 0: slow Ok
        // Lane 1: fast Ok
        // Lane 2: slow Ok
        // Winner must be lane 1.

        let (l0, mut r0) = make_lane(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<&'static str, CoreError>("zero")
        });
        let (l1, mut r1) = make_lane(async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok::<&'static str, CoreError>("one")
        });
        let (l2, mut r2) = make_lane(async {
            tokio::time::sleep(Duration::from_millis(80)).await;
            Ok::<&'static str, CoreError>("two")
        });

        let result = run(&cfg(3, 50), vec![l0, l1, l2]).await;
        match result {
            RaceResult::Won { value, lane_index } => {
                assert_eq!(value, "one");
                assert_eq!(lane_index, 1);
            }
            RaceResult::AllFailed { .. } => panic!("expected Won"),
        }

        // The losers must have been signalled.
        assert!(r0.try_recv().is_ok(), "lane 0 should have been signalled");
        assert!(
            r1.try_recv().is_err(),
            "lane 1 (winner) should not be signalled"
        );
        assert!(r2.try_recv().is_ok(), "lane 2 should have been signalled");
    }

    #[tokio::test]
    async fn race_size_3_all_fail_returns_highest_priority() {
        // Three errors with different priorities:
        //   lane 0: parse         (priority 5)
        //   lane 1: 401           (priority 3)
        //   lane 2: timeout       (priority 0)   <-- highest priority
        let (l0, _r0) = make_lane(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Err::<(), _>(CoreError::Parse("bad json".into()))
        });
        let (l1, _r1) = make_lane(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Err::<(), _>(CoreError::UpstreamError {
                status: 401,
                provider: "p".into(),
                model: "m".into(),
                body: "unauthorized".into(),
            })
        });
        let (l2, _r2) = make_lane(async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Err::<(), _>(CoreError::UpstreamTimeout {
                phase: "ttft".into(),
                ms: 200,
            })
        });

        let result = run(&cfg(3, 50), vec![l0, l1, l2]).await;
        match result {
            RaceResult::AllFailed { last_error } => match last_error {
                CoreError::UpstreamTimeout { phase, ms } => {
                    assert_eq!(phase, "ttft");
                    assert_eq!(ms, 200);
                }
                other => panic!(
                    "expected highest-priority (UpstreamTimeout) error, got {:?}",
                    other
                ),
            },
            RaceResult::Won { .. } => panic!("expected AllFailed"),
        }
    }

    #[tokio::test]
    async fn race_size_2_loser_cancellation_grace() {
        // Lane 0: fails slowly with UpstreamConnection.
        // Lane 1: succeeds fast.
        // After Lane 1 wins, Lane 0 must receive the abort signal within the grace window
        // and `run` must return well before Lane 0's 500ms would expire.
        let (l0, mut r0) = make_lane(async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Err::<(), _>(CoreError::UpstreamConnection("slow".into()))
        });
        let (l1, _r1) = make_lane(async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok::<(), CoreError>(())
        });

        let grace_ms = 50u64;
        let t0 = Instant::now();
        let result = run(&cfg(2, grace_ms), vec![l0, l1]).await;
        let elapsed = t0.elapsed();

        match result {
            RaceResult::Won {
                value: (),
                lane_index,
            } => assert_eq!(lane_index, 1),
            _ => panic!("expected Won with lane_index=1"),
        }

        // The loser should have been signalled.
        assert!(
            r0.try_recv().is_ok(),
            "loser should have received AbortReason::Lost"
        );
        // Total time should be much less than the 500ms the loser would have taken.
        assert!(
            elapsed < Duration::from_millis(grace_ms + 200),
            "race took too long ({:?}), grace exceeded",
            elapsed
        );
    }

    #[tokio::test]
    async fn abort_grace_timeout_aborts_handle() {
        // Lane 0: returns Ok quickly.
        // Lane 1: returns Ok quickly, but the lane's TASK does not exit on its own
        //         (we use a parked task that ignores its return value).
        //         `run` must hard-abort the handle after the grace window.
        //
        // We verify by wall-clock: if the runtime had waited for the (parked) handle to
        // finish naturally, the call would hang. We bound the call to grace + epsilon.
        let parked = Arc::new(tokio::sync::Notify::new());
        let parked_clone = parked.clone();
        let (l0, _r0) = make_lane(async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok::<(), CoreError>(())
        });
        // Lane 1: future returns Ok quickly, but the JoinHandle task awaits a Notify
        // that we never call. This simulates a "non-compliant" lane that ignores the
        // abort signal and never cleans up.
        let (abort_tx, _abort_rx) = oneshot::channel::<AbortReason>();
        let handle = tokio::spawn(async move {
            parked_clone.notified().await;
        });
        let l1 = Lane {
            future: Box::pin(async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok::<(), CoreError>(())
            }),
            handle,
            abort_signal: abort_tx,
        };

        let grace_ms = 80u64;
        let t0 = Instant::now();
        let result = run(&cfg(2, grace_ms), vec![l0, l1]).await;
        let elapsed = t0.elapsed();

        // Release the parked task (it might have been aborted anyway).
        parked.notify_one();

        match result {
            RaceResult::Won {
                value: (),
                lane_index,
            } => assert_eq!(lane_index, 0),
            _ => panic!("expected Won with lane_index=0"),
        }

        // If the runtime had waited for the (intentionally blocked) handle, the call
        // would hang. We bound it to grace + a generous epsilon.
        assert!(
            elapsed < Duration::from_millis(grace_ms + 500),
            "run did not hard-abort the handle within grace: took {:?}",
            elapsed
        );
    }

    // -- Auxiliary tests for helpers -----------------------------------------------

    #[test]
    fn error_priority_ordering() {
        let timeout = CoreError::UpstreamTimeout {
            phase: "p".into(),
            ms: 1,
        };
        let s5xx = CoreError::UpstreamError {
            status: 503,
            provider: "p".into(),
            model: "m".into(),
            body: "x".into(),
        };
        let rate = CoreError::RateLimited {
            provider: "p".into(),
            retry_after_ms: 100,
            is_proxy_rotated: false,
        };
        let s4xx = CoreError::UpstreamError {
            status: 404,
            provider: "p".into(),
            model: "m".into(),
            body: "x".into(),
        };
        let net = CoreError::UpstreamConnection("x".into());
        let parse = CoreError::Parse("x".into());

        assert!(error_priority(&timeout) < error_priority(&s5xx));
        assert!(error_priority(&s5xx) < error_priority(&rate));
        assert!(error_priority(&rate) < error_priority(&s4xx));
        assert!(error_priority(&s4xx) < error_priority(&net));
        assert!(error_priority(&net) < error_priority(&parse));
    }

    #[test]
    fn empty_lanes_returns_internal_error() {
        let cfg = cfg(1, 10);
        let fut = run::<()>(&cfg, vec![]);
        let result = futures::executor::block_on(fut);
        match result {
            RaceResult::AllFailed { last_error } => {
                assert!(matches!(last_error, CoreError::Internal(_)));
            }
            _ => panic!("expected AllFailed for empty lanes"),
        }
    }
}
