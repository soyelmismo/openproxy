// handlers/proxy-source-handlers.test.ts — unit tests for
// buildProxySourceBodyFromForm().
//
// Scope: pure form→JSON builder shared by `createProxySource` and
// `updateProxySource`. Trim semantics for `name`/`url`, `priority`
// falling back to `0`, and checkbox semantics for `active`.

import { describe, it, expect, beforeEach } from "vitest";
import { buildProxySourceBodyFromForm } from "./proxy-source-handlers.js";

function makeSourceForm(fields: {
  name?: string;
  url?: string;
  priority?: string;
  active?: boolean;
} = {}): HTMLFormElement {
  const form = document.createElement("form");
  document.body.appendChild(form);

  const input = (name: string, value: string, type = "text"): void => {
    const i = document.createElement("input");
    i.name = name;
    i.type = type;
    i.value = value;
    form.appendChild(i);
  };
  const checkbox = (name: string, on: boolean): void => {
    const c = document.createElement("input");
    c.type = "checkbox";
    c.name = name;
    c.checked = on;
    form.appendChild(c);
  };
  input("name", fields.name ?? "");
  input("url", fields.url ?? "");
  input("priority", fields.priority ?? "0", "number");
  checkbox("active", fields.active ?? false);
  return form;
}

describe("buildProxySourceBodyFromForm", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("returns a fully-populated body for a typical create-source submission", () => {
    const form = makeSourceForm({
      name: "  My Proxy List  ",
      url: "https://example.com/proxies.txt",
      priority: "5",
      active: true,
    });
    expect(buildProxySourceBodyFromForm(form)).toEqual({
      name: "My Proxy List",         // trimmed
      url: "https://example.com/proxies.txt",
      priority: 5,
      active: true,
    });
  });

  it("falls back to priority 0 when the input is blank or missing", () => {
    const blank = makeSourceForm({ priority: "" });
    expect(buildProxySourceBodyFromForm(blank).priority).toBe(0);
    const missing = (() => {
      const f = document.createElement("form");
      document.body.appendChild(f);
      const n = document.createElement("input"); n.name = "name"; n.value = "x"; f.appendChild(n);
      const u = document.createElement("input"); u.name = "url"; u.value = "https://x"; f.appendChild(u);
      const a = document.createElement("input"); a.type = "checkbox"; a.name = "active"; f.appendChild(a);
      return f;
    })();
    expect(buildProxySourceBodyFromForm(missing).priority).toBe(0);
  });

  it("encodes an unticked active checkbox as false (not undefined)", () => {
    const form = makeSourceForm({ active: false });
    expect(buildProxySourceBodyFromForm(form).active).toBe(false);
  });

  it("preserves leading/trailing url spaces only via trim (no whitespace tolerated)", () => {
    const form = makeSourceForm({ url: "  https://x.test  " });
    expect(buildProxySourceBodyFromForm(form).url).toBe("https://x.test");
  });
});