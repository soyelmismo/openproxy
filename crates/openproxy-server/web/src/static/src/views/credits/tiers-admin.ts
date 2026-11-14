// views/credits/tiers-admin.ts — tier management (admin only).
//
// A tier is the access contract: models + quota caps a leveraged key gets
// when the plan tied to it is bought. It is also the SINGLE SOURCE OF TRUTH:
// editing a tier re-shapes every key linked to it (backend propagation).
//
// The editor is a modal with a searchable, paged multi-select over the full
// admin catalog (every model + every combo, active or not). All interactions
// re-render the modal body locally — no view refetch per keystroke.

import { html, type TemplateResult } from "lit-html";
import { api } from "../../state/api.js";
import { showToast } from "../../components/toast.js";
import {
  catalogModels, ensureCatalog, requestRefresh, tierDraft,
  setTierDraft, tierSearchQuery, setTierSearchQuery,
  tierCatalogPage, setTierCatalogPage, resetTierCatalogBrowser,
} from "./shared.js";
import { openModal, refocusInput, type ModalHandle } from "./modal.js";
import type { AdminTier, CatalogModel, CreditsData, TierDraft } from "./types.js";

const PAGE_SIZE = 60;
let kindFilter: "all" | "model" | "combo" = "all";
let handle: ModalHandle | null = null;

/** Display id: combos lose their `combo:` prefix (the badge already says it). */
const disp = (id: string): string => (id.startsWith("combo:") ? id.slice(6) : id);

function syncScalars(): void {
  if (!tierDraft) return;
  const name = (document.getElementById("tier-name") as HTMLInputElement | null)?.value ?? tierDraft.name;
  const weekly = (document.getElementById("tier-weekly") as HTMLInputElement | null)?.value ?? tierDraft.weekly;
  const fiveHour = (document.getElementById("tier-fivehour") as HTMLInputElement | null)?.value ?? tierDraft.fiveHour;
  const price = (document.getElementById("tier-price") as HTMLInputElement | null)?.value ?? tierDraft.price;
  const credit = (document.getElementById("tier-credit") as HTMLInputElement | null)?.value ?? tierDraft.credit;
  const ttlDays = (document.getElementById("tier-ttl") as HTMLInputElement | null)?.value ?? tierDraft.ttlDays;
  const stock = (document.getElementById("tier-stock") as HTMLInputElement | null)?.value ?? tierDraft.stock;
  setTierDraft({ ...tierDraft, name, weekly, fiveHour, price, credit, ttlDays, stock });
}

function filtered(): CatalogModel[] {
  const q = tierSearchQuery.trim().toLowerCase();
  return (catalogModels ?? []).filter((m) => {
    if (kindFilter !== "all" && m.kind !== kindFilter) return false;
    if (!q) return true;
    return `${m.id} ${m.name ?? ""} ${m.provider ?? ""} ${m.kind}`.toLowerCase().includes(q);
  });
}

function repaint(): void {
  syncScalars();
  handle?.repaint(body());
}

function toggleDraftModel(id: string): void {
  if (!tierDraft) return;
  const has = tierDraft.models.includes(id);
  setTierDraft({
    ...tierDraft,
    models: has ? tierDraft.models.filter((m) => m !== id) : [...tierDraft.models, id],
  });
  repaint();
}

function selectPage(ids: string[]): void {
  if (!tierDraft) return;
  setTierDraft({ ...tierDraft, models: [...new Set([...tierDraft.models, ...ids])] });
  repaint();
}

function clearSelection(): void {
  if (!tierDraft) return;
  setTierDraft({ ...tierDraft, models: [] });
  repaint();
}

async function saveTier(): Promise<void> {
  if (!tierDraft) return;
  syncScalars();
  const d = tierDraft;
  const name = d.name.trim();
  const weekly = parseFloat(d.weekly);
  const fiveHour = parseFloat(d.fiveHour);
  const price = parseFloat(d.price);
  const credit = parseFloat(d.credit);
  const ttlDays = parseInt(d.ttlDays, 10);
  if (!name) {
    showToast("tier name is required", "error");
    return;
  }
  if (d.models.length === 0) {
    showToast("a tier without models leaves the key unrestricted — pick at least one", "error");
    return;
  }
  if (d.price.trim() !== "" && (!Number.isFinite(price) || price <= 0)) {
    showToast("price must be a positive number (blank = draft plan)", "error");
    return;
  }
  if (d.credit.trim() !== "" && (!Number.isFinite(credit) || credit <= 0)) {
    showToast("credit must be a positive number (blank = 1:1 with the price)", "error");
    return;
  }
  if (!Number.isFinite(ttlDays) || ttlDays < 1) {
    showToast("TTL must be at least 1 day", "error");
    return;
  }
  const stock = d.stock.trim() === "" ? 0 : parseInt(d.stock, 10);
  if (!Number.isInteger(stock) || stock < 0) {
    showToast("stock must be 0 (unlimited) or a positive seat count", "error");
    return;
  }
  // Split the merged selection: catalog combo ids are always `combo:<name>`
  // (models never carry the prefix), so the tier API gets clean fields.
  const comboNames = d.models
    .filter((m) => m.startsWith("combo:"))
    .map((m) => m.slice("combo:".length));
  const models = d.models.filter((m) => !m.startsWith("combo:"));
  const config = {
    weekly_usd_cap: Number.isFinite(weekly) ? weekly : null,
    five_hour_usd_cap: Number.isFinite(fiveHour) ? fiveHour : null,
    allowed_models: models,
    allowed_providers: null,
    allowed_combos: comboNames.length > 0 ? comboNames : null,
    only_whitelisted: true,
  };
  const plan = {
    price_usd: Number.isFinite(price) && price > 0 ? price : null,
    credit_usd: Number.isFinite(credit) && credit > 0 ? credit : null,
    ttl_days: ttlDays,
    stock,
  };
  try {
    if (d.id == null) {
      await api("/credits/tiers", { method: "POST", body: JSON.stringify({ name, config, ...plan }) });
      showToast("plan created", "success");
    } else {
      await api(`/credits/tiers/${d.id}`, {
        method: "PATCH",
        body: JSON.stringify({ name, config, active: d.active, ...plan }),
      });
      showToast("plan saved — linked keys re-shaped", "success");
    }
    handle?.close();
    setTierDraft(null);
    requestRefresh();
  } catch (err) {
    showToast(err instanceof Error ? err.message : "save failed", "error");
  }
}

function closeEditor(): void {
  handle?.close();
  setTierDraft(null);
  requestRefresh();
}

function body(): TemplateResult {
  const d = tierDraft as TierDraft;
  const rows = filtered();
  const pages = Math.max(1, Math.ceil(rows.length / PAGE_SIZE));
  const page = Math.min(tierCatalogPage, pages - 1);
  const shown = rows.slice(page * PAGE_SIZE, page * PAGE_SIZE + PAGE_SIZE);
  const selected = new Set(d.models);
  const pageIds = shown.map((m) => m.id);
  const allPageSelected = pageIds.length > 0 && pageIds.every((id) => selected.has(id));
  return html`
    <div style="display:flex;gap:8px;flex-wrap:wrap;align-items:flex-end">
      <div><label style="font-size:11px;color:var(--color-text-muted);display:block">Name</label>
        <input id="tier-name" .value=${d.name} placeholder="e.g. Flash" style="max-width:170px"
               @input=${(e: Event) => { if (tierDraft) setTierDraft({ ...tierDraft, name: (e.target as HTMLInputElement).value }); }}></div>
      <div><label style="font-size:11px;color:var(--color-text-muted);display:block">Price USD</label>
        <input id="tier-price" .value=${d.price} placeholder="draft" style="max-width:90px"
               @input=${(e: Event) => { if (tierDraft) setTierDraft({ ...tierDraft, price: (e.target as HTMLInputElement).value }); }}></div>
      <div><label style="font-size:11px;color:var(--color-text-muted);display:block">Credit USD</label>
        <input id="tier-credit" .value=${d.credit} placeholder="1:1" style="max-width:90px"
               @input=${(e: Event) => { if (tierDraft) setTierDraft({ ...tierDraft, credit: (e.target as HTMLInputElement).value }); }}></div>
      <div><label style="font-size:11px;color:var(--color-text-muted);display:block">TTL days</label>
        <input id="tier-ttl" .value=${d.ttlDays} placeholder="30" style="max-width:70px"
               @input=${(e: Event) => { if (tierDraft) setTierDraft({ ...tierDraft, ttlDays: (e.target as HTMLInputElement).value }); }}></div>
      <div><label style="font-size:11px;color:var(--color-text-muted);display:block">Seats</label>
        <input id="tier-stock" .value=${d.stock} placeholder="∞" style="max-width:70px"
               @input=${(e: Event) => { if (tierDraft) setTierDraft({ ...tierDraft, stock: (e.target as HTMLInputElement).value }); }}></div>
      <div><label style="font-size:11px;color:var(--color-text-muted);display:block">Weekly cap USD</label>
        <input id="tier-weekly" .value=${d.weekly} placeholder="none" style="max-width:100px"
               @input=${(e: Event) => { if (tierDraft) setTierDraft({ ...tierDraft, weekly: (e.target as HTMLInputElement).value }); }}></div>
      <div><label style="font-size:11px;color:var(--color-text-muted);display:block">5h cap USD</label>
        <input id="tier-fivehour" .value=${d.fiveHour} placeholder="none" style="max-width:90px"
               @input=${(e: Event) => { if (tierDraft) setTierDraft({ ...tierDraft, fiveHour: (e.target as HTMLInputElement).value }); }}></div>
    </div>

    <div style="margin-top:12px;font-size:12px;color:var(--color-text-muted)">
      Models in this tier — <b>${d.models.length}</b> selected
      ${d.models.length > 0 ? html` · <a style="cursor:pointer" @click=${() => clearSelection()}>clear all</a>` : null}
    </div>
    <div style="margin-top:6px;display:flex;gap:6px;flex-wrap:wrap;max-height:90px;overflow:auto">
      ${d.models.length === 0
        ? html`<span style="font-size:11px;color:var(--color-text-muted)">none yet</span>`
        : d.models.map((m) => html`
          <span style="font-size:11px;background:var(--color-border);border-radius:20px;padding:2px 10px;white-space:nowrap">
            ${disp(m)} <a style="cursor:pointer;margin-left:4px" @click=${() => toggleDraftModel(m)}>×</a>
          </span>`)}
    </div>

    <div style="margin-top:12px;display:flex;gap:6px;flex-wrap:wrap;align-items:center">
      <input id="tier-search" .value=${tierSearchQuery} placeholder="search ${catalogModels == null ? "catalog…" : `${(catalogModels ?? []).length} entries…`}"
             style="flex:1;min-width:200px"
             @input=${(e: Event) => {
               setTierSearchQuery((e.target as HTMLInputElement).value);
               setTierCatalogPage(0);
               repaint();
               refocusInput("tier-search");
             }}>
      <select style="max-width:120px" @change=${(e: Event) => {
        kindFilter = (e.target as HTMLSelectElement).value as typeof kindFilter;
        setTierCatalogPage(0);
        repaint();
      }}>
        <option value="all" ?selected=${kindFilter === "all"}>all kinds</option>
        <option value="model" ?selected=${kindFilter === "model"}>models</option>
        <option value="combo" ?selected=${kindFilter === "combo"}>combos</option>
      </select>
      <button class="secondary" style="padding:2px 10px"
              @click=${() => (allPageSelected ? pageIds.forEach((id) => toggleDraftModel(id)) : selectPage(pageIds))}>
        ${allPageSelected ? "unselect page" : `select page (${pageIds.length})`}
      </button>
    </div>

    <div style="margin-top:6px;border:1px solid var(--color-border);border-radius:8px;max-height:280px;overflow:auto">
      ${catalogModels == null
        ? html`<div style="color:var(--color-text-muted);font-size:12px;padding:8px">loading catalog…</div>`
        : shown.length === 0
          ? html`<div style="color:var(--color-text-muted);font-size:12px;padding:8px">no entries match</div>`
          : shown.map((m) => html`
            <label style="display:flex;gap:8px;align-items:center;font-size:12px;padding:3px 8px;cursor:pointer;${m.active ? "" : "opacity:.55"}">
              <input type="checkbox" ?checked=${selected.has(m.id)} @change=${() => toggleDraftModel(m.id)}>
              <span style="font-family:monospace">${disp(m.id)}</span>
              ${m.kind === "combo"
                ? html`<span style="font-size:10px;border:1px solid var(--color-primary);border-radius:10px;padding:0 6px;color:var(--color-primary)">combo</span>`
                : null}
              ${m.active ? null : html`<span style="font-size:10px;color:var(--color-warn)">inactive</span>`}
              <span style="color:var(--color-text-muted);font-size:11px;margin-left:auto">
                ${m.kind === "combo" ? (m.name ?? "") : `${m.provider ?? ""}${(m.providers ?? 1) > 1 ? ` +${(m.providers ?? 1) - 1}` : ""}`}
              </span>
            </label>`)}
    </div>

    <div style="display:flex;justify-content:space-between;align-items:center;margin-top:6px;font-size:11px;color:var(--color-text-muted)">
      <span>${rows.length === 0 ? "0 entries" : `${page * PAGE_SIZE + 1}–${Math.min(rows.length, (page + 1) * PAGE_SIZE)} of ${rows.length}`}</span>
      <span style="display:flex;gap:6px">
        <button class="secondary" style="padding:1px 8px" ?disabled=${page === 0}
                @click=${() => { setTierCatalogPage(page - 1); repaint(); }}>prev</button>
        <button class="secondary" style="padding:1px 8px" ?disabled=${page >= pages - 1}
                @click=${() => { setTierCatalogPage(page + 1); repaint(); }}>next</button>
      </span>
    </div>

    <div style="margin-top:14px;display:flex;gap:8px">
      <button class="primary" @click=${() => void saveTier()}>${d.id == null ? "Create tier" : "Save tier"}</button>
      <button class="secondary" @click=${() => closeEditor()}>Cancel</button>
    </div>
    <div style="font-size:11px;color:var(--color-text-muted);margin-top:8px">
      A plan is the product: price, credit (leverage), TTL days. Seats (000130) caps how many keys the plan can sell: 0 or blank = unlimited; a sold-out plan stops selling at the checkout. Blank price = draft (not for sale).
      Saving an existing tier re-shapes every linked key immediately.
    </div>`;
}

function openTierEditor(tier: AdminTier | null): void {
  setTierDraft(tier == null
    ? { id: null, name: "", models: [], weekly: "", fiveHour: "", active: true, price: "", credit: "", ttlDays: "30", stock: "" }
    : {
        id: tier.id,
        name: tier.name,
        models: [
          ...(tier.config.allowed_models ?? []),
          ...(tier.config.allowed_combos ?? []).map((c) => (c.startsWith("combo:") ? c : `combo:${c}`)),
        ],
        weekly: tier.config.weekly_usd_cap == null ? "" : String(tier.config.weekly_usd_cap),
        fiveHour: tier.config.five_hour_usd_cap == null ? "" : String(tier.config.five_hour_usd_cap),
        active: tier.active,
        price: tier.price_usd == null ? "" : String(tier.price_usd),
        credit: tier.credit_usd == null ? "" : String(tier.credit_usd),
        ttlDays: String(tier.ttl_days ?? 30),
        stock: tier.stock > 0 ? String(tier.stock) : "",
      });
  resetTierCatalogBrowser();
  kindFilter = "all";
  handle = openModal(tier == null ? "New tier" : `Edit tier — ${tier.name}`, body());
  void ensureCatalog().then(() => repaint());
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

export function renderTiers(data: CreditsData): TemplateResult {
  return html`
    <section class="card" style="margin-top:16px">
      <div class="card-title">Tiers (access contracts: models + quota caps)</div>
      <div class="card-body" style="font-size:13px">
        ${data.adminTiers.length === 0
          ? html`<div style="color:var(--color-text-muted)">No plans yet — create one to sell it.</div>`
          : html`
        <div style="overflow-x:auto">
        <table style="width:100%;border-collapse:collapse">
          <thead><tr style="color:var(--color-text-muted);font-size:11px;text-align:left">
            <th style="padding:4px 8px 4px 0">Name</th><th style="padding:4px 8px">Models</th>
            <th style="padding:4px 8px">Caps (w / 5h)</th><th style="padding:4px 8px">Inherited by</th>
            <th style="padding:4px 8px">Active</th><th></th>
          </tr></thead>
          <tbody>
            ${data.adminTiers.map((t) => html`
            <tr style="border-top:1px solid var(--color-border)">
              <td style="padding:6px 8px 6px 0;font-weight:600">${t.name}</td>
              <td style="padding:6px 8px;max-width:300px">
                ${t.config.allowed_models == null && t.config.allowed_combos == null
                  ? html`<span style="color:var(--color-warn)">unrestricted</span>`
                  : html`<span style="color:var(--color-text-muted)">${t.config.allowed_models?.length ?? 0} model(s) · ${t.config.allowed_combos?.length ?? 0} combo(s)</span>
                      <div style="font-size:10px;color:var(--color-text-muted);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:300px">
                        ${(t.config.allowed_models ?? []).map(disp).join(", ")}</div>`}
              </td>
              <td style="padding:6px 8px;color:var(--color-text-muted);font-size:12px">
                ${t.config.weekly_usd_cap ?? "∞"} / ${t.config.five_hour_usd_cap ?? "∞"}
              </td>
              <td style="padding:6px 8px">${t.price_usd != null ? `$${t.price_usd}` : "draft"} · ${t.key_count} keys${t.stock > 0 ? ` · ${t.key_count}/${t.stock} seats` : ""}</td>
              <td style="padding:6px 8px">${t.active ? "yes" : "no"}</td>
              <td style="padding:6px 0 6px 8px;white-space:nowrap">
                <button class="secondary" style="padding:2px 8px" @click=${() => openTierEditor(t)}>edit</button>
                <button class="secondary" style="padding:2px 8px" @click=${() => void toggleTier(t.id, !t.active)}>${t.active ? "disable" : "enable"}</button>
                <button class="secondary" style="padding:2px 8px" @click=${() => void deleteTier(t.id)}>delete</button>
              </td>
            </tr>`)}
          </tbody>
        </table>
        </div>`}
        <div style="margin-top:12px">
          <button class="primary" @click=${() => openTierEditor(null)}>New tier</button>
        </div>
      </div>
    </section>`;
}
