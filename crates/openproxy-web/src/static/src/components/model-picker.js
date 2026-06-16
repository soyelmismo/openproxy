// components/model-picker.js — search + multi-select modal for
// the Keys view's "Allowed models" field. Singleton (one modal
// node in the DOM, toggled by display style).
//
// Per spec §3 + §13.8 we do not attach to `window.*`. Each
// function is exported and registered in handlers/registry.js
// so the data-action shim can find it.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { appendModal } from "../lib/dom.js";

function ensureModalNode() {
  if (document.getElementById("model-picker-modal")) return;
  const html = `
    <div class="modal-bg modal-picker-bg" id="model-picker-modal" style="display:none;" data-action="closeModelPickerModal">
      <div class="modal modal-picker">
        <div class="modal-header">
          <h2>Select models</h2>
          <button type="button" class="close-btn" data-action="closeModelPickerModal" aria-label="Close">&times;</button>
        </div>
        <div class="picker-search">
          <input type="text" id="model-picker-search" placeholder="Search models..." data-action="filterModelPicker">
        </div>
        <div class="modal-body">
          <div class="model-picker-list" id="model-picker-list"></div>
        </div>
        <div class="modal-footer">
          <button type="button" data-action="clearModelPicker">Clear all</button>
          <button type="button" class="primary" data-action="closeModelPickerModal">Done</button>
        </div>
      </div>
    </div>
  `;
  appendModal(html);
}

export function getCurrentAllowedModels() {
  const hidden = document.querySelector('input[name="allowed_models"]');
  if (!hidden) return null;
  const v = hidden.value;
  if (v === "") return null;
  if (v === " ") return [];
  return v.split(",").map(s => s.trim()).filter(Boolean);
}

export function renderAllowedModelsChips() {
  const display = document.getElementById("model-picker-display");
  if (!display) return;
  const models = getCurrentAllowedModels();
  if (models === null) {
    display.innerHTML = '<span class="muted">all models</span> <button type="button" class="link-btn" data-action="openModelPickerModal">Edit</button>';
  } else if (models.length === 0) {
    display.innerHTML = '<span class="muted">no models</span> <button type="button" class="link-btn" data-action="openModelPickerModal">Edit</button>';
  } else {
    const chips = models.map(m =>
      `<span class="model-chip">${escapeHtml(m)} <button type="button" data-action="removeModelFromKey" data-arg1="${escapeAttr(m)}">&times;</button></span>`
    ).join("");
    display.innerHTML = `${chips} <button type="button" class="link-btn" data-action="openModelPickerModal">Edit</button>`;
  }
}

function renderModelPickerList() {
  const list = document.getElementById("model-picker-list");
  if (!list) return;
  const allModels = state.models || [];
  const search = ((document.getElementById("model-picker-search") || {}).value || "").toLowerCase();
  const filtered = allModels.filter(m => !search || m.model_id.toLowerCase().includes(search));
  if (filtered.length === 0) {
    list.innerHTML = `<div class="model-picker-row"><span class="muted">No models match.</span></div>`;
    return;
  }
  list.innerHTML = filtered.map(m => {
    const checked = state.modelPickerSelection.has(m.model_id);
    return `
      <label class="model-picker-row">
        <input type="checkbox" ${checked ? "checked" : ""} data-action="toggleModelPicker" data-arg1="${escapeAttr(m.model_id)}">
        <span class="model-id">${escapeHtml(m.model_id)}</span>
        <span class="model-provider">${escapeHtml(m.provider_id)}</span>
      </label>
    `;
  }).join("");
}

export function openModelPickerModal() {
  ensureModalNode();
  const current = getCurrentAllowedModels();
  state.modelPickerSelection = new Set(current || []);
  const m = document.getElementById("model-picker-modal");
  if (m) m.style.display = "flex";
  const s = document.getElementById("model-picker-search");
  if (s) { s.value = ""; s.focus(); }
  renderModelPickerList();
}

export function closeModelPickerModal() {
  const hidden = document.querySelector('input[name="allowed_models"]');
  if (hidden) {
    if (state.modelPickerSelection.size === 0) {
      const hadModels = hidden.value !== "" && hidden.value !== " ";
      if (hadModels) hidden.value = " ";
    } else {
      hidden.value = Array.from(state.modelPickerSelection).join(",");
    }
  }
  renderAllowedModelsChips();
  const m = document.getElementById("model-picker-modal");
  if (m) m.style.display = "none";
}

export function clearModelPicker() {
  state.modelPickerSelection = new Set();
  const hidden = document.querySelector('input[name="allowed_models"]');
  if (hidden) hidden.value = " ";
  renderModelPickerList();
}

export function toggleModelPicker(modelId, e) {
  const checked = e && e.target ? !!e.target.checked : false;
  if (checked) state.modelPickerSelection.add(modelId);
  else state.modelPickerSelection.delete(modelId);
}

export function filterModelPicker() { renderModelPickerList(); }

export function removeModelFromKey(modelId) {
  const hidden = document.querySelector('input[name="allowed_models"]');
  if (hidden) {
    const wasNoModels = hidden.value === " ";
    const current = (wasNoModels ? [] : hidden.value.split(",").map(s => s.trim()).filter(Boolean));
    const next = current.filter(m => m !== modelId);
    hidden.value = next.length === 0 ? " " : next.join(",");
  }
  const pickerOpen = document.getElementById("model-picker-modal") && document.getElementById("model-picker-modal").style.display !== "none";
  if (pickerOpen) {
    state.modelPickerSelection.delete(modelId);
    renderModelPickerList();
  }
  renderAllowedModelsChips();
}
