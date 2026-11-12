// views/credits/tiers-admin.ts — tier management (admin only).
//
// A tier is the access contract: models + quota caps a leveraged key gets
// when a package tied to it is paid. The editor is a searchable multi-select
// over the server's model catalog; saving replaces the whole config.

import { html, type TemplateResult } from "lit-html";
import { api } from "../../state/api.js";
import { showToast } from "../../components/toast.js";
import {
  catalogModels, ensureCatalog, requestRefresh, tierDraft,
  setTierDraft,
} from "./shared.js";
import type { AdminTier, CreditsData, TierDraft } from "./types.js";

function openTierEditor(tier: AdminTier | null): void {
  setTierDraft(tier == null
    ? { id: null, name: "", models: [], monthly: "", weekly: "", fiveHour: "", active: true }
    : {
        id: tier.id,
        name: tier.name,
        models: tier.config.allowed_models ?? [],
        monthly: tier.config.monthly_usd_cap == null ? "" : String(tier.config.monthly_usd_cap),
        weekly: tier.config.weekly_usd_cap == null ? "" : String(tier.config.weekly_usd_cap),
        fiveHour: tier.config.five_hour_usd_cap == null ? "" : String(tier.config.five_hour_usd_cap),
        active: tier.active,
      });
  requestRefresh();
  void ensureCatalog().then(requestRefresh);
}

function toggleDraftModel(id: string): void {
  if (!tierDraft) return;
  setTierDraft({
    ...tierDraft,
    models: tierDraft.models.includes(id)
      ? tierDraft.models.filter((m) => m !== id)
      : [...tierDraft.models, id],
  });
  requestRefresh();
}

async function saveTier(): Promise<void> {
  if (!tierDraft) return;
  const name = (document.getElementById("tier-name") as HTMLInputElement | null)?.value.trim() ?? tierDraft.name;
  const monthly = parseFloat((document.getElementById("tier-monthly") as HTMLInputElement | null)?.value ?? "");
  const weekly = parseFloat((document.getElementById("tier-weekly") as HTMLInputElement | null)?.value ?? "");
  const fiveHour = parseFloat((document.getElementById("tier-fivehour") as HTMLInputElement | null)?.value ?? "");
  if (!name) {
    showToast("tier name is required", "error");
    return;
  }
  if (tierDraft.models.length === 0) {
    showToast("a tier without models leaves the key unrestricted — pick at least one", "error");
    return;
  }
  const config = {
    monthly_usd_cap: Number.isFinite(monthly) ? monthly : null,
    weekly_usd_cap: Number.isFinite(weekly) ? weekly : null,
    five_hour_usd_cap: Number.isFinite(fiveHour) ? fiveHour : null,
    allowed_models: tierDraft.models,
    allowed_providers: null,
    allowed_combos: null,
    only_whitelisted: true,
  };
  try {
    if (tierDraft.id == null) {
      await api("/credits/tiers", { method: "POST", body: JSON.stringify({ name, config }) });
    } else {
      await api(`/credits/tiers/${tierDraft.id}`, {
        method: "PATCH",
        body: JSON.stringify({ name, config, active: tierDraft.active }),
      });
    }
    showToast("tier saved", "success");
    setTierDraft(null);
    requestRefresh();
  } catch (err) {
    showToast(err instanceof Error ? err.message : "save failed", "error");
  }
}

async function toggleTier(id: number, active: boolean): Promise<void> {
  try {
    await api(`/credits/tiers/${id}`, { method: "PATCH", body: JSON.stringify({ active }) });
    requestRefresh();
  } catch (err) {
    showToast(err instanceof Error ? err.message : "update failed", "error");
  }
}

async function deleteTier(id: number): Promise<void> {
  try {
    await api(`/credits/tiers/${id}`, { method: "DELETE" });
    showToast("tier deleted", "success");
    requestRefresh();
  } catch (err) {
    showToast(err instanceof Error ? err.message : "delete failed — a tier in use cannot be deleted", "error");
  }
}

function renderTierEditor(): TemplateResult {
  const d = tierDraft as TierDraft;
  const shown = (catalogModels ?? []).slice(0, 80);
  return html`
    <div style="border:1px solid var(--accent,#6366f1);border-radius:10px;padding:12px;margin-top:12px;background:var(--surface,#141824)">
      <div style="font-weight:700;margin-bottom:8px">${d.id == null ? "New tier" : `Edit tier #${d.id}`}</div>
      <div style="display:flex;gap:8px;flex-wrap:wrap;align-items:flex-end">
        <div><label style="font-size:11px;color:var(--muted);display:block">Name</label>
          <input id="tier-name" .value=${d.name} placeholder="e.g. Flash" style="max-width:160px"></div>
        <div><label style="font-size:11px;color:var(--muted);display:block">Monthly cap USD</label>
          <input id="tier-monthly" .value=${d.monthly} placeholder="none" style="max-width:100px"></div>
        <div><label style="font-size:11px;color:var(--muted);display:block">Weekly cap USD</label>
          <input id="tier-weekly" .value=${d.weekly} placeholder="none" style="max-width:100px"></div>
        <div><label style="font-size:11px;color:var(--muted);display:block">5h cap USD</label>
          <input id="tier-fivehour" .value=${d.fiveHour} placeholder="none" style="max-width:90px"></div>
      </div>
      <div style="margin-top:10px;font-size:12px;color:var(--muted)">
        Models in this tier — ${d.models.length} selected
      </div>
      <div style="margin-top:6px;display:flex;gap:6px;flex-wrap:wrap">
        ${d.models.length === 0
          ? html`<span style="font-size:11px;color:var(--muted)">none yet</span>`
          : d.models.map((m) => html`
            <span style="font-size:11px;background:var(--border,#232a3d);border-radius:20px;padding:2px 10px">
              ${m} <a style="cursor:pointer;margin-left:4px" @click=${() => toggleDraftModel(m)}>×</a>
            </span>`)}
      </div>
      <input id="tier-search" placeholder="filter the catalog…" style="margin-top:10px;width:100%;max-width:320px"
             @input=${(e: Event) => {
               const q = (e.target as HTMLInputElement).value.toLowerCase();
               document.querySelectorAll<HTMLElement>("#tier-model-list label").forEach((el) => {
                 const s = el.dataset["search"] ?? "";
                 el.style.display = s.includes(q) ? "flex" : "none";
               });
             }}>
      <div id="tier-model-list" style="max-height:190px;overflow:auto;border:1px solid var(--border,#232a3d);border-radius:8px;margin-top:6px;padding:4px">
        ${catalogModels == null
          ? html`<div style="color:var(--muted);font-size:12px;padding:6px">loading catalog…</div>`
          : shown.length === 0
            ? html`<div style="color:var(--muted);font-size:12px;padding:6px">catalog empty</div>`
            : shown.map((m) => html`
              <label data-search=${`${m.id} ${m.provider ?? ""}`.toLowerCase()}
                     style="display:flex;gap:8px;align-items:center;font-size:12px;padding:3px 6px;cursor:pointer">
                <input type="checkbox" ?checked=${d.models.includes(m.id)}
                       @change=${() => toggleDraftModel(m.id)}>
                <span style="font-family:monospace">${m.id}</span>
                <span style="color:var(--muted);font-size:11px">${m.provider ?? ""}</span>
              </label>`)}
      </div>
      <div style="margin-top:10px;display:flex;gap:8px">
        <button class="primary" @click=${() => void saveTier()}>${d.id == null ? "Create tier" : "Save tier"}</button>
        <button class="secondary" @click=${() => { setTierDraft(null); requestRefresh(); }}>Cancel</button>
      </div>
      <div style="font-size:11px;color:var(--muted);margin-top:8px">
        An allowlist forces only_whitelisted on the buyer's key: the leveraged credit can only reach these models.
      </div>
    </div>`;
}

export function renderTiers(data: CreditsData): TemplateResult {
  return html`
    <section class="card" style="margin-top:16px">
      <div class="card-title">Tiers (access contracts: models + quota caps)</div>
      <div class="card-body" style="font-size:13px">
        ${data.adminTiers.length === 0
          ? html`<div style="color:var(--muted)">No tiers yet — create one to gate a package.</div>`
          : html`
        <table style="width:100%;border-collapse:collapse">
          <thead><tr style="color:var(--muted);font-size:11px;text-align:left">
            <th style="padding:4px 8px 4px 0">Name</th><th style="padding:4px 8px">Models</th>
            <th style="padding:4px 8px">Caps (m / w / 5h)</th><th style="padding:4px 8px">Used by</th>
            <th style="padding:4px 8px">Active</th><th></th>
          </tr></thead>
          <tbody>
            ${data.adminTiers.map((t) => html`
            <tr style="border-top:1px solid var(--border,#232a3d)">
              <td style="padding:6px 8px 6px 0;font-weight:600">${t.name}</td>
              <td style="padding:6px 8px;max-width:260px">
                ${t.config.allowed_models == null
                  ? html`<span style="color:var(--warn,#f59e0b)">unrestricted</span>`
                  : html`<span style="color:var(--muted)">${t.config.allowed_models.length} model(s)</span>
                      <div style="font-size:10px;color:var(--muted);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:260px">
                        ${t.config.allowed_models.join(", ")}</div>`}
              </td>
              <td style="padding:6px 8px;color:var(--muted);font-size:12px">
                ${t.config.monthly_usd_cap ?? "∞"} / ${t.config.weekly_usd_cap ?? "∞"} / ${t.config.five_hour_usd_cap ?? "∞"}
              </td>
              <td style="padding:6px 8px">${t.package_count} pkg</td>
              <td style="padding:6px 8px">${t.active ? "yes" : "no"}</td>
              <td style="padding:6px 0 6px 8px;white-space:nowrap">
                <button class="secondary" style="padding:2px 8px" @click=${() => openTierEditor(t)}>edit</button>
                <button class="secondary" style="padding:2px 8px" @click=${() => void toggleTier(t.id, !t.active)}>${t.active ? "disable" : "enable"}</button>
                <button class="secondary" style="padding:2px 8px" @click=${() => void deleteTier(t.id)}>delete</button>
              </td>
            </tr>`)}
          </tbody>
        </table>`}
        ${tierDraft == null
          ? html`<div style="margin-top:12px">
              <button class="primary" @click=${() => openTierEditor(null)}>New tier</button>
            </div>`
          : renderTierEditor()}
      </div>
    </section>`;
}
