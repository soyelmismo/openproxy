// handlers/combo-target-handlers/drag-and-drop.ts — drag-and-drop
// reorder for the combo targets table.
//
// Split from combo-target-handlers.ts to keep each module < 600 LOC.

import { api } from "../../state/api.js";
import { html, render } from "lit-html";
import { mutateAndRefresh } from "../../lib/mutate.js";

// ---- Drag-and-Drop module state ----
let dragSourceId: number | null = null;
let dragComboId: number | null = null;
let dropPlaceholder: HTMLTableRowElement | null = null;
let dragFromHandle = false;

// Number of columns in the targets table. Computed once on
// `initDragAndDrop` from the `<thead>` so the drop placeholder's
// `<td colspan>` matches the actual layout.
let dropPlaceholderColspan = 8;

function removePlaceholder(): void {
  if (dropPlaceholder && dropPlaceholder.parentNode) {
    dropPlaceholder.parentNode.removeChild(dropPlaceholder);
  }
  dropPlaceholder = null;
}

function readOrderFromDOM(tbody: HTMLElement): number[] {
  const ids: number[] = [];
  for (const row of tbody.querySelectorAll("tr[data-drag-id]")) {
    const id = parseInt(row.getAttribute("data-drag-id") || "", 10);
    if (!Number.isNaN(id)) ids.push(id);
  }
  return ids;
}

function onDragStart(e: DragEvent): void {
  const row = (e.target as HTMLElement).closest("tr[data-drag-id]");
  if (!row) return;

  if (!dragFromHandle) { e.preventDefault(); return; }
  dragFromHandle = false;

  dragSourceId = parseInt(row.getAttribute("data-drag-id") || "", 10);
  dragComboId = parseInt(row.getAttribute("data-combo-id") || "", 10);

  if (Number.isNaN(dragSourceId) || Number.isNaN(dragComboId)) {
    dragSourceId = null;
    dragComboId = null;
    return;
  }

  row.classList.add("dnd-dragging");
  if (e.dataTransfer) {
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData("text/plain", String(dragSourceId));
    const rect = row.getBoundingClientRect();
    e.dataTransfer.setDragImage(row, e.clientX - rect.left, e.clientY - rect.top);
  }
}

function onDragOver(e: DragEvent): void {
  e.preventDefault();
  if (e.dataTransfer) e.dataTransfer.dropEffect = "move";

  const tbody = document.getElementById("targets-tbody");
  const row = (e.target as HTMLElement).closest("tr[data-drag-id]");
  if (!tbody || !row || !dragSourceId) return;

  const targetId = parseInt(row.getAttribute("data-drag-id") || "", 10);
  if (targetId === dragSourceId) return;

  const rect = row.getBoundingClientRect();
  const midpoint = rect.top + rect.height / 2;
  const insertBefore = e.clientY < midpoint;

  removePlaceholder();

  dropPlaceholder = document.createElement("tr");
  dropPlaceholder.className = "dnd-placeholder";
  render(html`<td colspan=${dropPlaceholderColspan}></td>`, dropPlaceholder);

  if (insertBefore) {
    tbody.insertBefore(dropPlaceholder, row);
  } else {
    tbody.insertBefore(dropPlaceholder, row.nextSibling);
  }
}

function onDragEnter(_e: DragEvent): void {
  // No-op — dragover handles positioning.
}

function onDragLeave(e: DragEvent): void {
  const related = e.relatedTarget as HTMLElement | null;
  const tbody = document.getElementById("targets-tbody");
  if (tbody && related && tbody.contains(related)) return;
  removePlaceholder();
}

// CRITICAL FIX C2: compute drop position from e.clientY + bounding rects,
// NOT from e.target.closest() which fails when dropping on placeholder.
async function onDrop(e: DragEvent): Promise<void> {
  e.preventDefault();
  if (dragSourceId === null || dragComboId === null) { removePlaceholder(); return; }

  const tbody = document.getElementById("targets-tbody");
  removePlaceholder();
  if (!tbody) return;

  const rows = [...tbody.querySelectorAll("tr[data-drag-id]")];
  let targetRow: Element | null = null;
  let insertAfter = false;
  for (const r of rows) {
    const rect = r.getBoundingClientRect();
    if (e.clientY <= rect.top + rect.height / 2) {
      targetRow = r;
      break;
    }
  }
  if (!targetRow) {
    insertAfter = true;
    targetRow = rows[rows.length - 1] || null;
  }
  if (!targetRow) return;

  const dropTargetId = parseInt(targetRow.getAttribute("data-drag-id") || "", 10);
  if (Number.isNaN(dropTargetId) || dropTargetId === dragSourceId) return;

  const orderedIds = readOrderFromDOM(tbody);
  const fromIdx = orderedIds.indexOf(dragSourceId);
  const toIdx = orderedIds.indexOf(dropTargetId);
  if (fromIdx < 0 || toIdx < 0) return;

  const newOrder = [...orderedIds];
  newOrder.splice(fromIdx, 1);
  const adjustedIdx = insertAfter
    ? newOrder.indexOf(dropTargetId) + 1
    : newOrder.indexOf(dropTargetId);
  newOrder.splice(adjustedIdx, 0, dragSourceId);

  const comboId = dragComboId;
  await mutateAndRefresh({
    apiCall: () => api(`/combos/${comboId}/targets/reorder`, {
      method: "POST",
      body: JSON.stringify({ target_ids: newOrder }),
    }),
    errorMessage: "Error reordering",
  });
}

function onDragEnd(_e: DragEvent): void {
  dragSourceId = null;
  dragComboId = null;
  removePlaceholder();

  const tbody = document.getElementById("targets-tbody");
  if (tbody) {
    tbody.querySelectorAll(".dnd-dragging").forEach((el) =>
      el.classList.remove("dnd-dragging")
    );
  }
}

export function initDragAndDrop(): void {
  const tbody = document.getElementById("targets-tbody");
  if (!tbody) return;

  const table = tbody.closest("table");
  const ths = table ? table.querySelectorAll("thead th") : null;
  if (ths && ths.length > 0) dropPlaceholderColspan = ths.length;

  tbody.addEventListener("mousedown", (e) => {
    dragFromHandle = !!(e.target as HTMLElement).closest(".drag-handle");
  });

  tbody.addEventListener("dragstart", onDragStart);
  tbody.addEventListener("dragover", onDragOver);
  tbody.addEventListener("dragenter", onDragEnter);
  tbody.addEventListener("dragleave", onDragLeave);
  tbody.addEventListener("drop", onDrop);
  tbody.addEventListener("dragend", onDragEnd);
}
