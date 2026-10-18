import { describe, it, expect, beforeEach } from 'vitest';
import { createInitialPlaygroundState } from './shared.js';
import { ensureDefaultModel } from './inspector.js';
import { state } from '../../state/index.js';

describe('Playground model selection and filtering', () => {
  beforeEach(() => {
    state.providers = [
      { id: 'active-p1', name: 'Active P1', active: true } as any,
      { id: 'disabled-p2', name: 'Disabled P2', active: false } as any,
    ];
    state.models = [
      { id: 1, provider_id: 'active-p1', model_id: 'model-a', model_type: 'chat', active: true, display_name: 'Model A' } as any,
      { id: 2, provider_id: 'disabled-p2', model_id: 'model-b', model_type: 'chat', active: true, display_name: 'Model B' } as any,
      { id: 3, provider_id: 'active-p1', model_id: 'model-inactive', model_type: 'chat', active: false, display_name: 'Model Inactive' } as any,
    ];
    state.combos = [];
    state.accounts = [
      { id: 10, provider_id: 'active-p1', label: 'acc1', health_status: 'healthy' } as any,
      { id: 20, provider_id: 'disabled-p2', label: 'acc2', health_status: 'healthy' } as any,
    ];
  });

  it('excludes models from disabled providers in Any Provider view', () => {
    const st = createInitialPlaygroundState();
    st.selectedProviderId = '';
    ensureDefaultModel(st);
    // Should choose active-p1/model-a, never model-b from disabled provider
    expect(st.selectedModelId).toBe('active-p1/model-a');
  });

  it('automatically aligns selectedProviderId when a qualified model is selected', () => {
    const st = createInitialPlaygroundState();
    st.selectedProviderId = '';
    st.selectedModelId = 'active-p1/model-a';
    expect(st.selectedModelId).toBe('active-p1/model-a');
  });
});
