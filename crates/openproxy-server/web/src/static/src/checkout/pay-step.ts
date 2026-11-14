// checkout/pay-step.ts — the payment step: QR, leverage line, countdown and
// the on-chain poll that flips the page to the key / success step.
//
// Lifecycle rules (money UI):
// - countdown runs on the SQLite UTC `expires_at`, never on a local-time parse;
// - expiry does NOT stop polling and the page never claims the order became
//   uncreditable: the matcher compares BLOCK time and the watcher re-scans
//   recent blocks, so a transfer sent inside the window can still be credited
//   hours or days later. Past `expires_at + confirmation_grace_secs` the poll
//   only drops to a slow cadence, so a forgotten tab stops hammering the API;
// - an `expired` REPLAY (or a restored expired snapshot) keeps the order
//   snapshot AND the receipt: that receipt is the only capability that can
//   claim a key minted by a late confirmation. Payment instructions (QR,
//   amount, address, "send the exact amount") are hidden, and the page never
//   claims nothing was charged nor pushes a second transfer;
// - there is exactly one poll chain and one countdown timeout at a time, and an
//   answer from a replaced order (or an abandoned request) is discarded instead
//   of painting over the current one;
// - every terminal transition (paid, renewal, reveal, new order) clears them all;
// - a watchdog timeout is owned by the chain: stopping the poll cancels it, so
//   an abandoned request can never start an obsolete poll;
// - a renewal order (`api_key_id`) ends in a success step, NEVER in a reveal;
// - the reveal is retryable: the receipt is stored BEFORE the request and the
//   key is stored BEFORE the ack, so a refresh cannot lose a paid key. If the
//   order changed while `/reveal` was in flight, the answer is dropped before
//   storing/acking/painting it.

import { $, hide, show } from "./dom.js";
import type { CheckoutOrder, OrderStatus } from "./api.js";
import { getJson, postJson } from "./api.js";
import {
  clearOrder,
  endCreate,
  isRenewal,
  loadReceipt,
  parseUtc,
  saveKey,
  saveOrder,
} from "./session.js";

/** Cadence while the order can still be credited at block time. */
const POLL_FAST_MS = 5_000;
/** Cadence afterwards: a late confirmation is still accepted, just less often. */
const POLL_SLOW_MS = 60_000;
/** A request that never settles must not freeze the poll chain. */
const POLL_TIMEOUT_MS = 15_000;
/** Server default when the order omits the field (matches the matcher). */
const DEFAULT_GRACE_SECS = 600;

const LATE_STATUS =
  "Price window closed. Still checking for a late on-chain confirmation: " +
  "a transfer sent before the deadline is credited as soon as it confirms.";

let order: CheckoutOrder | null = null;
let pollTimer: ReturnType<typeof setTimeout> | null = null;
let countdownTimer: ReturnType<typeof setTimeout> | null = null;
/** Timeout of the request currently in flight (one chain, one owner). */
let watchdogTimer: ReturnType<typeof setTimeout> | null = null;
/** Bumped whenever the current poll chain is abandoned, so an awaited answer
 * from an older chain is dropped instead of acting on the wrong order. */
let pollGeneration = 0;
/** The current order is past its price window: watch, never charge again. */
let lateWatch = false;

/** Current order (null until a buy or a plan renewal creates one). */
export function currentOrder(): CheckoutOrder | null {
  return order;
}

/** What a create/replay answer means for the page. */
export type OrderEntry =
  | { kind: "pay"; durable: boolean }
  /** Price window closed: no payment instructions, still watching for a late
   *  on-chain confirmation with the receipt that is bound to this order. */
  | { kind: "late"; durable: boolean }
  | { kind: "settled" };

/**
 * Take ownership of an answered order — a fresh creation OR the replay of an
 * Idempotency-Key. The server now identifies ONE purchase with that key for
 * ever, so a retry can answer with the ORIGINAL order in ANY state:
 *
 * - `expired`: the price window closed, which is NOT the matcher's verdict.
 *   Payment instructions must not be shown (a transfer sent now is outside the
 *   window), but a transfer sent before the deadline can still confirm.
 *   Keep the snapshot and the receipt and watch slowly — dropping the receipt
 *   would lose a key minted by that late confirmation and would let the buyer
 *   start a second payment for the same intent;
 * - `paid`/`minted` on a renewal: already credited, nothing left to pay;
 * - `pending`, or a minted anonymous order: the normal path (the poll reveals
 *   a key that is already claimable).
 */
export function enterOrder(o: CheckoutOrder): OrderEntry {
  if (o.status === "expired") {
    // Retain everything: this is a clock verdict, not the matcher's.
    const durable = renderLateStep(o);
    startPolling(); // slow cadence; the watcher still credits late blocks
    return { kind: "late", durable };
  }
  if (isRenewal(o) && (o.status === "paid" || o.status === "minted")) {
    clearTimers();
    order = null;
    clearOrder();
    // The purchase is delivered: a later Buy must mint a new receipt instead
    // of replaying this settled order for ever.
    endCreate();
    hide("step-pay");
    show("step-done");
    return { kind: "settled" };
  }
  const durable = renderPayStep(o);
  startCountdown();
  startPolling();
  return { kind: "pay", durable };
}

function graceMs(o: CheckoutOrder): number {
  const secs = o.confirmation_grace_secs ?? DEFAULT_GRACE_SECS;
  return Math.max(0, secs) * 1000;
}

/** Last instant at which the matcher can still credit this transfer. */
function creditedUntil(o: CheckoutOrder): number {
  return parseUtc(o.expires_at) + graceMs(o);
}

function nextDelay(o: CheckoutOrder): number {
  // A reopened expired order starts on the slow beat: the window is closed,
  // only a late confirmation can still change the page.
  if (lateWatch) return POLL_SLOW_MS;
  return Date.now() < creditedUntil(o) ? POLL_FAST_MS : POLL_SLOW_MS;
}

function clearCountdown(): void {
  if (countdownTimer) {
    clearTimeout(countdownTimer);
    countdownTimer = null;
  }
}

/**
 * Paint the payment step (QR, amount, leverage line, address).
 * Returns whether the snapshot is DURABLE: the caller must not settle the
 * "creation in flight" marker before that is true, or a reload would orphan a
 * payable order whose receipt is already spent server-side.
 */
export function renderPayStep(o: CheckoutOrder): boolean {
  // A new order replaces the previous one's whole lifecycle: no timer from the
  // old order may paint over the new payment step.
  clearTimers();
  lateWatch = false;
  order = o;
  const durable = saveOrder(o);
  show("qr");
  show("amount");
  show("addr");
  paymentNote(false);
  const qr = $("qr");
  qr.innerHTML = o.qr_svg;
  if (!o.qr_svg) {
    // Never leave a blank box: show the EIP-681 payload + address instead.
    qr.textContent = o.qr_uri + " → " + o.collector_address;
  }
  $("amount").textContent = o.amount_display + " USDC";
  attachCopy($("amount"), o.amount_display + " USDC");
  const old = document.getElementById("amount-credit");
  if (o.credit_usd != null && o.credit_usd !== o.base_usd) {
    const cred = old ?? document.createElement("div");
    cred.id = "amount-credit";
    cred.style.cssText = "color:#6366f1;font-weight:600;margin-top:4px";
    if (!old) $("amount").parentElement?.appendChild(cred);
    cred.textContent = "pays out $" + o.credit_usd + " of spendable credit";
  } else if (old) {
    // A 1:1 order must not inherit the previous order's leverage line.
    old.remove();
  }
  $("addr").textContent = o.collector_address;
  attachCopy($("addr"), o.collector_address);
  // A resumed order (refresh days later) must not pretend the price still
  // holds; it says what is actually happening instead.
  $("pay-status").textContent =
    Date.now() < creditedUntil(o)
      ? "Waiting for the transfer on Base. This page updates by itself."
      : LATE_STATUS;
  return durable;
}

/**
 * Tap-to-copy for the two values a buyer must reproduce outside the QR
 * (exchange withdrawals do not parse EIP-681: Binance fills only the
 * address). Flashing green confirms the copy; failure is silent — the value
 * stays selectable and the page works without clipboard permission.
 */
function attachCopy(el: HTMLElement, text: string): void {
  el.onclick = () => {
    void navigator.clipboard?.writeText(text).then(
      () => {
        el.classList.add("copied");
        setTimeout(() => el.classList.remove("copied"), 1_200);
      },
      () => {},
    );
  };
}

/**
 * `expired` replay / restored expired snapshot: keep the purchase watchable
 * but strip every payment instruction.
 *
 * The order is not dead — the matcher compares block time, so a transfer sent
 * before the deadline can confirm later and mint the key. Only the receipt
 * persisted with this order can claim that key, so both records are retained.
 * Showing a QR now would invite a transfer the matcher will not accept AND
 * hide the first payment's key behind a second purchase.
 */
function renderLateStep(o: CheckoutOrder): boolean {
  clearTimers();
  lateWatch = true;
  order = o;
  const durable = saveOrder(o); // a refresh must resume THIS watch
  const qr = $("qr");
  qr.innerHTML = ""; // never re-serve a stale QR from a previous order
  hide("qr");
  hide("amount");
  hide("addr");
  paymentNote(true);
  document.getElementById("amount-credit")?.remove();
  $("timer").textContent = "Expired";
  $("pay-status").textContent = LATE_STATUS;
  return durable;
}

/**
 * The "Send the exact amount shown…" note is payment instructions: hide it
 * while only a late confirmation is being watched and bring it back for a
 * payable order. It has no id (the page is not ours to restructure), so it is
 * the `.field` inside the payment step.
 */
function paymentNote(hidden: boolean): void {
  const note = $("step-pay").querySelector<HTMLElement>(".field");
  note?.classList.toggle("hidden", hidden);
}

/** One countdown timeout at a time. Expiry keeps polling alive. */
export function startCountdown(): void {
  clearCountdown();
  if (!order) return;
  const o = order;
  const deadline = parseUtc(o.expires_at);
  const tick = (): void => {
    const left = Math.max(0, Math.floor((deadline - Date.now()) / 1000));
    const m = Math.floor(left / 60);
    const s = left % 60;
    $("timer").textContent =
      left > 0
        ? "Price holds for " + m + ":" + String(s).padStart(2, "0")
        : "Expired";
    if (left > 0) {
      countdownTimer = setTimeout(tick, 1000);
      return;
    }
    countdownTimer = null;
    // Keep the poll: the transfer may already be on-chain but not yet `safe`
    // (or mined days later, which the watcher still credits).
    if (order === o) $("pay-status").textContent = LATE_STATUS;
  };
  tick();
}

/** Poll until the order reaches paid/minted. Replaces any previous chain. */
export function startPolling(): void {
  stopPolling();
  if (!order) return;
  const gen = pollGeneration;
  void pollOnce(gen);
}

function schedulePoll(gen: number, delay: number): void {
  if (gen !== pollGeneration) return;
  if (pollTimer) clearTimeout(pollTimer);
  pollTimer = setTimeout(() => {
    pollTimer = null;
    void pollOnce(gen);
  }, delay);
}

async function pollOnce(gen: number): Promise<void> {
  const o = order;
  if (!o || gen !== pollGeneration) return;
  // Chained timeouts keep ONE request in flight; the watchdog only abandons a
  // request that never settles (dead radio/proxy) and keeps the chain alive.
  let timedOut = false;
  const watchdog = setTimeout(() => {
    timedOut = true;
    // Retry immediately: the 15 s wait already was the backoff.
    schedulePoll(gen, 0);
  }, POLL_TIMEOUT_MS);
  watchdogTimer = watchdog;
  let st: OrderStatus | null = null;
  try {
    st = await getJson<OrderStatus>("v1/credits/order/" + o.order_code);
  } catch {
    st = null; // transient (network/5xx): retry on the next beat
  } finally {
    clearTimeout(watchdog);
    if (watchdogTimer === watchdog) watchdogTimer = null;
  }
  // Drop stale answers: another order took over, polling was stopped, or the
  // watchdog already scheduled the next attempt.
  if (timedOut || gen !== pollGeneration || order !== o) return;
  if (!st) {
    schedulePoll(gen, nextDelay(o));
    return;
  }
  if (st.order_code && st.order_code !== o.order_code) return;
  // `expired` from the status endpoint is NOT proof the watcher gave up: a
  // transfer confirmed by a late block is still matched, so keep watching.
  if (st.status !== "paid" && st.status !== "minted") {
    schedulePoll(gen, nextDelay(o));
    return;
  }
  // Paid — possibly long after expiry, when the block confirmed late. A paid
  // order is paid: never re-litigate it against the clock.
  clearTimers();
  $("pay-status").textContent =
    "Paid. Confirmed on-chain (block " + (st.block_number ?? "?") + ").";
  // Only the ORDER decides: a renewal targets a key from the start. An
  // anonymous mint ALSO stores api_key_id once the matcher mints the key, so
  // "status has api_key_id" must never be read as "this was a renewal" —
  // that would swallow a freshly bought key.
  if (isRenewal(o)) {
    // Renewal: the server applied the plan to the buyer's key. There is no
    // plaintext to reveal; showing the key step would end in a 409.
    order = null;
    clearOrder();
    endCreate();
    hide("step-pay");
    show("step-done");
    return;
  }
  try {
    const key = await revealOnce();
    // The order may have changed during the /reveal (or its /ack): never paint
    // a previous purchase's key over the current payment step.
    if (key && order === o) showKey(key);
  } catch (e) {
    // Leave the manual "Show my key" button as the retry surface.
    $("pay-status").textContent = (e as Error).message;
    hide("step-pay");
    show("step-key");
  }
}

/**
 * Retryable reveal + immediate ack once the key is safely stored.
 * The receipt is persisted BEFORE the request; the key is persisted BEFORE
 * the ack. If the response or the ack is lost, the next attempt reuses both.
 */
export async function revealOnce(): Promise<string | null> {
  const o = order;
  if (!o) return null;
  // The receipt was minted and persisted BEFORE the order POST and its hash
  // is bound to the order at creation. Minting a new one here would be a
  // guaranteed 409 — and a lost receipt is unrecoverable by design.
  const receipt = loadReceipt();
  if (!receipt) {
    throw new Error(
      "The checkout receipt is missing from this browser (storage cleared); " +
        "the key cannot be revealed without it.",
    );
  }
  const res = await postJson<{ key?: string; warning?: string }>(
    "v1/credits/order/" + o.order_code + "/reveal",
    {},
    { "Idempotency-Key": receipt },
  );
  if (!res.ok || !res.body.key) {
    throw new Error(res.body.error?.message || "reveal failed (HTTP " + res.status + ")");
  }
  const key = res.body.key;
  // A new order may have replaced this one while /reveal was in flight. Do not
  // store the key, do not ack (which wipes the server copy) and do not let the
  // caller paint it: that key belongs to a purchase the page no longer shows.
  if (order !== o) return null;
  // ACK only after the key is DURABLY stored: ack wipes the server-side
  // ciphertext, so acknowledging after a failed sessionStorage write (quota,
  // private mode) would destroy the only copy of a paid key.
  const durable = saveKey(key);
  // Delivered: settle the creation marker so a later Buy mints a new receipt
  // instead of replaying this order. With a memory-only key the marker stays,
  // because a reload must be able to resume the reveal.
  if (durable) endCreate();
  if (!durable) return key;
  try {
    await postJson<{ ok?: boolean }>(
      "v1/credits/order/" + o.order_code + "/ack",
      {},
      { "Idempotency-Key": receipt },
    );
  } catch {
    /* ack is best-effort; the ciphertext sweep will catch it */
  }
  // The ack is an await too: drop the key if the page moved on meanwhile.
  return order === o ? key : null;
}

/** Paint the key step with a key already obtained (fresh or restored). */
export function showKey(key: string): void {
  const el = $("key");
  // A delivered key ends the payment step: no timer may touch the page again.
  clearTimers();
  el.textContent = key;
  el.classList.remove("hidden");
  $("reveal").classList.add("hidden");
  hide("step-pay");
  show("step-key");
}

function clearTimers(): void {
  stopPolling();
  clearCountdown();
}

export function stopPolling(): void {
  // Bumping the generation invalidates the request that may still be in flight.
  pollGeneration += 1;
  if (pollTimer) {
    clearTimeout(pollTimer);
    pollTimer = null;
  }
  // Cancel the watchdog of a request that is still in flight: it belongs to the
  // chain we just abandoned and must not schedule an obsolete poll.
  if (watchdogTimer) {
    clearTimeout(watchdogTimer);
    watchdogTimer = null;
  }
}
