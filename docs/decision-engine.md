# Decision Engine & Semantic Router (Jev & Laya Native)

OpenProxy features a low-latency **Semantic Decision Engine** designed for real-time prompt classification, dynamic intent routing, and hierarchical combo execution. 

The engine operates on the **System One** decision paradigm: fast, non-autoregressive sequence classification calibrated with entropy-based confidence scoring ($1 - H / \ln k$), executed before dispatching requests to large generative LLMs.

---

## 1. Overview & Supported Backends

OpenProxy supports two complementary decision backends:

```text
                       Incoming Client Prompt
                                 │
                                 ▼
                     ┌───────────────────────┐
                     │ Combo Decision Stage  │
                     │  (Target Evaluation)  │
                     └───────────┬───────────┘
                                 │
               ┌─────────────────┴─────────────────┐
               ▼                                   ▼
    [ Jev (External HTTP) ]             [ Laya (Native In-Process) ]
    • Upstream HTTP API                 • Rust C FFI (libonnxruntime.so)
    • Remote/self-hosted cluster        • Pure Rust HuggingFace Tokenizer
    • Zero local CPU/RAM overhead       • Zero network hops (0.0 ms network)
    • Fallback when Laya is inactive    • Dynamic lifecycle (0 MB when disabled)
```

| Feature | Jev (HTTP Upstream) | Laya Native (In-Process C FFI) |
| :--- | :--- | :--- |
| **Execution Mode** | Remote / External HTTP | In-Process (Shared Memory, Native Threads) |
| **Network Overhead** | 20 – 150 ms network hop | **0.0 ms** (in-memory evaluation) |
| **Memory Footprint** | 0 MB local RAM | Configurable (325 MB INT8 / 1.29 GB FP32) |
| **Lifecycle** | Stateless HTTP client | Dynamic (unloaded to 0 MB when deactivated) |
| **Hardware Acceleration** | Remote GPU / TPU | Local CPU (ARM NEON / AVX2 / `asimddp`) |
| **Configuration** | Upstream URL & API key in Provider | Built-in provider toggle in dashboard |

---

## 2. Architecture & Decision Routing

### 2.1 How Combo Decision Routing Works

When a combo configures `decision_model` (e.g., `laya/laya-multilingual` or `jev-latest`):
1. **Candidate Extraction:** Targets in the combo that have a non-empty `description` field are collected as categorical choices.
2. **Batch Sequence Formulation:** The user's prompt (state) and the candidate target descriptions are packaged into a single `choice` evaluation.
3. **Calibrated Inference:** The decision model computes raw logits, scales them with temperature, and normalizes them through softmax to generate probabilities and confidence scores.
4. **Elastic Hysteresis:** If the conversation session is already pinned to a target, the engine requires a confidence margin ($\Delta p \ge 0.15$) before switching targets, preventing cache thrashing and preserving upstream KV-cache.
5. **Target Promotion:** The winning target is reordered to position `0` in the candidate list for immediate dispatch.

### 2.2 Hierarchical Decision Routing (Sub-Combos)

Combos can nest other combos as targets. When a top-level combo (e.g., `topics`) evaluates choices across sub-combos (`finance`, `health`, `coding`, `chat`), the decision engine selects the appropriate domain sub-combo, which can then perform a second-level selection among specialized models.

---

## 3. Native Laya Engine Setup

The native Laya engine is implemented in pure Rust (`openproxy-adapters`) using C FFI directly to the official ONNX Runtime C API (`libonnxruntime.so`) and native Hugging Face `tokenizers`. It requires **zero external daemons, zero Python runtime, and zero external crate bloat**.

### 3.1 Prerequisites

1. **ONNX Runtime Shared Library:**
   `libonnxruntime.so` (version 1.20+) must be present in standard library paths or specified in `LD_LIBRARY_PATH`:
   - Ubuntu / Debian: `/usr/lib/libonnxruntime.so` or `/usr/local/lib/libonnxruntime.so`
   - Python venvs or custom paths are auto-discovered from `/root/code/laya-playground/.venv/lib/python*/site-packages/onnxruntime/capi/`.

2. **Cargo Feature Gate:**
   The feature `laya-engine` is enabled by default in `Cargo.toml`:
   ```toml
   [dependencies]
   openproxy-adapters = { path = "../openproxy-adapters", features = ["laya-engine"] }
   ```

### 3.2 Model Checkpoints & Quantization

OpenProxy automatically selects the optimal model path based on hardware compatibility:

| Checkpoint | Path | Precision | Size | CPU Latency | Best Use Case |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **FP32 Native** *(Default)* | `model-fp32/model.onnx` | Float32 | 1.29 GB | ~550 – 650 ms | **Production Default:** 100% semantic accuracy, native ARM NEON SIMD. |
| **INT8 Selective** | `model-int8/model.onnx` | QInt8 (Per-Channel) | 325 MB | ~250 – 300 ms | **Low-Memory / High-Throughput:** Accelerated by hardware `asimddp` instructions. |
| **FP16 WebGPU** | `model-onnx/model.onnx` | Float16 | 617 MB | ~3,900 ms | *Not recommended for CPU:* Lacks native CPU vector instructions; causes emulation overhead. |

### 3.3 Environment Variables

| Variable | Default Value | Description |
| :--- | :--- | :--- |
| `OPENPROXY_LAYA_MODEL` | `/root/code/laya-playground/model-fp32/model.onnx` | Path to the ONNX model file. |
| `OPENPROXY_LAYA_TOKENIZER` | `/root/code/laya-playground/model-onnx/tokenizer/tokenizer.json` | Path to the Hugging Face `tokenizer.json`. |
| `OPENPROXY_LAYA_CONFIG` | `/root/code/laya-playground/model-onnx/rl_agent_config.json` | Path to the calibration temperature configuration. |
| `OPENPROXY_LAYA_THREADS` | Available CPU cores (clamped to 1..4) | Number of intra-op threads for ONNX Runtime. |

---

## 4. Lifecycle Management & Zero Memory Footprint

To ensure OpenProxy remains ultra-lightweight when operators only want to use Jev or standard LLMs, the Laya engine implements an **explicit dynamic lifecycle**.

### 4.1 Inactive Provider: 0 MB Overhead

When the `laya` provider is marked inactive:
- No background threads run.
- The ONNX session, memory arena, and tokenizers are completely dropped from RAM.
- OpenProxy's base memory footprint drops back to **< 50 MB**.

### 4.2 Activating & Deactivating via REST API

You can toggle the Laya engine on or off dynamically without restarting OpenProxy:

#### Deactivate (Release all memory to 0 MB):
```bash
curl -s -X POST http://localhost:8787/admin/api/providers/laya/active \
  -H "Authorization: Bearer <ADMIN_KEY>" \
  -H "Content-Type: application/json" \
  -d '{"active": false}'
```

*Server log confirmation:*
```text
{"level":"INFO","message":"Laya internal engine shutdown (memory released)"}
```

#### Activate (Load in background thread without blocking proxy traffic):
```bash
curl -s -X POST http://localhost:8787/admin/api/providers/laya/active \
  -H "Authorization: Bearer <ADMIN_KEY>" \
  -H "Content-Type: application/json" \
  -d '{"active": true}'
```

*Server log confirmation:*
```text
{"level":"INFO","message":"Initializing Laya internal C FFI engine"}
{"level":"INFO","message":"Laya ONNX internal engine ready for in-process inference"}
```

### 4.3 Activating & Deactivating via Admin Web Dashboard

1. Open the OpenProxy Dashboard at `http://<host>:8787/admin`.
2. Navigate to the **Providers** tab.
3. Locate **Laya (Self-Hosted)**.
4. Toggle the **Active** switch.
   - Deactivating immediately triggers `shutdown()` and frees memory.
   - Activating spawns a non-blocking background initialization thread.

---

## 5. API Usage & Protocol Contracts

### 5.1 Public Decision Protocol (`POST /v1/systemone`)

Any client can evaluate decisions directly via the OpenAI-compatible System One contract:

```bash
curl -s -X POST http://localhost:8787/v1/systemone \
  -H "Authorization: Bearer <API_KEY>" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "laya-multilingual",
    "state": "The user is asking about quarterly EBITDA and net income calculations",
    "questions": {
      "domain": {
        "type": "choice",
        "instructions": "Identify the primary topic domain",
        "options": ["finance", "health", "technology", "general"]
      },
      "urgency": {
        "type": "score",
        "instructions": "Determine priority level",
        "criteria": ["low", "medium", "high"]
      },
      "is_business": {
        "type": "noul",
        "instructions": "Is this inquiry business or enterprise related?",
        "criteria": {
          "true": "yes, related to business finance",
          "false": "no, personal or unrelated topic"
        }
      }
    }
  }'
```

**Response:**
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
        "general": 0.0120,
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

When active, Laya models appear in the standard catalog with enriched decision metadata:

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

### 5.3 Automated Model Health Verification

The dashboard and admin API can verify in-process engine health without network calls:

```bash
curl -s -X POST http://localhost:8787/admin/api/models/<ROW_ID>/test \
  -H "Authorization: Bearer <ADMIN_KEY>"
```

Returns `status: 200`, execution latency in milliseconds, and the diagnostic classification payload.
