// components/codex-resets-modal.ts — modal to inspect and redeem Codex banked reset credits.

import { html, render, type TemplateResult } from 'lit-html';
import { api } from '../state/api.js';
import { state } from '../state/index.js';
import { requestUpdate } from '../state/reactive.js';
import { showToast } from './toast.js';
import { showApiError } from '../lib/ui-utils.js';
import { renderOpModal, type OpModalHandle } from './render-op-modal.js';

export interface CodexResetCredit {
  id: string;
  reset_type?: string | null;
  status?: string | null;
  expires_at?: string | null;
  title?: string | null;
  description?: string | null;
}

export interface CodexResetListResponse {
  available_count: number;
  credits: CodexResetCredit[];
}

interface ModalState {
  loading: boolean;
  error: string | null;
  credits: CodexResetCredit[];
  availableCount: number;
  selectedCreditId: string | null;
  redeeming: boolean;
}

function formatExpiration(expStr: string | null | undefined): { text: string; urgent: boolean } {
  if (!expStr) return { text: 'No expiration date specified', urgent: false };
  let date: Date;
  const num = Number(expStr);
  if (!isNaN(num) && num > 0) {
    date = new Date(num > 1e11 ? num : num * 1000);
  } else {
    date = new Date(expStr);
  }
  if (isNaN(date.getTime())) {
    return { text: expStr, urgent: false };
  }
  const now = Date.now();
  const diffMs = date.getTime() - now;
  if (diffMs <= 0) {
    return { text: 'Expired', urgent: true };
  }
  const diffHrs = Math.floor(diffMs / (1000 * 60 * 60));
  const diffDays = Math.floor(diffHrs / 24);
  const formattedDate = date.toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    year: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });

  if (diffDays > 0) {
    const remHrs = diffHrs % 24;
    return {
      text: `${formattedDate} (in ${diffDays}d ${remHrs}h)`,
      urgent: diffDays <= 3,
    };
  }
  const diffMins = Math.floor((diffMs % (1000 * 60 * 60)) / (1000 * 60));
  return {
    text: `${formattedDate} (in ${diffHrs}h ${diffMins}m)`,
    urgent: true,
  };
}

export function showCodexResetsModal(accountId: number, accountLabel?: string): void {
  const modalState: ModalState = {
    loading: true,
    error: null,
    credits: [],
    availableCount: 0,
    selectedCreditId: null,
    redeeming: false,
  };

  let modalHandle: OpModalHandle | null = null;

  const updateView = (): void => {
    if (!modalHandle) return;
    const body = renderBody();
    const actions = renderActions();
    const title = accountLabel
      ? `Codex Reset Credits — ${accountLabel}`
      : `Codex Reset Credits (Account #${accountId})`;

    // Re-render modal inside its existing wrapper
    const titleEl = modalHandle.el.querySelector('#op-modal-title');
    if (titleEl) titleEl.textContent = title;
    const bodyEl = modalHandle.el.querySelector<HTMLElement>('.modal-body');
    if (bodyEl) render(body, bodyEl);
    const footerEl = modalHandle.el.querySelector<HTMLElement>('.modal-footer');
    if (footerEl) render(actions, footerEl);
  };

  const loadCredits = async (): Promise<void> => {
    modalState.loading = true;
    modalState.error = null;
    updateView();

    try {
      const res = (await api(`/accounts/${accountId}/codex-resets`)) as CodexResetListResponse;
      modalState.loading = false;
      modalState.credits = res.credits || [];
      modalState.availableCount = res.available_count ?? modalState.credits.length;
      if (modalState.credits.length > 0 && !modalState.selectedCreditId && modalState.credits[0]) {
        modalState.selectedCreditId = modalState.credits[0].id;
      }
    } catch (err: unknown) {
      modalState.loading = false;
      modalState.error = (err as Error)?.message || 'Failed to load Codex reset credits';
    }
    updateView();
  };

  const onRedeem = async (): Promise<void> => {
    if (!modalState.selectedCreditId || modalState.redeeming) return;
    modalState.redeeming = true;
    updateView();

    try {
      const res = (await api(`/accounts/${accountId}/redeem-codex-reset`, {
        method: 'POST',
        body: JSON.stringify({ credit_id: modalState.selectedCreditId }),
      })) as { success?: boolean; outcome?: string; message?: string };

      if (res && res.success) {
        showToast(res.message || 'Codex rate limit reset applied successfully!', 'success');
        modalHandle?.close();
        state.accounts = (await api('/accounts')) as typeof state.accounts;
        requestUpdate();
      } else {
        showToast(res?.message || 'Failed to apply Codex reset', 'warning');
        modalState.redeeming = false;
        updateView();
      }
    } catch (err: unknown) {
      showApiError(err, 'Error redeeming Codex reset credit');
      modalState.redeeming = false;
      updateView();
    }
  };

  const renderBody = (): TemplateResult => {
    if (modalState.loading) {
      return html`
        <div style="text-align: center; padding: var(--space-6); color: var(--color-text-muted);">
          <p>Loading available reset credits from OpenAI Codex...</p>
        </div>
      `;
    }

    if (modalState.error) {
      return html`
        <div style="padding: var(--space-4);">
          <div class="empty-state error" style="margin-bottom: var(--space-3);">
            <p><strong>Error:</strong> ${modalState.error}</p>
          </div>
          <button type="button" @click=${loadCredits}>Retry</button>
        </div>
      `;
    }

    if (modalState.credits.length === 0) {
      return html`
        <div style="padding: var(--space-2);">
          <div class="empty-state" style="text-align: center; padding: var(--space-5); background: var(--color-surface-2); border-radius: var(--radius-md);">
            <p style="font-size: var(--fs-md); font-weight: 600; margin-bottom: var(--space-2);">
              No banked reset credits available
            </p>
            <p style="font-size: var(--fs-sm); color: var(--color-text-muted); max-width: 480px; margin: 0 auto;">
              This account does not currently have any banked rate limit reset credits. Rate limits will automatically reset on their natural 5-hour and 7-day rolling windows.
            </p>
          </div>
        </div>
      `;
    }

    return html`
      <div>
        <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: var(--space-2);">
          <span style="font-size: var(--fs-sm); color: var(--color-text-muted);">
            Select a credit to redeem:
          </span>
          <span class="quota-tag credits">
            ⚡ ${modalState.availableCount} Available
          </span>
        </div>

        <div class="codex-credits-list">
          ${modalState.credits.map((credit, idx) => {
            const isSelected = modalState.selectedCreditId === credit.id;
            const exp = formatExpiration(credit.expires_at);
            const title = credit.title || (credit.reset_type ? `Reset (${credit.reset_type})` : `Banked Reset #${idx + 1}`);

            return html`
              <div
                class="codex-credit-item ${isSelected ? 'selected' : ''}"
                @click=${() => {
                  modalState.selectedCreditId = credit.id;
                  updateView();
                }}
              >
                <input
                  type="radio"
                  class="codex-credit-radio"
                  name="codex-credit-choice"
                  .checked=${isSelected}
                  @change=${() => {
                    modalState.selectedCreditId = credit.id;
                    updateView();
                  }}
                />
                <div class="codex-credit-info">
                  <div class="codex-credit-top">
                    <span class="codex-credit-title">${title}</span>
                    <span class="codex-credit-id" title="${credit.id}">${credit.id.slice(0, 16)}...</span>
                  </div>
                  ${credit.description ? html`<div class="codex-credit-desc">${credit.description}</div>` : html``}
                  <div class="codex-credit-exp ${exp.urgent ? 'urgent' : ''}">
                    <span>🕒 Expiration:</span>
                    <span>${exp.text}</span>
                  </div>
                </div>
              </div>
            `;
          })}
        </div>

        <div class="codex-resets-hint">
          <strong>💡 How it works:</strong> Redeeming a reset credit immediately restores both your 5-hour rolling limit and weekly limit, restarting your 7-day weekly reset timer.
        </div>
      </div>
    `;
  };

  const renderActions = (): TemplateResult => {
    const hasCredits = modalState.credits.length > 0;
    return html`
      <button type="button" @click=${() => modalHandle?.close()} ?disabled=${modalState.redeeming}>
        ${hasCredits ? 'Cancel' : 'Close'}
      </button>
      ${hasCredits
        ? html`
            <button
              type="button"
              class="primary"
              ?disabled=${!modalState.selectedCreditId || modalState.redeeming}
              @click=${onRedeem}
            >
              ${modalState.redeeming ? 'Redeeming...' : '⚡ Redeem Selected Credit'}
            </button>
          `
        : html``}
    `;
  };

  const title = accountLabel
    ? `Codex Reset Credits — ${accountLabel}`
    : `Codex Reset Credits (Account #${accountId})`;

  modalHandle = renderOpModal({
    title,
    body: renderBody(),
    actions: renderActions(),
  });

  void loadCredits();
}
