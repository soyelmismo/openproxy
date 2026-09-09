import { describe, expect, it } from "vitest";
import { extractApiErrorMessage } from "./escape.js";

describe("extractApiErrorMessage", () => {
  it("extracts the message from a standard ApiError JSON body", () => {
    const err = new Error('409: {"error":{"message":"already exists"}}');
    expect(extractApiErrorMessage(err)).toBe("already exists");
  });

  it("returns null for a non-JSON error message", () => {
    expect(extractApiErrorMessage(new Error("something broke"))).toBeNull();
  });

  it("unescapes JSON escape sequences in the message", () => {
    const err = new Error('422: {"error":{"message":"line\\"quote\\" \\\\ done"}}');
    expect(extractApiErrorMessage(err)).toBe('line"quote" \\ done');
  });

  it("returns null for a non-JSON status body", () => {
    expect(extractApiErrorMessage(new Error("502: Bad Gateway"))).toBeNull();
  });

  it("returns null for an alternative JSON shape without the envelope", () => {
    const err = new Error('409: {"error":"already exists"}');
    expect(extractApiErrorMessage(err)).toBeNull();
  });

  it("returns null for a non-Error input", () => {
    expect(extractApiErrorMessage("plain string")).toBeNull();
    expect(extractApiErrorMessage(null)).toBeNull();
  });

  it("returns an empty string for an empty message field", () => {
    const err = new Error('400: {"error":{"message":""}}');
    expect(extractApiErrorMessage(err)).toBe("");
  });
});
