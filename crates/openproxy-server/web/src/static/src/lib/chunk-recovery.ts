// lib/chunk-recovery.ts — self-healing for lazy-view chunk loads.
//
// A deploy replaces the hashed chunks under /admin/dist/chunks/. A tab opened
// before the deploy still runs the old app.js and asks for chunks that no
// longer exist: every lazy route then dies with "Failed to fetch dynamically
// imported module". The server keeps the previous generation (build.mjs chunk
// retention), but if a chunk is still missing (two deploys in one session) the
// only correct recovery is a full reload: the fresh HTML + app.js reference the
// chunks that exist NOW.
//
// The guard key rate-limits reloads so a genuinely broken build cannot put the
// tab into a reload loop.

const GUARD_KEY = "op:chunk-reload-at";
const RELOAD_TTL_MS = 60_000;
const CLEAR_AFTER_MS = 10_000;

/** Chrome/Edge/Firefox/Safari phrasings of a failed dynamic import. */
export function isChunkLoadError(reason: unknown): boolean {
  const msg = reason instanceof Error ? reason.message : String(reason ?? "");
  return /dynamically imported module|Importing a module script failed|error loading dynamically imported module|ChunkLoadError|Loading chunk \d+ failed/i.test(
    msg,
  );
}

export function installChunkRecovery(): void {
  const tryReload = (reason: unknown): void => {
    if (!isChunkLoadError(reason)) return;
    let last = 0;
    try {
      last = Number(sessionStorage.getItem(GUARD_KEY) ?? 0);
    } catch {
      // Private mode with storage disabled: reload once per page life.
    }
    if (Date.now() - last < RELOAD_TTL_MS) return;
    try {
      sessionStorage.setItem(GUARD_KEY, String(Date.now()));
    } catch {
      /* no-op: the reload below still runs */
    }
    location.reload();
  };

  window.addEventListener("unhandledrejection", (e) => {
    tryReload(e.reason);
  });
  // Script/link load failures surface as capture-phase `error` events on the
  // element (they do not bubble).
  window.addEventListener(
    "error",
    (e) => {
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "SCRIPT" || t.tagName === "LINK")) {
        tryReload(new Error("error loading dynamically imported module"));
      }
    },
    true,
  );
  // A healthy boot clears the guard, so the NEXT deploy can recover again.
  window.addEventListener(
    "load",
    () => {
      setTimeout(() => {
        try {
          sessionStorage.removeItem(GUARD_KEY);
        } catch {
          /* ignore */
        }
      }, CLEAR_AFTER_MS);
    },
    { once: true },
  );
}
