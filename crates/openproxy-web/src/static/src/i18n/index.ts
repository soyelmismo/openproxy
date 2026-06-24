// Minimal i18n client. Loads a single language pack from the backend at
// boot, exposes a `t(key, params?)` function with {{param}} interpolation.
//
// Pluralization: keys ending in `_one` / `_other` are picked based on a
// `count` param. English plural rule: count === 1 → _one, else _other.
// For languages with more plural forms (Arabic, Russian), the plural rule
// function would need to be extended — see the `make-plural` library if
// you need it later.

export type Lang = 'en'; // | 'es' | 'fr' | ... — add when we ship more

interface Strings {
  [key: string]: string;
}

let currentLang: Lang = 'en';
let strings: Strings = {};
let loadPromise: Promise<void> | null = null;

/** Fetch a language pack from the backend and set it as the active language. */
export async function loadLang(lang: Lang): Promise<void> {
  if (loadPromise) return loadPromise; // dedupe concurrent calls
  loadPromise = (async () => {
    try {
      const res = await fetch(`/admin/i18n/${lang}.json`, {
        cache: 'force-cache', // hashed content; safe to cache aggressively
      });
      if (!res.ok) {
        console.error(`i18n: failed to load ${lang}:`, res.status);
        if (lang !== 'en') {
          // Fall back to English silently. NOTE: this branch is
          // unreachable while Lang = 'en' only — see concerns in the
          // F3 worklog entry for a latent self-reference bug if more
          // languages are added without refactoring this fn first.
          return loadLang('en');
        }
        return;
      }
      strings = await res.json();
      currentLang = lang;
    } catch (e) {
      console.error('i18n: network error loading', lang, e);
    } finally {
      loadPromise = null;
    }
  })();
  return loadPromise;
}

/** Synchronous translate. Returns the key itself if not found. */
export function t(key: string, params?: Record<string, string | number>): string {
  let template = strings[key];
  if (template == null) {
    // Fall back to the key itself — visible in the UI so missing strings
    // are obvious during development.
    return key;
  }
  // Pluralization: if `count` is in params and the key has _one/_other
  // variants, pick the right one.
  if (params && typeof params['count'] === 'number') {
    const pluralKey = params['count'] === 1 ? `${key}_one` : `${key}_other`;
    const pluralTemplate = strings[pluralKey];
    if (pluralTemplate != null) {
      template = pluralTemplate;
    }
  }
  // {{param}} interpolation
  if (params) {
    for (const [k, v] of Object.entries(params)) {
      template = template.replace(new RegExp(`\\{\\{\\s*${k}\\s*\\}\\}`, 'g'), String(v));
    }
  }
  return template;
}

/** Get the current language code. */
export function getLang(): Lang {
  return currentLang;
}

/** Check if a string is loaded (for conditional UI). */
export function isLoaded(): boolean {
  return Object.keys(strings).length > 0;
}
