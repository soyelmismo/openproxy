// The server's `admin_auth_middleware` requires a `Bearer <token>` header on
// every `/admin/api/*` request. A browser cannot set that header on the
// `/admin/ws` upgrade, so `state/ws.ts` exchanges the token for a single-use
// 30-second ticket (`POST /admin/api/ws-ticket`) and connects with `?ticket=`.
// The API key never goes in a URL: reverse proxies log the request line.
//
// The token is the user's manage-scope API key, identical to what a CLI client
// would use, so a logout or a server-side revocation means re-entering it.

import { state } from "./index.js";

// localStorage, not a cookie: the dashboard has no CSRF protection, and a
// cookie would ride along on every same-origin request. localStorage is
// readable only by same-origin JS, the trust boundary the dashboard already
// runs under.
const STORAGE_KEY = "openproxy_admin_token";

// Memory cache, seeded from localStorage on first read, so the hot `api()`
// path skips a synchronous localStorage read per fetch.
let currentToken: string | null = null;

/** Read the token from localStorage. Null when unset, empty, or when
 *  localStorage throws (private mode, blocked cookies). */
function load(): string | null {
  try {
    const v: string | null = localStorage.getItem(STORAGE_KEY);
    if (!v) return null;
    return v;
  } catch {
    // SecurityError under blocked cookies: treat as no token stored.
    return null;
  }
}

/** Persist the token and cache it. Trims whitespace: keys are pasted from CLI
 *  output that often carries a trailing newline. */
export function setToken(token: string): void {
  currentToken = token.trim();
  try {
    localStorage.setItem(STORAGE_KEY, currentToken);
  } catch (e: unknown) {
    // Persistence failed (quota or disabled storage); the in-memory copy
    // survives this session and the user re-enters the key after a reload.
    console.warn("Could not persist admin token to localStorage:", e);
  }
}

/** Clear the token. The next `api()` call sends no Authorization header, the
 *  server answers 401, and the router's auth gate redirects to the login view. */
export function clearToken(): void {
  currentToken = null;
  clearProfile();
  try {
    localStorage.removeItem(STORAGE_KEY);
  } catch {
    // Nothing persisted to remove; the in-memory copy is already null.
  }
}

/** Current token, seeding the cache from localStorage on first access. Null
 *  means the server will answer 401. */
export function getToken(): string | null {
  if (currentToken === null) {
    currentToken = load();
  }
  return currentToken;
}

export interface UserSessionProfile {
  key_id: number | null;
  key_prefix: string | null;
  label: string | null;
  scopes: string[];
  is_admin: boolean;
}

const PROFILE_KEY = "openproxy_user_profile";
let currentProfile: UserSessionProfile | null = null;

function loadProfile(): UserSessionProfile | null {
  try {
    const raw = localStorage.getItem(PROFILE_KEY);
    if (!raw) return null;
    return JSON.parse(raw) as UserSessionProfile;
  } catch {
    return null;
  }
}

export function setProfile(profile: UserSessionProfile): void {
  currentProfile = profile;
  try {
    localStorage.setItem(PROFILE_KEY, JSON.stringify(profile));
  } catch (e: unknown) {
    console.warn("Could not persist user profile to localStorage:", e);
  }
}

export function getProfile(): UserSessionProfile | null {
  if (currentProfile === null) {
    currentProfile = loadProfile();
  }
  return currentProfile;
}

export function clearProfile(): void {
  currentProfile = null;
  try {
    localStorage.removeItem(PROFILE_KEY);
  } catch {
    // ignore
  }
}

export function isFullAdmin(): boolean {
  const profile = getProfile();
  if (!profile) return true; // optimistic fallback
  return profile.is_admin;
}

/** Whether a token is present. Drives the router's auth gate. */
export function isLoggedIn(): boolean {
  return getToken() !== null;
}

/** Check if a given key id or prefix matches the active session token. */
export function isCurrentSessionKey(
  keyIdOrObj: number | { id?: number; key_prefix?: string | null },
  keyPrefix?: string | null,
): boolean {
  const token = getToken();
  if (!token) return false;
  const id = typeof keyIdOrObj === "object" ? keyIdOrObj.id : keyIdOrObj;
  const prefix = (typeof keyIdOrObj === "object" ? keyIdOrObj.key_prefix : keyPrefix) ?? null;
  if (prefix && token.startsWith(prefix)) return true;
  if (id != null) {
    const found = (state.apiKeys || []).find((k) => (k as { id: number }).id === id);
    if (found?.key_prefix && token.startsWith(found.key_prefix)) {
      return true;
    }
  }
  return false;
}

/** Clear the stored token and route back to the login view. */
export function invalidateSession(): void {
  clearToken();
  if (typeof location !== "undefined" && !location.hash.startsWith("#/login")) {
    location.hash = "#/login";
  }
}

