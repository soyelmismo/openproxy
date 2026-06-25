//! Dashboard SPA embedded in the server binary.
//!
//! The frontend is built by `pnpm build` in `crates/openproxy-server/web/`
//! and emits to `crates/openproxy-server/web/src/static/dist/`. We embed
//! the whole `crates/openproxy-server/web/src/static/` tree at compile
//! time via `rust-embed`, so the server binary is self-contained and
//! ships both the API and the dashboard on the same port.
//!
//! Routes mounted at `/admin/*` (NOT `/admin/api/*` or `/admin/ws` —
//! those are the API and WS, served by other handlers). See
//! `router.rs::build_router` for the nesting structure:
//!
//! - `GET /admin`            → SPA shell (`index_html`)
//! - `GET /admin/`           → SPA shell (`index_html`)
//! - `GET /admin/callback.html` → OAuth callback page (`callback_html`)
//! - `GET /admin/dist/*`     → embedded built bundle (this module)
//! - `GET /admin/styles/*`   → embedded CSS (this module)
//! - `GET /admin/fonts/*`    → embedded fonts (this module)
//! - any other `/admin/*`    → SPA fallback to `index.html` (this module)
//!
//! `index.html` and `callback.html` are pulled in via `include_str!`
//! (rather than `RustEmbed::get`) because they're the SPA entry points
//! and we want them as `&'static str` so `Html<&'static str>` can be
//! returned without an owned-buffer hop.

use axum::{
    body::Body,
    extract::Path,
    http::{HeaderValue, StatusCode, Uri, header},
    response::{Html, IntoResponse, Response},
};
use mime_guess::from_path;
use rust_embed::RustEmbed;

/// Embedded copy of `crates/openproxy-server/web/src/static/`.
///
/// `rust-embed` resolves the `#[folder]` path relative to this crate's
/// `Cargo.toml`, so `web/src/static/` points at
/// `crates/openproxy-server/web/src/static/`. We embed the whole tree
/// (not just `dist/`) so the SPA's `index.html` can reference
/// `/admin/dist/app.js`, `/admin/styles/index.css`, and
/// `/admin/fonts/...` from a single embedded namespace.
///
/// `dist/` is the esbuild output — it's produced by `pnpm build` and
/// is in the frontend's `.gitignore`, so a fresh checkout has no
/// `dist/` directory. `rust-embed` happily embeds the rest of the tree
/// (HTML, CSS, fonts, i18n JSON) without it; a real release build runs
/// `pnpm build` before `cargo build` (see `Dockerfile` and
/// `.github/workflows/ci.yml`) so the binary ships with the full
/// dashboard bundle.
#[derive(RustEmbed)]
#[folder = "web/src/static/"]
struct DashboardAssets;

/// Embedded copy of `crates/openproxy-server/web/src/static/src/i18n/`
/// — the per-language JSON string packs consumed by the frontend's
/// `i18n/index.ts` `loadLang()` helper.
///
/// Served at `/admin/i18n/{lang}.json` by [`serve_i18n`]. The folder
/// path is relative to this crate's `Cargo.toml` (same convention as
/// [`DashboardAssets`]).
///
/// Only files present at compile time are exposed; if a future
/// translation is added as `es.json`, it must land in
/// `crates/openproxy-server/web/src/static/src/i18n/` before the server
/// is rebuilt — `rust-embed` bakes the tree into the binary. We do NOT
/// serve from disk at runtime, so operators can't drop new language
/// packs into a running server; that's intentional (the dashboard
/// string contract is part of the binary, not a runtime config).
#[derive(RustEmbed)]
#[folder = "web/src/static/src/i18n/"]
struct I18nAssets;

/// Serve the SPA shell. The HTML is `include_str!`-embedded so the
/// handler returns `Html<&'static str>` with no allocation.
pub async fn index_html() -> Html<&'static str> {
    Html(include_str!("../web/src/static/index.html"))
}

/// Serve the OAuth callback page (a tiny static HTML file that grabs
/// the `code` query param and `postMessage`s it back to the opener
/// window). Same `include_str!` strategy as `index_html`.
pub async fn callback_html() -> Html<&'static str> {
    Html(include_str!("../web/src/static/callback.html"))
}

/// Serve a static asset from the embedded `src/static/` tree.
///
/// The router mounts this handler as the `fallback` for `/admin/*`,
/// so the URI we receive is the full request path (e.g.
/// `/admin/dist/app.js`). We strip the leading `/admin/` (or
/// `/admin`) segment, then look the rest up in the embedded tree.
///
/// If the asset exists, we serve it with the `mime_guess`-derived
/// Content-Type and an aggressive cache header for hashed bundles
/// (`dist/*`) or a no-cache header for everything else (HTML, CSS,
/// fonts — anything that could change between deploys without a
/// filename change).
///
/// If the asset doesn't exist, we fall back to `index_html` so
/// client-side SPA routes (e.g. `/admin/combos/42/edit`) keep
/// working — the SPA's hash-router takes over and renders the right
/// view. This is the standard "SPA fallback" pattern.
///
/// Path traversal is blocked: any `..` segment short-circuits to the
/// SPA fallback (no asset in the embedded tree is named with `..`
/// anyway, but the explicit guard keeps the handler obviously safe).
pub async fn serve_asset(uri: Uri) -> Response {
    let raw = uri.path();
    // Strip the `/admin/` prefix (or `/admin` with no trailing slash).
    // `strip_prefix("/admin/")` covers `/admin/dist/app.js` →
    // `dist/app.js`; the fallback handles `/admin` (no slash) by
    // returning the SPA shell.
    let stripped = raw
        .strip_prefix("/admin/")
        .or_else(|| raw.strip_prefix("/admin"))
        .unwrap_or(raw);
    let path = stripped.trim_start_matches('/');

    if path.is_empty() || path.contains("..") {
        return index_html().await.into_response();
    }

    if let Some(file) = DashboardAssets::get(path) {
        let mime = from_path(path).first_or_octet_stream();
        // `dist/` is the esbuild output — bundles that change content
        // on every build. We don't currently content-hash filenames
        // (esbuild emits `app.js`, not `app.<hash>.js`), so an
        // immutable cache would be a footgun on redeploys. Keep the
        // no-cache policy for now; if we add content hashing later,
        // flip `dist/` to `public, max-age=31536000, immutable`.
        let cache = "no-cache, no-store, must-revalidate";
        let mut headers = axum::http::HeaderMap::new();
        // `mime.as_ref()` is `&str`; `HeaderValue::from_str` only
        // fails on invisible ASCII / control chars, which a
        // `mime_guess`-derived type never contains.
        if let Ok(ct) = HeaderValue::from_str(mime.as_ref()) {
            headers.insert(header::CONTENT_TYPE, ct);
        }
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
        let body = Body::from(file.data.into_owned());
        return (StatusCode::OK, headers, body).into_response();
    }

    // SPA fallback: unknown `/admin/*` paths (e.g. client-side
    // routes like `/admin/combos/42/edit`) get the SPA shell so the
    // hash-router can take over.
    index_html().await.into_response()
}

/// `GET /admin/i18n/{lang}` — serve a language pack.
///
/// The frontend's `i18n/index.ts::loadLang()` calls this at boot (before
/// the first render) to pull the user's language strings, fetching the
/// URL `/admin/i18n/en.json`. The route is registered as `/i18n/{lang}`
/// (NOT `/i18n/{lang}.json` — axum 0.8 rejects literal-suffix path
/// params, see `router.rs`), so the captured `lang` value can be either
/// `en` or `en.json` depending on the caller. We strip the optional
/// `.json` extension before lookup so both URLs work.
///
/// The response is the raw JSON file as embedded by [`I18nAssets`]; we
/// set `Content-Type: application/json; charset=utf-8` and
/// `Cache-Control: public, max-age=86400` (24h) because:
///
///   - The pack is content-addressed in the binary: a server upgrade
///     ships a new binary, and the SPA's `app.js` is also re-fetched
///     (no-cache, see [`serve_asset`]). The 24h ceiling is short
///     enough that the next day the browser will pick up a refreshed
///     pack after a server upgrade, and long enough to keep the boot
///     path off the network on subsequent same-day reloads.
///
///   - `force-cache` on the fetch side (frontend) makes the browser
///     cache hit immediate, so the second-boot path is one round-trip
///     cheaper.
///
/// Returns `404 language not found` if `lang` doesn't have a matching
/// `.json` in the embedded tree. The frontend's `loadLang` falls back
/// to `en` in that case.
///
/// Path-traversal safety: axum's `Path<String>` extractor captures a
/// single path segment for `{lang}` (no `/`), so `..` and `/` are not
/// reachable here. We additionally validate `lang` against
/// `[a-zA-Z0-9_-]+` after stripping `.json` — a future `pt-BR` code
/// is the most exotic shape we'd ship, and this guard keeps the
/// lookup table closed.
pub async fn serve_i18n(lang: Path<String>) -> Response {
    let mut lang = lang.0;
    // Strip the optional `.json` suffix so the route also accepts
    // `/admin/i18n/en` (without the extension) — both URLs hit the
    // same asset.
    if lang.ends_with(".json") {
        lang.truncate(lang.len() - ".json".len());
    }
    // Allow letters, digits, hyphen, underscore — covers every ISO 639-1
    // code plus regional variants (`pt-BR`, `zh-Hans`). Reject anything
    // else so the embedded-tree lookup can never be probed with a
    // crafted path.
    if !lang
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || lang.is_empty()
    {
        return (
            StatusCode::NOT_FOUND,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            "language not found",
        )
            .into_response();
    }
    let filename = format!("{}.json", lang);
    if let Some(file) = I18nAssets::get(&filename) {
        let body = Body::from(file.data.into_owned());
        return (
            StatusCode::OK,
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json; charset=utf-8"),
                ),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("public, max-age=86400"),
                ),
            ],
            body,
        )
            .into_response();
    }
    (
        StatusCode::NOT_FOUND,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        "language not found",
    )
        .into_response()
}
