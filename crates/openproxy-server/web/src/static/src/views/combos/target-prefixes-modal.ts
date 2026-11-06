import { html, render } from "lit-html";
import { api } from "../../lib/api.js";
import { ensureModalRoot, showApiError } from "../../lib/ui-utils.js";
import { showToast } from "../../components/toast.js";
import { icons } from "../../lib/icons.js";
import type { ComboTargetSystemPrefix, CreateSystemPrefixInput, UpdateSystemPrefixInput } from "../../lib/types/combos.js";

export async function showTargetPrefixesModal(
  comboId: number,
  targetId: number,
  modelName: string,
  onUpdate: () => void,
): Promise<void> {
  const root = ensureModalRoot();
  const wrapper = document.createElement("div");
  root.appendChild(wrapper);

  let prefixes: ComboTargetSystemPrefix[] = [];
  let isCreating = false;
  let editingPrefixId: number | null = null;
  let editFormState: { name: string; criteria: string; system_text: string; priority_order: number } = {
    name: "",
    criteria: "",
    system_text: "",
    priority_order: 0,
  };

  async function loadPrefixes(): Promise<void> {
    try {
      prefixes = (await api(`/combos/${comboId}/targets/${targetId}/prefixes`)) as ComboTargetSystemPrefix[];
    } catch (err: unknown) {
      showApiError(err, "Failed to load system prefixes");
    }
  }

  await loadPrefixes();

  function closeModal(): void {
    wrapper.remove();
    onUpdate();
  }

  function startEditing(p: ComboTargetSystemPrefix): void {
    editingPrefixId = p.id;
    editFormState = {
      name: p.name,
      criteria: p.criteria,
      system_text: p.system_text,
      priority_order: p.priority_order ?? 0,
    };
    renderModal();
  }

  function cancelEditing(): void {
    editingPrefixId = null;
    renderModal();
  }

  function renderModal(): void {
    const listHtml = prefixes.length === 0
      ? html`<div class="empty-state" style="padding:20px;text-align:center;color:var(--text-muted, #888);font-size:0.9rem;">
          No system prefixes configured for this target yet.<br>
          Add prefixes with custom semantic criteria for dynamic persona routing (e.g. trading, tweet evaluation, uncensored).
        </div>`
      : html`
        <div class="prefixes-list" style="display:flex;flex-direction:column;gap:12px;max-height:420px;overflow-y:auto;padding-right:4px;">
          ${prefixes.map((p) => {
            const isEditingThis = editingPrefixId === p.id;

            if (isEditingThis) {
              return html`
                <div class="prefix-card edit-card" style="border:1px solid var(--accent, #6366f1);border-radius:6px;padding:12px;background:var(--bg-surface, rgba(255,255,255,0.03));">
                  <div style="font-weight:600;font-size:0.9rem;margin-bottom:10px;color:var(--text-primary, #eee);display:flex;align-items:center;justify-content:space-between;">
                    <span>Edit Prefix: <strong>${p.name}</strong></span>
                    <span class="chip ${p.active ? "chip-enabled" : "chip-disabled"}" style="font-size:0.75rem;padding:2px 6px;">
                      ${p.active ? "Active" : "Paused"}
                    </span>
                  </div>
                  <form @submit=${async (e: Event) => {
                    e.preventDefault();
                    const form = e.target as HTMLFormElement;
                    const data = new FormData(form);
                    const name = String(data.get("name") || "").trim();
                    const criteria = String(data.get("criteria") || "").trim();
                    const system_text = String(data.get("system_text") || "").trim();

                    if (!name || !criteria || !system_text) {
                      showToast("Name, criteria, and system text are required", "error");
                      return;
                    }

                    const input: UpdateSystemPrefixInput = {
                      name,
                      criteria,
                      system_text,
                    };

                    try {
                      await api(`/combos/${comboId}/targets/${targetId}/prefixes/${p.id}`, {
                        method: "PATCH",
                        body: JSON.stringify(input),
                      });
                      showToast(`Updated prefix "${name}"`, "success");
                      editingPrefixId = null;
                      await loadPrefixes();
                      renderModal();
                    } catch (err) {
                      showApiError(err, "Failed to update prefix");
                    }
                  }}>
                    <div style="display:flex;flex-direction:column;gap:8px;">
                      <div>
                        <label style="display:block;font-size:0.75rem;font-weight:600;color:var(--text-muted, #aaa);margin-bottom:2px;">
                          Name / Tag:
                        </label>
                        <input type="text" name="name" required class="cw-input" style="width:100%;font-size:0.85rem;"
                          .value=${editFormState.name}>
                      </div>

                      <div>
                        <label style="display:block;font-size:0.75rem;font-weight:600;color:var(--text-muted, #aaa);margin-bottom:2px;">
                          Selection Criteria (evaluated by Laya/Jev):
                        </label>
                        <textarea name="criteria" required class="cw-input" rows="2" style="width:100%;font-family:monospace;font-size:0.82rem;"
                          .value=${editFormState.criteria}></textarea>
                      </div>

                      <div>
                        <label style="display:block;font-size:0.75rem;font-weight:600;color:var(--text-muted, #aaa);margin-bottom:2px;">
                          Injected System Text:
                        </label>
                        <textarea name="system_text" required class="cw-input" rows="4" style="width:100%;font-family:monospace;font-size:0.82rem;"
                          .value=${editFormState.system_text}></textarea>
                      </div>

                      <div style="display:flex;justify-content:flex-end;gap:6px;margin-top:6px;">
                        <button type="button" class="small" @click=${cancelEditing}>
                          Cancel
                        </button>
                        <button type="submit" class="small primary">
                          Save Changes
                        </button>
                      </div>
                    </div>
                  </form>
                </div>
              `;
            }

            return html`
              <div class="prefix-card" style="border:1px solid var(--border, #333);border-radius:6px;padding:12px;background:var(--bg-surface, rgba(255,255,255,0.02));">
                <div style="display:flex;align-items:center;justify-content:space-between;margin-bottom:8px;">
                  <div style="display:flex;align-items:center;gap:8px;">
                    <strong style="font-size:0.95rem;color:var(--text-primary, #eee);">${p.name}</strong>
                    <span class="chip ${p.active ? "chip-enabled" : "chip-disabled"}" style="font-size:0.75rem;padding:2px 6px;">
                      ${p.active ? "Active" : "Paused"}
                    </span>
                  </div>
                  <div style="display:flex;align-items:center;gap:6px;">
                    <button type="button" class="small" title="Edit prefix inline"
                      @click=${() => startEditing(p)}>
                      ${icons.pencil()} Edit
                    </button>
                    <button type="button" class="small" title=${p.active ? "Deactivate" : "Activate"}
                      @click=${async () => {
                        try {
                          await api(`/combos/${comboId}/targets/${targetId}/prefixes/${p.id}`, {
                            method: "PATCH",
                            body: JSON.stringify({ active: !p.active }),
                          });
                          await loadPrefixes();
                          renderModal();
                        } catch (err) {
                          showApiError(err, "Failed to update status");
                        }
                      }}>
                      ${p.active ? icons.pause() : icons.play()}
                    </button>
                    <button type="button" class="small danger" title="Delete prefix"
                      @click=${async () => {
                        if (!confirm(`Delete system prefix "${p.name}"?`)) return;
                        try {
                          await api(`/combos/${comboId}/targets/${targetId}/prefixes/${p.id}`, {
                            method: "DELETE",
                          });
                          showToast(`Deleted prefix "${p.name}"`, "success");
                          await loadPrefixes();
                          renderModal();
                        } catch (err) {
                          showApiError(err, "Failed to delete prefix");
                        }
                      }}>
                      ${icons.close()}
                    </button>
                  </div>
                </div>

                <div style="margin-bottom:6px;">
                  <label style="font-size:0.75rem;font-weight:600;color:var(--text-muted, #aaa);text-transform:uppercase;letter-spacing:0.5px;">
                    Selection Criteria (evaluated by Laya/Jev):
                  </label>
                  <div style="font-size:0.85rem;color:var(--text-secondary, #ccc);background:rgba(0,0,0,0.2);padding:6px 8px;border-radius:4px;font-family:monospace;white-space:pre-wrap;margin-top:2px;">
                    ${p.criteria}
                  </div>
                </div>

                <div>
                  <label style="font-size:0.75rem;font-weight:600;color:var(--text-muted, #aaa);text-transform:uppercase;letter-spacing:0.5px;">
                    Injected System Text:
                  </label>
                  <div style="font-size:0.82rem;color:var(--text-secondary, #bbb);background:rgba(0,0,0,0.3);padding:6px 8px;border-radius:4px;max-height:90px;overflow-y:auto;white-space:pre-wrap;font-family:monospace;margin-top:2px;">
                    ${p.system_text}
                  </div>
                </div>
              </div>
            `;
          })}
        </div>
      `;

    const createFormHtml = !isCreating
      ? html`
        <div style="margin-top:16px;display:flex;justify-content:flex-end;">
          <button type="button" class="primary" @click=${() => { isCreating = true; renderModal(); }}>
            ${icons.plus()} Add System Prefix
          </button>
        </div>`
      : html`
        <form style="margin-top:16px;border:1px solid var(--accent, #6366f1);border-radius:6px;padding:14px;background:var(--bg-surface, rgba(255,255,255,0.03));"
          @submit=${async (e: Event) => {
            e.preventDefault();
            const form = e.target as HTMLFormElement;
            const data = new FormData(form);
            const name = String(data.get("name") || "").trim();
            const criteria = String(data.get("criteria") || "").trim();
            const system_text = String(data.get("system_text") || "").trim();

            if (!name || !criteria || !system_text) {
              showToast("Name, criteria, and system text are required", "error");
              return;
            }

            const input: CreateSystemPrefixInput = {
              name,
              criteria,
              system_text,
              active: true,
              priority_order: prefixes.length,
            };

            try {
              await api(`/combos/${comboId}/targets/${targetId}/prefixes`, {
                method: "POST",
                body: JSON.stringify(input),
              });
              showToast(`Created prefix "${name}"`, "success");
              isCreating = false;
              await loadPrefixes();
              renderModal();
            } catch (err) {
              showApiError(err, "Failed to create system prefix");
            }
          }}>
          <div style="font-weight:600;font-size:0.95rem;margin-bottom:12px;color:var(--text-primary, #eee);">
            Create System Prefix
          </div>
          <div style="display:flex;flex-direction:column;gap:10px;">
            <div>
              <label style="display:block;font-size:0.8rem;font-weight:500;margin-bottom:4px;">
                Prefix Name / Tag:
              </label>
              <input type="text" name="name" required class="cw-input" style="width:100%;"
                placeholder="e.g. gemini_strict_eval, trading_specialist, nsfw_persona">
            </div>

            <div>
              <label style="display:block;font-size:0.8rem;font-weight:500;margin-bottom:4px;">
                Selection Criteria (instructions for Laya/Jev router):
              </label>
              <textarea name="criteria" required class="cw-input" rows="2" style="width:100%;font-family:monospace;font-size:0.85rem;"
                placeholder="e.g. When the prompt asks for tweet analysis, JSON originality check, or Gemini 3.5 persona."></textarea>
            </div>

            <div>
              <label style="display:block;font-size:0.8rem;font-weight:500;margin-bottom:4px;">
                System Prompt to Inject:
              </label>
              <textarea name="system_text" required class="cw-input" rows="4" style="width:100%;font-family:monospace;font-size:0.85rem;"
                placeholder="You are Gemini 3.5 Flash Lite... (Injected dynamically into the target payload, tokens subtracted from usage)"></textarea>
            </div>

            <div style="display:flex;justify-content:flex-end;gap:8px;margin-top:6px;">
              <button type="button" @click=${() => { isCreating = false; renderModal(); }}>
                Cancel
              </button>
              <button type="submit" class="primary">
                Save Prefix
              </button>
            </div>
          </div>
        </form>
      `;

    const template = html`
          <div class="modal-backdrop modal-bg" @click=${closeModal}>
            <div class="modal-content modal" @click=${(e: Event) => e.stopPropagation()}>
              <div class="modal-header">
                <div>
                  <h3 style="margin:0;font-size:1.1rem;display:flex;align-items:center;gap:8px;">
                    <span>System Prefixes</span>
                    <span class="chip" style="font-size:0.75rem;font-weight:normal;">${modelName}</span>
                  </h3>
                  <p style="margin:3px 0 0;font-size:0.78rem;color:var(--color-text-muted, #888);">
                    Dynamic persona routing: Laya selects the best prefix based on query criteria. Injected tokens are discounted from client billing.
                  </p>
                </div>
                <button type="button" class="small close-btn" @click=${closeModal} aria-label="Close">
                  ${icons.close()}
                </button>
              </div>

              <div class="modal-body" style="padding:16px 18px;overflow-y:auto;flex:1;">
                ${listHtml}
                ${createFormHtml}
              </div>

              <div class="modal-footer">
                <button type="button" class="primary" @click=${closeModal}>
                  Done
                </button>
              </div>
            </div>
          </div>
    `;

    render(template, wrapper);
  }

  renderModal();
}
