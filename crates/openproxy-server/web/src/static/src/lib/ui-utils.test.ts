import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { ensureModalRoot, flashButton, showApiError } from "./ui-utils.js";
import * as toastModule from "../components/toast.js";

describe("ui-utils", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
    vi.restoreAllMocks();
  });

  afterEach(() => {
    document.body.innerHTML = "";
    vi.restoreAllMocks();
    vi.useRealTimers();
  });

  describe("ensureModalRoot", () => {
    it("creates modal-root if it does not exist", () => {
      expect(document.getElementById("modal-root")).toBeNull();
      const root = ensureModalRoot();
      expect(root).not.toBeNull();
      expect(root.id).toBe("modal-root");
      expect(root.style.position).toBe("relative");
      expect(root.style.zIndex).toBe("1000");
      expect(document.body.contains(root)).toBe(true);
    });

    it("returns existing modal-root if already present", () => {
      const existing = document.createElement("div");
      existing.id = "modal-root";
      existing.dataset['test'] = "existing";
      document.body.appendChild(existing);

      const root = ensureModalRoot();
      expect(root).toBe(existing);
      expect(root.dataset['test']).toBe("existing");
      expect(document.querySelectorAll("#modal-root").length).toBe(1);
    });
  });

  describe("flashButton", () => {
    it("safely handles null button gracefully", () => {
      expect(() => flashButton(null, "Saved", "green")).not.toThrow();
    });

    it("updates text and background, then resets background after 1500ms", () => {
      vi.useFakeTimers();
      const btn = document.createElement("button");
      btn.textContent = "Save";
      btn.style.background = "";
      document.body.appendChild(btn);

      flashButton(btn, "Saved!", "rgb(0, 255, 0)");

      expect(btn.textContent).toBe("Saved!");
      expect(btn.style.background).toBe("rgb(0, 255, 0)");

      vi.advanceTimersByTime(1499);
      expect(btn.style.background).toBe("rgb(0, 255, 0)");

      vi.advanceTimersByTime(1);
      expect(btn.style.background).toBe("");
    });
  });

  describe("showApiError", () => {
    it("formats Error instance with default prefix", () => {
      const toastSpy = vi.spyOn(toastModule, "showToast").mockImplementation(() => {});
      const error = new Error("Connection refused");

      showApiError(error);

      expect(toastSpy).toHaveBeenCalledWith("Error: Connection refused", "error");
    });

    it("formats Error instance with custom prefix", () => {
      const toastSpy = vi.spyOn(toastModule, "showToast").mockImplementation(() => {});
      const error = new Error("Request timeout");

      showApiError(error, "Fetch failed");

      expect(toastSpy).toHaveBeenCalledWith("Fetch failed: Request timeout", "error");
    });

    it("formats non-Error primitives or objects with default and custom prefix", () => {
      const toastSpy = vi.spyOn(toastModule, "showToast").mockImplementation(() => {});

      showApiError("Something went wrong");
      expect(toastSpy).toHaveBeenCalledWith("Error: Something went wrong", "error");

      showApiError(500, "Server responded");
      expect(toastSpy).toHaveBeenCalledWith("Server responded: 500", "error");
    });
  });
});
