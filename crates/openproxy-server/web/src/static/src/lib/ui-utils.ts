import { showToast } from "../components/toast.js";

export function ensureModalRoot(): HTMLElement {
  let root = document.getElementById("modal-root");
  if (!root) {
    root = document.createElement("div");
    root.id = "modal-root";
    root.style.cssText = "position:relative;z-index:1000;";
    document.body.appendChild(root);
  }
  return root;
}

export function flashButton(btn: HTMLButtonElement | null, text: string, color: string): void {
  if (!btn) return;
  btn.textContent = text;
  btn.style.background = color;
  setTimeout(() => { btn.style.background = ""; }, 1500);
}

export function showApiError(err: unknown, prefix: string = "Error"): void {
  const msg = err instanceof Error ? err.message : String(err);
  showToast(`${prefix}: ${msg}`, "error");
}
