# Laya System One Reference Server

This reference server allows hosting open-source System One models such as [`convaiinnovations/laya`](https://huggingface.co/convaiinnovations/laya) as a local inference backend for **OpenProxy**.

## Quick Start

### 1. Install Dependencies
```bash
pip install -r requirements.txt
```

### 2. Run the Server
```bash
python main.py --model convaiinnovations/laya --port 8000
```
For GPU acceleration with CUDA:
```bash
python main.py --model convaiinnovations/laya --device cuda --port 8000
```

### 3. Connect to OpenProxy
In OpenProxy:
1. Register the provider as `laya`:
   - Base URL: `http://localhost:8000/v1`
   - Provider Format: `systemone`
   - Auth Type: `none`
2. Add models:
   - `laya`
   - `laya-multilingual`
   - `laya-typed-decisions`
3. Use in combos:
   - Set combo `priority_mode` to `decision`
   - Set `decision_model` to `laya`
   - Add semantic descriptions (routing criteria) to each target in the combo.
