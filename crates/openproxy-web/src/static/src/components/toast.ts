// components/toast.ts — short-lived non-blocking notification.
// The original app used an inline `showToast` function; we keep
// the same call signature so handlers can keep using it.

export type ToastType = "info" | "success" | "error" | "warning" | string;

export function showToast(message: string, type: ToastType = "info"): void {
  const toast: HTMLDivElement = document.createElement("div");
  toast.className = `toast toast-${type}`;
  toast.textContent = message;
  document.body.appendChild(toast);
  setTimeout(() => toast.classList.add("show"), 10);
  setTimeout(() => {
    toast.classList.remove("show");
    setTimeout(() => toast.remove(), 300);
  }, 3000);
}
