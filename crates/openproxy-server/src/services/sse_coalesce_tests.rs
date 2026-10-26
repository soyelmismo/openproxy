use super::*;
use futures::StreamExt;

fn frames(items: &[&'static str]) -> Vec<Result<Bytes, Infallible>> {
    items
        .iter()
        .map(|s| Ok(Bytes::from_static(s.as_bytes())))
        .collect()
}

async fn collect(
    stream: CoalescedSse<impl Stream<Item = Result<Bytes, Infallible>> + Unpin>,
) -> Vec<Bytes> {
    stream.map(|r| r.unwrap()).collect().await
}

#[tokio::test]
async fn ready_frames_join_but_first_frame_leaves_alone() {
    let input = futures::stream::iter(frames(&[
        "data: a\n\n",
        "data: b\n\n",
        "data: c\n\n",
        "data: [DONE]\n\n",
    ]));
    let out = collect(CoalescedSse::new(input, Duration::from_millis(2))).await;
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(&out[0][..], b"data: a\n\n");
    assert_eq!(&out[1][..], b"data: b\n\ndata: c\n\ndata: [DONE]\n\n");
}

#[tokio::test]
async fn zero_window_is_passthrough() {
    let input = futures::stream::iter(frames(&["x", "y", "z"]));
    let out = collect(CoalescedSse::new(input, Duration::ZERO)).await;
    assert_eq!(out.len(), 3);
}

#[tokio::test]
async fn byte_stream_is_preserved_and_buffer_is_bounded() {
    let big: &'static str = Box::leak("z".repeat(MAX_BUFFER_BYTES / 3 + 1).into_boxed_str());
    let items = vec![big; 10];
    let input = futures::stream::iter(frames(&items));
    let out = collect(CoalescedSse::new(input, Duration::from_millis(2))).await;
    let joined: Vec<u8> = out.iter().flat_map(|b| b.iter().copied()).collect();
    assert_eq!(joined, big.repeat(10).into_bytes());
    assert!(out.iter().all(|b| b.len() <= MAX_BUFFER_BYTES + big.len()));
    assert!(
        out.len() > 2,
        "full buffer must flush without waiting for the end"
    );
}

/// Upstream that is always ready and never returns `Pending`, emitting one
/// small frame every `gap`. Models a burst that keeps the channel non-empty.
struct AlwaysReady {
    left: usize,
    gap: std::time::Duration,
}

impl Stream for AlwaysReady {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.left == 0 {
            return Poll::Ready(None);
        }
        self.left -= 1;
        std::thread::sleep(self.gap);
        Poll::Ready(Some(Ok(Bytes::from_static(b"data: t\n\n"))))
    }
}

#[tokio::test]
async fn window_is_enforced_while_upstream_never_goes_idle() {
    const FRAME: &[u8] = b"data: t\n\n";
    const FRAMES: usize = 1000;
    let window = Duration::from_millis(2);
    let gap = Duration::from_micros(200);
    // ~200 ms of continuous readiness and ~9 KB in total: far below the byte
    // cap, so only the window can split it.
    let input = AlwaysReady { left: FRAMES, gap };
    let mut stream = CoalescedSse::new(input, window);
    let mut sizes = Vec::new();
    while let Some(chunk) = stream.next().await {
        sizes.push(chunk.unwrap().len() / FRAME.len());
    }
    assert_eq!(sizes.iter().sum::<usize>(), FRAMES);
    // Each frame takes at least `gap` of wall time to appear, so a chunk of n
    // frames spans at least (n - 1) * gap. Closing it once the window has
    // elapsed caps n at window/gap + 2. Scheduler stalls only stretch wall
    // time, which lowers n, so the bound does not depend on machine load.
    let cap = (window.as_micros() / gap.as_micros()) as usize + 2;
    let largest = sizes.iter().copied().max().unwrap_or(0);
    assert!(
        largest <= cap,
        "a never-idle upstream held {largest} frames past the window (cap {cap})"
    );
}

#[tokio::test]
async fn pending_frame_is_flushed_by_the_window_while_upstream_idles() {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(8);
    let mut stream = CoalescedSse::new(
        tokio_stream::wrappers::ReceiverStream::new(rx),
        Duration::from_millis(20),
    );
    tx.send(Ok(Bytes::from_static(b"first"))).await.unwrap();
    assert_eq!(&stream.next().await.unwrap().unwrap()[..], b"first");
    tx.send(Ok(Bytes::from_static(b"second"))).await.unwrap();
    // Sender still open and idle: the window, not the end of stream, must flush.
    let started = std::time::Instant::now();
    let got = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("window flush");
    assert_eq!(&got.unwrap().unwrap()[..], b"second");
    assert!(
        started.elapsed() >= Duration::from_millis(15),
        "flushed before the window"
    );
    drop(tx);
    assert!(stream.next().await.is_none());
}
