import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { render } from 'lit-html';
import { state } from '../../state/index.js';
import type { Model, Provider } from '../../lib/types/api.js';
import { api } from '../../state/api.js';
import { showToast } from '../../components/toast.js';
import { renderProvidersGrid } from './list.js';
import { renderModelsSection } from './models.js';
import { onTestModel } from './models-row.js';
import { getProviderUi } from './shared.js';

vi.mock('../../state/api.js', () => ({ api: vi.fn() }));
vi.mock('../../state/reactive.js', () => ({ requestUpdate: vi.fn() }));
vi.mock('../../components/toast.js', () => ({ showToast: vi.fn() }));

const provider = { id: 'codex', name: 'Codex (ChatGPT)', active: true, base_url: '', format: 'openai' } as Provider;
const model = { row_id: 901, provider_id: 'codex', model_id: 'gpt-long-model-name', active: true, last_test_status: null } as Model;

beforeEach(() => {
  vi.clearAllMocks();
  state.providers = [provider];
  state.models = [{ ...model }];
  state.accounts = [];
  localStorage.clear();
  document.body.innerHTML = '<main></main>';
});
afterEach(() => { document.body.innerHTML = ''; });

function paintModels(): HTMLButtonElement {
  render(renderModelsSection(provider, state.models, getProviderUi(provider.id)), document.querySelector('main')!);
  return document.querySelector<HTMLButtonElement>('[title="Test model"]')!;
}

describe('provider list and model test regressions', () => {
  it('shows only the official name, not the shaded provider ID', () => {
    render(renderProvidersGrid(), document.querySelector('main')!);
    expect(document.querySelector('.provider-name-text')?.textContent?.trim()).toBe(provider.name);
    expect(document.querySelector('.provider-id-code')).toBeNull();
  });

  it('executes the real grid quick action when clicking its SVG and shows pending feedback', async () => {
    let finish!: (value: unknown) => void;
    vi.mocked(api).mockReturnValue(new Promise(resolve => { finish = resolve; }));
    const btn = paintModels();
    btn.querySelector('svg')!.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    expect(api).toHaveBeenCalledWith('/models/901/test', expect.objectContaining({ method: 'POST' }));
    expect(btn.getAttribute('aria-busy')).toBe('true');
    expect(btn.disabled).toBe(true);
    // A programmatic/context-menu invocation must not start a duplicate request.
    const duplicate = onTestModel(model.row_id, null);
    expect(api).toHaveBeenCalledTimes(1);
    finish({ status: 200, elapsed_ms: 42 });
    await duplicate;
    await vi.waitFor(() => expect(state.models[0]!.last_test_status).toBe(200));
    expect(btn.disabled).toBe(false);
    expect(btn.getAttribute('aria-busy')).toBeNull();
    expect(showToast).toHaveBeenCalledWith(expect.stringContaining('200'), 'success');
  });

  it('runs from the context menu without requiring a button', async () => {
    vi.mocked(api).mockResolvedValue({ status: 429, elapsed_ms: 14 });
    await onTestModel(model.row_id, null);
    expect(api).toHaveBeenCalledTimes(1);
    expect(state.models[0]!.last_test_status).toBe(429);
    expect(showToast).toHaveBeenCalledWith(expect.stringContaining('429'), 'error');
  });

  it('restores the button after a rejected request and permits retry', async () => {
    vi.mocked(api).mockRejectedValueOnce(new Error('network unavailable'));
    const btn = paintModels();
    await onTestModel(model.row_id, btn);
    expect(btn.disabled).toBe(false);
    expect(btn.getAttribute('aria-busy')).toBeNull();
    vi.mocked(api).mockResolvedValueOnce({ status: 200, elapsed_ms: 1 });
    await onTestModel(model.row_id, btn);
    expect(api).toHaveBeenCalledTimes(2);
  });
});
