import { describe, it, expect } from "vitest";
import { formatContext, formatCost, formatMs, formatNumber } from "./format";

describe("formatContext", () => {
  it("handles nullish and invalid inputs", () => {
    expect(formatContext(null)).toBe("—");
    expect(formatContext(undefined)).toBe("—");
    expect(formatContext(NaN)).toBe("—");
    expect(formatContext(Infinity)).toBe("—");
    expect(formatContext("invalid")).toBe("—");
  });

  it("formats numbers < 1000 exactly", () => {
    expect(formatContext(0)).toBe("0");
    expect(formatContext(999)).toBe("999");
  });

  it("formats numbers >= 1000 and < 10000 with 1 decimal k", () => {
    expect(formatContext(1000)).toBe("1.0k");
    expect(formatContext(1500)).toBe("1.5k");
    expect(formatContext(9999)).toBe("10.0k");
  });

  it("formats numbers >= 10000 and < 1000000 with integer k", () => {
    expect(formatContext(10000)).toBe("10k");
    expect(formatContext(10500)).toBe("11k"); // rounding
    expect(formatContext(999999)).toBe("1000k");
  });

  it("formats numbers >= 1000000 with 1 decimal M", () => {
    expect(formatContext(1000000)).toBe("1.0M");
    expect(formatContext(1500000)).toBe("1.5M");
  });
});

describe("formatCost", () => {
  it("formats costs to 4 decimals with $", () => {
    expect(formatCost(1.23)).toBe("$1.2300");
    expect(formatCost(0)).toBe("$0.0000");
    expect(formatCost("1.23456")).toBe("$1.2346");
    expect(formatCost(null)).toBe("$0.0000");
    expect(formatCost(undefined)).toBe("$0.0000");
    expect(formatCost(NaN)).toBe("$0.0000");
  });
});

describe("formatMs", () => {
  it("handles nullish inputs", () => {
    expect(formatMs(null)).toBe("—");
    expect(formatMs(undefined)).toBe("—");
  });

  it("rounds milliseconds and appends ms", () => {
    expect(formatMs(100)).toBe("100ms");
    expect(formatMs(100.4)).toBe("100ms");
    expect(formatMs(100.5)).toBe("101ms");
    expect(formatMs("200")).toBe("200ms");
  });
});

describe("formatNumber", () => {
  it("formats numbers using Intl.NumberFormat", () => {
    expect(formatNumber(1000)).toBe("1,000"); // default en-US locale in node/vitest
    expect(formatNumber(1234.56, { maximumFractionDigits: 1 })).toBe("1,234.6");
  });
});
