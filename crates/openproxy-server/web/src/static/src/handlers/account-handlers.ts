// handlers/account-handlers.ts — create / delete accounts, open
// the create modal, set the health pill.
//
// Per spec §3 + §13.8 we no longer attach handlers to `window.*`.
// Each function is exported by name and registered in
// handlers/registry.ts so the central data-action shim can find it.
//
// Migrated to lit-html: modals are rendered into a wrapper `<div>`
// under `#modal-root` via `render()`. All `data-action` attributes
// are replaced with direct `@click` / `@submit` handlers; lit-html
// auto-escapes interpolation so we no longer call `escapeHtml` /
// `escapeAttr`.

import { html, render } from 'lit-html';
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";
import { copyToClipboard } from "../lib/clipboard.js";
import { showApiError, ensureModalRoot } from "../lib/ui-utils.js";
import { showConfirm, showPrompt } from "../lib/show-confirm.js";
import { mutateAndRefresh } from "../lib/mutate.js";
import { renderOpModal } from "../components/render-op-modal.js";

function summarizeApiKey(key: string): string {
  const trimmed = key.trim();
  if (trimmed.length <= 10) return trimmed;
  const prefix = trimmed.slice(0, 6);
  const suffix = trimmed.slice(-4);
  return `${prefix}...${suffix}`;
}

export interface ParsedKeyEntry {
  key: string;
  label?: string | undefined;
}

export function cleanApiKeyToken(token: string): string {
  return token.replace(/^["'`(\[{<\s]+|["'`)\]}>,;\.\s]+$/g, "").trim();
}

export function looksLikeApiKey(token: string): boolean {
  const cleaned = cleanApiKeyToken(token);
  if (cleaned.length < 8) return false;

  // 1. Google AI Studio (AIzaSy...) & Vertex OAuth/refresh (AQ.Ab...)
  if (/^AIza[0-9A-Za-z\-_]{30,}$/.test(cleaned)) return true;
  if (/^AQ\.[a-zA-Z0-9_\-\.]{20,}$/.test(cleaned)) return true;

  // 2. JWT tokens (eyJh...)
  if (/^eyJh[a-zA-Z0-9_\-]+\.[a-zA-Z0-9_\-]+\.[a-zA-Z0-9_\-]+$/.test(cleaned)) return true;

  // 3. UUIDs: 8-4-4-4-12
  if (/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(cleaned)) {
    return true;
  }

  // 4. Hex strings >= 32 chars (support 32 up to 128 hex chars)
  if (/^[0-9a-f]{32,128}$/i.test(cleaned)) {
    return true;
  }

  // 5. Generalized prefixed keys: 2-10 alphanumeric prefix + '-' or '_' + >= 12 chars token
  // (Covers sk-, sk-ant-, nvapi-, gsk_, pk-, cfut_, cpk_, XET-, and future provider prefixes)
  // Must have digits OR mixed case to avoid matching lowercase hyphenated text
  if (/^[a-zA-Z0-9]{2,10}[-_][a-zA-Z0-9_\-\.]{12,}$/.test(cleaned)) {
    const hasDigit = /[0-9]/.test(cleaned);
    const hasMixed = /[a-z]/.test(cleaned) && /[A-Z]/.test(cleaned);
    if (hasDigit || hasMixed) return true;
  }

  // 6. Generic high-entropy base64 or alphanumeric tokens (>= 20 chars, contains digits and letters)
  if (
    /^[A-Za-z0-9_\-\.]{20,}$/.test(cleaned) &&
    /[0-9]/.test(cleaned) &&
    /[a-zA-Z]/.test(cleaned)
  ) {
    return true;
  }

  return false;
}

const GENERIC_KEY_NAMES = new Set([
  "api_key",
  "apikey",
  "api-key",
  "key",
  "secret",
  "token",
  "access_token",
  "authorization",
  "bearer",
  "openai_api_key",
  "openai_key",
  "openaikey",
  "anthropic_api_key",
  "anthropic_key",
  "gemini_api_key",
  "gemini_key",
  "groq_api_key",
  "groq_key",
  "openrouter_api_key",
  "openrouter_key",
  "password",
  "credential",
]);

export function isGenericLabel(label: string): boolean {
  const lower = label.toLowerCase().trim();
  const norm = lower.replace(/[-_\s]/g, "");
  if (GENERIC_KEY_NAMES.has(lower) || GENERIC_KEY_NAMES.has(norm)) {
    return true;
  }
  return (
    norm.endsWith("apikey") ||
    norm.endsWith("secret") ||
    (norm.endsWith("key") &&
      (norm.startsWith("openai") ||
        norm.startsWith("anthropic") ||
        norm.startsWith("gemini") ||
        norm.startsWith("groq") ||
        norm.startsWith("openrouter")))
  );
}

function deduplicateKeys(entries: ParsedKeyEntry[]): ParsedKeyEntry[] {
  const seen = new Set<string>();
  const result: ParsedKeyEntry[] = [];
  for (const entry of entries) {
    const k = cleanApiKeyToken(entry.key);
    if (!k || seen.has(k)) continue;
    seen.add(k);
    const lbl = entry.label?.trim();
    result.push({
      key: k,
      label: lbl && !isGenericLabel(lbl) ? lbl : undefined,
    });
  }
  return result;
}

function extractKeysFromDirtyLine(line: string): ParsedKeyEntry[] {
  const seen = new Set<string>();
  const results: ParsedKeyEntry[] = [];

  const add = (k: string): void => {
    const cleaned = cleanApiKeyToken(k);
    if (cleaned && looksLikeApiKey(cleaned) && !seen.has(cleaned)) {
      seen.add(cleaned);
      results.push({ key: cleaned });
    }
  };

  // 1. JWT: header.payload.signature
  const jwtRegex = /\b(eyJh[a-zA-Z0-9_\-]+\.[a-zA-Z0-9_\-]+\.[a-zA-Z0-9_\-]+)\b/g;
  let m: RegExpExecArray | null;
  while ((m = jwtRegex.exec(line)) !== null) {
    if (m[1]) add(m[1]);
  }

  // 2. Google AI Studio & Vertex
  const googleRegex = /\b(AIza[0-9A-Za-z\-_]{30,}|AQ\.[a-zA-Z0-9_\-\.]{20,})\b/g;
  while ((m = googleRegex.exec(line)) !== null) {
    if (m[1]) add(m[1]);
  }

  // 3. Generalized prefixed keys (e.g. sk-*, nvapi-*, pk-*, cpk_*, etc.)
  const prefixRegex = /\b([a-zA-Z0-9]{2,10}[-_][a-zA-Z0-9_\-\.]{12,})\b/g;
  while ((m = prefixRegex.exec(line)) !== null) {
    if (m[1]) add(m[1]);
  }

  // 4. UUIDs
  const uuidRegex = /\b([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\b/gi;
  while ((m = uuidRegex.exec(line)) !== null) {
    if (m[1]) add(m[1]);
  }

  // 5. Hex strings (32-128 chars)
  const hexRegex = /\b([0-9a-f]{32,128})\b/gi;
  while ((m = hexRegex.exec(line)) !== null) {
    if (m[1]) add(m[1]);
  }

  // 6. Tokenize by whitespace or common delimiters
  const tokens = line.split(/[\s,;"'<>`()\[\]{}|=:]+/);
  for (const t of tokens) {
    add(t);
  }

  return results;
}

export function parseBulkApiKeys(rawText: string): ParsedKeyEntry[] {
  if (!rawText || !rawText.trim()) return [];

  const text = rawText.trim();

  // 1. Check for JSON array or object
  if (text.startsWith("[") || text.startsWith("{")) {
    try {
      const parsed = JSON.parse(text);
      if (Array.isArray(parsed)) {
        const entries: ParsedKeyEntry[] = [];
        for (const item of parsed) {
          if (typeof item === "string" && item.trim()) {
            entries.push({ key: item.trim() });
          } else if (item && typeof item === "object") {
            const rec = item as Record<string, unknown>;
            const rawKey = rec["api_key"] ?? rec["key"] ?? rec["secret"] ?? rec["token"] ?? rec["apiKey"];
            const k = typeof rawKey === "string" ? rawKey : undefined;
            if (k && k.trim()) {
              const rawLbl = rec["label"] ?? rec["name"] ?? rec["id"];
              const lbl = typeof rawLbl === "string" ? rawLbl.trim() : undefined;
              entries.push({ key: k.trim(), label: lbl });
            }
          }
        }
        if (entries.length > 0) {
          return deduplicateKeys(entries);
        }
      } else if (parsed && typeof parsed === "object") {
        const entries: ParsedKeyEntry[] = [];
        for (const [key, val] of Object.entries(parsed)) {
          if (typeof val === "string" && val.trim()) {
            entries.push({ key: val.trim(), label: key });
          }
        }
        if (entries.length > 0) {
          return deduplicateKeys(entries);
        }
      }
    } catch {
      // Continue to line / dirty parser
    }
  }

  const entries: ParsedKeyEntry[] = [];
  const lines = text.split(/\r?\n/);

  for (const rawLine of lines) {
    let line = rawLine.trim();
    if (!line) continue;

    // Check if line is purely a comment (starts with # or //)
    if (/^(?:#|\/\/)/.test(line)) {
      const strippedComment = line.replace(/^(?:#|\/\/)\s*/, "").trim();
      if (looksLikeApiKey(strippedComment) || (!strippedComment.includes(" ") && strippedComment.length >= 8)) {
        line = strippedComment;
      } else {
        const matches = extractKeysFromDirtyLine(line);
        if (matches.length > 0) {
          entries.push(...matches);
        }
        continue;
      }
    }

    // Strip markdown list markers: "1. ", "- ", "* ", "• ", "[1] "
    line = line.replace(/^(?:[-*+•]|\d+[\.\)]|\[\d+\])\s+/, "").trim();

    // Check for inline comment: "key # label" or "key // label"
    let inlineComment: string | undefined;
    const commentIdx = line.search(/\s+(?:#|\/\/)\s+/);
    if (commentIdx !== -1) {
      inlineComment = line.slice(commentIdx).replace(/^\s*(?:#|\/\/)\s*/, "").trim();
      line = line.slice(0, commentIdx).trim();
    }

    // Check for env var syntax: export KEY="val" or KEY="val"
    const envMatch = line.match(/^(?:export\s+)?([A-Za-z0-9_]+)\s*=\s*(.+)$/);
    if (envMatch && envMatch[1] && envMatch[2]) {
      const varName = envMatch[1];
      const val = cleanApiKeyToken(envMatch[2]);
      if (val) {
        entries.push({
          key: val,
          label: isGenericLabel(varName) ? inlineComment : varName,
        });
        continue;
      }
    }

    // Check for delimiter `:` or `=` or `|` or `\t` (e.g. "Label: sk-...")
    if (!line.startsWith("http://") && !line.startsWith("https://")) {
      const delimMatch = line.match(/^([^:=|\t]+)\s*[:=|\t]\s*(.+)$/);
      if (delimMatch && delimMatch[1] && delimMatch[2]) {
        const left = cleanApiKeyToken(delimMatch[1]);
        let right = cleanApiKeyToken(delimMatch[2]);
        if (/^bearer\s+/i.test(right)) {
          right = cleanApiKeyToken(right.replace(/^bearer\s+/i, ""));
        }
        if (looksLikeApiKey(right) || (!right.includes(" ") && right.length >= 8)) {
          entries.push({
            key: right,
            label: isGenericLabel(left) ? inlineComment : left,
          });
          continue;
        }
        if (looksLikeApiKey(left) || (!left.includes(" ") && left.length >= 8)) {
          entries.push({
            key: left,
            label: inlineComment || (isGenericLabel(right) ? undefined : right),
          });
          continue;
        }
      }
    }

    // Strip Bearer prefix if present
    if (/^bearer\s+/i.test(line)) {
      line = cleanApiKeyToken(line.replace(/^bearer\s+/i, ""));
    }

    // Strip outer quotes
    line = cleanApiKeyToken(line);

    // If line has no spaces and is >= 6 characters:
    // It's a clean 1-key-per-line entry
    if (!line.includes(" ") && line.length >= 6) {
      entries.push({
        key: line,
        label: inlineComment,
      });
      continue;
    }

    // If line has multiple comma or semicolon separated tokens
    if (line.includes(",") || line.includes(";")) {
      const parts = line.split(/[,;]/).map(cleanApiKeyToken).filter(Boolean);
      const allLookLikeKeys =
        parts.length > 1 &&
        parts.every((p) => looksLikeApiKey(p) || (!p.includes(" ") && p.length >= 10));
      if (allLookLikeKeys) {
        for (const p of parts) {
          entries.push({ key: p });
        }
        continue;
      }
    }

    // Dirty line / sentence / free text
    const extracted = extractKeysFromDirtyLine(line);
    if (extracted.length > 0) {
      entries.push(...extracted);
    }
  }

  return deduplicateKeys(entries);
}

export function showCreateAccount(providerId: string): void {
  let activeTab: "single" | "bulk" = "single";
  let singleLabel = "";
  let singleSecret = "";
  let singleScopes = "";
  let bulkText = "";
  let bulkScopes = "";
  let parsedKeys: ParsedKeyEntry[] = [];
  let isSubmitting = false;

  const root = ensureModalRoot();
  const wrapper = document.createElement("div");
  root.appendChild(wrapper);

  let closed = false;
  const close = (): void => {
    if (closed) return;
    closed = true;
    window.removeEventListener("keydown", onKeydown, true);
    render(html``, wrapper);
    wrapper.remove();
  };

  const onKeydown = (e: KeyboardEvent): void => {
    if (e.key === "Escape") {
      e.stopPropagation();
      close();
    }
  };
  window.addEventListener("keydown", onKeydown, true);

  const saveCurrentInputs = (): void => {
    const labelInput = wrapper.querySelector<HTMLInputElement>("#account-label");
    if (labelInput) singleLabel = labelInput.value;
    const secretInput = wrapper.querySelector<HTMLInputElement>("#account-secret");
    if (secretInput) singleSecret = secretInput.value;
    const scopesInput = wrapper.querySelector<HTMLInputElement>("#account-scopes");
    if (scopesInput) singleScopes = scopesInput.value;
    const bulkInput = wrapper.querySelector<HTMLTextAreaElement>("#bulk-keys-input");
    if (bulkInput) bulkText = bulkInput.value;
    const bulkScopesInput = wrapper.querySelector<HTMLInputElement>("#bulk-account-scopes");
    if (bulkScopesInput) bulkScopes = bulkScopesInput.value;
  };

  const submitSingle = async (): Promise<void> => {
    const rawSecret = singleSecret.trim();
    if (!rawSecret) {
      showToast("API Key / Secret is required", "error");
      renderModal();
      return;
    }
    const rawLabel = singleLabel.trim();
    const label = rawLabel || summarizeApiKey(rawSecret);
    const scopes = singleScopes.split(",").map((s) => s.trim()).filter(Boolean);
    const body = {
      provider_id: providerId,
      label,
      api_key: rawSecret,
      scopes,
    };
    try {
      await api("/accounts", { method: "POST", body: JSON.stringify(body) });
      state.accounts = (await api("/accounts")) as typeof state.accounts;
      close();
      requestUpdate();
      showToast("Account created", "success");
    } catch (err: unknown) {
      showApiError(err, "Error");
      renderModal();
    }
  };

  const submitBulk = async (): Promise<void> => {
    if (parsedKeys.length === 0) {
      showToast("No API keys detected", "error");
      renderModal();
      return;
    }
    const scopes = bulkScopes.split(",").map((s) => s.trim()).filter(Boolean);
    const payload = {
      provider_id: providerId,
      items: parsedKeys.map((item) => ({
        api_key: item.key,
        label: item.label || null,
      })),
      scopes,
    };
    try {
      await api("/accounts/bulk", {
        method: "POST",
        body: JSON.stringify(payload),
      });
      state.accounts = (await api("/accounts")) as typeof state.accounts;
      close();
      requestUpdate();
      showToast(`Created ${parsedKeys.length} accounts`, "success");
    } catch (err: unknown) {
      showApiError(err, "Error");
      renderModal();
    }
  };

  const renderModal = (): void => {
    const title = `New account for ${providerId}`;
    const body = html`
      <div class="filter-tabs" style="margin-bottom: var(--space-3);">
        <button
          type="button"
          class="filter-tab ${activeTab === "single" ? "active" : ""}"
          @click=${() => {
            saveCurrentInputs();
            activeTab = "single";
            renderModal();
          }}
        >
          Single key
        </button>
        <button
          type="button"
          class="filter-tab ${activeTab === "bulk" ? "active" : ""}"
          @click=${() => {
            saveCurrentInputs();
            activeTab = "bulk";
            renderModal();
          }}
        >
          Bulk keys
        </button>
      </div>

      ${activeTab === "single"
        ? html`
            <div class="field">
              <label for="account-label">Label (optional)</label>
              <input
                id="account-label"
                name="label"
                type="text"
                placeholder="auto-generated from key if empty"
                .value=${singleLabel}
                @input=${(e: Event) => {
                  singleLabel = (e.target as HTMLInputElement).value;
                }}
              />
            </div>
            <div class="field">
              <label for="account-secret">API Key / Secret</label>
              <input
                id="account-secret"
                name="secret"
                type="password"
                placeholder="paste the API key here"
                .value=${singleSecret}
                @input=${(e: Event) => {
                  singleSecret = (e.target as HTMLInputElement).value;
                }}
              />
            </div>
            <div class="field">
              <label for="account-scopes">Scopes (comma separated)</label>
              <input
                id="account-scopes"
                name="scopes"
                type="text"
                placeholder="chat,manage"
                .value=${singleScopes}
                @input=${(e: Event) => {
                  singleScopes = (e.target as HTMLInputElement).value;
                }}
              />
            </div>
          `
        : html`
            <div class="field">
              <label for="bulk-keys-input">
                API Keys
                <small style="color: var(--color-text-muted); font-weight: normal; margin-left: var(--space-2);">
                  1 per line or paste dirty text (env, json, list, comments)
                </small>
              </label>
              <textarea
                id="bulk-keys-input"
                name="bulk_keys"
                rows="6"
                placeholder="Paste keys here:&#10;sk-ant-api03-...&#10;Primary: sk-proj-...&#10;export OPENAI_API_KEY=&quot;sk-...&quot;"
                .value=${bulkText}
                @input=${(e: Event) => {
                  bulkText = (e.target as HTMLTextAreaElement).value;
                  parsedKeys = parseBulkApiKeys(bulkText);
                  renderModal();
                }}
                style="font-family: var(--font-mono); font-size: var(--fs-xs); width: 100%; resize: vertical;"
              ></textarea>
            </div>

            <div class="bulk-keys-preview" style="margin-top: var(--space-2); margin-bottom: var(--space-3);">
              ${parsedKeys.length === 0
                ? html`<div style="font-size: var(--fs-xs); color: var(--color-text-muted); font-style: italic;">
                    No API keys detected yet
                  </div>`
                : html`
                    <div style="font-size: var(--fs-xs); font-weight: 600; margin-bottom: var(--space-1); color: var(--color-primary);">
                      ${parsedKeys.length} ${parsedKeys.length === 1 ? "key" : "keys"} detected:
                    </div>
                    <div style="max-height: 120px; overflow-y: auto; background: var(--color-surface-2); border: 1px solid var(--color-border); border-radius: var(--radius-sm); padding: var(--space-2);">
                      ${parsedKeys.map(
                        (item, idx) => html`
                          <div style="display: flex; justify-content: space-between; font-size: var(--fs-xs); font-family: var(--font-mono); padding: 2px 0; border-bottom: 1px dashed var(--color-border);">
                            <span>#${idx + 1} ${item.label ? html`<strong>${item.label}</strong>: ` : html``}${summarizeApiKey(item.key)}</span>
                            <span style="color: var(--color-text-muted);">${item.key.length} chars</span>
                          </div>
                        `
                      )}
                    </div>
                  `}
            </div>

            <div class="field">
              <label for="bulk-account-scopes">Scopes (comma separated, applied to all)</label>
              <input
                id="bulk-account-scopes"
                name="scopes"
                type="text"
                placeholder="chat,manage"
                .value=${bulkScopes}
                @input=${(e: Event) => {
                  bulkScopes = (e.target as HTMLInputElement).value;
                }}
              />
            </div>
          `}
    `;

    const actions = html`
      <button type="button" @click=${() => close()}>Cancel</button>
      <button
        type="button"
        class="primary"
        ?disabled=${isSubmitting || (activeTab === "bulk" && parsedKeys.length === 0)}
        @click=${async (e: Event) => {
          e.preventDefault();
          saveCurrentInputs();
          if (isSubmitting) return;
          isSubmitting = true;
          renderModal();
          try {
            if (activeTab === "single") {
              await submitSingle();
            } else {
              await submitBulk();
            }
          } finally {
            isSubmitting = false;
          }
        }}
      >
        ${activeTab === "bulk" && parsedKeys.length > 0
          ? `Create (${parsedKeys.length} accounts)`
          : "Create"}
      </button>
    `;

    const template = html`
      <div
        class="modal-bg"
        role="dialog"
        aria-modal="true"
        aria-labelledby="create-account-modal-title"
        @click=${(e: Event) => {
          if (e.target === e.currentTarget) close();
        }}
      >
        <div class="modal" @click=${(e: Event) => e.stopPropagation()}>
          <div class="modal-header">
            <h2 id="create-account-modal-title">${title}</h2>
            <button
              type="button"
              class="close-btn"
              aria-label="Close"
              @click=${() => close()}
            >&times;</button>
          </div>
          <div class="modal-body">${body}</div>
          <div class="modal-footer">${actions}</div>
        </div>
      </div>
    `;

    render(template, wrapper);
  };

  renderModal();
}

export function closeCreateAccount(): void {
  const root = document.getElementById("modal-root");
  if (!root) return;
  const last = root.lastElementChild;
  if (!last) return;
  const closeBtn = last.querySelector<HTMLButtonElement>(".close-btn");
  closeBtn?.click();
}

export async function createAccount(providerId: string, e?: Event, close?: () => void): Promise<void> {
  let rawLabel = "";
  let rawSecret = "";
  let scopes: string[] = [];

  const target = e?.target;
  if (target instanceof HTMLFormElement) {
    const f = new FormData(target);
    rawLabel = (f.get("label") || "").toString().trim();
    rawSecret = (f.get("secret") || "").toString().trim();
    scopes = (f.get("scopes") || "").toString().split(",").map((s) => s.trim()).filter(Boolean);
  } else {
    const root = document.getElementById("modal-root");
    const modal = root?.lastElementChild;
    const labelInput = modal?.querySelector<HTMLInputElement>("#account-label");
    const secretInput = modal?.querySelector<HTMLInputElement>("#account-secret");
    const scopesInput = modal?.querySelector<HTMLInputElement>("#account-scopes");
    if (labelInput) rawLabel = labelInput.value.trim();
    if (secretInput) rawSecret = secretInput.value.trim();
    if (scopesInput) scopes = scopesInput.value.split(",").map((s) => s.trim()).filter(Boolean);
  }

  const label = rawLabel || (rawSecret ? summarizeApiKey(rawSecret) : null);
  const body = {
    provider_id: providerId,
    label,
    api_key: rawSecret || null,
    scopes,
  };
  try {
    await api("/accounts", { method: "POST", body: JSON.stringify(body) });
    state.accounts = await api("/accounts") as typeof state.accounts;
    if (close) close(); else closeCreateAccount();
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

export async function deleteAccount(id: number): Promise<void> {
  if (!(await showConfirm({
    title: "Delete account",
    message: "Delete account #" + id + "?",
    danger: true,
    confirmLabel: "Delete",
  }))) return;
  await mutateAndRefresh({
    apiCall: async () => {
      await api("/accounts/" + id, { method: "DELETE" });
      state.accounts = await api("/accounts") as typeof state.accounts;
    },
  });
}

export function showUpdateAccountKey(id: number): void {
  const handle = renderOpModal({
    title: `Update API key for account #${id}`,
    body: html`
      <div class="field">
        <label for="account-key">New API key</label>
        <input id="account-key" name="api_key" type="password" placeholder="paste the new API key here">
      </div>
      <p><small>Leave empty and submit to <strong>clear</strong> the key (OAuth-only account).</small></p>
    `,
    actions: html`
      <button type="button" @click=${() => handle.close()}>Cancel</button>
      <button type="submit" class="primary"
        @click=${(e: Event) => { e.preventDefault(); void updateAccountKey(id, e, handle.close); }}>
        Save key
      </button>
    `,
  });
}

export function closeUpdateAccountKey(): void {
  // Backwards-compat shim: the registry still exposes this as a
  // data-action target. renderOpModal owns the wrapper, so we just
  // close the most recent modal-bg in the modal-root.
  const root = document.getElementById("modal-root");
  if (!root) return;
  const last = root.lastElementChild;
  if (!last) return;
  const closeBtn = last.querySelector<HTMLButtonElement>(".close-btn");
  closeBtn?.click();
}

export async function updateAccountKey(id: number, e: Event, close?: () => void): Promise<void> {
  const target = e.target;
  let apiKey: string | null = null;
  if (target instanceof HTMLFormElement) {
    const f = new FormData(target);
    apiKey = f.get("api_key")?.toString().trim() || null;
  } else {
    const root = document.getElementById("modal-root");
    const modal = root?.lastElementChild;
    const input = modal?.querySelector<HTMLInputElement>("#account-key");
    apiKey = input?.value.trim() || null;
  }
  try {
    await api("/accounts/" + id + "/api-key", {
      method: "PUT",
      body: JSON.stringify({ api_key: apiKey }),
    });
    state.accounts = await api("/accounts") as typeof state.accounts;
    if (close) close(); else closeUpdateAccountKey();
    // We do NOT call requestUpdate() here — the API key is
    // not displayed in the underlying accounts table, so there's
    // nothing visible to refresh. A full rebuild would close any
    // open `<select>` (e.g. the per-account health dropdown on a
    // sibling row) and steal focus from any input the user might
    // still be editing. Mirrors patchComboField in combo-handlers.ts.
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

export async function updateAccountLabel(id: number, currentLabel: string): Promise<void> {
  const newLabel = await showPrompt("Rename account label", "New label:", currentLabel || "");
  if (newLabel == null) return;
  const trimmed = newLabel.trim();
  if (trimmed === currentLabel) return;

  await mutateAndRefresh({
    apiCall: async () => {
      await api("/accounts/" + id + "/label", {
        method: "PATCH",
        body: JSON.stringify({ label: trimmed || null }),
      });
      state.accounts = await api("/accounts") as typeof state.accounts;
    },
    errorMessage: "Error updating label",
  });
}

export async function copyAccountApiKey(id: number): Promise<void> {
  try {
    const res = await api("/accounts/" + id + "/api-key", { method: "GET" }) as { api_key?: string };
    if (res && res.api_key) {
      try {
        await copyToClipboard(res.api_key);
        showToast("API key copied to clipboard", "success");
      } catch (_e: unknown) {
        // Both navigator.clipboard and execCommand failed — show a
        // read-only modal so the operator can manually select+copy.
        showApiKeyDisplay(res.api_key);
      }
    } else {
      showToast("No API key returned", "error");
    }
  } catch (err: unknown) {
    showApiError(err, "Error copying API key");
  }
}

/**
 * Display an API key in a read-only modal so the operator can
 * manually select and copy it. Used as a last-resort fallback when
 * both clipboard paths (navigator.clipboard + execCommand) fail.
 */
function showApiKeyDisplay(apiKey: string): void {
  const handle = renderOpModal({
    title: "API Key Created",
    body: html`
      <p style="margin: 0 0 var(--space-3); color: var(--color-text-muted); font-size: var(--fs-sm);">
        Copy this key. It will not be shown again.
      </p>
      <input
        type="text"
        readonly
        .value=${apiKey}
        style="width: 100%; padding: var(--space-2); font-family: var(--font-mono); font-size: var(--fs-sm); background: var(--color-surface-2); border: 1px solid var(--color-border); border-radius: var(--radius-sm); color: var(--color-text); user-select: all; cursor: text;"
      />
    `,
    actions: html`
      <button type="button" @click=${() => handle.close()}>Close</button>
      <button type="button" class="primary"
        @click=${async () => {
          try {
            await copyToClipboard(apiKey);
            showToast("API key copied to clipboard", "success");
            handle.close();
          } catch {
            showToast("Copy failed — select the text in the field above and press Ctrl+C", "warning");
          }
        }}
      >Copy</button>
    `,
  });
}

