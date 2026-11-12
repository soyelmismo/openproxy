// components/windowed-list.test.ts — the shared list engine contract.
//
// Every large catalog list renders through WindowedList; these tests pin the
// guarantees consumers rely on: DOM budget, append-only extension, grouping
// order, and reset on data change.

import { describe, it, expect, beforeEach } from "vitest";
import { WindowedList, WindowModel, LIST_PAGE, LIST_WINDOW } from "./windowed-list.js";

interface Item {
  id: number;
  group: string;
}

function makeItems(n: number, groupEvery = 100): Item[] {
  return Array.from({ length: n }, (_, i) => ({
    id: i,
    group: "g" + Math.floor(i / groupEvery),
  }));
}

function renderItem(item: Item): HTMLElement {
  const el = document.createElement("div");
  el.className = "row";
  el.dataset["id"] = String(item.id);
  return el;
}

function host(): HTMLElement {
  const el = document.createElement("div");
  document.body.appendChild(el);
  return el;
}

describe("WindowedList", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("caps the initial DOM at the budget and pages from there", () => {
    const el = host();
    const list = new WindowedList<Item>({
      container: el,
      items: makeItems(1000),
      renderItem,
      initialBudget: 100,
      pageSize: 50,
    });
    expect(list.totalCount).toBe(1000);
    expect(list.renderedCount).toBe(100);
    expect(el.querySelectorAll(".row")).toHaveLength(100);
    expect(list.pendingCount).toBe(900);

    const more = el.querySelector<HTMLButtonElement>(".windowed-more");
    expect(more?.textContent).toContain("Show 50 more of 900");
    more!.click();
    expect(list.renderedCount).toBe(150);
    expect(el.querySelectorAll(".row")).toHaveLength(150);
  });

  it("extension is append-only: existing nodes survive", () => {
    const el = host();
    const list = new WindowedList<Item>({
      container: el,
      items: makeItems(200),
      renderItem,
      initialBudget: 50,
      pageSize: 25,
    });
    const first = el.querySelector(".row");
    el.querySelector<HTMLButtonElement>(".windowed-more")!.click();
    expect(el.querySelector(".row")).toBe(first);
    expect(list.renderedCount).toBe(75);
  });

  it("groups in first-seen order and budgets across groups", () => {
    const el = host();
    const list = new WindowedList<Item>({
      container: el,
      items: makeItems(300, 100), // g0 g1 g2, 100 each
      groupIdOf: (i) => i.group,
      renderItem,
      initialBudget: 120,
      pageSize: 60,
    });
    const sections = [...el.querySelectorAll<HTMLElement>(".windowed-group")];
    expect(sections.map((s) => s.dataset["groupId"])).toEqual(["g0", "g1", "g2"]);
    // First group fills to pageSize, second gets what the budget leaves, third starts empty.
    expect(sections[0]!.querySelectorAll(".row")).toHaveLength(60);
    expect(sections[1]!.querySelectorAll(".row")).toHaveLength(60);
    expect(sections[2]!.querySelectorAll(".row")).toHaveLength(0);
    // The empty third group still offers its "show" button.
    expect(sections[2]!.querySelector(".windowed-more")).not.toBeNull();
    expect(list.renderedCount).toBe(120);

    sections[2]!.querySelector<HTMLButtonElement>(".windowed-more")!.click();
    expect(sections[2]!.querySelectorAll(".row")).toHaveLength(60);
    expect(list.renderedCount).toBe(180);
  });

  it("reset swaps the data set and restores the budget", () => {
    const el = host();
    const list = new WindowedList<Item>({
      container: el,
      items: makeItems(500),
      renderItem,
      initialBudget: 40,
      pageSize: 20,
    });
    list.reset(makeItems(3));
    expect(list.totalCount).toBe(3);
    expect(list.renderedCount).toBe(3);
    expect(el.querySelector(".windowed-more")).toBeNull();
  });

  it("forEachRendered visits exactly the DOM rows in order", () => {
    const el = host();
    const list = new WindowedList<Item>({
      container: el,
      items: makeItems(30),
      renderItem,
      initialBudget: 10,
      pageSize: 5,
    });
    const ids: number[] = [];
    list.forEachRendered((_row, item) => ids.push(item.id));
    expect(ids).toEqual([0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
  });
});

describe("WindowModel (shared windowing policy)", () => {
  it("paints the initial budget then pages by pageSize", () => {
    const m = new WindowModel(5202, { initialBudget: LIST_WINDOW, pageSize: LIST_PAGE });
    expect(m.total).toBe(5202);
    expect(m.visible).toBe(LIST_WINDOW);
    expect(m.pending).toBe(5202 - LIST_WINDOW);
    expect(m.hasPending).toBe(true);
    expect(m.nextChunk).toBe(LIST_PAGE);
    expect(m.showMore()).toBe(LIST_WINDOW + LIST_PAGE);
    expect(m.visible).toBe(LIST_WINDOW + LIST_PAGE);
  });

  it("never overshoots the total and clears pending at the end", () => {
    const m = new WindowModel(120, { initialBudget: 100, pageSize: 50 });
    expect(m.showMore()).toBe(120);
    expect(m.pending).toBe(0);
    expect(m.hasPending).toBe(false);
    expect(m.showMore()).toBe(120);
  });

  it("resumes at a caller-kept window size", () => {
    const m = new WindowModel(1000, { initialBudget: 150, pageSize: 100, visible: 350 });
    expect(m.visible).toBe(350);
    expect(m.pending).toBe(650);
  });

  it("clamps a stale visible beyond the new total", () => {
    const m = new WindowModel(40, { initialBudget: 150, pageSize: 100, visible: 500 });
    expect(m.visible).toBe(40);
    expect(m.hasPending).toBe(false);
  });

  it("labels the pager with grouped thousands", () => {
    const m = new WindowModel(5202);
    expect(m.moreLabel()).toBe("Show 100 more (150 of 5,202 shown)");
  });

  it("is a no-op for tiny lists", () => {
    const m = new WindowModel(12);
    expect(m.visible).toBe(12);
    expect(m.hasPending).toBe(false);
  });
});
