// views/notifications/index.ts — mount entry point for the tray view.

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

export async function mountNotifications(
  target?: HTMLElement | null | string,
): Promise<(() => void) | void> {
  const container = (target instanceof HTMLElement) ? target : document.getElementById("main");
  if (!container || !container.isConnected) return;

  resetListState();
  resetDndState();

  setDndOnMarkAsRead((id) => { void markAsRead(id); });

  const cleanupReactive: () => void = mountView(container, renderView);

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
