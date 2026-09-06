// components/log-detail/json-prune.ts — JSON pruning / truncation for the
// log-detail viewer. Every function here transforms a JSON value into a
// display-friendly, bounded representation (truncated strings, capped
// `messages` arrays, depth-limited recursion).
//
// `formatJson` is the single entry point consumed by the other modules;
// the rest are module-internal helpers.
//
// Split out of the former components/log-detail.ts monolith (Q19).

function truncateText(text: string, maxLen = 150): string {
  if (text.length <= maxLen) return text;
  return text.slice(0, maxLen) + `… [truncated, ${text.length - maxLen} chars]`;
}

function pruneTextBlock(block: unknown, maxLen = 150): unknown {
  if (!block || typeof block !== "object" || Array.isArray(block)) return block;
  const b = { ...(block as Record<string, unknown>) };
  if (typeof b["text"] === "string" && b["text"].length > maxLen) {
    b["text"] = truncateText(b["text"], maxLen);
  }
  return b;
}

function pruneMessageContent(content: unknown, maxLen = 150): unknown {
  if (typeof content === "string") {
    return truncateText(content, maxLen);
  }
  if (Array.isArray(content)) {
    return (content as unknown[]).map((part) => pruneTextBlock(part, maxLen));
  }
  return content;
}

function pruneSingleMessage(msg: unknown): unknown {
  if (!msg || typeof msg !== "object" || Array.isArray(msg)) return msg;
  const m = { ...(msg as Record<string, unknown>) };
  m["content"] = pruneMessageContent(m["content"]);
  return m;
}

function pruneMessagesArray(arr: unknown[]): unknown[] {
  const MAX_MESSAGES = 10;
  if (arr.length > MAX_MESSAGES) {
    const head = arr.slice(0, 5).map(pruneSingleMessage);
    const tail = arr.slice(arr.length - 2).map(pruneSingleMessage);
    return [...head, `… [${arr.length - 7} messages omitted, total ${arr.length} items]`, ...tail];
  }
  return arr.map(pruneSingleMessage);
}

function pruneSystemField(item: unknown, depth: number): unknown {
  if (typeof item === "string") {
    return truncateText(item);
  }
  if (Array.isArray(item)) {
    return (item as unknown[]).map((block) => pruneTextBlock(block));
  }
  return pruneValueForDisplay(item, depth + 1);
}

function tryParseJsonString(val: string): unknown | null {
  const trimmed = val.trim();
  const isJsonEnclosed =
    (trimmed.startsWith("{") && trimmed.endsWith("}")) ||
    (trimmed.startsWith("[") && trimmed.endsWith("]"));
  if (!isJsonEnclosed) return null;
  try {
    return JSON.parse(val);
  } catch (_e) {
    return null;
  }
}

function pruneObjectForDisplay(obj: Record<string, unknown>, depth: number): Record<string, unknown> {
  const res: Record<string, unknown> = {};
  const keys = Object.keys(obj);
  const normalKeys = keys.filter((k) => k !== "messages" && k !== "request_body_json");
  const orderedKeys = [
    ...normalKeys,
    ...(keys.includes("request_body_json") ? ["request_body_json"] : []),
    ...(keys.includes("messages") ? ["messages"] : []),
  ];

  for (const key of orderedKeys) {
    const item = obj[key];
    if (key === "messages" && Array.isArray(item)) {
      res[key] = pruneMessagesArray(item as unknown[]);
    } else if (key === "system") {
      res[key] = pruneSystemField(item, depth);
    } else {
      res[key] = pruneValueForDisplay(item, depth + 1);
    }
  }
  return res;
}

function pruneValueForDisplay(val: unknown, depth = 0): unknown {
  if (depth > 10 || val == null) return val;

  if (typeof val === "string") {
    const parsed = tryParseJsonString(val);
    return parsed !== null ? pruneValueForDisplay(parsed, depth + 1) : val;
  }

  if (Array.isArray(val)) {
    return val.map((item) => pruneValueForDisplay(item, depth + 1));
  }

  if (typeof val === "object") {
    return pruneObjectForDisplay(val as Record<string, unknown>, depth);
  }

  return val;
}

/** Pretty-print a JSON value for display inside a `<pre>` tag.
 *  lit-html auto-escapes the returned string when interpolated
 *  via `${...}`, so we no longer escape here. Returns "(empty)"
 *  for null/undefined so the viewer always has something to
 *  render. */
export function formatJson(value: unknown): string {
  if (value == null) return "(empty)";
  try {
    const pruned = pruneValueForDisplay(value);
    return typeof pruned === "string" ? pruned : JSON.stringify(pruned, null, 2);
  } catch (_e: unknown) {
    return String(value);
  }
}
