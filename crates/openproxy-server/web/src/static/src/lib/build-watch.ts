// Build fingerprint watchdog.
//
// A tab keeps running the bundle it loaded, and a bundle can outlive the
// endpoints it calls: after a deploy that retires an API, the stale tab keeps
// requesting it and surfaces the server's refusal to the user. There is no way
// for already-loaded code to know it is stale, so every boot records the
// current build id and a later mismatch — a deploy replaced the binary —
// reloads the tab once onto the fresh bundle.
const STORAGE_KEY = "openproxy_build_id";
const POLL_MS = 120_000;

async function fetchBuildId(): Promise<string | null> {
  try {
    const res = await fetch("/admin/build.json", {
      cache: "no-store",
      headers: { Accept: "application/json" },
    });
    if (!res.ok) return null;
    const body = (await res.json()) as { id?: unknown };
    return typeof body.id === "string" && body.id.length > 0 ? body.id : null;
  } catch {
    return null;
  }
}

function check(id: string, reload: () => void): void {
  const seen = localStorage.getItem(STORAGE_KEY);
  if (seen === null) {
    localStorage.setItem(STORAGE_KEY, id);
    return;
  }
  if (seen === id) return;
  // Record the new id BEFORE reloading: if the reload somehow lands on a
  // cached copy of the old bundle, the next check must not loop.
  localStorage.setItem(STORAGE_KEY, id);
  reload();
}

/** Start the watchdog. Returns a disposer (used by tests). */
export function installBuildWatch(reload: () => void = () => location.reload()): () => void {
  const tick = (): void => {
    void fetchBuildId().then((id) => {
      if (id) check(id, reload);
    });
  };
  tick();
  const timer = window.setInterval(() => {
    if (document.visibilityState === "visible") tick();
  }, POLL_MS);
  const onVisible = (): void => {
    if (document.visibilityState === "visible") tick();
  };
  document.addEventListener("visibilitychange", onVisible);
  return () => {
    window.clearInterval(timer);
    document.removeEventListener("visibilitychange", onVisible);
  };
}
