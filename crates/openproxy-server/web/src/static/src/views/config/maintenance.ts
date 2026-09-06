// views/config/maintenance.ts — Database Maintenance card: auto-VACUUM
// settings, usage retention, manual VACUUM trigger, and the vacuum
// status poller.
//
// VACUUM failure handling: the error message includes repair
// instructions if the DB is corrupt. It is shown as a toast and, when
// it mentions "disk I/O" or "integrity", the recover endpoint is
// queried for diagnostics.

import { html, type TemplateResult } from "lit-html";
import { api } from "../../state/api.js";
import { requestUpdate } from "../../state/reactive.js";
import { showToast } from "../../components/toast.js";
import { card, errStr, type VacuumStatus } from "./shared.js";

// ── Maintenance / VACUUM state ──────────────────────────────────────

let liveAutoVacuum = true;
let liveVacuumIntervalHours = 6;
let liveUsageRetentionDays = 7;
let vacuumStatus: VacuumStatus = {
  last_run: null, last_result: null, in_progress: false, next_scheduled: null,
};

/** Fetch `/config/maintenance` and seed the maintenance state. Called
 *  by mountConfig; a missing endpoint keeps the defaults (non-fatal). */
export async function loadMaintenanceState(): Promise<void> {
  try {
    const maint = await api("/config/maintenance") as {
      auto_vacuum?: boolean; vacuum_interval_hours?: number; usage_retention_days?: number;
      vacuum_status?: { last_run?: string | null; last_result?: string | null; in_progress?: boolean; next_scheduled?: string | null };
    };
    liveAutoVacuum = maint.auto_vacuum ?? true;
    liveVacuumIntervalHours = maint.vacuum_interval_hours ?? 6;
    liveUsageRetentionDays = maint.usage_retention_days ?? 7;
    if (maint.vacuum_status) {
      vacuumStatus = {
        last_run: maint.vacuum_status.last_run ?? null,
        last_result: maint.vacuum_status.last_result ?? null,
        in_progress: maint.vacuum_status.in_progress ?? false,
        next_scheduled: maint.vacuum_status.next_scheduled ?? null,
      };
    }
  } catch {
    // Maintenance endpoint not available — keep defaults
  }
}

export async function pollVacuumStatus(): Promise<void> {
  try {
    const data = await api("/config/vacuum-status") as { last_run?: string | null; last_result?: string | null; in_progress?: boolean; next_scheduled?: string | null };
    vacuumStatus = {
      last_run: data.last_run ?? null,
      last_result: data.last_result ?? null,
      in_progress: data.in_progress ?? false,
      next_scheduled: data.next_scheduled ?? null,
    };
    requestUpdate();
  } catch {
    // Non-fatal — the status will update on the next poll.
  }
}

async function patchMaintenance(): Promise<void> {
  try {
    await api("/config/maintenance", {
      method: "PUT",
      body: JSON.stringify({
        auto_vacuum: liveAutoVacuum,
        vacuum_interval_hours: liveVacuumIntervalHours,
        usage_retention_days: liveUsageRetentionDays,
      }),
    });
    showToast("Maintenance config updated", "success");
    requestUpdate();
  } catch (e: unknown) {
    showToast("Error: " + errStr(e), "error");
  }
}

async function triggerVacuum(): Promise<void> {
  if (vacuumStatus.in_progress) return;
  vacuumStatus.in_progress = true;
  requestUpdate();
  try {
    const result = await api("/debug/vacuum", { method: "POST" }) as { vacuumed?: boolean; partial?: boolean; integrity_check?: string; message?: string };
    if (result.partial) {
      showToast("VACUUM partial: " + (result.message || "see details"), "warning");
    } else {
      showToast("VACUUM completed successfully", "success");
    }
  } catch (e: unknown) {
    // VACUUM failed — the error message includes repair instructions
    // if the DB is corrupt. Show it as a toast and also try the
    // recover endpoint for diagnostics.
    const errMsg = errStr(e);
    showToast("VACUUM failed: " + errMsg, "error");
    // If the error mentions "disk I/O" or "integrity", auto-trigger
    // the recover diagnostic so the operator sees the repair instructions.
    if (errMsg.includes("disk I/O") || errMsg.includes("integrity")) {
      try {
        const recovery = await api("/debug/recover", { method: "POST" }) as { instructions?: string; tables?: unknown[]; needs_manual_repair?: boolean };
        if (recovery.needs_manual_repair && recovery.instructions) {
          // Show the repair instructions in a more prominent way —
          // a longer-lived toast with the full instructions.
          showToast("DB needs manual repair. Check console for instructions.", "error");
          console.error("=== DATABASE REPAIR INSTRUCTIONS ===\n" + recovery.instructions + "\n=== END INSTRUCTIONS ===");
        }
      } catch {
        // Recovery endpoint also failed — non-fatal, the operator
        // already has the VACUUM error message.
      }
    }
  } finally {
    await pollVacuumStatus();
  }
}

// ── Card template ───────────────────────────────────────────────────

export function renderMaintenanceCard(): TemplateResult {
  const vacuumBtnLabel = vacuumStatus.in_progress
    ? "⏳ VACUUM in progress…"
    : "🧹 Run VACUUM now";
  const lastRunText = vacuumStatus.last_run
    ? new Date(vacuumStatus.last_run).toLocaleString()
    : "never";
  const lastResultText = vacuumStatus.last_result
    ? (vacuumStatus.last_result === "ok" ? "✅ ok" : "❌ " + vacuumStatus.last_result)
    : "—";
  const nextScheduledText = vacuumStatus.next_scheduled
    ? new Date(vacuumStatus.next_scheduled).toLocaleString()
    : (liveAutoVacuum ? "scheduled (next tick)" : "disabled");
  return card("Database Maintenance", html`
    <div class="config-field">
      <label class="checkbox-label">
        <input type="checkbox" ?checked=${liveAutoVacuum} @change=${(e: Event) => { liveAutoVacuum = (e.target as HTMLInputElement).checked; void patchMaintenance(); }}>
        <span>Automatic VACUUM</span>
      </label>
      <p class="muted">When enabled, the server runs VACUUM every ${liveVacuumIntervalHours}h to compact freed pages. Disable to run VACUUM only manually.</p>
    </div>
    <div class="config-field">
      <label>VACUUM interval (hours)</label>
      <input type="number" inputmode="numeric" min="1" max="168" .value=${String(liveVacuumIntervalHours)} @change=${(e: Event) => { const v = parseInt((e.target as HTMLInputElement).value, 10); if (v >= 1) { liveVacuumIntervalHours = v; void patchMaintenance(); } }}>
    </div>
    <div class="config-field">
      <label>Usage retention (days)</label>
      <input type="number" inputmode="numeric" min="0" max="365" .value=${String(liveUsageRetentionDays)} @change=${(e: Event) => { const v = parseInt((e.target as HTMLInputElement).value, 10); if (v >= 0) { liveUsageRetentionDays = v; void patchMaintenance(); } }}>
      <p class="muted">Rows older than this are deleted hourly. 0 = keep forever (not recommended).</p>
    </div>
    <div class="config-field">
      <button class="primary"
              ?disabled=${vacuumStatus.in_progress}
              @click=${() => void triggerVacuum()}>
        ${vacuumBtnLabel}
      </button>
    </div>
    <div class="config-field">
      <span class="label">Last run:</span> <span class="value">${lastRunText}</span>
      <span class="label" style="margin-left:1rem;">Result:</span> <span class="value">${lastResultText}</span>
    </div>
    <div class="config-field">
      <span class="label">Next scheduled:</span> <span class="value">${nextScheduledText}</span>
    </div>
  `);
}
