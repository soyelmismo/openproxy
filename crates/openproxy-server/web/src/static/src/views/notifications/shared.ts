// views/notifications/shared.ts — types, helpers, and constants shared
// between the notification list and the DnD overlay modules.

import type {
  NotificationRow,
  NotificationKind,
} from "../../lib/types/notifications.js";
import type {
  ComboTargetWithModel,
} from "../../lib/types/api.js";

// ==========
// Constants
// ==========

/** Per-kind CSS color variable for the card's left border accent and
 *  background tint. */
export const KIND_COLOR_VAR: Readonly<Record<NotificationKind, string>> = {
  model_new: "var(--color-success, #22c55e)",
  model_gone: "var(--color-error, #ef4444)",
  model_auto_activated: "var(--color-info, #3b82f6)",
  system: "var(--color-text-muted, #6b7280)",
};

/** Per-code CSS color variable for system notification cards. */
export const SYSTEM_CODE_CARD_COLOR: Readonly<Record<string, string>> = {
  discovery_failed: "var(--color-warn, #f59e0b)",
  account_key_decrypt_failed: "var(--color-error, #ef4444)",
  circuit_open: "var(--color-error, #ef4444)",
  oauth_expired: "var(--color-warn, #f59e0b)",
  account_invalid: "var(--color-error, #ef4444)",
  quota_low: "var(--color-warn, #f59e0b)",
};

/** Per-code CSS color variable for `system` notification icons. */
export const SYSTEM_CODE_COLOR_VAR: Readonly<Record<string, string>> = {
  discovery_failed: "var(--color-warn)",
  account_key_decrypt_failed: "var(--color-error)",
  circuit_open: "var(--color-error)",
  oauth_expired: "var(--color-warn)",
  account_invalid: "var(--color-error)",
  quota_low: "var(--color-warn)",
};

/** Kinds that are draggable — carry a `model_id` we can add to a combo. */
export const DRAGGABLE_KINDS: ReadonlySet<NotificationKind> =
  new Set<NotificationKind>(["model_new", "model_auto_activated"]);

/** Custom MIME type for the drag transfer. */
export const DND_MIME: string = "application/x-openproxy-notification";

/** TTL for the per-combo targets cache. */
export const TARGETS_CACHE_TTL_MS: number = 30_000;

/** Maximum number of notifications fetched per page. */
export const PAGE_LIMIT: number = 50;

// ==========
// Types
// ==========

export interface DragPayload {
  notification_id: number;
  provider_id: string;
  model_id: string;
}

export interface CachedTargets {
  targets: ComboTargetWithModel[];
  fetchedAt: number;
}

// ==========
// Payload helpers
// ==========

export function isUnread(r: NotificationRow): boolean {
  return r.read_at === null && r.archived_at === null;
}

export function payloadString(p: Record<string, unknown>, key: string): string {
  const v: unknown = p[key];
  return typeof v === "string" ? v : "";
}

export function payloadProviderId(r: NotificationRow): string {
  const fromPayload: string = payloadString(r.payload, "provider_id");
  return fromPayload || (r.provider_id ?? "");
}

export function payloadModelId(r: NotificationRow): string {
  return payloadString(r.payload, "model_id");
}
