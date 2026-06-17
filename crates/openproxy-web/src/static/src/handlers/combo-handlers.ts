// handlers/combo-handlers.ts — combo CRUD: create, delete, race
// size, test-all. Target-specific actions live in
// combo-target-handlers.ts.
//
// Per spec §3 + §13.8 we do not attach to `window.*`.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { appendModal } from "../lib/dom.js";
import { rerenderCurrentView } from "../state/router.js";

export function showCreateCombo(): void {
  const html = `
    <div class="modal-bg" id="create-combo-modal" data-action="closeCreateCombo" data-arg1="self">
      <div class="modal">
        <div class="modal-header">
          <h2>New combo</h2>
          <button type="button" class="close-btn" data-action="closeCreateCombo" aria-label="Close">&times;</button>
        </div>
        <form data-action="createCombo">
          <div class="modal-body">
            <div class="field"><label for="combo-name">Name</label><input id="combo-name" name="name" type="text" required></div>
            <div class="field">
              <label for="combo-strategy">Strategy</label>
              <select id="combo-strategy" name="strategy">
                <option value="priority">priority</option>
                <option value="round_robin">round_robin</option>
                <option value="shuffle">shuffle</option>
              </select>
            </div>
            <div class="field"><label for="combo-race-size">Race size</label><input id="combo-race-size" name="race_size" type="number" min="1" max="8" value="1"></div>
          </div>
          <div class="modal-footer">
            <button type="button" data-action="closeCreateCombo">Cancel</button>
            <button type="submit" class="primary">Create</button>
          </div>
        </form>
      </div>
    </div>
  `;
  appendModal(html);
}

export function closeCreateCombo(): void {
  const m = document.getElementById("create-combo-modal");
  if (m) m.remove();
}

export async function createCombo(e: Event): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const body: Record<string, unknown> = Object.fromEntries(f);
  body["race_size"] = parseInt(String(body["race_size"]));
  try {
    await api("/combos", { method: "POST", body: JSON.stringify(body) });
    closeCreateCombo();
    rerenderCurrentView();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    alert("Error: " + msg);
  }
}

export async function deleteCombo(id: number): Promise<void> {
  if (!confirm("Delete combo " + id + "?")) return;
  try {
    await api("/combos/" + id, { method: "DELETE" });
    rerenderCurrentView();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    alert("Error: " + msg);
  }
}

export async function updateRaceSize(id: number, e: Event | null): Promise<void> {
  const val = e && e.target ? parseInt((e.target as HTMLInputElement).value, 10) : NaN;
  if (!Number.isFinite(val) || val < 1 || val > 8) { alert("Must be 1-8"); rerenderCurrentView(); return; }
  try {
    await api("/combos/" + id, { method: "PATCH", body: JSON.stringify({ race_size: val }) });
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    alert("Error: " + msg);
    rerenderCurrentView();
  }
}

// testAllTargets: receives (comboId, e). The e.target is the button
// that was clicked; we toggle a "Testing…" state on it. The
// original code used `window.event` to find the button, which is
// non-standard; the e.target path is more reliable.
export async function testAllTargets(comboId: number, e: Event | null): Promise<void> {
  const btn = e && e.target ? (e.target as HTMLElement).closest("button") : null;
  const oldText = btn ? btn.textContent : null;
  if (btn) { btn.disabled = true; btn.textContent = "🧪 Testing..."; }
  try {
    const results = await api(`/combos/${comboId}/test-all`, { method: "POST" });
    state.comboTestResults[comboId] = Array.isArray(results) ? results : [];
    rerenderCurrentView();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    alert("Test all failed: " + msg);
  } finally {
    if (btn) { btn.disabled = false; btn.textContent = oldText || "🧪 Test all"; }
  }
}
