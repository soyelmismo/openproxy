// checkout/session.ts — durable (sessionStorage) state of the checkout.
//
// A refresh must never lose a paid order or a key the server already handed
// over. Two records:
//   * the order snapshot → resume polling / show the key step;
//   * the reveal receipt + the key → retry the reveal and survive a refresh
//     between "server wiped the ciphertext" and "buyer saved the key".
//
// The receipt is now bound to the order AT CREATION: the server stores its
// hash with the order, so only the browser that created the order can ever
// claim delivery. That makes the receipt a precondition of the POST, not
// something produced after it — and it must be DURABLE before the request,
// or a refresh would orphan a payable order.

import type { CheckoutOrder } from "./api.js";

const ORDER_KEY = "op.checkout.order";
const KEY_KEY = "op.checkout.key";
const RECEIPT_KEY = "op.checkout.receipt";
const CREATING_KEY = "op.checkout.creating";

/**
 * Parse a SQLite UTC timestamp (`YYYY-MM-DD HH:MM:SS`) as UTC.
 * `new Date("2026-01-01 10:00:00")` is interpreted as LOCAL time by browsers,
 * which shifted the countdown by the timezone offset (expiry looked hours
 * early/late). The server stores UTC; say so explicitly.
 */
export function parseUtc(sqliteUtc: string): number {
  const trimmed = sqliteUtc.trim();
  const iso = /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/.test(trimmed)
    ? trimmed.replace(" ", "T") + "Z"
    : trimmed.includes("T") && !/[zZ]|[+-]\d{2}:?\d{2}$/.test(trimmed)
      ? trimmed + "Z"
      : trimmed;
  return new Date(iso).getTime();
}

/** UUID-like receipt the server needs for the retryable reveal + ack. */
export function newReceipt(): string {
  const c = globalThis.crypto;
  // randomUUID needs a secure context; getRandomValues does not, so keep it
  // as the fallback (still 128 bits, still unguessable).
  if (c?.randomUUID) return c.randomUUID();
  if (!c?.getRandomValues) {
    throw new Error("secure random is unavailable; open the checkout over HTTPS");
  }
  const bytes = new Uint8Array(16);
  c.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * In-memory mirror: when sessionStorage is unavailable (private mode, quota,
 * storage disabled) a value written here still survives in-page retries for
 * the lifetime of the tab. It does NOT survive a reload.
 */
const memory = new Map<string, string>();

/**
 * Read the newest value this tab knows.
 *
 * The memory mirror WINS over storage: `write` mirrors first and only then
 * tries `sessionStorage`, so when the storage write fails (quota, private
 * mode) storage still holds the STALE previous value. Reading storage first
 * would resurrect it — a previous order's snapshot driving the current
 * payment step — while the fresh value sits in memory.
 */
function read(key: string): string | null {
  if (memory.has(key)) return memory.get(key) ?? null;
  try {
    const v = sessionStorage.getItem(key);
    if (v != null) return v;
  } catch {
    /* storage unavailable: only the in-memory mirror exists */
  }
  return null;
}

/** Persist and report whether the value is durable (not just in memory). */
function write(key: string, value: string): boolean {
  memory.set(key, value);
  try {
    sessionStorage.setItem(key, value);
    return true;
  } catch {
    return false;
  }
}

/**
 * Persist the order snapshot. `false` = memory only, so a reload would lose
 * the way back to this order; the caller keeps the creation marker alive and
 * warns the buyer instead of pretending the checkout is resumable.
 */
export function saveOrder(order: CheckoutOrder): boolean {
  return write(ORDER_KEY, JSON.stringify(order));
}

export function loadOrder(): CheckoutOrder | null {
  const raw = read(ORDER_KEY);
  if (!raw) return null;
  try {
    return JSON.parse(raw) as CheckoutOrder;
  } catch {
    return null;
  }
}

export function clearOrder(): void {
  memory.delete(ORDER_KEY);
  try {
    sessionStorage.removeItem(ORDER_KEY);
  } catch {
    /* ignore */
  }
}

/**
 * Store the delivered key. Returns `true` only when it is DURABLE: the caller
 * must not acknowledge (which wipes the server copy) otherwise.
 */
export function saveKey(key: string): boolean {
  return write(KEY_KEY, key);
}

export function loadKey(): string | null {
  return read(KEY_KEY);
}

/** Forget a delivered key when a NEW order starts (never show a stale key). */
export function clearKey(): void {
  memory.delete(KEY_KEY);
  try {
    sessionStorage.removeItem(KEY_KEY);
  } catch {
    /* ignore */
  }
}

/** Persist the delivery receipt. `false` = not durable (memory only). */
export function saveReceipt(receipt: string): boolean {
  return write(RECEIPT_KEY, receipt);
}

export function loadReceipt(): string | null {
  return read(RECEIPT_KEY);
}

/**
 * Persist the receipt AND the "creation in flight" marker before the POST.
 *
 * Both writes must be durable: if the response is lost (or the tab reloads
 * before it arrives) the same receipt must be reused so the retry returns the
 * order the server already created instead of opening a second payable one.
 */
export function beginCreate(receipt: string): boolean {
  // No marker unless the receipt itself is durable: a memory-only receipt
  // must never look like a resumable creation.
  if (!write(RECEIPT_KEY, receipt)) return false;
  return write(CREATING_KEY, "1");
}

/** Receipt of a creation attempt whose outcome is not known yet, if any. */
export function pendingReceipt(): string | null {
  if (!read(CREATING_KEY)) return null;
  return read(RECEIPT_KEY);
}

/**
 * Seed a receipt that arrived in the URL (?receipt=… of a recovery link).
 *
 * The browser has no order snapshot for it, but the order POST that follows
 * is idempotent per receipt server-side: reusing THIS receipt returns the
 * original order (any state) instead of opening a second payable one. The
 * in-flight marker makes `prepareCreate` reuse it at the next Buy press.
 */
export function seedReceipt(receipt: string): boolean {
  if (!write(RECEIPT_KEY, receipt)) return false;
  return write(CREATING_KEY, "1");
}

/** Clear the in-flight marker once the creation outcome is definitive. */
export function endCreate(): void {
  memory.delete(CREATING_KEY);
  try {
    sessionStorage.removeItem(CREATING_KEY);
  } catch {
    /* ignore */
  }
}

/** Result of preparing the receipt that must precede `POST /order`. */
export type CreatePrep =
  | { ok: true; receipt: string; reused: boolean }
  | { ok: false; reason: "no_crypto" | "no_storage"; message: string };

/**
 * Produce the receipt to send with order creation.
 *
 * Reuses the receipt of an attempt whose response never arrived (same
 * order, no second payable amount), otherwise mints a fresh one. `ok: false`
 * means the checkout must NOT proceed: without durable storage the buyer
 * could pay and lose the only capability that reveals the key.
 */
export function prepareCreate(): CreatePrep {
  const pending = pendingReceipt();
  if (pending) return { ok: true, receipt: pending, reused: true };
  let receipt: string;
  try {
    receipt = newReceipt();
  } catch (e) {
    return { ok: false, reason: "no_crypto", message: (e as Error).message };
  }
  if (!beginCreate(receipt)) {
    return {
      ok: false,
      reason: "no_storage",
      message:
        "This browser cannot save the checkout receipt (private mode or storage " +
        "disabled). Enable site storage and reload: without it the key could not " +
        "be recovered after payment.",
    };
  }
  return { ok: true, receipt, reused: false };
}

/** An order that tops up an existing key never reveals a plaintext key. */
export function isRenewal(order: CheckoutOrder): boolean {
  return order.api_key_id != null;
}
