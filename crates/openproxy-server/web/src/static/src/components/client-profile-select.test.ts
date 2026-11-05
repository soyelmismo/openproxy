import { describe, it, expect, vi } from 'vitest';
import { render } from 'lit-html';
import { renderClientProfileSelect } from './client-profile-select.js';

describe('renderClientProfileSelect component', () => {
  it('renders default options without inherit', () => {
    const container = document.createElement('div');
    render(
      renderClientProfileSelect({
        value: 'claude-code',
        allowInherit: false,
        noneLabel: 'None (direct / default)',
        onChange: vi.fn(),
      }),
      container
    );

    const select = container.querySelector('select');
    expect(select).not.toBeNull();
    expect(select?.value).toBe('claude-code');

    const options = Array.from(container.querySelectorAll('option'));
    expect(options[0]?.textContent?.trim()).toBe('None (direct / default)');
    const claudeOpt = options.find((opt) => opt.value === 'claude-code');
    expect(claudeOpt?.textContent?.trim()).toBe('Claude Code');
    expect(claudeOpt?.selected).toBe(true);
  });

  it('renders inherit label showing the inherited profile name', () => {
    const container = document.createElement('div');
    render(
      renderClientProfileSelect({
        value: null,
        inheritedProfile: 'claude-code',
        allowInherit: true,
        onChange: vi.fn(),
      }),
      container
    );

    const select = container.querySelector('select');
    expect(select?.value).toBe('');

    const options = Array.from(container.querySelectorAll('option'));
    expect(options[0]?.textContent?.trim()).toBe('Inherit (Claude Code)');
  });

  it('triggers onChange with parsed profile or null on clear', () => {
    const onChange = vi.fn();
    const container = document.createElement('div');
    render(
      renderClientProfileSelect({
        value: 'codex',
        allowInherit: true,
        onChange,
      }),
      container
    );

    const select = container.querySelector('select')!;
    select.value = 'cline';
    select.dispatchEvent(new Event('change'));
    expect(onChange).toHaveBeenCalledWith('cline');

    select.value = '';
    select.dispatchEvent(new Event('change'));
    expect(onChange).toHaveBeenCalledWith(null);
  });
});
