import { html, type TemplateResult } from 'lit-html';
import type { Account } from "../lib/types/api.js";
import { showCodexResetsModal } from "../components/codex-resets-modal.js";
import { renderQuotaPools } from "./quota-pools.js";
import { icons } from "../lib/icons.js";

// Format reset time as a short exact relative hint.
const resetHint = (ts: string | null | undefined): string => {
  const formatted = formatResetTime(ts);
  return formatted ? ` · ${formatted}` : "";
};

const formatResetTime = (ts: string | null | undefined): string | null => {
  if (!ts) return null;
  try {
    let d: Date;
    const num = Number(ts);
    if (!isNaN(num) && num > 0) {
      d = new Date(num > 1e11 ? num : num * 1000);
    } else {
      d = new Date(ts);
    }
    if (isNaN(d.getTime())) return null;
    const now = Date.now();
    const diffMs = d.getTime() - now;
    if (diffMs <= 0) return "resets soon";

    const diffHrs = Math.floor(diffMs / (1000 * 60 * 60));
    const diffMins = Math.floor((diffMs % (1000 * 60 * 60)) / (1000 * 60));

    if (diffHrs >= 24) {
      const diffDays = Math.floor(diffHrs / 24);
      const remainingHrs = diffHrs % 24;
      return `resets in ${diffDays}d ${remainingHrs}h`;
    }
    return `resets in ${diffHrs}h ${diffMins}m`;
  } catch { return null; }
};

function getQuotaColor(pct: number | null): string {
  if (pct == null) return "unknown";
  if (pct > 80) return "danger";
  if (pct > 50) return "warn";
  return "ok";
}

function getQuotaText(used: number | null, limit: number | null): string {
  if (used == null) return "—";
  if (limit === 100) return `${used}% used`;
  return `${used} / ${limit ?? "—"}`;
}

interface ProviderSpecificMeta {
  credit_balance?: string | number;
  last_checkin_date?: string;
  streak_days?: number;
  reset_credits?: number;
  banked_resets?: number;
  /** Presence of an AutoClaw credential, not its value. */
  autoclaw_access_token?: string | null;
}

function parseProviderSpecific(raw: string | null | undefined): ProviderSpecificMeta | null {
  if (!raw) return null;
  try {
    return JSON.parse(raw) as ProviderSpecificMeta;
  } catch {
    return null;
  }
}

function formatCredits(val: string | number): string {
  const num = typeof val === "number" ? val : parseFloat(val);
  if (isNaN(num)) return String(val);
  return new Intl.NumberFormat().format(num);
}

function renderMetaBadges(meta: ProviderSpecificMeta | null, account?: Account): TemplateResult | null {
  const badges: TemplateResult[] = [];

  if (meta) {
    const resetCredits = meta.reset_credits ?? meta.banked_resets;
    if (resetCredits != null && resetCredits > 0) {
      const isCodex = account?.provider_id === 'codex';
      badges.push(
        html`<span
          class="quota-tag credits ${isCodex ? 'clickable' : ''}"
          role="${isCodex ? 'button' : 'status'}"
          tabindex="${isCodex ? '0' : '-1'}"
          title="${isCodex ? 'Click to inspect & redeem Codex banked rate limit resets' : 'Available Banked Rate Limit Resets'}"
          @click=${isCodex && account ? () => showCodexResetsModal(account.id, account.label || account.email || undefined) : null}
          @keydown=${isCodex && account ? (e: KeyboardEvent) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); showCodexResetsModal(account.id, account.label || account.email || undefined); } } : null}
        >${icons.lightning()} <span>${resetCredits} Banked Reset${resetCredits > 1 ? "s" : ""}</span></span>`
      );
    }

    if (meta.credit_balance != null && meta.credit_balance !== "") {
      const formatted = formatCredits(meta.credit_balance);
      badges.push(
        html`<span class="quota-tag credits" title="Available Credits">${icons.tag()} <span>${formatted} credits</span></span>`
      );
    }

    // The check-in badge is driven by capability, not by stored data: an
    // AutoClaw account that has never recorded a check-in still has one
    // pending, so keying this off `last_checkin_date` alone hid the badge
    // entirely on fresh accounts.
    const supportsCheckin =
      account?.provider_id === "zai" &&
      (meta?.autoclaw_access_token != null ||
        meta?.last_checkin_date != null ||
        meta?.streak_days != null);
    if (supportsCheckin) {
      const todayStr = new Date().toISOString().slice(0, 10);
      const isToday = meta?.last_checkin_date === todayStr;
      const streak = meta?.streak_days ?? 0;
      if (isToday) {
        badges.push(
          html`<span class="quota-tag checkin-done" title="Daily check-in completed today">${icons.check()} <span>Checked in${streak > 0 ? ` (${streak}d streak)` : ""}</span></span>`
        );
      } else {
        badges.push(
          html`<span class="quota-tag checkin-pending" title="Daily check-in pending for today"><span>Check-in pending${streak > 0 ? ` (${streak}d streak)` : ""}</span></span>`
        );
      }
    }
  }

  if (account?.provider_id === 'codex' && account.quota_session_used != null) {
    const hasActiveReserve = account.quota_model_details?.some(
      (d) => d.model_id.toLowerCase().includes('reserve')
    );
    if (!hasActiveReserve) {
      badges.push(
        html`<span
          class="quota-tag reserve"
          title="gpt-reserve es la cuota suplementaria de OpenAI. Se activa automáticamente cuando se agota la ventana principal de 5h o semanal."
        >${icons.shield()} <span>gpt-reserve: Standby</span></span>`
      );
    }
  }

  if (badges.length === 0) return null;
  return html`<div class="quota-meta-row">${badges}</div>`;
}

interface QuotaWindowData {
  title: string;
  used: number | null | undefined;
  limit: number | null | undefined;
  resetAt: string | null | undefined;
}

interface QuotaFamilyData {
  id: string;
  name: string;
  badge: { text: string; css: "danger" | "warn" | "ok" } | null;
  windows: QuotaWindowData[];
}

function computeFamilyBadge(sessionPct: number | null, weeklyPct: number | null): { text: string; css: "danger" | "warn" | "ok" } | null {
  if (weeklyPct != null && weeklyPct >= 100) {
    return { text: "Semanal agotada", css: "danger" };
  }
  if (sessionPct != null && sessionPct >= 100) {
    return { text: "5h agotada", css: "danger" };
  }
  if (weeklyPct != null && weeklyPct > 80) {
    return { text: `${weeklyPct}% semanal`, css: "warn" };
  }
  if (sessionPct != null && sessionPct > 80) {
    return { text: `${sessionPct}% 5h`, css: "warn" };
  }
  if (sessionPct != null || weeklyPct != null) {
    return { text: "Disponible", css: "ok" };
  }
  return null;
}

function renderQuotaWindow(w: QuotaWindowData): TemplateResult {
  const pct = (w.limit && w.limit > 0 && w.used != null)
    ? Math.round((w.used / w.limit) * 100)
    : null;
  const color = getQuotaColor(pct);
  const isExhausted = pct != null && pct >= 100;
  const numberClass = isExhausted ? "used" : (pct != null && pct > 80 ? "warn" : "available");
  const text = getQuotaText(w.used ?? null, w.limit ?? null);
  const resetText = formatResetTime(w.resetAt);

  let hintText: string | null = null;
  if (isExhausted) {
    hintText = resetText ? `Agotada · ${resetText}` : "Agotada";
  } else if (resetText) {
    hintText = resetText;
  }

  return html`<div class="quota-window">
    <div class="quota-window-header">
      <span class="quota-window-title">${w.title}</span>
      <span class="quota-window-numbers ${numberClass}">${text}</span>
    </div>
    <div class="quota-bar mini ${color}">
      <div class="quota-bar-track" role="meter" aria-label="${w.title}" aria-valuenow="${pct ?? 0}" aria-valuemin="0" aria-valuemax="100">
        <div class="quota-bar-fill" style="width: ${pct == null ? 0 : Math.min(100, Math.max(0, pct))}%"></div>
      </div>
    </div>
    ${hintText ? html`<small class="quota-window-hint">${hintText}</small>` : null}
  </div>`;
}

const selectedFamilyByAccount = new Map<string | number, string>();

function onFamilyTabClick(e: MouseEvent, accountId: string | number, familyId: string): void {
  e.preventDefault();
  selectedFamilyByAccount.set(accountId, familyId);
  const btn = e.currentTarget as HTMLElement;
  const container = btn.closest(".account-quota-families");
  if (!container) return;
  const tabs = container.querySelectorAll<HTMLElement>(".family-tab");
  const panels = container.querySelectorAll<HTMLElement>(".family-panel");
  tabs.forEach((t) => {
    const isTarget = t.dataset["family"] === familyId;
    t.setAttribute("aria-selected", String(isTarget));
    t.tabIndex = isTarget ? 0 : -1;
  });
  panels.forEach((p) => {
    const isTarget = p.dataset["family"] === familyId;
    p.hidden = !isTarget;
  });
}

function onFamilyTabKeydown(e: KeyboardEvent): void {
  const currentTab = e.currentTarget as HTMLElement;
  const tablist = currentTab.closest(".family-tabs");
  if (!tablist) return;
  const tabs = Array.from(tablist.querySelectorAll<HTMLElement>(".family-tab"));
  const idx = tabs.indexOf(currentTab);
  let nextIdx = -1;
  if (e.key === "ArrowRight") nextIdx = (idx + 1) % tabs.length;
  else if (e.key === "ArrowLeft") nextIdx = (idx - 1 + tabs.length) % tabs.length;
  else if (e.key === "Home") nextIdx = 0;
  else if (e.key === "End") nextIdx = tabs.length - 1;
  if (nextIdx >= 0) {
    e.preventDefault();
    const nextTab = tabs[nextIdx];
    if (nextTab) {
      nextTab.focus();
      nextTab.click();
    }
  }
}

export function renderQuotaCell(a: Account): TemplateResult {
  const meta = parseProviderSpecific(a.oauth_provider_specific);
  const badgesResult = renderMetaBadges(meta, a);

  // Z.ai dual-quota path. Rendered from per-source pools, so a top-level fetch
  // error or legacy SessionWindow 0 must never hide a healthy sibling pool.
  const pools = a.quota_pools;
  if (pools && pools.length > 0) {
    const poolsHtml = renderQuotaPools(pools);
    const topError = a.quota_fetch_error
      ? html`<div class="quota-pool-error top" role="status"><small>✗ ${a.quota_fetch_error}</small></div>`
      : null;
    return html`<div class="quota-cell">
      ${topError}
      ${poolsHtml}
      ${badgesResult}
    </div>`;
  }

  if (a.quota_fetch_error) {
    return html`<div class="quota-cell error"><small>✗ ${a.quota_fetch_error}</small></div>`;
  }
  if (a.quota_session_used == null && a.quota_weekly_used == null) {
    if (badgesResult) {
      return html`<div class="quota-cell">
        ${badgesResult}
      </div>`;
    }
    return html`<div class="quota-cell muted"><small>${a.quota_last_fetched_at ? "no quota data" : "quota: not fetched"}</small></div>`;
  }
  const sessionPct = (a.quota_session_limit && a.quota_session_limit > 0 && a.quota_session_used != null)
    ? Math.round(a.quota_session_used / a.quota_session_limit * 100) : null;
  const weeklyPct = (a.quota_weekly_limit && a.quota_weekly_limit > 0 && a.quota_weekly_used != null)
    ? Math.round(a.quota_weekly_used / a.quota_weekly_limit * 100) : null;

  const isAntigravity = a.provider_id === "antigravity";
  const isCodex = a.provider_id === "codex";

  const monthlyDetail = a.quota_model_details?.find(
    (d) => d.model_id === "Monthly Limit" || d.model_id === "Monthly Window" || d.model_id === "Monthly"
  );
  const reserveDetail = isCodex
    ? a.quota_model_details?.find(
        (d) => d.model_id.toLowerCase().includes("reserve")
      )
    : undefined;
  const claudeWeeklyDetail = isAntigravity
    ? a.quota_model_details?.find(
        (d) => d.model_id === "Claude (Weekly)" || d.model_id === "Claude Weekly Window"
      )
    : undefined;
  const claude5hDetail = isAntigravity
    ? a.quota_model_details?.find(
        (d) => d.model_id === "Claude (5h)" || d.model_id === "Claude 5h Window"
      ) || a.quota_model_details?.find(
        (d) => d.model_id.startsWith("claude-sonnet") || d.model_id.startsWith("claude-opus")
      )
    : undefined;

  const claude5hPct = (claude5hDetail && claude5hDetail.session_limit > 0 && claude5hDetail.session_used != null)
    ? Math.round(claude5hDetail.session_used / claude5hDetail.session_limit * 100) : null;
  const claudeWeeklyPct = (claudeWeeklyDetail && claudeWeeklyDetail.session_limit > 0 && claudeWeeklyDetail.session_used != null)
    ? Math.round(claudeWeeklyDetail.session_used / claudeWeeklyDetail.session_limit * 100) : null;

  const sessionLabel = "5h Window";
  const weeklyLabel = "Weekly Window";

  // Build families
  const families: QuotaFamilyData[] = [];

  const geminiWindows: QuotaWindowData[] = [
    { title: sessionLabel, used: a.quota_session_used, limit: a.quota_session_limit, resetAt: a.quota_session_reset_at },
  ];
  if (a.quota_weekly_used != null || a.quota_weekly_limit != null) {
    geminiWindows.push({ title: weeklyLabel, used: a.quota_weekly_used, limit: a.quota_weekly_limit, resetAt: a.quota_weekly_reset_at });
  }

  families.push({
    id: "gemini",
    name: isAntigravity ? "Gemini" : "Quota",
    badge: computeFamilyBadge(sessionPct, weeklyPct),
    windows: geminiWindows,
  });

  if (claude5hDetail || claudeWeeklyDetail) {
    const claudeWindows: QuotaWindowData[] = [];
    if (claude5hDetail) {
      claudeWindows.push({ title: "5h Window", used: claude5hDetail.session_used, limit: claude5hDetail.session_limit, resetAt: claude5hDetail.session_reset_at });
    }
    if (claudeWeeklyDetail) {
      claudeWindows.push({ title: "Weekly Window", used: claudeWeeklyDetail.session_used, limit: claudeWeeklyDetail.session_limit, resetAt: claudeWeeklyDetail.session_reset_at });
    }
    families.push({
      id: "claude",
      name: "Claude",
      badge: computeFamilyBadge(claude5hPct, claudeWeeklyPct),
      windows: claudeWindows,
    });
  }

  const activeFamilyId = selectedFamilyByAccount.get(a.id) || (families[0]?.id ?? "gemini");

  return html`<div class="account-quota-families" data-account-id="${a.id}">
    ${families.length > 1 ? html`
      <div class="family-tabs" role="tablist" aria-label="Quota families">
        ${families.map((f) => {
          const isSelected = activeFamilyId === f.id;
          return html`<button
            type="button"
            class="family-tab"
            role="tab"
            id="tab-${a.id}-${f.id}"
            data-family="${f.id}"
            aria-controls="panel-${a.id}-${f.id}"
            aria-selected="${isSelected ? 'true' : 'false'}"
            tabindex="${isSelected ? '0' : '-1'}"
            @click=${(e: MouseEvent) => onFamilyTabClick(e, a.id, f.id)}
            @keydown=${onFamilyTabKeydown}
          >
            <span>${f.name}</span>
            ${f.badge ? html`<span class="family-tab-badge ${f.badge.css}">${f.badge.text}</span>` : null}
          </button>`;
        })}
      </div>
    ` : null}

    ${families.map((f) => {
      const isSelected = families.length === 1 || activeFamilyId === f.id;
      return html`<div
        class="family-panel"
        role="tabpanel"
        id="panel-${a.id}-${f.id}"
        aria-labelledby="tab-${a.id}-${f.id}"
        data-family="${f.id}"
        ?hidden=${!isSelected}
      >
        <div class="family-quota-grid">
          ${f.windows.map(renderQuotaWindow)}
        </div>
      </div>`;
    })}

    ${monthlyDetail ? html`
      <div class="family-extra-quota">
        ${renderQuotaWindow({
          title: monthlyDetail.model_id,
          used: monthlyDetail.session_used,
          limit: monthlyDetail.session_limit,
          resetAt: monthlyDetail.session_reset_at,
        })}
      </div>
    ` : null}

    ${reserveDetail ? html`
      <div class="quota-bar ${getQuotaColor(reserveDetail.session_limit > 0 ? Math.round(reserveDetail.session_used / reserveDetail.session_limit * 100) : null)}">
        <div class="quota-bar-header">
          <span class="quota-bar-label-left">${icons.shield()} ${reserveDetail.model_id}</span>
          <span class="quota-bar-label-right">${getQuotaText(reserveDetail.session_used, reserveDetail.session_limit)}${resetHint(reserveDetail.session_reset_at)}</span>
        </div>
        <div class="quota-bar-track">
          <div class="quota-bar-fill" style="width: ${Math.min(100, Math.max(0, reserveDetail.session_used))}%"></div>
        </div>
      </div>
    ` : null}

    ${badgesResult}
  </div>`;
}

