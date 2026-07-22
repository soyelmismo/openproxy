// components/model-picker.ts — search + multi-select modal for
// the Keys view's "Allowed models" field. Singleton (one modal
// node in the DOM, toggled by display style).
//
// Per spec §3 + §13.8 we do not attach to `window.*`. Each
// function is exported and registered in handlers/registry.js
// so the data-action shim can find it.
//
// Migrated to lit-html: the modal is rendered into a wrapper
// `<div>` under `#modal-root` via `render()` (the wrapper sticks
// around for the lifetime of the page — the modal is shown/hidden
// by toggling `display` on its `.modal-bg`). The chips display
// and the picker list are likewise rendered via `render()`. All
// `data-action` attributes have been replaced with direct
// `@click` / `@input` / `@change` handlers; lit-html auto-escapes
// the model ids so we no longer call `escapeHtml` / `escapeAttr`.

import { html, render, type TemplateResult } from "lit-html";
import { state } from "../state/index.js";
import { ensureModalRoot } from "../lib/ui-utils.js";

// The wrapper that hosts the singleton modal. Lazily created on
// first open and reused for the lifetime of the page.
let modalWrapper: HTMLDivElement | null = null;

function modelPickerModalTemplate(): TemplateResult {
  return html`
    <div class="modal-bg modal-picker-bg" id="model-picker-modal" style="display:none;"
         @click=${(e: Event) => { if (e.target === e.currentTarget) closeModelPickerModal(); }}>
      <div class="modal modal-picker">
        <div class="modal-header">
          <h2>Select models</h2>
          <button type="button" class="close-btn" @click=${closeModelPickerModal} aria-label="Close">&times;</button>
        </div>
        <div class="picker-search">
          <input type="text" id="model-picker-search" placeholder="Search models..." @input=${filterModelPicker}>
        </div>
        <div class="modal-body">
          <div class="model-picker-list" id="model-picker-list"></div>
        </div>
        <div class="modal-footer">
          <button type="button" @click=${clearModelPicker}>Clear all</button>
          <button type="button" class="primary" @click=${closeModelPickerModal}>Done</button>
        </div>
      </div>
    </div>
  `;
}

function ensureModalNode(): void {
  if (modalWrapper && document.getElementById("model-picker-modal")) return;
  if (!modalWrapper) {
    modalWrapper = document.createElement("div");
    ensureModalRoot().appendChild(modalWrapper);
  }
  render(modelPickerModalTemplate(), modalWrapper);
}

export function getCurrentAllowedModels(): string[] | null {
  const hidden: HTMLInputElement | null = document.querySelector('input[name="allowed_models"]');
  if (!hidden) return null;
  const v: string = hidden.value;
  if (v === "") return null;
  if (v === " ") return [];
  return v.split(",").map((s) => s.trim()).filter(Boolean);
}

function allowedModelsChipsTemplate(): TemplateResult {
  const models: string[] | null = getCurrentAllowedModels();
  if (models === null) {
    return html`<span class="muted">all models</span> <button type="button" class="link-btn" @click=${openModelPickerModal}>Edit models</button>`;
  }
  if (models.length === 0) {
    return html`<span class="muted">no models</span> <button type="button" class="link-btn" @click=${openModelPickerModal}>Edit models</button>`;
  }
  return html`${models.map((m) => html`
    <span class="model-chip">${m} <button type="button" @click=${() => removeModelFromKey(m)}>&times;</button></span>
  `)} <button type="button" class="link-btn" @click=${openModelPickerModal}>Edit models</button>`;
}

export function renderAllowedModelsChips(): void {
  const display: HTMLElement | null = document.getElementById("model-picker-display");
  if (!display) return;
  render(allowedModelsChipsTemplate(), display);
}

function modelPickerListTemplate(): TemplateResult {
  const allModels = state.models || [];
  const searchEl: HTMLInputElement | null = document.getElementById("model-picker-search") as HTMLInputElement | null;
  const search: string = ((searchEl && searchEl.value) || "").toLowerCase();
  const filtered = allModels.filter((m) => !search || m.model_id.toLowerCase().includes(search));
  if (filtered.length === 0) {
    return html`<div class="model-picker-row"><span class="muted">No models match.</span></div>`;
  }
  return html`${filtered.map((m) => {
    const checked: boolean = state.modelPickerSelection.has(m.model_id);
    return html`
      <label class="model-picker-row">
        <input type="checkbox" ?checked=${checked} @change=${(e: Event) => toggleModelPicker(m.model_id, e)}>
        <span class="model-id">${m.model_id}</span>
        <span class="model-provider">${m.provider_id}</span>
      </label>
    `;
  })}`;
}

function renderModelPickerList(): void {
  const list: HTMLElement | null = document.getElementById("model-picker-list");
  if (!list) return;
  render(modelPickerListTemplate(), list);
}

export function openModelPickerModal(): void {
  ensureModalNode();
  const current: string[] | null = getCurrentAllowedModels();
  state.modelPickerSelection = new Set(current || []);
  const m: HTMLElement | null = document.getElementById("model-picker-modal");
  if (m) m.style.display = "flex";
  const s: HTMLInputElement | null = document.getElementById("model-picker-search") as HTMLInputElement | null;
  if (s) { s.value = ""; s.focus(); }
  renderModelPickerList();
}

export function closeModelPickerModal(): void {
  const hidden: HTMLInputElement | null = document.querySelector('input[name="allowed_models"]');
  if (hidden) {
    if (state.modelPickerSelection.size === 0) {
      const hadModels: boolean = hidden.value !== "" && hidden.value !== " ";
      if (hadModels) hidden.value = " ";
    } else {
      hidden.value = Array.from(state.modelPickerSelection).join(",");
    }
  }
  renderAllowedModelsChips();
  const m: HTMLElement | null = document.getElementById("model-picker-modal");
  if (m) m.style.display = "none";
}

export function clearModelPicker(): void {
  state.modelPickerSelection = new Set();
  const hidden: HTMLInputElement | null = document.querySelector('input[name="allowed_models"]');
  if (hidden) hidden.value = " ";
  renderModelPickerList();
}

export function toggleModelPicker(modelId: string, e: Event | null): void {
  const checked: boolean = !!(e && e.target && (e.target as HTMLInputElement).checked);
  if (checked) state.modelPickerSelection.add(modelId);
  else state.modelPickerSelection.delete(modelId);
}

export function filterModelPicker(): void { renderModelPickerList(); }

export function removeModelFromKey(modelId: string): void {
  const hidden: HTMLInputElement | null = document.querySelector('input[name="allowed_models"]');
  if (hidden) {
    const wasNoModels: boolean = hidden.value === " ";
    const current: string[] = (wasNoModels ? [] : hidden.value.split(",").map((s) => s.trim()).filter(Boolean));
    const next: string[] = current.filter((m) => m !== modelId);
    hidden.value = next.length === 0 ? " " : next.join(",");
  }
  const modal: HTMLElement | null = document.getElementById("model-picker-modal");
  const pickerOpen: boolean = !!modal && modal.style.display !== "none";
  if (pickerOpen) {
    state.modelPickerSelection.delete(modelId);
    renderModelPickerList();
  }
  renderAllowedModelsChips();
}
