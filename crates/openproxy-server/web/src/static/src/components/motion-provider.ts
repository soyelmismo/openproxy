// components/motion-provider.ts — respect the user's `prefers-reduced-motion`
// system preference. Toggles a `reduced-motion` class on
// `<html>` so the CSS layer can neutralise animations and
// transitions. Also listens for live changes so a user who flips
// the OS setting during a session gets the new behavior
// immediately without a page reload.
//
// The CSS counterpart (`.reduced-motion * { animation-duration:
// 0.01ms !important; transition-duration: 0.01ms !important;
// scroll-behavior: auto !important; }`) lives in
// `styles/base.css` and overrides every per-element rule via
// cascade order. The `!important` is intentional and scoped —
// users who explicitly opt in get all motion suppressed; users
// who don't see no change at all.

type MediaQueryListChangeHandler = (e: MediaQueryListEvent) => void;

function apply(reduce: boolean): void {
  document.documentElement.classList.toggle("reduced-motion", reduce);
}

let installed: boolean = false;

/** Install the motion provider. Idempotent — safe to call from
 *  the boot sequence and from any later re-render path. */
export function installMotionProvider(): void {
  if (installed) return;
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
  const mql: MediaQueryList = window.matchMedia("(prefers-reduced-motion: reduce)");
  apply(mql.matches);
  // `addEventListener` is the modern API; the legacy `addListener`
  // shim only exists on very old Safari (< 14) which we don't
  // target. `MediaQueryListEvent` is the standard change-event
  // payload carrying the new `.matches` value.
  const handler: MediaQueryListChangeHandler = (e: MediaQueryListEvent): void => {
    apply(e.matches);
  };
  mql.addEventListener("change", handler);
  installed = true;
}
