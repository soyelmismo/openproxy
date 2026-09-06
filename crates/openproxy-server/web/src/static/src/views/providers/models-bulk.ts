// views/providers/models-bulk.ts — bulk-model action handlers.
//
// Split out of the former detail.ts monolith (FU1). Every function
// in this file is a handler that operates on `state.selectedModels`
// (the current checkbox selection set) and the full `state.models`
// array, toggling, testing, deleting, or tagging models in bulk.

import { state } from '../../state/index.js';
import { api } from '../../state/api.js';
import { requestUpdate } from '../../state/reactive.js';
import { showToast } from '../../components/toast.js';
import { showApiError } from '../../lib/ui-utils.js';
import { showConfirm } from '../../lib/show-confirm.js';
import type { Model } from '../../lib/types/api.js';

// ---- Bulk toggle all (enable/disable all non-custom) ----

export async function onBulkToggleModels(
  providerId: string,
  active: boolean,
): Promise<void> {
  const models = (state.models || []).filter(
    (m) => m.provider_id === providerId,
  );
  const customCount = models.filter((m) => m.custom).length;
  const toToggleCount = models.filter((m) => !m.custom && m.active !== active)
    .length;
  if (toToggleCount === 0) {
    showToast('Nothing to toggle.', 'info');
    return;
  }
  const msg = active
    ? `Enable ${toToggleCount} non-custom models? (${customCount} custom models will not be touched)`
    : `Disable ${toToggleCount} non-custom models? (${customCount} custom models will not be touched)`;
  if (
    !(await showConfirm({
      title: active ? 'Enable models' : 'Disable models',
      message: msg,
      confirmLabel: active ? 'Enable' : 'Disable',
    }))
  )
    return;
  try {
    await api('/models/bulk-toggle', {
      method: 'POST',
      body: JSON.stringify({ provider_id: providerId, active }),
    });
    state.models = (await api(
      '/models?provider_id=' + encodeURIComponent(providerId),
    )) as typeof state.models;
    state.modelsComplete = false;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

// ---- Bulk toggle/select/clear ----

async function onBulkSetSelected(
  providerId: string,
  active: boolean,
): Promise<void> {
  const ids = Array.from(state.selectedModels).map((n) => Number(n));
  if (ids.length === 0) return;
  if (
    !(await showConfirm({
      title: active ? 'Enable models' : 'Disable models',
      message: `${active ? 'Enable' : 'Disable'} ${ids.length} models?`,
      confirmLabel: active ? 'Enable' : 'Disable',
    }))
  )
    return;
  try {
    await Promise.all(
      ids.map((rowId) =>
        api('/models/' + rowId + '/toggle', {
          method: 'POST',
          body: JSON.stringify({ active }),
        }).catch((err: unknown) => console.error('Failed toggle', rowId, err)),
      ),
    );
    state.models = (await api(
      '/models?provider_id=' + encodeURIComponent(providerId),
    )) as Model[];
    state.modelsComplete = false;
    state.selectedModels.clear();
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

export function onBulkEnableSelected(providerId: string): Promise<void> {
  return onBulkSetSelected(providerId, true);
}

export function onBulkDisableSelected(providerId: string): Promise<void> {
  return onBulkSetSelected(providerId, false);
}

// ---- Bulk test selected ----

export async function onBulkTestSelected(providerId: string): Promise<void> {
  void providerId;
  const ids = Array.from(state.selectedModels).map((n) => Number(n));
  if (ids.length === 0) return;
  if (
    !(await showConfirm({
      title: 'Test models',
      message: `Test ${ids.length} models sequentially?`,
      confirmLabel: 'Test',
    }))
  )
    return;
  try {
    for (const rowId of ids) {
      const btn = document.getElementById(
        `test-btn-${rowId}`,
      ) as HTMLButtonElement | null;
      if (btn) {
        btn.disabled = true;
        btn.textContent = 'Testing...';
      }

      const accountSelect = document.getElementById(
        `test-account-${rowId}`,
      ) as HTMLSelectElement | null;
      const proxySelect = document.getElementById(
        `test-proxy-${rowId}`,
      ) as HTMLSelectElement | null;
      const accountId =
        accountSelect && accountSelect.value
          ? parseInt(accountSelect.value, 10)
          : null;
      const proxyId =
        proxySelect && proxySelect.value ? proxySelect.value : null;

      const result = (await api(`/models/${rowId}/test`, {
        method: 'POST',
        body: JSON.stringify({ account_id: accountId, proxy_id: proxyId }),
      })) as { status: number; elapsed_ms: number; row_id?: number };
      const m = (state.models || []).find((x) => x.row_id === rowId);
      if (m) {
        m.last_test_status = result.status;
        m.last_test_at = new Date().toISOString();
      }
      if (btn) {
        if (result.status >= 200 && result.status < 300) {
          btn.textContent = '✓';
          btn.style.background = '#a6e3a1';
        } else {
          btn.textContent = '✗ ' + result.status;
          btn.style.background = '#f38ba8';
        }
        setTimeout(() => {
          btn.textContent = 'Test';
          btn.style.background = '';
          btn.disabled = false;
        }, 1500);
      }
    }
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

// ---- Bulk delete selected ----

export async function onBulkDeleteSelected(providerId: string): Promise<void> {
  const ids = Array.from(state.selectedModels).map((n) => Number(n));
  if (ids.length === 0) return;
  if (
    !(await showConfirm({
      title: 'Delete models',
      message: `Delete ${ids.length} models? This cannot be undone.`,
      danger: true,
      confirmLabel: 'Delete',
    }))
  )
    return;
  try {
    await Promise.all(
      ids.map((rowId) =>
        api('/models/' + rowId, { method: 'DELETE' }).catch((err: unknown) =>
          console.error('Failed delete', rowId, err),
        ),
      ),
    );
    state.models = (await api(
      '/models?provider_id=' + encodeURIComponent(providerId),
    )) as Model[];
    state.modelsComplete = false;
    state.selectedModels.clear();
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

// ---- Bulk set modality / tag selected ----

export async function onBulkSetModalitySelected(newType: string): Promise<void> {
  const ids = Array.from(state.selectedModels).map((n) => Number(n));
  if (ids.length === 0) return;
  try {
    await Promise.all(
      ids.map((rowId) =>
        api(`/models/${rowId}`, {
          method: 'PATCH',
          body: JSON.stringify({ model_type: newType }),
        }),
      ),
    );
    for (const rowId of ids) {
      const m = (state.models || []).find((x) => x.row_id === rowId);
      if (m) m.model_type = newType;
    }
    showToast(`Tagged ${ids.length} models as ${newType}`, 'success');
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}
