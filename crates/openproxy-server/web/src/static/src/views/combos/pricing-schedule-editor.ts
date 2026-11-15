// views/combos/pricing-schedule-editor.ts — accessible UTC editor for a
// combo's peak/off-peak pricing schedule.
//
// Everything is explicit: weekdays per window, zero-padded HH:MM times,
// optional holiday dates, and a preset that only fills the form. Saving
// PATCHes `pricing_schedule` (object) or `pricing_schedule: null` when the
// toggle is off. The modal never fetches on its own and never touches another
// combo.

import { html, render, type TemplateResult } from "lit-html";
import { ensureModalRoot } from "../../lib/ui-utils.js";
import { showToast } from "../../components/toast.js";
import { parsePricingSchedule } from "../../lib/pricing-schedule.js";
import { saveComboPricingSchedule } from "../../handlers/combo-handlers.js";
import type { Combo } from "../../lib/types/api.js";
import {
  draftFromSchedule,
  presetDraft,
  scheduleFromDraft,
  type DraftIssue,
  type RatesDraft,
  type ScheduleDraft,
} from "./pricing-schedule-draft.js";

type Band = "peak" | "off_peak";
type RateKey = keyof RatesDraft;

const WEEKDAYS: readonly { value: number; label: string }[] = [
  { value: 1, label: "Mon" },
  { value: 2, label: "Tue" },
  { value: 3, label: "Wed" },
  { value: 4, label: "Thu" },
  { value: 5, label: "Fri" },
  { value: 6, label: "Sat" },
  { value: 7, label: "Sun" },
];

export interface PricingScheduleEditorOptions {
  combo: Combo;
  /** Called after a successful PATCH so the detail view can refresh. */
  onSaved?: () => void;
}

export function showPricingScheduleEditor(options: PricingScheduleEditorOptions): void {
  const { combo } = options;
  const parsed = parsePricingSchedule(combo.pricing_schedule);
  let draft: ScheduleDraft = draftFromSchedule(parsed.schedule, {
    input_per_1m: combo.pricing_input_per_1m ?? null,
    output_per_1m: combo.pricing_output_per_1m ?? null,
    cached_per_1m: combo.pricing_cached_per_1m ?? null,
  });
  let issues: DraftIssue[] = [];
  let saving = false;
  let finished = false;

  const root = ensureModalRoot();
  const wrapper = document.createElement("div");
  root.appendChild(wrapper);

  const close = (): void => {
    if (finished) return;
    finished = true;
    window.removeEventListener("keydown", onKeydown, true);
    wrapper.remove();
  };

  const onKeydown = (e: KeyboardEvent): void => {
    if (e.key === "Escape") {
      e.stopPropagation();
      close();
    }
  };

  function setRate(band: Band, key: RateKey, value: string): void {
    draft[band][key] = value;
  }

  function toggleDay(windowIndex: number, day: number, on: boolean): void {
    const window = draft.windows[windowIndex];
    if (!window) return;
    window.days = on ? [...window.days, day] : window.days.filter((d) => d !== day);
  }

  function invalid(field: string): boolean {
    return issues.some((issue) => issue.field === field);
  }

  function focusFirstInvalid(): void {
    wrapper.querySelector<HTMLElement>('[aria-invalid="true"]')?.focus();
  }

  function rateField(band: Band, key: RateKey, label: string): TemplateResult {
    const field = `${band}.${key}`;
    const id = `ps-rate-${band}-${key}`;
    return html`<label class="ps-rate-field" for=${id}>
      <span class="ps-rate-label">${label}</span>
      <input
        id=${id}
        type="number"
        min="0"
        step="0.0001"
        inputmode="decimal"
        data-rate=${field}
        .value=${draft[band][key]}
        placeholder=${key === "cached" ? "same as input" : ""}
        aria-invalid=${invalid(field) ? "true" : "false"}
        @input=${(e: Event) => setRate(band, key, (e.target as HTMLInputElement).value)}
      />
    </label>`;
  }

  function ratesGroup(band: Band, legend: string): TemplateResult {
    return html`<fieldset class="ps-rate-group">
      <legend>${legend}</legend>
      ${rateField(band, "input", "Input")}
      ${rateField(band, "output", "Output")}
      ${rateField(band, "cached", "Cached")}
    </fieldset>`;
  }

  function windowsEditor(): TemplateResult {
    if (draft.windows.length === 0) {
      return html`<p class="ps-empty">No peak windows: every day bills at the off-peak rate.</p>`;
    }
    return html`${draft.windows.map((window, index) => {
      const daysField = `windows.${index}.days`;
      const startField = `windows.${index}.start`;
      const endField = `windows.${index}.end`;
      return html`<div class="ps-window" data-window-index=${String(index)} role="group" aria-label=${`Peak window ${index + 1}`}>
        <div class="ps-weekdays" role="group" aria-label=${`Weekdays for peak window ${index + 1}`}>
          ${WEEKDAYS.map(
            (day) => html`<label class="ps-weekday">
              <input
                type="checkbox"
                .checked=${window.days.includes(day.value)}
                aria-invalid=${invalid(daysField) ? "true" : "false"}
                @change=${(e: Event) => toggleDay(index, day.value, (e.target as HTMLInputElement).checked)}
              />
              <span>${day.label}</span>
            </label>`,
          )}
        </div>
        <div class="ps-window-times">
          <label class="ps-time-field">
            <span>Start (UTC)</span>
            <input
              type="time"
              step="60"
              data-window-start=${String(index)}
              .value=${window.start}
              aria-invalid=${invalid(startField) ? "true" : "false"}
              @input=${(e: Event) => {
                const w = draft.windows[index];
                if (w) w.start = (e.target as HTMLInputElement).value;
              }}
            />
          </label>
          <span class="ps-time-arrow" aria-hidden="true">→</span>
          <label class="ps-time-field">
            <span>End (UTC)</span>
            <input
              type="time"
              step="60"
              data-window-end=${String(index)}
              .value=${window.end}
              aria-invalid=${invalid(endField) ? "true" : "false"}
              @input=${(e: Event) => {
                const w = draft.windows[index];
                if (w) w.end = (e.target as HTMLInputElement).value;
              }}
            />
          </label>
        </div>
        <button
          type="button"
          class="small danger ps-remove"
          aria-label=${`Remove peak window ${index + 1}`}
          @click=${() => {
            draft.windows.splice(index, 1);
            issues = [];
            renderModal();
          }}
        >Remove</button>
      </div>`;
    })}`;
  }

  function datesEditor(): TemplateResult {
    return html`${draft.off_peak_dates.map((date, index) => html`<div class="ps-date-row">
      <label class="ps-date-label" for=${`ps-date-${index}`}>Off-peak date ${index + 1}</label>
      <input
        id=${`ps-date-${index}`}
        type="date"
        .value=${date}
        aria-invalid=${invalid(`dates.${index}`) ? "true" : "false"}
        @input=${(e: Event) => {
          draft.off_peak_dates[index] = (e.target as HTMLInputElement).value;
        }}
      />
      <button
        type="button"
        class="small danger ps-remove"
        aria-label=${`Remove off-peak date ${index + 1}`}
        @click=${() => {
          draft.off_peak_dates.splice(index, 1);
          issues = [];
          renderModal();
        }}
      >Remove</button>
    </div>`)}`;
  }

  function template(): TemplateResult {
    const warning = parsed.status === "invalid"
      ? html`<div class="ps-warning" role="status">
          The stored schedule could not be read (${parsed.reason ?? "invalid value"}). Rebuild it below and save.
        </div>`
      : html``;
    return html`<div
      class="modal-bg ps-modal-bg"
      role="dialog"
      aria-modal="true"
      aria-labelledby="ps-title"
      @click=${(e: Event) => {
        if (e.target === e.currentTarget) close();
      }}
    >
      <div class="modal ps-modal">
        <div class="modal-header">
          <h2 id="ps-title">Pricing schedule — ${combo.name}</h2>
          <button type="button" class="close-btn" aria-label="Close pricing schedule editor" @click=${close}>&times;</button>
        </div>
        <div class="modal-body">
          ${warning}
          <p class="ps-hint">
            All times are UTC. Windows are half-open <code>[start, end)</code>; an end earlier than the start
            runs overnight and stays anchored to the start day.
          </p>
          <label class="ps-enable">
            <input
              type="checkbox"
              id="ps-enabled"
              .checked=${draft.enabled}
              @change=${(e: Event) => {
                draft.enabled = (e.target as HTMLInputElement).checked;
                issues = [];
                renderModal();
              }}
            />
            <span>Enable peak/off-peak schedule</span>
          </label>
          <fieldset class="ps-fields" ?disabled=${!draft.enabled}>
            <legend>Rates (USD per 1M tokens)</legend>
            <div class="ps-rates-grid">
              ${ratesGroup("peak", "Peak")}
              ${ratesGroup("off_peak", "Off-peak")}
            </div>

            <div class="ps-section-head">
              <h3>Peak windows (UTC)</h3>
              <button
                type="button"
                class="small"
                @click=${() => {
                  draft.windows.push({ days: [1, 2, 3, 4, 5], start: "09:00", end: "17:00" });
                  issues = [];
                  renderModal();
                }}
              >Add window</button>
            </div>
            ${windowsEditor()}

            <div class="ps-section-head">
              <h3>Off-peak dates (UTC holidays)</h3>
              <button
                type="button"
                class="small"
                @click=${() => {
                  draft.off_peak_dates.push("");
                  issues = [];
                  renderModal();
                }}
              >Add date</button>
            </div>
            <p class="ps-hint">
              Manual list only: no automatic holiday data is fetched. Add each date you want billed off-peak.
            </p>
            ${datesEditor()}
          </fieldset>

          ${issues.length > 0
            ? html`<ul class="ps-errors" role="alert">
                ${issues.map((issue) => html`<li>${issue.message}</li>`)}
              </ul>`
            : html``}

          <div class="ps-preset-row">
            <button
              type="button"
              class="small secondary"
              @click=${() => {
                draft = presetDraft();
                issues = [];
                renderModal();
              }}
            >Load DeepSeek preset</button>
            <span class="ps-hint">Fills this form only — review and Save. It is not applied to any other combo.</span>
          </div>
        </div>
        <div class="modal-footer">
          <button type="button" @click=${close}>Cancel</button>
          <button type="button" class="primary" ?disabled=${saving} @click=${() => void onSave()}>
            ${saving ? "Saving…" : "Save schedule"}
          </button>
        </div>
      </div>
    </div>`;
  }

  function renderModal(): void {
    render(template(), wrapper);
  }

  async function onSave(): Promise<void> {
    if (saving) return;
    const result = scheduleFromDraft(draft);
    issues = result.issues;
    if (issues.length > 0) {
      renderModal();
      focusFirstInvalid();
      return;
    }
    saving = true;
    renderModal();
    try {
      await saveComboPricingSchedule(combo.id, result.schedule);
      showToast(
        result.schedule ? "Pricing schedule saved" : "Pricing schedule cleared",
        "success",
      );
      close();
      options.onSaved?.();
    } catch (err: unknown) {
      saving = false;
      renderModal();
      showToast(
        "Could not save the pricing schedule: " + (err instanceof Error ? err.message : String(err)),
        "error",
      );
    }
  }

  window.addEventListener("keydown", onKeydown, true);
  renderModal();
  wrapper.querySelector<HTMLElement>("#ps-enabled")?.focus();
}
