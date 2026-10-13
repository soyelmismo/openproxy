# Upload and generation budgets

`UpstreamClient` distinguishes connection acquisition, request upload and
response generation. A timeout never races the full Hyper response future
under the upload budget.

- DNS/dial/TLS keep their connector budgets and total ceiling.
- `request_send_ms` starts after connection acquisition, including reuse.
  Completion requires body consumption and a subsequent successful IO flush.
  Bodies are delivered as bounded 16 KiB frames, preserving backpressure.
- Streaming TTFT starts after upload and keeps the same deadline after HTTP
  headers. A stub event does not count as model output. Meaningful output
  switches to the idle chunk budget. Total is always an absolute ceiling.
- Unary generation waits under `total_ms` both before headers and between body
  chunks. It does not apply streaming TTFT or idle chunk budgets. Upload and
  connection timeouts still apply.
- Cancellation remains active in all phases.

HTTP flush means the transport accepted the upload, not that the provider
acknowledged processing. Upload tracking is per request; connection flush
metadata is shared safely for pooled HTTP/1 and multiplexed HTTP/2.

Regression tests: `upstream::upload_tests` uses real Hyper IO for delayed
headers/body, connection reuse, blocked uploads, concurrent HTTP/2 and
cancellation. Existing TTFT/idle/compression tests remain in `upstream::tests`.
