// components/codex-resets-modal.ts — modal to inspect and redeem Codex banked reset credits.

import { html, render, type TemplateResult } from 'lit-html';
import { api } from '../state/api.js';
import { state } from '../state/index.js';
import { requestUpdate } from '../state/reactive.js';
import { showToast } from './toast.js';
import { showApiError } from '../lib/ui-utils.js';
import { ensureModalRoot } from '../lib/ui-utils.js';
import { icons } from '../lib/icons.js';

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

function formatCreditId(id: string): string {
  if (!id) return '';
  if (id.startsWith('RateLimitResetCredit_')) {
    const rest = id.slice('RateLimitResetCredit_'.length);
    return rest.length > 10 ? `…${rest.slice(-8)}` : rest;
  }
  if (id.includes('_')) {
    const parts = id.split('_');
    const suffix = parts[parts.length - 1] ?? '';
    return suffix.length > 10 ? `…${suffix.slice(-8)}` : suffix;
  }
  return id.length > 14 ? `…${id.slice(-8)}` : id;
}

export function showCodexResetsModal(accountId: number, accountLabel?: string): void {
  // Prevent duplicate modals if one is already open
  const existingWrapper = document.getElementById('codex-resets-modal-wrapper');
  if (existingWrapper) {
    existingWrapper.remove();
  }

  const root = ensureModalRoot();
  const wrapper = document.createElement('div');
  wrapper.id = 'codex-resets-modal-wrapper';
  root.appendChild(wrapper);

  let closed = false;
  const close = (): void => {
    if (closed) return;
    closed = true;
    window.removeEventListener('keydown', onKeydown, true);
    render(html``, wrapper);
    wrapper.remove();
  };

  const onKeydown = (e: KeyboardEvent): void => {
    if (e.key === 'Escape') {
      e.stopPropagation();
      close();
    }
  };
  window.addEventListener('keydown', onKeydown, true);

  const modalState: ModalState = {
    loading: true,
    error: null,
    credits: [],
    availableCount: 0,
    selectedCreditId: null,
    redeeming: false,
  };

  const title = accountLabel
    ? `Codex Reset Credits — ${accountLabel}`
    : `Codex Reset Credits (Account #${accountId})`;

  const renderBody = (): TemplateResult => {
    if (modalState.loading) {
      return html`
        <div class="codex-modal-loading">
          <div class="codex-modal-spinner">${icons.refresh('icon spin')}</div>
          <p>Loading available reset credits from OpenAI Codex...</p>
        </div>
      `;
    }

    if (modalState.error) {
      return html`
        <div class="codex-modal-error">
          <div class="empty-state error">
            <div class="empty-state-icon">${icons.warning()}</div>
            <p><strong>Error:</strong> ${modalState.error}</p>
          </div>
          <button type="button" class="small" @click=${loadCredits}>
            ${icons.refresh()} Retry
          </button>
        </div>
      `;
    }

    if (modalState.credits.length === 0) {
      return html`
        <div class="codex-modal-empty">
          <div class="codex-empty-icon">${icons.shield('icon-large')}</div>
          <p class="codex-empty-title">No banked reset credits available</p>
          <p class="codex-empty-desc">
            This account does not currently have any banked rate limit reset credits. Rate limits will automatically reset on their natural 5-hour and 7-day rolling windows.
          </p>
        </div>
      `;
    }

    return html`
      <div>
        <div class="codex-resets-toolbar">
          <span class="codex-resets-toolbar-label">Select a credit to redeem:</span>
          <span class="quota-tag credits">
            ${icons.lightning()} <span>${modalState.availableCount} Available</span>
          </span>
        </div>

        <div class="codex-credits-list" role="radiogroup" aria-label="Codex reset credits">
          ${modalState.credits.map((credit, idx) => {
            const isSelected = modalState.selectedCreditId === credit.id;
            const exp = formatExpiration(credit.expires_at);
            const prefix = `Reset #${idx + 1}`;
            const itemTitle = credit.title
              ? (credit.title.toLowerCase().startsWith('banked reset') ? credit.title : `${prefix}: ${credit.title}`)
              : (credit.reset_type ? `${prefix} (${credit.reset_type})` : prefix);

            return html`
              <div
                class="codex-credit-item ${isSelected ? 'selected' : ''}"
                role="radio"
                aria-checked=${String(isSelected)}
                tabindex="0"
                @click=${() => {
                  modalState.selectedCreditId = credit.id;
                  updateView();
                }}
                @keydown=${(e: KeyboardEvent) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    modalState.selectedCreditId = credit.id;
                    updateView();
                  }
                }}
              >
                <input
                  type="radio"
                  class="codex-credit-radio"
                  name="codex-credit-choice"
                  aria-hidden="true"
                  tabindex="-1"
                  .checked=${isSelected}
                  @change=${() => {
                    modalState.selectedCreditId = credit.id;
                    updateView();
                  }}
                />
                <div class="codex-credit-info">
                  <div class="codex-credit-top">
                    <span class="codex-credit-title">${itemTitle}</span>
                    <span class="codex-credit-id" title="${credit.id}">
                      ${formatCreditId(credit.id)}
                    </span>
                  </div>
                  ${credit.description ? html`<div class="codex-credit-desc">${credit.description}</div>` : html``}
                  <div class="codex-credit-exp ${exp.urgent ? 'urgent' : ''}">
                    ${icons.clock()}
                    <span class="codex-credit-exp-label">Expiration:</span>
                    <span class="codex-credit-exp-val">${exp.text}</span>
                  </div>
                </div>
              </div>
            `;
          })}
        </div>

        <div class="codex-resets-hint">
          <span class="codex-resets-hint-icon">${icons.info()}</span>
          <div class="codex-resets-hint-text">
            <strong>How it works:</strong> Redeeming a reset credit immediately restores both your 5-hour rolling limit and weekly limit, restarting your 7-day weekly reset timer.
          </div>
        </div>
      </div>
    `;
  };

  const renderActions = (): TemplateResult => {
    // If loading, error, or no credits: single clean Close button
    if (modalState.loading || modalState.error || modalState.credits.length === 0) {
      return html`
        <button type="button" @click=${close}>Close</button>
      `;
    }

    // When credits are available: Cancel on the left, Primary action on the right
    return html`
      <button type="button" @click=${close} ?disabled=${modalState.redeeming}>
        Cancel
      </button>
      <button
        type="button"
        class="primary"
        ?disabled=${!modalState.selectedCreditId || modalState.redeeming}
        @click=${onRedeem}
      >
        ${modalState.redeeming
          ? html`${icons.refresh('icon spin')} <span>Redeeming...</span>`
          : html`${icons.lightning()} <span>Redeem Selected Credit</span>`}
      </button>
    `;
  };

  const updateView = (): void => {
    if (closed) return;

    const template = html`
      <div
        class="modal-bg"
        role="dialog"
        aria-modal="true"
        aria-labelledby="op-modal-title"
        @click=${(e: Event) => {
          if (e.target === e.currentTarget) close();
        }}
      >
        <div class="modal" @click=${(e: Event) => e.stopPropagation()}>
          <div class="modal-header">
            <h2 id="op-modal-title">${title}</h2>
            <button
              type="button"
              class="close-btn"
              aria-label="Close"
              @click=${close}
            >&times;</button>
          </div>
          <div class="modal-body">${renderBody()}</div>
          <div class="modal-footer">${renderActions()}</div>
        </div>
      </div>
    `;

    render(template, wrapper);
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
        close();
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

  // Initial render
  updateView();
  void loadCredits();
}
