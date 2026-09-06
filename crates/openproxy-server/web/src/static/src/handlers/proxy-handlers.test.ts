// handlers/proxy-handlers.test.ts — unit tests for
// buildCustomProxyBodyFromForm().
//
// Scope: pure form→JSON builder extracted from `createCustomProxy`.
// Notes on the `country_code` semantics:
//   - blank  → `null`  (significant: "" would deserialize as Some(""))
//   - lowercased → uppercased (ISO-3166 alpha-2 is canonically upper)

import { describe, it, expect, beforeEach } from "vitest";
import { buildCustomProxyBodyFromForm } from "./proxy-handlers.js";

function makeProxyForm(fields: {
  host?: string;
  port?: string;
  type?: string;
  countryCode?: string;
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
  const sel = (name: string, value: string): void => {
    const s = document.createElement("select");
    s.name = name;
    const opt = document.createElement("option");
    opt.value = value;
    s.appendChild(opt);
    s.value = value;
    form.appendChild(s);
  };
  input("host", fields.host ?? "");
  input("port", fields.port ?? "8080", "number");
  sel("type", fields.type ?? "http");
  if (fields.countryCode !== undefined) input("country_code", fields.countryCode);
  return form;
}

describe("buildCustomProxyBodyFromForm", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("returns a fully-populated body for a typical custom-proxy submission", () => {
    const form = makeProxyForm({
      host: "1.2.3.4",
      port: "8080",
      type: "http",
      countryCode: "US",
    });
    expect(buildCustomProxyBodyFromForm(form)).toEqual({
      host: "1.2.3.4",
      port: 8080,
      type: "http",
      country_code: "US",
    });
  });

  it("uppercases a lowercase country code", () => {
    const form = makeProxyForm({ countryCode: "de" });
    expect(buildCustomProxyBodyFromForm(form).country_code).toBe("DE");
  });

  it("encodes blank country_code as null (not empty string)", () => {
    const form = makeProxyForm({ countryCode: "" });
    expect(buildCustomProxyBodyFromForm(form).country_code).toBeNull();
  });

  it("omits the country_code key entirely when the input is missing from the DOM", () => {
    const form = makeProxyForm();
    expect(buildCustomProxyBodyFromForm(form).country_code).toBeNull();
  });

  it("defaults the proxy type to 'http' when missing", () => {
    const minimal = document.createElement("form");
    document.body.appendChild(minimal);
    const host = document.createElement("input"); host.name = "host"; host.value = "x"; minimal.appendChild(host);
    const port = document.createElement("input"); port.name = "port"; port.value = "8080"; port.type = "number"; minimal.appendChild(port);
    expect(buildCustomProxyBodyFromForm(minimal).type).toBe("http");
  });

  it("parses port as a number (not a string)", () => {
    const form = makeProxyForm({ port: "3128" });
    expect(buildCustomProxyBodyFromForm(form).port).toBe(3128);
  });
});