import { beforeEach, afterEach, describe, expect, it, vi } from 'vitest';
import { render } from 'lit-html';
import { renderRequestTab, renderResponseTab } from './index.js';
import { renderLogDetailModal, logDetailTabClick, updateOpenLogDetail } from './modal.js';
import { getLogDetailMarkdown, setLogDetailMarkdown, setPinnedIdentity, clearPinnedIdentity } from './state.js';
import { state } from '../../state/index.js';
import { liveLogsStore } from '../../state/live-logs-store.js';
import { renderMarkdownContent } from '../markdown-content.js';
import { renderFormattedResponse } from '../../views/playground/formatted-response.js';
import { createInitialPlaygroundState } from '../../views/playground/shared.js';

const text = '#### Heading\n\n**bold** and `code`\n\n- first\n- second\n\n```js\nconst x = 1;\n```';
const request = { model: 'test', system: '**system**', messages: [
  { role: 'user', content: text },
  { role: 'assistant', content: [{ type: 'text', text: '**part**' }, { type: 'image_url', image_url: { url: 'https://example.com/image.png' } }] },
  { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'tool-1', content: [{ type: 'text', text: '**result**' }] }] },
], tools: [{ type: 'function', function: { name: 'test', description: '**not formatted**', parameters: {} } }] };
const response = { choices: [{ message: { content: text, reasoning_content: '**reasoning**', tool_calls: [{ function: { name: 'tool', arguments: '{"value":"**literal**"}' } }] } }] };
const log = { id: 42, request_id: 'md-test', trace_id: 'md-trace', request_body_json: request, response_body_json: response };
let host: HTMLElement;

beforeEach(() => {
  setLogDetailMarkdown(false);
  state.logs.selectedIdentity = null;
  clearPinnedIdentity();
  host = document.createElement('div');
  host.className = 'log-detail-modal-wrapper';
  document.body.append(host);
});
afterEach(() => {
  host.remove();
  state.logs.selectedIdentity = null;
  clearPinnedIdentity();
  setLogDetailMarkdown(false);
  vi.restoreAllMocks();
});

describe('Live Log shared Markdown', () => {
  it('defaults to the original escaped JSON/text presentation', () => {
    render(renderRequestTab(request), host);
    expect(host.querySelector('.md-formatted-content')).toBeNull();
    expect(host.textContent).toContain('**bold**');
    render(renderResponseTab(response), host);
    expect(host.querySelector('.md-formatted-content')).toBeNull();
    expect(host.textContent).toContain('**reasoning**');
  });

  it('formats request messages, system and multimodal text without touching payloads or tools', () => {
    const snapshot = JSON.stringify(request);
    setLogDetailMarkdown(true);
    render(renderRequestTab(request), host);
    expect(Array.from(host.querySelectorAll('.md-formatted-content strong')).map(el => el.textContent)).toEqual(['system', 'bold', 'part', 'result']);
    expect(host.querySelector('.md-h4')?.textContent).toBe('Heading');
    expect(host.querySelectorAll('.md-list li')).toHaveLength(2);
    expect(host.textContent).toContain('image_url');
    expect(host.textContent).toContain('tool-1');
    expect(host.textContent).toContain('**not formatted**');
    expect(JSON.stringify(request)).toBe(snapshot);
  });

  it('formats response and reasoning but leaves raw response and tool arguments unchanged', () => {
    setLogDetailMarkdown(true);
    render(renderResponseTab(JSON.stringify(response)), host);
    expect(Array.from(host.querySelectorAll('.md-formatted-content strong')).map(el => el.textContent)).toEqual(['bold', 'reasoning']);
    expect(host.querySelectorAll('.json-viewer').length).toBeGreaterThan(0);
    expect(host.textContent).toContain('**literal**');
    expect(host.textContent).toContain('**bold**'); // raw response remains available
  });

  it('switches immediately, retains active tab and preference through live updates, then restores plain text', () => {
    state.logs.selectedIdentity = { kind: 'row_id', id: 42 };
    setPinnedIdentity('md-test', 'md-trace');
    vi.spyOn(liveLogsStore, 'selectDetail').mockReturnValue({ row: log, detail: log } as any);
    render(renderLogDetailModal(log as any), host);
    logDetailTabClick('response');
    const clickToggle = () => (host.querySelector('#log-detail-markdown-toggle') as HTMLButtonElement).click();
    clickToggle();
    expect(getLogDetailMarkdown()).toBe(true);
    expect(host.querySelector('[role="switch"]')?.getAttribute('aria-checked')).toBe('true');
    expect(host.querySelector('[data-arg1="response"]')?.classList.contains('active')).toBe(true);
    expect((host.querySelector('[data-log-tab="request"]') as HTMLElement).style.display).toBe('none');
    expect(host.querySelector('[data-log-tab="response"] strong')?.textContent).toBe('bold');
    updateOpenLogDetail(log as any);
    expect(host.querySelector('[role="switch"]')?.getAttribute('aria-checked')).toBe('true');
    expect(host.querySelector('[data-arg1="response"]')?.classList.contains('active')).toBe(true);
    clickToggle();
    expect(host.querySelector('.md-formatted-content')).toBeNull();
    expect(host.querySelector('[role="switch"]')?.getAttribute('aria-checked')).toBe('false');
  });

  it('uses identical rendered markup in Playground and Live Log', () => {
    const pg = createInitialPlaygroundState();
    pg.streamedChatContent = text;
    render(renderFormattedResponse(pg), host);
    const playground = host.querySelector('.md-formatted-content')?.innerHTML;
    expect(playground).toBeTruthy();
    setLogDetailMarkdown(true);
    render(renderResponseTab(response), host);
    expect(host.querySelector('.md-formatted-content')?.innerHTML).toBe(playground);
  });

  it('escapes HTML and rejects dangerous links using the existing renderer', () => {
    setLogDetailMarkdown(true);
    render(renderResponseTab({ choices: [{ message: { content: '<img src=x onerror=alert(1)> [click](javascript:alert)' } }] }), host);
    expect(host.querySelector('.md-formatted-content img')).toBeNull();
    expect(host.querySelector('.md-formatted-content a')).toBeNull();
    expect(host.textContent).toContain('<img');
  });

  it('shared code-copy handler copies once without loading Playground', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal('navigator', { clipboard: { writeText } });
    render(renderMarkdownContent('```js\nconst x = "<tag>";\n```'), host);
    const button = host.querySelector('.md-copy-btn') as HTMLButtonElement;
    button.click();
    await vi.waitFor(() => expect(button.textContent).toBe('Copied!'));
    expect(writeText).toHaveBeenCalledExactlyOnceWith('const x = "<tag>";');
    vi.unstubAllGlobals();
  });
});
