// components/model-picker.ts — search + multi-select modal for
// the Keys view's "Allowed models" and "Blacklisted models" fields. Singleton.

import { html, render, type TemplateResult } from "lit-html";
import { state } from "../state/index.js";
import { ensureModalRoot } from "../lib/ui-utils.js";

// The wrapper that hosts the singleton modal.
let modalWrapper: HTMLDivElement | null = null;
let activeTarget: "allowed_models" | "blacklisted_models" = "allowed_models";

export function getAvailableProviders(): string[] {
  const provSet = new Set<string>();
  if (Array.isArray(state.providers)) {
    for (const p of state.providers) {
      if (p.id) provSet.add(p.id);
    }
  }
  if (Array.isArray(state.models)) {
    for (const m of state.models) {
      if (m.provider_id) provSet.add(m.provider_id);
    }
  }
  return Array.from(provSet).sort();
}

function modelPickerModalTemplate(): TemplateResult {
  const title = activeTarget === "blacklisted_models" ? "Select models to blacklist" : "Select allowed models";
  const providers = getAvailableProviders();
  return html`
    <div class="modal-bg modal-picker-bg" id="model-picker-modal" style="display:none;"
         @click=${(e: Event) => { if (e.target === e.currentTarget) closeModelPickerModal(); }}>
      <div class="modal modal-picker">
        <div class="modal-header">
          <h2>${title}</h2>
          <button type="button" class="close-btn" @click=${closeModelPickerModal} aria-label="Close">&times;</button>
        </div>
        <div class="picker-search" style="display:flex;gap:8px;padding:8px 12px;border-bottom:1px solid var(--color-border);">
          <select id="model-picker-provider" @change=${filterModelPicker} style="max-width:180px;">
            <option value="">All providers</option>
            ${providers.map((p) => html`<option value="${p}">${p}</option>`)}
          </select>
          <input type="text" id="model-picker-search" placeholder="Search models by name or id..." @input=${filterModelPicker} style="flex:1;">
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

export function getCurrentBlacklistedModels(): string[] | null {
  const hidden: HTMLInputElement | null = document.querySelector('input[name="blacklisted_models"]');
  if (!hidden) return null;
  const v: string = hidden.value;
  if (v === "" || v === " ") return null;
  return v.split(",").map((s) => s.trim()).filter(Boolean);
}

function allowedModelsChipsTemplate(): TemplateResult {
  const models: string[] | null = getCurrentAllowedModels();
  if (models === null) {
    return html`<span class="muted">all models</span> <button type="button" class="link-btn" @click=${() => openModelPickerModal("allowed_models")}>Edit models</button>`;
  }
  if (models.length === 0) {
    return html`<span class="muted">no models</span> <button type="button" class="link-btn" @click=${() => openModelPickerModal("allowed_models")}>Edit models</button>`;
  }
  return html`${models.map((m) => html`
    <span class="model-chip">${m} <button type="button" @click=${() => removeModelFromKey(m, "allowed_models")}>&times;</button></span>
  `)} <button type="button" class="link-btn" @click=${() => openModelPickerModal("allowed_models")}>Edit models</button>`;
}

function blacklistedModelsChipsTemplate(): TemplateResult {
  const models: string[] | null = getCurrentBlacklistedModels();
  if (models === null || models.length === 0) {
    return html`<span class="muted">none</span> <button type="button" class="link-btn" @click=${() => openModelPickerModal("blacklisted_models")}>Pick models</button>`;
  }
  return html`${models.map((m) => html`
    <span class="model-chip">${m} <button type="button" @click=${() => removeModelFromKey(m, "blacklisted_models")}>&times;</button></span>
  `)} <button type="button" class="link-btn" @click=${() => openModelPickerModal("blacklisted_models")}>Pick models</button>`;
}

export function renderAllowedModelsChips(): void {
  const display: HTMLElement | null = document.getElementById("model-picker-display");
  if (!display) return;
  render(allowedModelsChipsTemplate(), display);
}

export function renderBlacklistedModelsChips(): void {
  const display: HTMLElement | null = document.getElementById("blacklisted-models-display");
  if (!display) return;
  render(blacklistedModelsChipsTemplate(), display);
}

function modelPickerListTemplate(): TemplateResult {
  const allModels = state.models || [];
  const provEl: HTMLSelectElement | null = document.getElementById("model-picker-provider") as HTMLSelectElement | null;
  const selectedProv: string = (provEl && provEl.value) || "";
  const searchEl: HTMLInputElement | null = document.getElementById("model-picker-search") as HTMLInputElement | null;
  const search: string = ((searchEl && searchEl.value) || "").toLowerCase();
  const filtered = allModels.filter((m) => {
    const matchesProv = !selectedProv || m.provider_id === selectedProv;
    const matchesSearch = !search || m.model_id.toLowerCase().includes(search) || m.provider_id.toLowerCase().includes(search);
    return matchesProv && matchesSearch;
  });
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

export function openModelPickerModal(target: "allowed_models" | "blacklisted_models" = "allowed_models"): void {
  activeTarget = target;
  if (modalWrapper) {
    render(modelPickerModalTemplate(), modalWrapper);
  } else {
    ensureModalNode();
  }
  const current: string[] | null = target === "blacklisted_models" ? getCurrentBlacklistedModels() : getCurrentAllowedModels();
  state.modelPickerSelection = new Set(current || []);
  const m: HTMLElement | null = document.getElementById("model-picker-modal");
  if (m) m.style.display = "flex";
  const s: HTMLInputElement | null = document.getElementById("model-picker-search") as HTMLInputElement | null;
  if (s) { s.value = ""; s.focus(); }
  const provEl: HTMLSelectElement | null = document.getElementById("model-picker-provider") as HTMLSelectElement | null;
  if (provEl) { provEl.value = ""; }
  renderModelPickerList();
}

export function closeModelPickerModal(): void {
  const hiddenName = activeTarget;
  const hidden: HTMLInputElement | null = document.querySelector(`input[name="${hiddenName}"]`);
  if (hidden) {
    if (state.modelPickerSelection.size === 0) {
      if (activeTarget === "allowed_models") {
        const hadModels: boolean = hidden.value !== "" && hidden.value !== " ";
        if (hadModels) hidden.value = " ";
      } else {
        hidden.value = "";
      }
    } else {
      hidden.value = Array.from(state.modelPickerSelection).join(",");
    }
  }
  if (activeTarget === "allowed_models") {
    renderAllowedModelsChips();
  } else {
    renderBlacklistedModelsChips();
  }
  const m: HTMLElement | null = document.getElementById("model-picker-modal");
  if (m) m.style.display = "none";
}

export function clearModelPicker(): void {
  state.modelPickerSelection = new Set();
  const hidden: HTMLInputElement | null = document.querySelector(`input[name="${activeTarget}"]`);
  if (hidden) hidden.value = activeTarget === "allowed_models" ? " " : "";
  renderModelPickerList();
}

export function toggleModelPicker(modelId: string, e: Event | null): void {
  const checked: boolean = !!(e && e.target && (e.target as HTMLInputElement).checked);
  if (checked) state.modelPickerSelection.add(modelId);
  else state.modelPickerSelection.delete(modelId);
}

export function filterModelPicker(): void { renderModelPickerList(); }

export function removeModelFromKey(modelId: string, target: "allowed_models" | "blacklisted_models" = "allowed_models"): void {
  const hidden: HTMLInputElement | null = document.querySelector(`input[name="${target}"]`);
  if (hidden) {
    const wasNoModels: boolean = hidden.value === " ";
    const current: string[] = (wasNoModels ? [] : hidden.value.split(",").map((s) => s.trim()).filter(Boolean));
    const next: string[] = current.filter((m) => m !== modelId);
    if (target === "allowed_models") {
      hidden.value = next.length === 0 ? " " : next.join(",");
    } else {
      hidden.value = next.join(",");
    }
  }
  const modal: HTMLElement | null = document.getElementById("model-picker-modal");
  const pickerOpen: boolean = !!modal && modal.style.display !== "none" && activeTarget === target;
  if (pickerOpen) {
    state.modelPickerSelection.delete(modelId);
    renderModelPickerList();
  }
  if (target === "allowed_models") {
    renderAllowedModelsChips();
  } else {
    renderBlacklistedModelsChips();
  }
}
