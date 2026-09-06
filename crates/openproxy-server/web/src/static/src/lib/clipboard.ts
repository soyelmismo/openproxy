// lib/clipboard.ts — centralized clipboard write with HTTP fallback.
//
// On HTTPS / secure contexts we use `navigator.clipboard.writeText`,
// which returns a Promise. On plain HTTP (e.g. LAN dashboard access)
// the modern API is unavailable or throws, so we fall back to a
// hidden `<textarea>` + `document.execCommand("copy")`. The helper
// throws on final failure so the caller can surface a toast / modal.
export async function copyToClipboard(text: string): Promise<void> {
  if (typeof navigator !== "undefined" && navigator.clipboard && typeof navigator.clipboard.writeText === "function") {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch {
      // Fall through to the legacy fallback below.
    }
  }
  const ta: HTMLTextAreaElement = document.createElement("textarea");
  ta.value = text;
  ta.style.position = "fixed";
  ta.style.left = "-9999px";
  ta.setAttribute("readonly", "");
  document.body.appendChild(ta);
  ta.select();
  try {
    const ok: boolean = document.execCommand("copy");
    if (!ok) throw new Error("execCommand returned false");
  } finally {
    document.body.removeChild(ta);
  }
}
