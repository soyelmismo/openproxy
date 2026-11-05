// views/providers/account-drag.ts — animated priority drag for the accounts list.
//
// One delegated listener set on `document` survives lit-html re-renders, so the
// drag state never references handlers owned by a template. Rows are
// `.accounts-list > .provider-entry` carrying `data-drag-id` (account id) and
// `data-provider` (provider id). Visual flow: the row is lifted as a fixed
// clone (ghost) that tracks the pointer, the original becomes a dashed
// placeholder (slot), siblings FLIP out of the way with Web Animations, and on
// release the ghost lands on the slot before the new order is persisted
// through reorderAccounts(). Escape / pointercancel / window blur restore the
// original order without persisting. Keyboard: ArrowUp/Down/Home/End on the
// handle move the row with the same sibling animation. prefers-reduced-motion
// disables lift scale and landing glide while keeping direct pointer tracking.

import { reorderAccounts } from '../../handlers/account-handlers.js';

interface DragState {
  list: HTMLElement;
  row: HTMLElement;
  handle: HTMLElement;
  pointer: number;
  startX: number;
  startY: number;
  x: number;
  y: number;
  offsetX: number;
  offsetY: number;
  width: number;
  active: boolean;
  ghost: HTMLElement | null;
  order: HTMLElement[];
}

let drag: DragState | null = null;
let landing = false;
let scrollFrame = 0;
let initialized = false;

const rowAnimations = new Map<HTMLElement, Animation>();

function reducedMotion(): boolean {
  return (
    typeof matchMedia === 'function' &&
    matchMedia('(prefers-reduced-motion: reduce)').matches
  );
}

function rowsOf(list: HTMLElement): HTMLElement[] {
  return Array.from(list.children) as HTMLElement[];
}

function accountIdOf(row: HTMLElement): number {
  return Number(row.getAttribute('data-drag-id'));
}

function prioritySpanOf(row: HTMLElement): HTMLElement | null {
  return row.querySelector<HTMLElement>('.account-priority');
}

function refreshPriorities(list: HTMLElement): void {
  rowsOf(list).forEach((row, i) => {
    const span = prioritySpanOf(row);
    if (span) span.textContent = `Priority ${i + 1}`;
  });
}

function capturePositions(list: HTMLElement): Map<HTMLElement, number> {
  return new Map(
    rowsOf(list).map((row) => [row, row.getBoundingClientRect().top]),
  );
}

function animatePositions(
  list: HTMLElement,
  before: Map<HTMLElement, number>,
  skip?: HTMLElement,
): void {
  rowAnimations.forEach((animation) => animation.cancel());
  rowAnimations.clear();
  if (reducedMotion()) return;
  for (const row of rowsOf(list)) {
    if (row === skip || !before.has(row)) continue;
    const delta = (before.get(row) ?? 0) - row.getBoundingClientRect().top;
    if (Math.abs(delta) < 1) continue;
    const animation = row.animate(
      [
        { transform: `translateY(${delta}px)` },
        { transform: 'translateY(0)' },
      ],
      { duration: 230, easing: 'cubic-bezier(.16,1,.3,1)' },
    );
    rowAnimations.set(row, animation);
    animation.finished
      .then(() => {
        if (rowAnimations.get(row) === animation) rowAnimations.delete(row);
      })
      .catch(() => {
        /* cancelled mid-flight */
      });
  }
}

function placeGhost(d: DragState): void {
  if (!d.ghost) return;
  const x = Math.max(3, Math.min(window.innerWidth - d.width - 3, d.x - d.offsetX));
  const y = d.y - d.offsetY;
  d.ghost.style.transform = `translate3d(${x}px, ${y}px, 0)`;
}

function liftCard(d: DragState): void {
  const rect = d.row.getBoundingClientRect();
  d.width = rect.width;
  d.offsetX = d.startX - rect.left;
  d.offsetY = d.startY - rect.top;

  const ghost = document.createElement('div');
  ghost.className = 'account-drag-ghost';
  ghost.setAttribute('aria-hidden', 'true');
  ghost.setAttribute('inert', '');
  ghost.style.cssText = `left:0;top:0;width:${rect.width}px;`;

  // Clone the live row: same markup, same container width, no duplicate ids.
  const copy = d.row.cloneNode(true) as HTMLElement;
  copy.removeAttribute('data-drag-id');
  copy.removeAttribute('data-provider');
  copy.removeAttribute('role');
  copy.classList.remove('drag-slot');
  copy.querySelectorAll('[id]').forEach((el) => el.removeAttribute('id'));
  copy
    .querySelectorAll('[aria-controls],[aria-labelledby]')
    .forEach((el) => {
      el.removeAttribute('aria-controls');
      el.removeAttribute('aria-labelledby');
    });
  const sourceSelects = d.row.querySelectorAll('select');
  copy.querySelectorAll('select').forEach((el, i) => {
    el.value = sourceSelects[i]?.value ?? el.value;
  });

  ghost.append(copy);
  document.body.append(ghost);
  d.ghost = ghost;
  d.row.classList.add('drag-slot');
  document.body.classList.add('account-drag-active');
  placeGhost(d);

  if (!reducedMotion()) {
    copy.animate(
      [
        { opacity: 0.8, transform: 'scale(1)' },
        { opacity: 1, transform: 'scale(1.008)' },
      ],
      { duration: 140, easing: 'ease-out' },
    );
  }
}

function moveDrag(x: number, y: number): void {
  const d = drag;
  if (!d) return;
  d.x = x;
  d.y = y;
  if (!d.active && Math.hypot(x - d.startX, y - d.startY) < 6) return;
  if (!d.active) {
    d.active = true;
    liftCard(d);
  }
  placeGhost(d);

  // Insertion thresholds from layout coordinates, never from the animated
  // rectangles: transformed siblings would oscillate around the pointer.
  const listTop = d.list.getBoundingClientRect().top;
  const before =
    rowsOf(d.list)
      .filter((n) => n !== d.row)
      .find((n) => y < listTop + n.offsetTop + n.offsetHeight / 2) ?? null;
  const currentNext = d.row.nextElementSibling;
  if ((before || null) !== currentNext) {
    const positions = capturePositions(d.list);
    if (before) d.list.insertBefore(d.row, before);
    else d.list.append(d.row);
    animatePositions(d.list, positions, d.row);
    if (!d.handle.hasPointerCapture(d.pointer)) {
      d.handle.setPointerCapture(d.pointer);
    }
    refreshPriorities(d.list);
    const ghostPriority = d.ghost?.querySelector('.account-priority');
    const rowPriority = prioritySpanOf(d.row);
    if (ghostPriority && rowPriority) {
      ghostPriority.textContent = rowPriority.textContent;
    }
  }
}

interface ScrollView {
  rect: () => DOMRect;
  scrollBy: (dy: number) => void;
}

function scrollableViewOf(list: HTMLElement): ScrollView {
  let node: HTMLElement | null = list.parentElement;
  while (node && node !== document.body) {
    const style = getComputedStyle(node);
    if (style.overflowY === 'auto' || style.overflowY === 'scroll') {
      const box = node;
      return {
        rect: () => box.getBoundingClientRect(),
        scrollBy: (dy: number) => {
          box.scrollTop += dy;
        },
      };
    }
    node = node.parentElement;
  }
  return {
    rect: () => new DOMRect(0, 0, window.innerWidth, window.innerHeight),
    scrollBy: (dy: number) => {
      window.scrollBy(0, dy);
    },
  };
}

function autoScroll(): void {
  scrollFrame = 0;
  const d = drag;
  if (!d?.active) return;
  const view = scrollableViewOf(d.list);
  const rect = view.rect();
  const edge = 56;
  const delta =
    d.y < rect.top + edge ? -12 : d.y > rect.bottom - edge ? 12 : 0;
  if (delta) {
    view.scrollBy(delta);
    moveDrag(d.x, d.y);
  }
  scrollFrame = requestAnimationFrame(autoScroll);
}

async function finishDrag(cancel = false): Promise<void> {
  const d = drag;
  if (!d) return;
  drag = null;
  landing = true;
  if (scrollFrame) {
    cancelAnimationFrame(scrollFrame);
    scrollFrame = 0;
  }
  if (d.handle.hasPointerCapture(d.pointer)) {
    d.handle.releasePointerCapture(d.pointer);
  }

  if (cancel) {
    const positions = capturePositions(d.list);
    d.order.forEach((node) => d.list.append(node));
    refreshPriorities(d.list);
    animatePositions(d.list, positions, d.row);
  }

  if (d.ghost) {
    const target = d.row.getBoundingClientRect();
    const from = d.ghost.getBoundingClientRect();
    d.ghost.classList.add('is-landing');
    const animation = d.ghost.animate(
      [
        { transform: `translate3d(${from.left}px, ${from.top}px, 0)` },
        { transform: `translate3d(${target.left}px, ${target.top}px, 0)` },
      ],
      {
        duration: reducedMotion() ? 0 : 200,
        easing: 'cubic-bezier(.16,1,.3,1)',
        fill: 'forwards',
      },
    );
    try {
      await animation.finished;
    } catch {
      /* cancelled */
    }
    d.row.classList.remove('drag-slot');
    d.ghost.remove();
  }

  document.body.classList.remove('account-drag-active');
  landing = false;

  if (d.active && !cancel) {
    const originalIds = d.order.map(accountIdOf);
    const ids = rowsOf(d.list).map(accountIdOf);
    const changed = originalIds.some((id, i) => id !== ids[i]);
    const providerId = d.row.getAttribute('data-provider') || '';
    if (changed && providerId && ids.every((id) => Number.isFinite(id))) {
      await reorderAccounts(providerId, ids);
    }
  }
  d.handle.focus({ preventScroll: true });
}

function accountDragContext(
  target: EventTarget | null,
): { handle: HTMLElement; row: HTMLElement; list: HTMLElement } | null {
  // The pointerdown target is usually the handle's inline `<svg>` icon, which is
  // an SVGElement and NOT an HTMLElement — match on Element, then narrow.
  if (!(target instanceof Element)) return null;
  const handle = target.closest<HTMLElement>('.account-drag-handle');
  if (!(handle instanceof HTMLElement)) return null;
  const row = handle.closest<HTMLElement>(
    '.provider-entry.account-card-row',
  );
  if (!(row instanceof HTMLElement)) return null;
  const list = row.parentElement;
  if (!(list instanceof HTMLElement) || !list.classList.contains('accounts-list')) return null;
  return { handle, row, list };
}

function onPointerDown(e: PointerEvent): void {
  if (e.button !== 0 || drag || landing) return;
  const ctx = accountDragContext(e.target);
  if (!ctx) return;
  e.preventDefault();
  drag = {
    list: ctx.list,
    row: ctx.row,
    handle: ctx.handle,
    pointer: e.pointerId,
    startX: e.clientX,
    startY: e.clientY,
    x: e.clientX,
    y: e.clientY,
    offsetX: 0,
    offsetY: 0,
    width: 0,
    active: false,
    ghost: null,
    order: rowsOf(ctx.list),
  };
  ctx.handle.setPointerCapture(e.pointerId);
}

function onPointerMove(e: PointerEvent): void {
  if (!drag || e.pointerId !== drag.pointer) return;
  e.preventDefault();
  moveDrag(e.clientX, e.clientY);
  if (drag?.active && !scrollFrame) {
    scrollFrame = requestAnimationFrame(autoScroll);
  }
}

function onPointerUp(e: PointerEvent): void {
  if (!drag || e.pointerId !== drag.pointer) return;
  void finishDrag(false);
}

function onPointerCancel(e: PointerEvent): void {
  if (!drag || e.pointerId !== drag.pointer) return;
  void finishDrag(true);
}

function onKeydown(e: KeyboardEvent): void {
  if (e.key === 'Escape' && drag) {
    e.preventDefault();
    void finishDrag(true);
    return;
  }
  if (drag || landing) return;
  const target = e.target;
  if (!(target instanceof Element)) return;
  const handle = target.closest<HTMLElement>('.account-drag-handle');
  if (!(handle instanceof HTMLElement)) return;
  if (!['ArrowUp', 'ArrowDown', 'Home', 'End'].includes(e.key)) return;
  const row = handle.closest<HTMLElement>(
    '.provider-entry.account-card-row',
  );
  if (!(row instanceof HTMLElement)) return;
  const list = row.parentElement;
  if (!(list instanceof HTMLElement)) return;
  e.preventDefault();

  const positions = capturePositions(list);
  if (e.key === 'ArrowUp' && row.previousElementSibling) {
    list.insertBefore(row, row.previousElementSibling);
  }
  if (e.key === 'ArrowDown' && row.nextElementSibling) {
    list.insertBefore(row.nextElementSibling!, row);
  }
  if (e.key === 'Home') list.prepend(row);
  if (e.key === 'End') list.append(row);
  refreshPriorities(list);
  animatePositions(list, positions);
  handle.focus();

  const providerId = row.getAttribute('data-provider') || '';
  const ids = rowsOf(list).map(accountIdOf);
  if (providerId && ids.length > 1 && ids.every((id) => Number.isFinite(id))) {
    void reorderAccounts(providerId, ids);
  }
}

export function initAccountDrag(): void {
  if (initialized || typeof document === 'undefined') return;
  initialized = true;
  document.addEventListener('pointerdown', onPointerDown);
  document.addEventListener('pointermove', onPointerMove);
  document.addEventListener('pointerup', onPointerUp);
  document.addEventListener('pointercancel', onPointerCancel);
  document.addEventListener('keydown', onKeydown);
  window.addEventListener('resize', () => void finishDrag(true));
  window.addEventListener('blur', () => void finishDrag(true));
}
