export function summarizeApiKey(key: string): string {
  const trimmed = key.trim();
  if (trimmed.length <= 10) return trimmed;
  const prefix = trimmed.slice(0, 6);
  const suffix = trimmed.slice(-4);
  return `${prefix}...${suffix}`;
}

export interface ParsedKeyEntry {
  key: string;
  label?: string | undefined;
}

export function cleanApiKeyToken(token: string): string {
  return token.replace(/^["'`(\[{<\s]+|["'`)\]}>,;\.\s]+$/g, "").trim();
}

export function looksLikeApiKey(token: string): boolean {
  const cleaned = cleanApiKeyToken(token);
  if (cleaned.length < 8) return false;

  // 1. Google AI Studio (AIzaSy...) & Vertex OAuth/refresh (AQ.Ab...)
  if (/^AIza[0-9A-Za-z\-_]{30,}$/.test(cleaned)) return true;
  if (/^AQ\.[a-zA-Z0-9_\-\.]{20,}$/.test(cleaned)) return true;

  // 2. JWT tokens (eyJh...)
  if (/^eyJh[a-zA-Z0-9_\-]+\.[a-zA-Z0-9_\-]+\.[a-zA-Z0-9_\-]+$/.test(cleaned)) return true;

  // 3. UUIDs: 8-4-4-4-12
  if (/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(cleaned)) {
    return true;
  }

  // 4. Hex strings >= 32 chars (support 32 up to 128 hex chars)
  if (/^[0-9a-f]{32,128}$/i.test(cleaned)) {
    return true;
  }

  // 5. Generalized prefixed keys: 2-10 alphanumeric prefix + '-' or '_' + >= 12 chars token
  if (/^[a-zA-Z0-9]{2,10}[-_][a-zA-Z0-9_\-\.]{12,}$/.test(cleaned)) {
    const hasDigit = /[0-9]/.test(cleaned);
    const hasMixed = /[a-z]/.test(cleaned) && /[A-Z]/.test(cleaned);
    if (hasDigit || hasMixed) return true;
  }

  // 6. Generic high-entropy base64 or alphanumeric tokens
  if (
    /^[A-Za-z0-9_\-\.]{20,}$/.test(cleaned) &&
    /[0-9]/.test(cleaned) &&
    /[a-zA-Z]/.test(cleaned)
  ) {
    return true;
  }

  return false;
}

const GENERIC_KEY_NAMES = new Set([
  "api_key",
  "apikey",
  "api-key",
  "key",
  "secret",
  "token",
  "access_token",
  "authorization",
  "bearer",
  "openai_api_key",
  "openai_key",
  "openaikey",
  "anthropic_api_key",
  "anthropic_key",
  "gemini_api_key",
  "gemini_key",
  "groq_api_key",
  "groq_key",
  "openrouter_api_key",
  "openrouter_key",
  "password",
  "credential",
]);

export function isGenericLabel(label: string): boolean {
  const lower = label.toLowerCase().trim();
  const norm = lower.replace(/[-_\s]/g, "");
  if (GENERIC_KEY_NAMES.has(lower) || GENERIC_KEY_NAMES.has(norm)) {
    return true;
  }
  return (
    norm.endsWith("apikey") ||
    norm.endsWith("secret") ||
    (norm.endsWith("key") &&
      (norm.startsWith("openai") ||
        norm.startsWith("anthropic") ||
        norm.startsWith("gemini") ||
        norm.startsWith("groq") ||
        norm.startsWith("openrouter")))
  );
}

export function deduplicateKeys(entries: ParsedKeyEntry[]): ParsedKeyEntry[] {
  const seen = new Set<string>();
  const result: ParsedKeyEntry[] = [];
  for (const entry of entries) {
    const k = cleanApiKeyToken(entry.key);
    if (!k || seen.has(k)) continue;
    seen.add(k);
    const lbl = entry.label?.trim();
    result.push({
      key: k,
      label: lbl && !isGenericLabel(lbl) ? lbl : undefined,
    });
  }
  return result;
}

export function extractKeysFromDirtyLine(line: string): ParsedKeyEntry[] {
  const seen = new Set<string>();
  const results: ParsedKeyEntry[] = [];

  const add = (k: string): void => {
    const cleaned = cleanApiKeyToken(k);
    if (cleaned && looksLikeApiKey(cleaned) && !seen.has(cleaned)) {
      seen.add(cleaned);
      results.push({ key: cleaned });
    }
  };

  const jwtRegex = /\b(eyJh[a-zA-Z0-9_\-]+\.[a-zA-Z0-9_\-]+\.[a-zA-Z0-9_\-]+)\b/g;
  let m: RegExpExecArray | null;
  while ((m = jwtRegex.exec(line)) !== null) {
    if (m[1]) add(m[1]);
  }

  const googleRegex = /\b(AIza[0-9A-Za-z\-_]{30,}|AQ\.[a-zA-Z0-9_\-\.]{20,})\b/g;
  while ((m = googleRegex.exec(line)) !== null) {
    if (m[1]) add(m[1]);
  }

  const prefixRegex = /\b([a-zA-Z0-9]{2,10}[-_][a-zA-Z0-9_\-\.]{12,})\b/g;
  while ((m = prefixRegex.exec(line)) !== null) {
    if (m[1]) add(m[1]);
  }

  const uuidRegex = /\b([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\b/gi;
  while ((m = uuidRegex.exec(line)) !== null) {
    if (m[1]) add(m[1]);
  }

  const hexRegex = /\b([0-9a-f]{32,128})\b/gi;
  while ((m = hexRegex.exec(line)) !== null) {
    if (m[1]) add(m[1]);
  }

  const tokens = line.split(/[\s,;"'<>`()\[\]{}|=:]+/);
  for (const t of tokens) {
    add(t);
  }

  return results;
}

export function parseBulkApiKeys(rawText: string): ParsedKeyEntry[] {
  if (!rawText || !rawText.trim()) return [];

  const text = rawText.trim();

  if (text.startsWith("[") || text.startsWith("{")) {
    try {
      const parsed = JSON.parse(text);
      if (Array.isArray(parsed)) {
        const entries: ParsedKeyEntry[] = [];
        for (const item of parsed) {
          if (typeof item === "string" && item.trim()) {
            entries.push({ key: item.trim() });
          } else if (item && typeof item === "object") {
            const rec = item as Record<string, unknown>;
            const rawKey = rec["api_key"] ?? rec["key"] ?? rec["secret"] ?? rec["token"] ?? rec["apiKey"];
            const k = typeof rawKey === "string" ? rawKey : undefined;
            if (k && k.trim()) {
              const rawLbl = rec["label"] ?? rec["name"] ?? rec["id"];
              const lbl = typeof rawLbl === "string" ? rawLbl.trim() : undefined;
              entries.push({ key: k.trim(), label: lbl });
            }
          }
        }
        if (entries.length > 0) {
          return deduplicateKeys(entries);
        }
      } else if (parsed && typeof parsed === "object") {
        const entries: ParsedKeyEntry[] = [];
        for (const [key, val] of Object.entries(parsed)) {
          if (typeof val === "string" && val.trim()) {
            entries.push({ key: val.trim(), label: key });
          }
        }
        if (entries.length > 0) {
          return deduplicateKeys(entries);
        }
      }
    } catch {
      // Fall through
    }
  }

  const entries: ParsedKeyEntry[] = [];
  const lines = text.split(/\r?\n/);

  for (const rawLine of lines) {
    let line = rawLine.trim();
    if (!line) continue;

    // Check if line is purely a comment (starts with # or //)
    if (/^(?:#|\/\/)/.test(line)) {
      const strippedComment = line.replace(/^(?:#|\/\/)\s*/, "").trim();
      if (looksLikeApiKey(strippedComment) || (!strippedComment.includes(" ") && strippedComment.length >= 8)) {
        line = strippedComment;
      } else {
        const matches = extractKeysFromDirtyLine(line);
        if (matches.length > 0) {
          entries.push(...matches);
        }
        continue;
      }
    }

    // Strip markdown list markers: "1. ", "- ", "* ", "• ", "[1] "
    line = line.replace(/^(?:[-*+•]|\d+[\.\)]|\[\d+\])\s+/, "").trim();

    // Check for inline comment: "key # label" or "key // label"
    let inlineComment: string | undefined;
    const commentIdx = line.search(/\s+(?:#|\/\/)\s+/);
    if (commentIdx !== -1) {
      inlineComment = line.slice(commentIdx).replace(/^\s*(?:#|\/\/)\s*/, "").trim();
      line = line.slice(0, commentIdx).trim();
    }

    // Check for env var syntax: export KEY="val" or KEY="val"
    const envMatch = line.match(/^(?:export\s+)?([A-Za-z0-9_]+)\s*=\s*(.+)$/);
    if (envMatch && envMatch[1] && envMatch[2]) {
      const varName = envMatch[1];
      const val = cleanApiKeyToken(envMatch[2]);
      if (val) {
        entries.push({
          key: val,
          label: isGenericLabel(varName) ? inlineComment : varName,
        });
        continue;
      }
    }

    // Check for delimiter `:` or `=` or `|` or `\t` (e.g. "Label: sk-...")
    if (!line.startsWith("http://") && !line.startsWith("https://")) {
      const delimMatch = line.match(/^([^:=|\t]+)\s*[:=|\t]\s*(.+)$/);
      if (delimMatch && delimMatch[1] && delimMatch[2]) {
        const left = cleanApiKeyToken(delimMatch[1]);
        let right = cleanApiKeyToken(delimMatch[2]);
        if (/^bearer\s+/i.test(right)) {
          right = cleanApiKeyToken(right.replace(/^bearer\s+/i, ""));
        }
        if (looksLikeApiKey(right) || (!right.includes(" ") && right.length >= 8)) {
          entries.push({
            key: right,
            label: isGenericLabel(left) ? inlineComment : left,
          });
          continue;
        }
        if (looksLikeApiKey(left) || (!left.includes(" ") && left.length >= 8)) {
          entries.push({
            key: left,
            label: inlineComment || (isGenericLabel(right) ? undefined : right),
          });
          continue;
        }
      }
    }

    // Strip Bearer prefix if present
    if (/^bearer\s+/i.test(line)) {
      line = cleanApiKeyToken(line.replace(/^bearer\s+/i, ""));
    }

    // Strip outer quotes
    line = cleanApiKeyToken(line);

    // If line has no spaces and is >= 6 characters:
    // It's a clean 1-key-per-line entry
    if (!line.includes(" ") && line.length >= 6) {
      entries.push({
        key: line,
        label: inlineComment,
      });
      continue;
    }

    // If line has multiple comma or semicolon separated tokens
    if (line.includes(",") || line.includes(";")) {
      const parts = line.split(/[,;]/).map(cleanApiKeyToken).filter(Boolean);
      const allLookLikeKeys =
        parts.length > 1 &&
        parts.every((p) => looksLikeApiKey(p) || (!p.includes(" ") && p.length >= 10));
      if (allLookLikeKeys) {
        for (const p of parts) {
          entries.push({ key: p });
        }
        continue;
      }
    }

    // Dirty line / sentence / free text
    const extracted = extractKeysFromDirtyLine(line);
    if (extracted.length > 0) {
      entries.push(...extracted);
    }
  }

  return deduplicateKeys(entries);
}
