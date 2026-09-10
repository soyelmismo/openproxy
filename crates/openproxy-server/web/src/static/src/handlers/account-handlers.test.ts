import { describe, it, expect } from "vitest";
import {
  parseBulkApiKeys,
  cleanApiKeyToken,
  looksLikeApiKey,
  isGenericLabel,
} from "./account-handlers.js";

describe("helpers", () => {
  it("cleanApiKeyToken strips quotes and delimiters", () => {
    expect(cleanApiKeyToken('"sk-12345",')).toBe("sk-12345");
  });
  it("looksLikeApiKey recognizes known key patterns", () => {
    expect(looksLikeApiKey("sk-ant-api03-abcdef1234567890")).toBe(true);
    expect(looksLikeApiKey("short")).toBe(false);
  });
  it("isGenericLabel detects standard names", () => {
    expect(isGenericLabel("OPENAI_KEY")).toBe(true);
    expect(isGenericLabel("prod_key")).toBe(false);
  });
});

describe("parseBulkApiKeys", () => {
  it("returns empty array on empty or whitespace input", () => {
    expect(parseBulkApiKeys("")).toEqual([]);
    expect(parseBulkApiKeys("   \n\t  \n  ")).toEqual([]);
  });

  it("parses single key per line correctly", () => {
    const input = `sk-ant-api03-abcdef1234567890
sk-ant-api03-uvwxyz0987654321
sk-proj-12345678901234567890`;
    const res = parseBulkApiKeys(input);
    expect(res).toEqual([
      { key: "sk-ant-api03-abcdef1234567890" },
      { key: "sk-ant-api03-uvwxyz0987654321" },
      { key: "sk-proj-12345678901234567890" },
    ]);
  });

  it("strips quotes, trailing commas and semicolons", () => {
    const input = `"sk-ant-api03-abcdef1234567890",
'sk-ant-api03-uvwxyz0987654321';
\`sk-proj-12345678901234567890\``;
    const res = parseBulkApiKeys(input);
    expect(res).toEqual([
      { key: "sk-ant-api03-abcdef1234567890" },
      { key: "sk-ant-api03-uvwxyz0987654321" },
      { key: "sk-proj-12345678901234567890" },
    ]);
  });

  it("handles list bullets, numbering, and brackets", () => {
    const input = `1. sk-ant-api03-abcdef1234567890
2) sk-ant-api03-uvwxyz0987654321
- sk-proj-12345678901234567890
* sk-proj-09876543210987654321
• sk-proj-abcdefabcdefabcdefab
[1] sk-proj-fedcbafedcbafedcba`;
    const res = parseBulkApiKeys(input);
    expect(res.map((r) => r.key)).toEqual([
      "sk-ant-api03-abcdef1234567890",
      "sk-ant-api03-uvwxyz0987654321",
      "sk-proj-12345678901234567890",
      "sk-proj-09876543210987654321",
      "sk-proj-abcdefabcdefabcdefab",
      "sk-proj-fedcbafedcbafedcba",
    ]);
  });

  it("extracts labels from key-value pairs (label: key, label | key)", () => {
    const input = `Production: sk-ant-api03-abcdef1234567890
Team Beta | sk-ant-api03-uvwxyz0987654321
Staging = sk-proj-12345678901234567890
Dev Key\tsk-proj-09876543210987654321`;
    const res = parseBulkApiKeys(input);
    expect(res).toEqual([
      { key: "sk-ant-api03-abcdef1234567890", label: "Production" },
      { key: "sk-ant-api03-uvwxyz0987654321", label: "Team Beta" },
      { key: "sk-proj-12345678901234567890", label: "Staging" },
      { key: "sk-proj-09876543210987654321", label: "Dev Key" },
    ]);
  });

  it("filters out generic labels like api_key or secret", () => {
    const input = `api_key: sk-ant-api03-abcdef1234567890
secret: sk-ant-api03-uvwxyz0987654321
OPENAI_API_KEY: sk-proj-12345678901234567890`;
    const res = parseBulkApiKeys(input);
    expect(res).toEqual([
      { key: "sk-ant-api03-abcdef1234567890" },
      { key: "sk-ant-api03-uvwxyz0987654321" },
      { key: "sk-proj-12345678901234567890" },
    ]);
  });

  it("extracts labels from inline comments (#, //)", () => {
    const input = `sk-ant-api03-abcdef1234567890 # personal account
sk-ant-api03-uvwxyz0987654321 // work account`;
    const res = parseBulkApiKeys(input);
    expect(res).toEqual([
      { key: "sk-ant-api03-abcdef1234567890", label: "personal account" },
      { key: "sk-ant-api03-uvwxyz0987654321", label: "work account" },
    ]);
  });

  it("handles env variables with export", () => {
    const input = `export ANTHROPIC_API_KEY="sk-ant-api03-abcdef1234567890"
OPENAI_KEY='sk-proj-12345678901234567890'
CUSTOM_PROD_KEY=sk-proj-09876543210987654321`;
    const res = parseBulkApiKeys(input);
    expect(res).toEqual([
      { key: "sk-ant-api03-abcdef1234567890" },
      { key: "sk-proj-12345678901234567890" },
      { key: "sk-proj-09876543210987654321", label: "CUSTOM_PROD_KEY" },
    ]);
  });

  it("handles Bearer token prefix", () => {
    const input = `Bearer sk-ant-api03-abcdef1234567890
bearer sk-proj-12345678901234567890`;
    const res = parseBulkApiKeys(input);
    expect(res).toEqual([
      { key: "sk-ant-api03-abcdef1234567890" },
      { key: "sk-proj-12345678901234567890" },
    ]);
  });

  it("parses JSON array of strings or objects", () => {
    const jsonStrings = JSON.stringify([
      "sk-ant-api03-abcdef1234567890",
      "sk-ant-api03-uvwxyz0987654321",
    ]);
    expect(parseBulkApiKeys(jsonStrings)).toEqual([
      { key: "sk-ant-api03-abcdef1234567890" },
      { key: "sk-ant-api03-uvwxyz0987654321" },
    ]);

    const jsonObjects = JSON.stringify([
      { api_key: "sk-ant-api03-abcdef1234567890", label: "Key 1" },
      { secret: "sk-ant-api03-uvwxyz0987654321", name: "Key 2" },
    ]);
    expect(parseBulkApiKeys(jsonObjects)).toEqual([
      { key: "sk-ant-api03-abcdef1234567890", label: "Key 1" },
      { key: "sk-ant-api03-uvwxyz0987654321", label: "Key 2" },
    ]);
  });

  it("parses JSON key-value map", () => {
    const jsonMap = JSON.stringify({
      AccountA: "sk-ant-api03-abcdef1234567890",
      AccountB: "sk-ant-api03-uvwxyz0987654321",
    });
    expect(parseBulkApiKeys(jsonMap)).toEqual([
      { key: "sk-ant-api03-abcdef1234567890", label: "AccountA" },
      { key: "sk-ant-api03-uvwxyz0987654321", label: "AccountB" },
    ]);
  });

  it("parses multiple keys separated by commas or semicolons on a single line", () => {
    const input = `sk-proj-11111111111111111111, sk-proj-22222222222222222222; sk-proj-33333333333333333333`;
    const res = parseBulkApiKeys(input);
    expect(res).toEqual([
      { key: "sk-proj-11111111111111111111" },
      { key: "sk-proj-22222222222222222222" },
      { key: "sk-proj-33333333333333333333" },
    ]);
  });

  it("parses dirty natural language text containing keys", () => {
    const input = `Here are the active keys: sk-proj-1234567890abcdefghijklmn and also the backup sk-ant-api03-abcdefghijklmnopqrstuv. Please rotate them tomorrow!`;
    const res = parseBulkApiKeys(input);
    expect(res).toEqual([
      { key: "sk-proj-1234567890abcdefghijklmn" },
      { key: "sk-ant-api03-abcdefghijklmnopqrstuv" },
    ]);
  });

  it("parses Gemini, Groq, HuggingFace, Replicate, UUID, and Hex keys", () => {
    const input = `AIzaSyB1234567890abcdef1234567890abcdef
gsk_123456789012345678901234567890
hf_abcdefghijklmnopqrstuvwxyz0123456789
r8_123456789012345678901234567890abcdef
12345678-1234-1234-1234-123456789abc
0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef`;
    const res = parseBulkApiKeys(input);
    expect(res).toEqual([
      { key: "AIzaSyB1234567890abcdef1234567890abcdef" },
      { key: "gsk_123456789012345678901234567890" },
      { key: "hf_abcdefghijklmnopqrstuvwxyz0123456789" },
      { key: "r8_123456789012345678901234567890abcdef" },
      { key: "12345678-1234-1234-1234-123456789abc" },
      { key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" },
    ]);
  });

  it("deduplicates identical keys while preserving order", () => {
    const input = `sk-ant-api03-abcdef1234567890
sk-ant-api03-abcdef1234567890
sk-proj-12345678901234567890`;
    const res = parseBulkApiKeys(input);
    expect(res).toEqual([
      { key: "sk-ant-api03-abcdef1234567890" },
      { key: "sk-proj-12345678901234567890" },
    ]);
  });

  it("handles JWT, Vertex OAuth, custom prefixes (pawan, chutes, modal, cloudflare), 128-hex, and future providers", () => {
    const input = `
# Pawan key (letters only, no digits)
pk-MockPawanKeyOnlyLettersMixedCaseExampleString
# Chutes key
cpk_chutes_test_1234567890abcdef1234567890
# Modal key
wk-MockModalToken1234567890abcdef
# Cloudflare Workers AI
cfut_1234567890abcdef1234567890abcdef
# Vertex OAuth token
AQ.MockVertexToken1234567890abcdef1234567890
# JWT token (synthetic RFC 7519 example)
eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c
# 128-char hex mock
0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
# Future unknown provider key
futureai-1234567890abcdef1234567890
`;
    const res = parseBulkApiKeys(input);
    expect(res.map((r) => r.key)).toEqual([
      "pk-MockPawanKeyOnlyLettersMixedCaseExampleString",
      "cpk_chutes_test_1234567890abcdef1234567890",
      "wk-MockModalToken1234567890abcdef",
      "cfut_1234567890abcdef1234567890abcdef",
      "AQ.MockVertexToken1234567890abcdef1234567890",
      "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
      "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
      "futureai-1234567890abcdef1234567890",
    ]);
  });

  it("extracts multiple mixed-format keys from dirty lines and logs", () => {
    const dirtyLine = `curl -H "Authorization: Bearer cpk_chutes_1234567890abcdef" -H "X-Secondary: pk-MockPawanKeyOnlyLettersMixedCaseExampleString" https://api.example.com`;
    const res = parseBulkApiKeys(dirtyLine);
    expect(res.map((r) => r.key)).toEqual([
      "cpk_chutes_1234567890abcdef",
      "pk-MockPawanKeyOnlyLettersMixedCaseExampleString",
    ]);
  });
});
