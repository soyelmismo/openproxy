// components/model-picker.ts — search + multi-select modal for the Keys view's
// "Allowed models", "Blacklisted models", and "Allowed combos" fields. Singleton.

import { html, render, type TemplateResult } from "lit-html";
import { state } from "../state/index.js";
import { ensureModalRoot } from "../lib/ui-utils.js";
import type { Combo } from "../lib/types/combos.js";

// Wrapper hosting the singleton modal.
let modalWrapper: HTMLDivElement | null = null;
let activeTarget: "allowed_models" | "blacklisted_models" | "allowed_combos" = "allowed_models";
const PAGE_SIZE = 50;
let displayedLimit = PAGE_SIZE;

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
  let title = "Select allowed models";
  if (activeTarget === "blacklisted_models") {
    title = "Select models to blacklist";
  } else if (activeTarget === "allowed_combos") {
    title = "Select allowed combos";
  }
  const providers = getAvailableProviders();
  const isCombos = activeTarget === "allowed_combos";
  return html`
    <div class="modal-bg modal-picker-bg" id="model-picker-modal" style="display:none;"
         @click=${(e: Event) => { if (e.target === e.currentTarget) closeModelPickerModal(); }}>
      <div class="modal modal-picker">
        <div class="modal-header">
          <h2>${title}</h2>
          <button type="button" class="close-btn" @click=${closeModelPickerModal} aria-label="Close">&times;</button>
        </div>
        <div class="picker-search" style="display:flex;gap:8px;padding:8px 12px;border-bottom:1px solid var(--color-border);">
          ${!isCombos ? html`
            <select id="model-picker-provider" class="model-picker-provider-select" @change=${onFilterChange}>
              <option value="">All providers</option>
              <option value="combo">combo (virtual)</option>
              ${providers.map((p) => html`<option value="${p}">${p}</option>`)}
            </select>
          ` : html``}
          <input type="text" id="model-picker-search" placeholder=${isCombos ? "Search combos by name or strategy..." : "Search models or combos..."} @input=${onFilterChange} style="flex:1;">
        </div>
        <div class="modal-body">
          <div class="model-picker-list" id="model-picker-list" @scroll=${onPickerScroll}></div>
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
  if (modalWrapper && document.getElementById("model-picker-modal") && modalWrapper.isConnected) return;
  if (!modalWrapper || !modalWrapper.isConnected) {
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

export function getCurrentAllowedCombos(): number[] | null {
  const hidden: HTMLInputElement | null = document.querySelector('input[name="allowed_combos"]');
  if (!hidden) return null;
  const v: string = hidden.value;
  if (v === "") return null;
  if (v === " ") return [];
  return v.split(",").map((s) => parseInt(s.trim(), 10)).filter((n) => Number.isFinite(n));
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

function allowedCombosChipsTemplate(): TemplateResult {
  const combos: number[] | null = getCurrentAllowedCombos();
  const allCombos = state.combos || [];
  const comboMap = new Map<number, string>(allCombos.map((c: Combo) => [c.id, c.name]));
  if (combos === null) {
    return html`<span class="muted">all combos</span> <button type="button" class="link-btn" @click=${() => openModelPickerModal("allowed_combos")}>Edit combos</button>`;
  }
  if (combos.length === 0) {
    return html`<span class="muted">no combos</span> <button type="button" class="link-btn" @click=${() => openModelPickerModal("allowed_combos")}>Edit combos</button>`;
  }
  return html`${combos.map((id) => {
    const name = comboMap.get(id) || `Combo #${id}`;
    return html`
      <span class="model-chip">${name} <button type="button" @click=${() => removeComboFromKey(id)}>&times;</button></span>
    `;
  })} <button type="button" class="link-btn" @click=${() => openModelPickerModal("allowed_combos")}>Edit combos</button>`;
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

export function renderAllowedCombosChips(): void {
  const display: HTMLElement | null = document.getElementById("allowed-combos-display");
  if (!display) return;
  render(allowedCombosChipsTemplate(), display);
}

interface PickerItem {
  id: string;
  name: string;
  subtitle: string;
  isCombo?: boolean;
}

function getFilteredPickerItems(search: string, selectedProv: string): PickerItem[] {
  if (activeTarget === "allowed_combos") {
    const allCombos: Combo[] = state.combos || [];
    const filtered = allCombos.filter((c: Combo) => {
      const name = (c.name || "").toLowerCase();
      const strat = (c.strategy || "").toLowerCase();
      return !search || name.includes(search) || strat.includes(search) || String(c.id).includes(search);
    });
    return filtered.map((c: Combo) => ({
      id: String(c.id),
      name: c.name,
      subtitle: `${c.strategy} (#${c.id})`,
      isCombo: true,
    }));
  }

  const items: PickerItem[] = [];

  // In allowed_models or blacklisted_models, include virtual combos unless filtered by another provider
  if (!selectedProv || selectedProv === "combo") {
    const allCombos: Combo[] = state.combos || [];
    for (const c of allCombos) {
      const comboName = c.name || "";
      const comboVirtualId = `combo:${comboName}`;
      const strat = c.strategy || "";
      const matchesSearch = !search
        || comboName.toLowerCase().includes(search)
        || comboVirtualId.toLowerCase().includes(search)
        || strat.toLowerCase().includes(search)
        || "combo".includes(search);
      if (matchesSearch) {
        items.push({
          id: comboVirtualId,
          name: comboVirtualId,
          subtitle: `combo • ${strat}`,
          isCombo: true,
        });
      }
    }
  }

  if (selectedProv !== "combo") {
    const allModels = state.models || [];
    for (const m of allModels) {
      const matchesProv = !selectedProv || m.provider_id === selectedProv;
      const matchesSearch = !search
        || m.model_id.toLowerCase().includes(search)
        || m.provider_id.toLowerCase().includes(search);
      if (matchesProv && matchesSearch) {
        items.push({
          id: m.model_id,
          name: m.model_id,
          subtitle: m.provider_id,
        });
      }
    }
  }

  return items;
}

function modelPickerListTemplate(): TemplateResult {
  const searchEl: HTMLInputElement | null = document.getElementById("model-picker-search") as HTMLInputElement | null;
  const search: string = ((searchEl && searchEl.value) || "").toLowerCase();
  const provEl: HTMLSelectElement | null = document.getElementById("model-picker-provider") as HTMLSelectElement | null;
  const selectedProv: string = (provEl && provEl.value) || "";

  const items = getFilteredPickerItems(search, selectedProv);

  if (items.length === 0) {
    const emptyMsg = activeTarget === "allowed_combos" ? "No combos match." : "No models or combos match.";
    return html`<div class="model-picker-row"><span class="muted">${emptyMsg}</span></div>`;
  }

  const slice = items.slice(0, displayedLimit);
  const remaining = items.length - slice.length;

  return html`
    ${slice.map((item: PickerItem) => {
      const checked: boolean = state.modelPickerSelection.has(item.id);
      return html`
        <label class="model-picker-row">
          <input type="checkbox" ?checked=${checked} @change=${(e: Event) => toggleModelPicker(item.id, e)}>
          <span class="model-id">${item.name}</span>
          <span class="model-provider">${item.subtitle}</span>
        </label>
      `;
    })}
    ${remaining > 0 ? html`
      <div style="padding:10px;text-align:center;">
        <button type="button" class="small" @click=${loadMorePickerItems} style="width:100%;">
          Load more (${remaining} remaining)
        </button>
      </div>
    ` : html``}
  `;
}

function renderModelPickerList(): void {
  const list: HTMLElement | null = document.getElementById("model-picker-list");
  if (!list) return;
  render(modelPickerListTemplate(), list);
}

export function onFilterChange(): void {
  displayedLimit = PAGE_SIZE;
  renderModelPickerList();
  const list = document.getElementById("model-picker-list");
  if (list) list.scrollTop = 0;
}

export function onPickerScroll(e: Event): void {
  const target = e.target as HTMLElement | null;
  if (!target) return;
  if (target.scrollTop + target.clientHeight >= target.scrollHeight - 60) {
    const searchEl = document.getElementById("model-picker-search") as HTMLInputElement | null;
    const search = ((searchEl && searchEl.value) || "").toLowerCase();
    const provEl = document.getElementById("model-picker-provider") as HTMLSelectElement | null;
    const selectedProv = (provEl && provEl.value) || "";
    const total = getFilteredPickerItems(search, selectedProv).length;
    if (displayedLimit < total) {
      displayedLimit += PAGE_SIZE;
      renderModelPickerList();
    }
  }
}

export function loadMorePickerItems(): void {
  displayedLimit += PAGE_SIZE;
  renderModelPickerList();
}

export function openModelPickerModal(target: "allowed_models" | "blacklisted_models" | "allowed_combos" = "allowed_models"): void {
  activeTarget = target;
  displayedLimit = PAGE_SIZE;
  ensureModalNode();
  if (modalWrapper) {
    render(modelPickerModalTemplate(), modalWrapper);
  }
  let current: string[] | null = null;
  if (target === "blacklisted_models") {
    current = getCurrentBlacklistedModels();
  } else if (target === "allowed_combos") {
    const combos = getCurrentAllowedCombos();
    current = combos ? combos.map(String) : null;
  } else {
    current = getCurrentAllowedModels();
  }
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
      if (activeTarget === "allowed_models" || activeTarget === "allowed_combos") {
        const hadValues: boolean = hidden.value !== "" && hidden.value !== " ";
        if (hadValues) hidden.value = " ";
      } else {
        hidden.value = "";
      }
    } else {
      hidden.value = Array.from(state.modelPickerSelection).join(",");
    }
  }
  if (activeTarget === "allowed_models") {
    renderAllowedModelsChips();
  } else if (activeTarget === "blacklisted_models") {
    renderBlacklistedModelsChips();
  } else if (activeTarget === "allowed_combos") {
    renderAllowedCombosChips();
  }
  const m: HTMLElement | null = document.getElementById("model-picker-modal");
  if (m) m.style.display = "none";
}

export function clearModelPicker(): void {
  state.modelPickerSelection = new Set();
  const hidden: HTMLInputElement | null = document.querySelector(`input[name="${activeTarget}"]`);
  if (hidden) hidden.value = (activeTarget === "allowed_models" || activeTarget === "allowed_combos") ? " " : "";
  renderModelPickerList();
}

export function toggleModelPicker(modelId: string, e: Event | null): void {
  const checked: boolean = !!(e && e.target && (e.target as HTMLInputElement).checked);
  if (checked) state.modelPickerSelection.add(modelId);
  else state.modelPickerSelection.delete(modelId);
}

export function filterModelPicker(): void { onFilterChange(); }

export function removeComboFromKey(comboId: number): void {
  const hidden: HTMLInputElement | null = document.querySelector('input[name="allowed_combos"]');
  if (hidden) {
    const raw = hidden.value;
    let list: string[] = [];
    if (raw !== "" && raw !== " ") {
      list = raw.split(",").map((s) => s.trim()).filter(Boolean);
    }
    const filtered = list.filter((id) => id !== String(comboId));
    hidden.value = filtered.length === 0 ? " " : filtered.join(",");
  }
  const modal: HTMLElement | null = document.getElementById("model-picker-modal");
  if (modal && modal.style.display !== "none" && activeTarget === "allowed_combos") {
    state.modelPickerSelection.delete(String(comboId));
    renderModelPickerList();
  }
  renderAllowedCombosChips();
}

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
