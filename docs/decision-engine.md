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
 [ Jev (HTTP Upstream) ]             [ Laya (In-Process C FFI) ]
 • Remote HTTP upstream              • Native C FFI (libonnxruntime.so)
 • Offloads compute to server        • Local memory inference (0 ms network)
 • 0 MB local model memory           • Dynamic lifecycle (0 MB when inactive)
```

| Property | Jev (HTTP Upstream) | Laya Native (In-Process) |
| :--- | :--- | :--- |
| **Execution** | Remote HTTP service | In-process native thread |
| **Network Hop** | 20 to 150 ms | 0 ms |
| **RAM Usage** | 0 MB local model memory | 325 MB (INT8) or 1.29 GB (FP32) |
| **Lifecycle** | Stateless HTTP client | Unloads to 0 MB when deactivated |
| **Hardware** | Remote GPU or server | Local CPU (ARM NEON, AVX2, or `asimddp`) |
| **Configuration** | Provider URL and API key | Built-in provider toggle in dashboard |

---

## 2. Combo Decision Routing

### 2.1 Route Evaluation Workflow

When you set `decision_model` on a combo (such as `laya/laya-multilingual` or `jev-latest`), the router executes these steps:

1. **Target extraction:** The router inspects targets assigned to the combo and filters those with a `description` field.
2. **Sequence construction:** The pipeline pairs the prompt with candidate target descriptions into a single choice question:
   ```text
   [CLS] choice question: <instructions> [SEP] [MASK] <desc_0> [MASK] <desc_1> ... [SEP] <prompt> [SEP]
   ```
3. **Calibrated evaluation:** The model calculates logits for each target marker, applies temperature scaling, and normalizes output through softmax.
4. **Elastic hysteresis:** If a session is already pinned to a target, switching requires a confidence difference of at least 0.15 ($\Delta p \ge 0.15$). This prevents route oscillations and preserves upstream KV-cache.
5. **Target reordering:** The pipeline swaps the winning target to index `0` for immediate dispatch.

### 2.2 Hierarchical Sub-Combos

Combos can nest other combos as targets. In this topology, the top-level combo evaluates domain targets (`finance`, `health`, `coding`, `chat`). Once selected, the child combo resolves its internal targets based on model-specific criteria.

---

## 3. Native Laya Engine Setup

OpenProxy implements the Laya engine in Rust using C FFI bindings to `libonnxruntime.so` and pure Rust tokenizers. It requires no Python interpreter, daemon processes, or external services.

### 3.1 Prerequisites

1. **ONNX Runtime:**
   Install `libonnxruntime.so` (version 1.20 or newer). OpenProxy scans standard paths (`/usr/lib`, `/usr/local/lib`) and discovers Python virtual environment installations in `/root/code/laya-playground/.venv/lib/`.
2. **Cargo Feature:**
   The `openproxy-adapters`, `openproxy-core`, `openproxy-pipeline`, and `openproxy-server` crates enable `laya-engine` by default.

### 3.2 Model Checkpoints

OpenProxy supports three model precision tiers:

| Checkpoint | Path | Precision | Disk/RAM | CPU Latency | Application |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **FP32 Native** *(Default)* | `model-fp32/model.onnx` | Float32 | 1.29 GB | 550 to 650 ms | Default for CPU. Uses ARM NEON instructions. Retains full accuracy. |
| **INT8 Selective** | `model-int8/model.onnx` | QInt8 | 325 MB | 250 to 300 ms | Uses ARMv8.2-A `asimddp` hardware dot-product instructions (`sdot`/`udot`). |
| **FP16 WebGPU** | `model-onnx/model.onnx` | Float16 | 617 MB | ~3,900 ms | Exported for WebGPU. CPUs lack native FP16 execution without GPU hardware and emulate operations in software. Avoid on CPU. |

### 3.3 Configuration Variables

Set these environment variables in your environment file or shell:

```bash
# Path to ONNX model file
OPENPROXY_LAYA_MODEL=/root/code/laya-playground/model-fp32/model.onnx

# Path to tokenizer.json
OPENPROXY_LAYA_TOKENIZER=/root/code/laya-playground/model-onnx/tokenizer/tokenizer.json

# Path to calibration temperature config
OPENPROXY_LAYA_CONFIG=/root/code/laya-playground/model-onnx/rl_agent_config.json

# Intra-op thread count (defaults to CPU cores, clamped to 1..4)
OPENPROXY_LAYA_THREADS=4
```

---

## 4. Lifecycle and Memory Management

When you route through Jev or standard LLMs, disable Laya to release all allocated memory.

### 4.1 Memory Footprint

- **Inactive:** Drops ONNX sessions, allocators, and tokenizer handles. Memory footprint drops to 0 MB.
- **Active:** Loads weights into memory and reserves worker threads.

### 4.2 Toggling via REST API

Toggle the engine state at runtime without restarting the server:

#### Deactivate (unloads model and frees RAM):
```bash
curl -s -X POST http://localhost:8787/admin/api/providers/laya/active \
  -H "Authorization: Bearer <ADMIN_KEY>" \
  -H "Content-Type: application/json" \
  -d '{"active": false}'
```

Log entry:
```json
{"level":"INFO","message":"Laya internal engine shutdown (memory released)"}
```

#### Activate (initializes session on background thread):
```bash
curl -s -X POST http://localhost:8787/admin/api/providers/laya/active \
  -H "Authorization: Bearer <ADMIN_KEY>" \
  -H "Content-Type: application/json" \
  -d '{"active": true}'
```

Log entry:
```json
{"level":"INFO","message":"Laya ONNX internal engine ready for in-process inference"}
```

### 4.3 Toggling via Web Dashboard

1. Navigate to `/admin` in your browser.
2. Select the **Providers** tab.
3. Find **Laya (Self-Hosted)** in the provider list.
4. Toggle the **Active** switch. The server updates the database and releases or loads model memory.

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
