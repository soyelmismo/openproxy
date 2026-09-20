"""Laya System One Reference Server for OpenProxy.

Provides a fast local inference server implementing the System One protocol
(compatible with TypeSafe Jev and Convai Laya).

Usage:
    pip install -r requirements.txt
    python main.py --model convaiinnovations/laya --port 8000
"""

import argparse
import time
from typing import Any, Dict, List, Optional
from fastapi import FastAPI, HTTPException
from pydantic import BaseModel, Field
import uvicorn

app = FastAPI(title="Laya System One Server", version="1.0.0")

class SystemOneQuestion(BaseModel):
    type: str = "categorical"
    prompt: Optional[str] = None
    choices: Optional[List[str]] = None
    schema_: Optional[Dict[str, Any]] = Field(default=None, alias="schema")

class SystemOneRequest(BaseModel):
    model: Optional[str] = "laya"
    state: str
    questions: Dict[str, SystemOneQuestion]

class SystemOneAnswer(BaseModel):
    value: Any
    confidence: Optional[float] = 1.0

class SystemOneUsage(BaseModel):
    input_tokens: int = 0
    output_tokens: int = 0

class SystemOneResponse(BaseModel):
    answers: Dict[str, SystemOneAnswer]
    model: str
    usage: Optional[SystemOneUsage] = None

# Global model references (initialized at startup if available)
MODEL_PIPELINE = None
MODEL_ID = "convaiinnovations/laya"

def init_model(model_id: str, device: str = "cpu"):
    global MODEL_PIPELINE, MODEL_ID
    MODEL_ID = model_id
    try:
        from transformers import pipeline
        import torch
        print(f"Loading Laya model '{model_id}' on {device}...")
        dtype = torch.float16 if device == "cuda" else torch.float32
        MODEL_PIPELINE = pipeline(
            "text-generation",
            model=model_id,
            torch_dtype=dtype,
            device=device,
        )
        print("Model loaded successfully.")
    except Exception as exc:
        print(f"Notice: Transformer model loading skipped ({exc}).")
        print("Running in fast heuristic zero-dependency mode for testing.")
        MODEL_PIPELINE = None

def evaluate_categorical_choice(state: str, choices: List[str]) -> str:
    """Heuristic fallback for semantic choice evaluation when weights are uninitialized."""
    state_lower = state.lower()
    # Check direct substring occurrences first
    for choice in choices:
        if choice.lower() in state_lower:
            return choice
    # Fallback to first available choice
    return choices[0] if choices else ""

@app.post("/v1/systemone", response_model=SystemOneResponse)
async def system_one_inference(req: SystemOneRequest):
    start = time.perf_counter()
    answers: Dict[str, SystemOneAnswer] = {}
    total_input_tokens = len(req.state.split())
    total_output_tokens = 0

    for q_id, question in req.questions.items():
        if question.type == "categorical" and question.choices:
            if MODEL_PIPELINE is not None:
                # Format prompt for Laya
                prompt = (
                    f"Task: Categorize the following input.\n"
                    f"Options: {', '.join(question.choices)}\n"
                    f"Input: {req.state}\n"
                    f"Answer:"
                )
                try:
                    outputs = MODEL_PIPELINE(prompt, max_new_tokens=16, do_sample=False)
                    raw_text = outputs[0]["generated_text"].split("Answer:")[-1].strip()
                    selected = evaluate_categorical_choice(raw_text, question.choices)
                except Exception as err:
                    print(f"Inference error: {err}, falling back to semantic match")
                    selected = evaluate_categorical_choice(req.state, question.choices)
            else:
                selected = evaluate_categorical_choice(req.state, question.choices)

            answers[q_id] = SystemOneAnswer(value=selected, confidence=0.95)
            total_output_tokens += 1
        elif question.type == "boolean":
            val = True if any(w in req.state.lower() for w in ("yes", "true", "si", "sí", "confirm")) else False
            answers[q_id] = SystemOneAnswer(value=val, confidence=0.9)
            total_output_tokens += 1
        else:
            answers[q_id] = SystemOneAnswer(value=req.state[:32], confidence=0.5)
            total_output_tokens += 5

    elapsed_ms = (time.perf_counter() - start) * 1000.0

    return SystemOneResponse(
        answers=answers,
        model=req.model or MODEL_ID,
        usage=SystemOneUsage(
            input_tokens=total_input_tokens,
            output_tokens=total_output_tokens,
        ),
    )

@app.get("/health")
def health_check():
    return {"status": "ok", "model": MODEL_ID}

def main():
    parser = argparse.ArgumentParser(description="Run Laya System One Server")
    parser.add_argument("--model", default="convaiinnovations/laya", help="Hugging Face model ID")
    parser.add_argument("--device", default="cpu", help="Device to run on ('cpu' or 'cuda')")
    parser.add_argument("--host", default="0.0.0.0", help="Host address")
    parser.add_argument("--port", type=int, default=8000, help="Port to listen on")
    args = parser.parse_args()

    init_model(args.model, args.device)
    uvicorn.run(app, host=args.host, port=args.port)

if __name__ == "__main__":
    main()
