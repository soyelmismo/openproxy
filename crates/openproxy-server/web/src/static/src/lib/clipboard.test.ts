// lib/clipboard.test.ts — unit tests for copyToClipboard().
//
// Covers the three code paths:
//   1. navigator.clipboard.writeText succeeds → early return
//   2. clipboard API unavailable or throws → fallback to execCommand
//   3. Both paths fail → error propagation

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { copyToClipboard } from "./clipboard.js";

describe("copyToClipboard", () => {
  // Local stub of document.execCommand because jsdom doesn't expose it.
  let execCommandMock: ReturnType<typeof vi.fn>;
  // Saved navigator.clipboard so we can restore it after each case.
  let savedClipboard: PropertyDescriptor | undefined;

  beforeEach(() => {
    execCommandMock = vi.fn();
    // jsdom's Document doesn't have execCommand so vi.spyOn can't wrap
    // it. Assign directly — clipboard.ts only calls
    // `document.execCommand("copy")`, so this is sufficient.
    (document as unknown as { execCommand: (cmd: string) => boolean }).execCommand =
      execCommandMock as unknown as (cmd: string) => boolean;
    // Capture the current navigator.clipboard descriptor so we can
    // restore it in afterEach.
    savedClipboard = Object.getOwnPropertyDescriptor(navigator, "clipboard");
  });

  afterEach(() => {
    if (savedClipboard) {
      Object.defineProperty(navigator, "clipboard", savedClipboard);
    }
    delete (document as unknown as { execCommand?: unknown }).execCommand;
    vi.restoreAllMocks();
  });

  /** Override `navigator.clipboard` for the duration of one test. */
  const setClipboard = (value: unknown): void => {
    Object.defineProperty(navigator, "clipboard", {
      value,
      configurable: true,
      writable: true,
    });
  };

  it("uses navigator.clipboard.writeText when available and succeeds", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    setClipboard({ writeText });

    await copyToClipboard("hello");

    expect(writeText).toHaveBeenCalledOnce();
    expect(writeText).toHaveBeenCalledWith("hello");
    // No textarea should have been created (early return).
    expect(document.querySelector("textarea")).toBeNull();
  });

  it("falls back to execCommand when navigator.clipboard is absent", async () => {
    setClipboard(undefined);
    execCommandMock.mockReturnValue(true);

    await copyToClipboard("fallback");

    expect(execCommandMock).toHaveBeenCalledWith("copy");
  });

  it("falls back to execCommand when clipboard.writeText throws", async () => {
    const writeText = vi.fn().mockRejectedValue(new Error("NotAllowedError"));
    setClipboard({ writeText });
    execCommandMock.mockReturnValue(true);

    await copyToClipboard("retry");

    expect(execCommandMock).toHaveBeenCalledWith("copy");
  });

  it("propagates error when execCommand returns false", async () => {
    setClipboard(undefined);
    execCommandMock.mockReturnValue(false);

    await expect(copyToClipboard("fail")).rejects.toThrow("execCommand returned false");
  });

  it("propagates error when execCommand throws", async () => {
    setClipboard(undefined);
    execCommandMock.mockImplementation(() => {
      throw new Error("exec error");
    });

    await expect(copyToClipboard("fail")).rejects.toThrow("exec error");
  });

  it("cleans up the hidden textarea even when execCommand throws", async () => {
    setClipboard(undefined);
    execCommandMock.mockImplementation(() => {
      throw new Error("boom");
    });

    await expect(copyToClipboard("leak-check")).rejects.toThrow();
    expect(document.querySelector("textarea")).toBeNull();
  });
});



