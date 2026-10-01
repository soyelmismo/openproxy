# Decision Engine & Semantic Router

The decision engine classifies incoming prompts and routes requests across targets using System One models. These models perform non-autoregressive sequence classification and calibrate probabilities using entropy-based confidence scoring:

$$\text{confidence} = 1 - \frac{H(p)}{\ln k}$$

where $H(p) = -\sum_{i=1}^k p_i \ln p_i$ and $k$ represents the number of candidates.

---

## 1. Supported Backends

OpenProxy supports two backends for decision routing:

```text
                       Incoming Prompt
                              │
                              ▼
                  ┌───────────────────────┐
                  │ Combo Decision Stage  │
                  └───────────┬───────────┘
                              │
            ┌─────────────────┴─────────────────┐
            ▼                                   ▼
 [ Jev (HTTP Upstream) ]             [ Laya (Sandboxed Worker) ]
 • Remote HTTP upstream              • Native C FFI (libonnxruntime.so)
 • Offloads compute to server        • Local memory inference (0 ms network)
 • 0 MB local model memory           • Dynamic lifecycle (0 MB when inactive)
```

| Property | Jev (HTTP Upstream) | Laya Native (Sandboxed Worker) |
| :--- | :--- | :--- |
| **Execution** | Remote HTTP service | Child process, private pipe IPC |
| **Network Hop** | 20 to 150 ms | 0 ms |
| **RAM Usage** | 0 MB local model memory | 325 MB (INT8) or 1.29 GB (FP32) |
| **Lifecycle** | Stateless HTTP client | Loads on demand; exits after 5 idle minutes or deactivation |
| **Hardware** | Remote GPU or server | Local CPU (ARM NEON, AVX2, or `asimddp`) |
| **Configuration** | Provider URL and API key | Built-in provider toggle in dashboard |

---

## 2. Combo Decision Routing

### 2.1 Route Evaluation Workflow

1. **Target extraction & Pre-filtering:** The router inspects targets assigned to the combo and prunes any targets currently in cooldown (rate limit backoffs or circuit breaker), disabled targets, or targets flagged by predictive skip. Only healthy targets with a `description` field are considered.
2. **Reputation scoring:** Surviving targets are weighted by their live reputation metrics (success rate, timeout frequency, and latency percentiles).
3. **Sequence construction:** The pipeline pairs the prompt with candidate target descriptions into a single choice question:
   ```text
   [CLS] choice question: <instructions> [SEP] [MASK] <desc_0> [MASK] <desc_1> ... [SEP] <prompt> [SEP]
   ```
4. **Calibrated evaluation:** The model calculates logits for each target marker, applies temperature scaling, and normalizes output through softmax.
5. **Elastic hysteresis:** If a session is already pinned to a target, switching requires a confidence difference of at least 0.05 ($\Delta p \ge 0.05$). This prevents route oscillations and preserves upstream KV-cache.
6. **Target reordering:** The pipeline swaps the winning target to index `0` for immediate dispatch.

### 2.2 Hierarchical Sub-Combos

Combos can nest other combos as targets. In this topology, the top-level combo evaluates domain targets (`finance`, `health`, `coding`, `chat`). Once selected, the child combo resolves its internal targets based on model-specific criteria.

---

## 3. Native Laya Engine Setup

OpenProxy implements the Laya engine in Rust using C FFI bindings to `libonnxruntime.so` and tokenizers. It requires no Python interpreter, daemon processes, or external services.

Native inference runs in a fresh execution of the same binary with `--laya-worker`,
before configuration, database, master keys or OAuth credentials are loaded.
Linux x86-64/AArch64 requires Landlock ABI 3 (Linux 6.2+ with Landlock enabled)
and seccomp. The worker refuses native inference if either restriction cannot be
installed; unsupported platforms can still use a self-hosted HTTP Laya backend.

### 3.1 Prerequisites

1. **ONNX Runtime:**
   Install `libonnxruntime.so` (version 1.20 or newer). OpenProxy scans standard dynamic linker paths (`/usr/lib`, `/usr/local/lib`, `/usr/lib/x86_64-linux-gnu`, `/usr/lib/aarch64-linux-gnu`) or the location specified by `OPENPROXY_ONNX_LIB`.
2. **Cargo Feature:**
   The `openproxy-adapters`, `openproxy-core`, `openproxy-pipeline`, and `openproxy-server` crates enable `laya-engine` by default.

### 3.2 Hugging Face Checkpoints & Download

OpenProxy uses CPU-optimized Laya checkpoints published on Hugging Face:
- **CPU-Optimized ONNX Checkpoint (Recommended):** [`soyelmismo/laya-multilingual-onnx`](https://huggingface.co/soyelmismo/laya-multilingual-onnx) (contains CPU-calibrated `model.onnx` [INT8], `model-fp32.onnx` [FP32], `tokenizer.json`, `rl_agent_config.json`)
- **PyTorch Base Checkpoint:** [`convaiinnovations/laya-multilingual`](https://huggingface.co/convaiinnovations/laya-multilingual) (contains `model.safetensors`, mmBERT architecture)

#### Standard Directory Layout

OpenProxy automatically detects files stored in `~/.openproxy/models/laya/`, `./models/laya/`, or container path `/var/lib/openproxy/models/laya/`:

```text
~/.openproxy/models/laya/ (or ./models/laya/)
├── model.onnx              # Target model (INT8 by default, ~325 MB)
├── tokenizer.json          # Fast Tokenizer (~34 MB)
└── rl_agent_config.json    # Decision temperatures and calibrated thresholds (473 B)
```

#### Download via huggingface-cli

Manual installation is optional. The first local inference downloads missing
default artifacts from revision `0966c4fa58da6878b39e7e14cb5e93313b82d828`,
streaming to temporary files and publishing only after size and SHA-256 checks.
Startup and provider activation do not trigger downloads. Cached default files
are verified before each worker load; explicit custom paths are never overwritten
and are the operator's responsibility. Disable automatic downloads for offline
deployments with `OPENPROXY_LAYA_AUTO_DOWNLOAD=0`.

```bash
# Global user directory:
mkdir -p ~/.openproxy/models/laya
huggingface-cli download soyelmismo/laya-multilingual-onnx \
  --local-dir ~/.openproxy/models/laya \
  --include "model.onnx" "tokenizer.json" "rl_agent_config.json"

# Or local project directory (for Docker Compose):
mkdir -p ./models/laya
huggingface-cli download soyelmismo/laya-multilingual-onnx \
  --local-dir ./models/laya \
  --include "model.onnx" "tokenizer.json" "rl_agent_config.json"
```

#### Download via curl

```bash
mkdir -p ~/.openproxy/models/laya
HF_BASE="https://huggingface.co/soyelmismo/laya-multilingual-onnx/resolve/main"

curl -L -o ~/.openproxy/models/laya/model.onnx "$HF_BASE/model.onnx"
curl -L -o ~/.openproxy/models/laya/tokenizer.json "$HF_BASE/tokenizer.json"
curl -L -o ~/.openproxy/models/laya/rl_agent_config.json "$HF_BASE/rl_agent_config.json"
```

> **Security (OP-09)**: models execute only in the isolated worker. The default
> revision is pinned and verified automatically. For custom models, verify the
> publisher's hashes before setting explicit paths:
>
> ```bash
> cd ~/.openproxy/models/laya
> sha256sum model.onnx tokenizer.json rl_agent_config.json
> # compare against the digests published in the model repository
> ```
>
> A mismatched or unknown model file must not be loaded: the ONNX graph is
> trusted input (see the shape validation in `laya_bridge.c` for the output
> tensor, which is a backstop, not a substitute for verifying the artifact).

### 3.3 Precision Tiers and CPU Latency

| Precision Tier | Format | RAM / Disk | CPU Latency | Notes |
| :--- | :--- | :--- | :--- | :--- |
| **INT8 Quantized (Recommended / Default)** | QInt8 | **325 MB** | **250 to 300 ms** | Default `model.onnx`. Executes via ARMv8.2-A `asimddp` instructions (`sdot`/`udot`) or x86 VNNI instructions. 4x smaller, 2x faster, with preserved accuracy. |
| **FP32 (Full Precision / Unquantized)** | Float32 | 1.29 GB | 550 to 650 ms | Full-precision `model-fp32.onnx`. Runs on ARM NEON and x86 AVX2 vector units. Bit-exact parity with PyTorch base checkpoint. |
| **FP16 (WebGPU Export)** | Float16 | 617 MB | ~3,900 ms | The default checkpoint in `mizchi/laya-multilingual-onnx` targets WebGPU. CPUs lack native FP16 compute pipelines and emulate half-precision in software. Convert weights to Float32 or INT8 for CPU deployment. |

### 3.4 Configuration Variables (Optional)

If model files reside in `~/.openproxy/models/laya/` or `./models/laya/`, the engine detects them automatically. Use these environment variables to override default paths:

```bash
# Path to ONNX model file
OPENPROXY_LAYA_MODEL=~/.openproxy/models/laya/model.onnx

# Path to tokenizer.json
OPENPROXY_LAYA_TOKENIZER=~/.openproxy/models/laya/tokenizer.json

# Path to calibration temperature config
OPENPROXY_LAYA_CONFIG=~/.openproxy/models/laya/rl_agent_config.json

# Intra-op thread count (defaults to CPU cores, clamped to 1..4)
OPENPROXY_LAYA_THREADS=4

# Unload the worker after this many idle minutes (1..1440; default 5)
OPENPROXY_LAYA_IDLE_TIMEOUT_MINUTES=5

# Optional: require preinstalled files instead of downloading missing artifacts
OPENPROXY_LAYA_AUTO_DOWNLOAD=0
```

---

## 4. Lifecycle and Memory Management

Laya is lazy: enabling the provider does not allocate a tokenizer or ONNX session.
The first inference verifies/downloads the artifacts and launches one worker;
subsequent requests reuse it. After 5 idle minutes (configurable), the worker is
killed, releasing its address space to the OS. The next request starts it again
from the disk cache, without another download. Disable Laya for immediate unload.

The supervisor serializes inference with a bounded queue of 8 requests. IPC frames
are capped at 4 MiB; batches at 64 questions and 128 options per question. Native
loading has a 120-second deadline, inference a 30-second deadline, and downloads a
10-minute deadline per artifact. Failure, crash or timeout discards the worker;
the next request creates a fresh one. Idle time starts after the last job finishes,
not while inference/download is in progress.

The Linux worker has an empty environment except explicit model/runtime settings,
no inherited gateway credentials, no core dumps, no Linux capabilities, a 4 GiB
virtual-address-space limit and a 64-descriptor limit. Landlock permits read-only
access to the model files, shared-library locations and CPU metadata, not the
gateway configuration/database. Seccomp blocks network sockets, execution of other
programs, new processes, ptrace/process-memory access and privileged system calls,
while allowing ONNX threads. The worker dies when its parent dies. This reduces
native-code blast radius; it is not a guarantee against kernel vulnerabilities.

### 4.1 Memory Footprint

- **Enabled but idle/unloaded:** No model/tokenizer memory; weights remain on disk.
- **Resident:** The child holds weights, tokenizer and ONNX threads. Actual RSS includes runtime overhead beyond the weight size.

### 4.2 Toggling via REST API

Toggle the engine state at runtime without restarting the server:

#### Deactivate (unloads model and frees RAM):
```bash
curl -s -X POST http://localhost:8787/admin/api/providers/laya/active \
  -H "Authorization: Bearer <ADMIN_KEY>" \
  -H "Content-Type: application/json" \
  -d '{"active": false}'
```

Deactivation cancels pending loading/inference and waits for the child to exit.
Automatic idle unloading logs `Laya idle timeout: stopping worker and releasing memory`.

#### Activate (enables lazy loading; no immediate allocation):
```bash
curl -s -X POST http://localhost:8787/admin/api/providers/laya/active \
  -H "Authorization: Bearer <ADMIN_KEY>" \
  -H "Content-Type: application/json" \
  -d '{"active": true}'
```

The first inference, not activation, starts the worker.

### 4.3 Toggling via Web Dashboard

1. Navigate to `/admin` in your browser.
2. Select the **Providers** tab.
3. Find **Laya (Self-Hosted)** in the provider list.
4. Toggle the **Active** switch. The server updates the database; disabling stops the worker, enabling permits loading on the next inference.

---

## 5. API Contracts

### 5.1 Public Decision Endpoint (`POST /v1/systemone`)

Submit classification requests using the System One contract:

```bash
curl -s -X POST http://localhost:8787/v1/systemone \
  -H "Authorization: Bearer <API_KEY>" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "laya-multilingual",
    "state": "The user asks for quarterly EBITDA and net income formulas",
    "questions": {
      "domain": {
        "type": "choice",
        "instructions": "Select the topic domain",
        "options": ["finance", "health", "technology", "chat"]
      },
      "urgency": {
        "type": "score",
        "instructions": "Rate inquiry urgency",
        "criteria": ["low", "medium", "high"]
      },
      "is_business": {
        "type": "noul",
        "instructions": "Relates to enterprise finance",
        "criteria": {
          "true": "business finance topic",
          "false": "unrelated topic"
        }
      }
    }
  }'
```

Response payload:
```json
{
  "model": "laya-multilingual",
  "answers": {
    "domain": {
      "type": "choice",
      "choice": "finance",
      "confidence": 0.8672,
      "probabilities": {
        "finance": 0.9421,
        "technology": 0.0450,
        "chat": 0.0120,
        "health": 0.0009
      }
    },
    "urgency": {
      "type": "score",
      "score": 1.1240,
      "confidence": 0.3215
    },
    "is_business": {
      "type": "noul",
      "noul": 0.9612,
      "confidence": 0.6240
    }
  },
  "usage": {
    "input_tokens": 124,
    "output_tokens": 0,
    "total_tokens": 124
  }
}
```

### 5.2 Model Catalog (`GET /v1/models`)

Active Laya models surface in the model catalog:

```json
{
  "id": "laya/laya-multilingual",
  "object": "model",
  "owned_by": "laya",
  "type": "decision",
  "family": "laya",
  "context_length": 8192,
  "max_input_tokens": 8192,
  "max_output_tokens": 1024,
  "input_modalities": ["text"],
  "output_modalities": ["text"],
  "capabilities": {
    "decisions": true,
    "structured_output": true,
    "temperature": true,
    "tool_calling": true
  }
}
```

### 5.3 Health Check

Test engine status and measure latency directly:

```bash
curl -s -X POST http://localhost:8787/admin/api/models/<ROW_ID>/test \
  -H "Authorization: Bearer <ADMIN_KEY>"
```

---

## 6. Container Deployment & Architecture Compatibility

### 6.1 Hardware Architecture Matrix

The Laya engine pairs pure Rust tokenization with dynamic runtime bindings to `libonnxruntime`. The ONNX model format is architecture-neutral: the same weights file runs across all target architectures.

| Architecture | Platform | Vector Acceleration | ONNX Runtime Library |
| :--- | :--- | :--- | :--- |
| **x86_64 (`amd64`)** | Linux with Landlock ABI 3 | AVX2, AVX-512, VNNI | `libonnxruntime.so` |
| **aarch64 (`arm64`)** | Linux (Graviton, Ampere, Pi 5) | ARM NEON, ARMv8.2-A `asimddp` | `libonnxruntime.so` |

Windows/macOS and older Linux kernels use self-hosted HTTP decision backends;
native worker inference has no unsandboxed fallback.

### 6.2 Zero-Breakage Dynamic Linking

The engine resolves ONNX symbols via `dlopen` at runtime rather than link-time:
- If `libonnxruntime` is absent from the container, OpenProxy still starts normally without loading the native engine.
- Remote proxying and HTTP upstream decision routing (Jev) operate with zero local dependencies.
- With the runtime installed, the first local request starts the sandboxed worker and loads installed or downloaded weights.

### 6.3 Docker Deployment

The official Docker image (`ghcr.io/soyelmismo/openproxy`) bundles ONNX Runtime libraries for both `linux/amd64` and `linux/arm64` out of the box via multi-stage build. You do not need to install or mount any runtime libraries from the host.

#### Running with Sandboxed Laya Decision Routing

Mount your model weights folder into the container working directory (`/var/lib/openproxy/models/laya`):

```yaml
services:
  openproxy:
    image: ghcr.io/soyelmismo/openproxy:latest
    ports:
      - "8787:8787"
    volumes:
      - ./config.toml:/etc/openproxy/config.toml:ro
      - openproxy-data:/var/lib/openproxy
      # Mount model weights folder (model.onnx, tokenizer.json, rl_agent_config.json)
      # Use either local directory `./models/laya` or host `~/.openproxy/models/laya`
      - ./models/laya:/var/lib/openproxy/models/laya:ro
    environment:
      - OPENPROXY_CONFIG=/etc/openproxy/config.toml
```

OpenProxy resolves these locations on demand, not at startup. For automatic
downloads, ensure the resolved cache directory is writable (or set the three
artifact paths explicitly). The container host must support Landlock ABI 3 and
allow its syscalls plus seccomp installation; otherwise the worker fails closed.
The model process exits after the idle timeout even while the provider stays active.
