//! Dashboard SPA embedded in the server binary.
//!
//! The frontend is built by `pnpm build` in `crates/openproxy-web/` and
//! emits to `crates/openproxy-web/src/static/dist/`. We embed the whole
//! `crates/openproxy-web/src/static/` tree at compile time via
//! `rust-embed`, so the server binary is self-contained and ships both
//! the API and the dashboard on the same port.
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
    http::{HeaderValue, StatusCode, Uri, header},
    response::{Html, IntoResponse, Response},
};
use mime_guess::from_path;
use rust_embed::RustEmbed;

/// Embedded copy of `crates/openproxy-web/src/static/`.
///
/// `rust-embed` resolves the `#[folder]` path relative to this crate's
/// `Cargo.toml`, so `../openproxy-web/src/static/` points at
/// `crates/openproxy-web/src/static/`. We embed the whole tree (not
/// just `dist/`) so the SPA's `index.html` can reference
/// `/admin/dist/app.js`, `/admin/styles/index.css`, and
/// `/admin/fonts/...` from a single embedded namespace.
///
/// A `.gitkeep` placeholder lives in `dist/` so a fresh checkout
/// (where `pnpm build` hasn't run yet) still compiles — `rust-embed`
/// happily embeds an empty directory.
#[derive(RustEmbed)]
#[folder = "../openproxy-web/src/static/"]
struct DashboardAssets;

/// Serve the SPA shell. The HTML is `include_str!`-embedded so the
/// handler returns `Html<&'static str>` with no allocation.
pub async fn index_html() -> Html<&'static str> {
    Html(include_str!("../../openproxy-web/src/static/index.html"))
}

/// Serve the OAuth callback page (a tiny static HTML file that grabs
/// the `code` query param and `postMessage`s it back to the opener
/// window). Same `include_str!` strategy as `index_html`.
pub async fn callback_html() -> Html<&'static str> {
    Html(include_str!("../../openproxy-web/src/static/callback.html"))
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
        let cache = if path.starts_with("dist/") {
            "no-cache, no-store, must-revalidate"
        } else {
            "no-cache, no-store, must-revalidate"
        };
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
