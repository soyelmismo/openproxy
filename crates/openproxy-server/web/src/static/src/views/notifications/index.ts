// views/notifications/index.ts — mount entry point for the notifications
// tray view. Re-exports mountNotifications() as the public API.

import { mountView, requestUpdate } from "../../state/reactive.js";
import { onUnreadCountChange } from "../../state/notifications-store.js";
import {
  resetListState,
  fetchInitial,
  renderView,
  hasUnread,
  markAllReadOnClose,
  markAsRead,
  subscribeNotificationEvents,
} from "./list.js";
import { closeOverlay, resetDndState, setDndOnMarkAsRead } from "./dnd-overlay.js";

let unsubCount: (() => void) | null = null;
let unsubEvents: (() => void) | null = null;

export async function mountNotifications(): Promise<(() => void) | void> {
  const main: HTMLElement | null = document.getElementById("main");
  if (!main) return;

  // Reset view-local state on every mount.
  resetListState();
  resetDndState();

  // Wire the DnD → list hook so drop handlers can mark as read.
  setDndOnMarkAsRead((id) => { void markAsRead(id); });

  const cleanupReactive: () => void = mountView(main, renderView);

  unsubEvents = subscribeNotificationEvents();
  unsubCount = onUnreadCountChange(() => {
    requestUpdate();
  });

  void fetchInitial();

  return () => {
    if (unsubEvents) { unsubEvents(); unsubEvents = null; }
    if (unsubCount) { unsubCount(); unsubCount = null; }
    closeOverlay();
    cleanupReactive();
    if (hasUnread()) {
      void markAllReadOnClose();
    }
  };
}
