// components/client-profile-select.ts — shared client profile dropdown for models and combo targets.
import { html, type TemplateResult } from 'lit-html';
import { CLIENT_PROFILE_OPTIONS, type ClientProfile } from '../lib/types/client-profile.js';

export interface ClientProfileSelectProps {
  id?: string | undefined;
  className?: string | undefined;
  value?: ClientProfile | string | null | undefined;
  inheritedProfile?: ClientProfile | string | null | undefined;
  allowInherit?: boolean | undefined;
  inheritLabel?: string | undefined;
  noneLabel?: string | undefined;
  disabled?: boolean | undefined;
  onChange: (newValue: ClientProfile | null) => void | Promise<void>;
}

export function renderClientProfileSelect(props: ClientProfileSelectProps): TemplateResult {
  const {
    id,
    className = 'combo-table-select',
    value = null,
    inheritedProfile = null,
    allowInherit = false,
    inheritLabel = 'Inherit (none)',
    noneLabel = 'None (direct)',
    disabled = false,
    onChange,
  } = props;

  const currentValue = value ?? '';

  let effectiveInheritLabel = inheritLabel;
  if (allowInherit && inheritedProfile) {
    const inheritedOption = CLIENT_PROFILE_OPTIONS.find((opt) => opt.id === inheritedProfile);
    effectiveInheritLabel = `Inherit (${inheritedOption ? inheritedOption.label : inheritedProfile})`;
  }

  const handleChange = (e: Event) => {
    const target = e.target as HTMLSelectElement | null;
    if (!target) return;
    const raw = target.value.trim();
    const parsed = raw === '' ? null : (raw as ClientProfile);
    void onChange(parsed);
  };

  return html`
    <select
      id=${id || ''}
      class=${className}
      .value=${currentValue}
      ?disabled=${disabled}
      title="Client profile spoofing preset"
      @change=${handleChange}
    >
      ${allowInherit
        ? html`<option value="" ?selected=${!currentValue}>${effectiveInheritLabel}</option>`
        : html`<option value="" ?selected=${!currentValue}>${noneLabel}</option>`}
      ${CLIENT_PROFILE_OPTIONS.map(
        (opt) => html`
          <option value=${opt.id} title=${opt.description} ?selected=${currentValue === opt.id}>
            ${opt.label}
          </option>
        `
      )}
    </select>
  `;
}
