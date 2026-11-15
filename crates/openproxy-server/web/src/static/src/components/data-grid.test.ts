// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, html } from "lit-html";
import { renderDataGrid, type DataGridProps } from "./data-grid.js";

interface TestItem {
  id: string;
  name: string;
}

describe("renderDataGrid UI & UX robustness", () => {
  let host: HTMLElement;

  beforeEach(() => {
    document.body.innerHTML = "";
    host = document.createElement("div");
    document.body.appendChild(host);
  });

  function mountGrid(overrides: Partial<DataGridProps<TestItem>> & { id: string }): () => void {
    const rows: TestItem[] = overrides.rows ? [...overrides.rows] : [
      { id: "r1", name: "Alpha" },
      { id: "r2", name: "Beta" },
    ];
    const refresh = () => {
      render(
        renderDataGrid<TestItem>({
          columns: [{ key: "name", label: "Name", elastic: true }],
          rows,
          rowKey: (r) => r.id,
          onAction: () => {},
          __refresh: refresh,
          ...overrides,
        }),
        host,
      );
    };
    refresh();
    return refresh;
  }

  it("clamps floating bulk bar within viewport on narrow mobile screens (< 416px)", () => {
    const origW = window.innerWidth;
    const origH = window.innerHeight;
    Object.defineProperty(window, "innerWidth", { value: 360, configurable: true });
    Object.defineProperty(window, "innerHeight", { value: 640, configurable: true });

    try {
      mountGrid({
        id: "mobile-bulk-grid",
        bulkActions: [{ key: "del", label: "Delete", run: () => {} }],
      });

      const row = host.querySelector<HTMLElement>('.dg-row[data-row-key="r1"]')!;
      row.dispatchEvent(new MouseEvent("click", { bubbles: true, clientX: 200, clientY: 150 }));

      const bulk = host.querySelector<HTMLElement>(".dg-bulk");
      expect(bulk).not.toBeNull();
      const style = bulk!.getAttribute("style") || "";
      const leftMatch = /left:(-?\d+(?:\.\d+)?)px/.exec(style);
      expect(leftMatch).not.toBeNull();
      const leftPx = Number(leftMatch![1]);
      expect(leftPx).toBeGreaterThanOrEqual(8);
    } finally {
      Object.defineProperty(window, "innerWidth", { value: origW, configurable: true });
      Object.defineProperty(window, "innerHeight", { value: origH, configurable: true });
    }
  });

  it("does not select row when clicking an anchor or button inside a cell", () => {
    mountGrid({
      id: "link-isolation-grid",
      columns: [
        {
          key: "name",
          label: "Name",
          render: (r) => html`<a class="row-link" href="#/item/${r.id}">${r.name}</a>`,
        },
      ],
      bulkActions: [{ key: "del", label: "Delete", run: () => {} }],
    });

    const link = host.querySelector<HTMLElement>(".row-link")!;
    link.dispatchEvent(new MouseEvent("click", { bubbles: true }));

    const row = host.querySelector<HTMLElement>('.dg-row[data-row-key="r1"]')!;
    expect(row.classList.contains("sel")).toBe(false);
    expect(host.querySelector(".dg-bulk")).toBeNull();
  });

  it("toggles expandedRow drawer when pressing Enter on a focused row", () => {
    mountGrid({
      id: "enter-expand-grid",
      expandedRow: (r) => html`<div class="drawer-content">Config for ${r.name}</div>`,
    });

    const row = host.querySelector<HTMLElement>('.dg-row[data-row-key="r1"]')!;
    row.focus();
    row.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));

    expect(host.querySelector(".dg-expand .drawer-content")?.textContent).toBe("Config for Alpha");

    const rowAfterExpand = host.querySelector<HTMLElement>('.dg-row[data-row-key="r1"]')!;
    rowAfterExpand.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    expect(host.querySelector(".dg-expand")).toBeNull();
  });

  it("focuses .dg-rename on F2 and restores focus to .dg-row on Enter commit", async () => {
    const onRename = vi.fn();
    mountGrid({
      id: "rename-focus-grid",
      renameable: (r) => r.name,
      onRename,
    });

    const row = host.querySelector<HTMLElement>('.dg-row[data-row-key="r1"]')!;
    row.focus();
    row.dispatchEvent(new KeyboardEvent("keydown", { key: "F2", bubbles: true }));

    await new Promise((r) => setTimeout(r, 10));
    const input = host.querySelector<HTMLInputElement>(".dg-rename");
    expect(input).not.toBeNull();
    expect(document.activeElement).toBe(input);

    input!.value = "Alpha Renamed";
    input!.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));

    await new Promise((r) => setTimeout(r, 10));
    expect(onRename).toHaveBeenCalledWith({ id: "r1", name: "Alpha" }, "Alpha Renamed");
    const rowAfter = host.querySelector<HTMLElement>('.dg-row[data-row-key="r1"]')!;
    expect(document.activeElement).toBe(rowAfter);
  });
});
