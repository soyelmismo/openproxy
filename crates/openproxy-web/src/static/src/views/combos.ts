// views/combos.ts — combo grid + combo detail (with target table).
//
// Per spec §3 + §13.8 we do not use inline `onclick="window.X()"`
// handlers. Buttons carry `data-action="X" data-arg-N="..."` and
// the document-level shim in app.js dispatches them. Checkboxes
// also use `data-action` with the new event being "change".

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml } from "../lib/escape.js";
import { pageHeader } from "../components/page-header.js";
import { card } from "../components/card.js";
import { statusPillClass } from "../lib/constants.js";
import { initDragAndDrop } from "../handlers/combo-target-handlers.js";
import type { Combo, ComboTargetWithModel } from "../lib/types/api.js";

// Shape of one row in the "Test all" results table. The endpoint
// hands back an array of these; the exact field set is admin-only
// (crates/openproxy-core/src/admin.rs) and the dashboard just
// renders it. The most important discriminator is `sub_combo_id`
// (a sub-combo target vs a flat target); the rest is timing /
// status metadata.
interface ComboTestResult {
  target_id: number;
  sub_combo_id: number | null;
  sub_combo_name: string | null;
  model_id?: string | null;
  model_display_name?: string | null;
  model_row_id?: number | null;
  provider_id: string | null;
  status: number;
  skipped: boolean;
  skip_reason: string | null;
  error_msg: string | null;
  elapsed_ms: number | null;
}

interface MountCombosOpts {
  detailId?: number;
}

let main: HTMLElement | null = null;

function renderComboTestResults(results: ComboTestResult[]): string {
  if (!Array.isArray(results) || results.length === 0) {
    return card("Test all — results", `<p class="empty">No targets to test.</p>`, { variant: "detail" });
  }
  const rows = results.map((r) => {
    const isSubCombo = r.sub_combo_id != null;
    const targetLabel = isSubCombo
      ? `<span class="chip combo-chip">→ combo: ${escapeHtml(r.sub_combo_name || ("#" + r.sub_combo_id))}</span>`
      : escapeHtml(r.model_display_name || r.model_id || `row #${r.model_row_id}`);
    const providerLabel = r.provider_id ? escapeHtml(r.provider_id) : "—";
    const statusClass = r.skipped ? "warn" : statusPillClass(r.status);
    const statusText = r.skipped ? "skipped" : String(r.status);
    const detail = r.skipped ? (r.skip_reason || r.error_msg || "skipped") : (r.error_msg || "");
    const detailHtml = detail ? `<small>${escapeHtml(detail)}</small>` : "";
    const elapsed = (r.elapsed_ms != null && r.elapsed_ms > 0) ? `${r.elapsed_ms} ms` : "—";
    return `<tr>
      <td>#${r.target_id}</td>
      <td>${providerLabel}</td>
      <td>${targetLabel}</td>
      <td><span class="status-pill ${statusClass}">${statusText}</span></td>
      <td>${elapsed}</td>
      <td>${detailHtml}</td>
    </tr>`;
  }).join("");
  return card("Test all — results (" + results.length + ")", `
    <table>
      <thead><tr><th>Target</th><th>Provider</th><th>Model / Sub-combo</th><th>Status</th><th>Latency</th><th>Detail</th></tr></thead>
      <tbody>${rows}</tbody>
    </table>
  `, { variant: "detail" });
}

async function renderComboDetail(comboId: number): Promise<void> {
  if (state.selectedTargetsCombo !== comboId) {
    state.selectedTargets.clear();
    state.selectedTargetsCombo = comboId;
  }
  const [combo, targets] = await Promise.all([
    api("/combos/" + comboId).catch(() => null) as Promise<Combo | null>,
    api("/combos/" + comboId + "/targets") as Promise<ComboTargetWithModel[]>,
  ]);
  if (!combo) {
    const el = document.getElementById("main");
    if (el) el.innerHTML = `<div class="banner banner-error">Combo ${comboId} not found. <a href="#/combos">← Back</a></div>`;
    return;
  }
  const cooldowns = targets.filter((t) => t.in_cooldown);
  const cooldownBanner = cooldowns.length === 0 ? "" :
    `<div class="cooldown-banner">⏸ ${cooldowns.length} of ${targets.length} target(s) in cooldown — engine will skip them for now.</div>`;
  const bulkBar = state.selectedTargets.size > 0 ? `
    <div class="bulk-actions-bar">
      <span><strong>${state.selectedTargets.size}</strong> selected</span>
      <button class="danger" data-action="bulkDeleteSelectedTargets" data-arg1="${comboId}">Delete selected</button>
      <button class="link" data-action="clearTargetSelection">Clear selection</button>
    </div>
  ` : "";
  // Compute the auto context window (min across targets with known
  // context_length) so the operator can see what the auto-compute
  // would produce, and compare it with the override.
  const knownContexts = targets
    .map((t) => t.context_length)
    .filter((c): c is number => c != null && c > 0);
  const autoCw = knownContexts.length > 0 ? Math.min(...knownContexts) : null;
  const autoCwLabel = autoCw != null ? formatTokens(autoCw) : "—";
  const overrideCw = combo.context_window ?? null;
  const overrideCwLabel = overrideCw != null ? formatTokens(overrideCw) : "";
  const effectiveCw = overrideCw ?? autoCw;
  const effectiveCwLabel = effectiveCw != null ? formatTokens(effectiveCw) : "—";

  const headerActions = `
    <button data-action="testAllTargets" data-arg1="${comboId}">🧪 Test all</button>
    <button class="danger" data-action="deleteCombo" data-arg1="${comboId}">Delete</button>
  `;
  const header = `
    <div class="page-header">
      <a href="#/combos" class="back-link">← All combos</a>
      <h2>${escapeHtml(combo.name)}</h2>
      <div class="actions">
        <span class="chip">${escapeHtml(combo.strategy)}</span>
        <label>Race size: <input type="number" min="1" max="8" value="${combo.race_size}" data-action="updateRaceSize" data-arg1="${comboId}" class="race-input"></label>
        ${headerActions}
      </div>
    </div>
    <div class="combo-context-window-bar">
      <label>Context window:
        <input type="number" min="1" placeholder="auto (${escapeHtml(autoCwLabel)})"
               value="${overrideCwLabel}"
               data-action="updateContextWindow" data-arg1="${comboId}"
               class="cw-input" title="Override del context window (tokens). Vacío = auto-compute (min de targets).">
      </label>
      <span class="cw-hint">
        Auto: <strong>${escapeHtml(autoCwLabel)}</strong> ·
        Effective: <strong>${escapeHtml(effectiveCwLabel)}</strong>
      </span>
    </div>
  `;
  let body = "";
  if (state.comboTestResults[comboId]) body += renderComboTestResults(state.comboTestResults[comboId] as ComboTestResult[]);
  body += `
    <section class="detail-section">
      <div class="section-header">
        <h3>Targets (${targets.length})</h3>
        <button class="primary" data-action="showAddTarget" data-arg1="${comboId}">+ Add target</button>
      </div>
      ${cooldownBanner}
      ${bulkBar}
  `;
  if (targets.length === 0) {
    body += `<p class="empty">No targets. Add a target to start routing.</p>`;
  } else {
    body += `<table>
      <thead><tr><th></th><th><input type="checkbox" id="target-select-all" data-action="toggleSelectAllTargets"></th><th>#</th><th>Provider</th><th>Account</th><th>Model</th><th>Context</th><th>Actions</th></tr></thead>
      <tbody id="targets-tbody">`;
    for (const t of [...targets].sort((a, b) => a.priority_order - b.priority_order)) {
      const isSubCombo = t.sub_combo_id != null;
      let cooldownBadge = "";
      if (t.in_cooldown) {
        const until = t.cooldown_until ? ` until ${escapeHtml(t.cooldown_until)}` : "";
        const reason = t.cooldown_reason ? ` — ${escapeHtml(t.cooldown_reason)}` : "";
        const title = `Cooldown${reason}${until}`;
        cooldownBadge = ` <span class="badge badge-cooldown" title="${escapeHtml(title)}">⏸ cooldown</span>`;
      }
      const resetCooldownBtn = (t.in_cooldown && !isSubCombo)
        ? `<button class="small" title="Force-clear the cooldown for this target" data-action="resetCooldown" data-arg1="${comboId}" data-arg2="${t.id}">🔄</button>`
        : "";
      const modelCell = isSubCombo
        ? `<span class="chip combo-chip">→ combo: ${escapeHtml(t.sub_combo_name || ("#" + t.sub_combo_id))}</span>`
        : escapeHtml(t.model_display_name || t.model_id || `row #${t.model_row_id}`) + cooldownBadge;
      const providerCell = isSubCombo
        ? `<span class="virtual-provider">${escapeHtml(t.provider_id)}</span>`
        : `<a href="#/providers/${encodeURIComponent(t.provider_id)}">${escapeHtml(t.provider_id)}</a>`;
      const accountCell = isSubCombo
        ? "<em>n/a</em>"
        : (t.account_id ? "#" + t.account_id : "<em>rotate</em>");
      const contextCell = isSubCombo
        ? "<em>sub-combo</em>"
        : (t.context_length != null ? `<span title="${escapeHtml(String(t.context_length))}">${escapeHtml(formatTokens(t.context_length))}</span>` : "—");
      const isSelected = state.selectedTargets.has(t.id);
      body += `
        <tr draggable="true" data-drag-id="${t.id}" data-combo-id="${comboId}" class="${isSelected ? "selected" : ""}">
          <td class="drag-handle" title="Drag to reorder">⠿</td>
          <td><input type="checkbox" ${isSelected ? "checked" : ""} data-target-id="${t.id}" data-action="toggleTargetSelection" data-arg1="${t.id}"></td>
          <td>${t.priority_order}</td>
          <td>${providerCell}</td>
          <td>${accountCell}</td>
          <td>${modelCell}</td>
          <td>${contextCell}</td>
          <td>
            <button class="small" data-action="changePriority" data-arg1="${comboId}" data-arg2="${t.id}" data-arg3="-1">↑</button>
            <button class="small" data-action="changePriority" data-arg1="${comboId}" data-arg2="${t.id}" data-arg3="1">↓</button>
            ${resetCooldownBtn}
            <button class="small danger" data-action="deleteTarget" data-arg1="${comboId}" data-arg2="${t.id}">×</button>
          </td>
        </tr>
      `;
    }
    body += `</tbody></table>`;
  }
  body += `</section>`;
  const el = document.getElementById("main");
  if (el) el.innerHTML = header + body;
  queueMicrotask(() => {
    initDragAndDrop();
    const master = document.getElementById("target-select-all") as HTMLInputElement | null;
    if (!master) return;
    const visibleIds = targets.map((t) => t.id);
    if (visibleIds.length === 0) { master.checked = false; master.indeterminate = false; return; }
    const selectedVisible = visibleIds.filter((id) => state.selectedTargets.has(id)).length;
    if (selectedVisible === 0) { master.checked = false; master.indeterminate = false; }
    else if (selectedVisible === visibleIds.length) { master.checked = true; master.indeterminate = false; }
    else { master.checked = false; master.indeterminate = true; }
  });
}

function renderComboGrid(): void {
  const list = state.combos || [];
  const cards = list.map((c) =>
    `<a class="combo-card" href="#/combos/${c.id}">
      <h3>${escapeHtml(c.name)}</h3>
      <div class="provider-meta"><span class="chip">${escapeHtml(c.strategy)}</span> · race ${c.race_size}</div>
    </a>`).join("");
  const grid = cards || `<p class="empty">No combos yet. Create one to start routing.</p>`;
  if (main) main.innerHTML = pageHeader({
    title: "Combos",
    actions: `<button class="primary" data-action="showCreateCombo">+ Create combo</button>`,
  }) + `<div class="combo-grid">${grid}</div>`;
}

export async function mountCombos(opts: MountCombosOpts = {}): Promise<void> {
  main = document.getElementById("main");
  if (!main) return;
  if (opts && opts.detailId) {
    return renderComboDetail(opts.detailId);
  }
  main.innerHTML = pageHeader({ title: "Combos" }) + `<div class="loading">Loading...</div>`;
  try {
    state.combos = await api("/combos") as Combo[];
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    main.innerHTML = pageHeader({ title: "Combos" }) +
      `<div class="banner banner-error">${escapeHtml(msg)}</div>`;
    return;
  }
  renderComboGrid();
}

/** Format a token count as a human-readable string (e.g. 128000 → "128k"). */
function formatTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1).replace(/\.0$/, "") + "M";
  if (n >= 1_000) return (n / 1_000).toFixed(0) + "k";
  return String(n);
}
