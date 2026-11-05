import { afterEach, describe, expect, it, vi } from 'vitest';
import { html } from 'lit-html';
import { mountView, requestUpdate } from '../state/reactive.js';
const { mount } = vi.hoisted(() => ({ mount: vi.fn() }));
vi.mock('../state/router.js', () => ({ mountRouteInto: mount }));
import { clearNativePanels, reconcileNativePanels } from './native-panels.js';
const settle = async () => { for (let i = 0; i < 8; i++) await Promise.resolve(); };
afterEach(async () => { clearNativePanels(); await settle(); document.body.innerHTML = ''; mount.mockReset(); });
function hosts(): HTMLElement {
  document.body.innerHTML = '<div id="workspace"><div data-tile="a"></div><div data-tile="b"></div></div>';
  return document.getElementById('workspace')!;
}
describe('native panel ownership', () => {
  it('keeps live mounts on focus/layout paints and tears down only the changed target', async () => {
    const root = hosts(); const cleanup = vi.fn();
    mount.mockImplementation(async (_route, target) => { mountView(target, () => html`live`); return cleanup; });
    reconcileNativePanels(root, [{ id: 'a', route: '#/providers' }, { id: 'b', route: '#/logs' }]);
    await settle(); const logs = root.querySelector('[data-tile="b"] .tile-view');
    reconcileNativePanels(root, [{ id: 'a', route: '#/providers' }, { id: 'b', route: '#/logs' }]);
    expect(mount).toHaveBeenCalledTimes(2);
    reconcileNativePanels(root, [{ id: 'a', route: '#/keys' }, { id: 'b', route: '#/logs' }]);
    await settle();
    expect(cleanup).toHaveBeenCalledTimes(1);
    expect(root.querySelector('[data-tile="b"] .tile-view')).toBe(logs);
    expect(root.querySelectorAll('iframe')).toHaveLength(0);
  });
  it('retires late async mounts before starting the next owner of that family', async () => {
    const root = hosts(); let release!: () => void; const cleanup = vi.fn();
    mount.mockImplementationOnce(async () => { await new Promise<void>(resolve => { release = resolve; }); return cleanup; });
    mount.mockResolvedValueOnce(() => {});
    reconcileNativePanels(root, [{ id: 'a', route: '#/providers/first' }]); await settle();
    const retired = root.querySelector('.tile-view')!;
    reconcileNativePanels(root, [{ id: 'a', route: '#/providers/second' }]); await settle();
    expect(retired.isConnected).toBe(false); expect(mount).toHaveBeenCalledTimes(1);
    release(); await settle();
    expect(cleanup).toHaveBeenCalledTimes(1); expect(mount).toHaveBeenCalledTimes(2);
  });
  it('stops detached views reacting when an async cleanup has not returned yet', async () => {
    const root = hosts(); let release!: () => void; let renders = 0;
    mount.mockImplementationOnce(async (_route, target) => {
      const cleanup = mountView(target, () => { renders++; return html`active`; });
      await new Promise<void>(resolve => { release = resolve; }); return cleanup;
    });
    reconcileNativePanels(root, [{ id: 'a', route: '#/logs' }]); await settle();
    clearNativePanels(); requestUpdate(); await settle();
    expect(renders).toBe(1); release(); await settle();
  });
});
