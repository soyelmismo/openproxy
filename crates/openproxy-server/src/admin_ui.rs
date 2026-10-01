//! Dashboard SPA embedded in the server binary.
//!
//! The frontend is built by `pnpm build` in `crates/openproxy-server/web/` into
//! `web/src/static/dist/`; `rust-embed` embeds only deployable HTML, bundles,
//! CSS and fonts, never TypeScript sources, tests or source maps.
//!
//! Routes mounted at `/admin/*` (not `/admin/api/*` or `/admin/ws` — those are
//! served by other handlers; see `router.rs::build_router` for the nesting):
//!
//! - `GET /admin`            → SPA shell (`index_html`)
//! - `GET /admin/`           → SPA shell (`index_html`)
//! - `GET /admin/callback.html` → OAuth callback page (`callback_html`)
//! - `GET /admin/dist/*`     → embedded built bundle
//! - `GET /admin/styles/*`   → embedded CSS
//! - `GET /admin/fonts/*`    → embedded fonts
//! - any other `/admin/*`    → 404 (the SPA uses hash routes)
//!
//! `index.html` and `callback.html` use `include_str!` rather than
//! `RustEmbed::get` so the handler returns `Html<&'static str>` with no owned
//! buffer.

use axum::{
    body::Body,
    extract::Path,
    http::{HeaderValue, StatusCode, Uri, header},
    response::{Html, IntoResponse, Response},
};
use mime_guess::from_path;
use rust_embed::RustEmbed;

/// Embedded copy of `crates/openproxy-server/web/src/static/`. The `#[folder]`
/// path resolves relative to this crate's `Cargo.toml`; deployable assets
/// are allowlisted so `index.html` can reference `/admin/dist/app.js`,
/// `/admin/styles/index.css` and `/admin/fonts/...` from one namespace.
///
/// `dist/` is esbuild output produced by `pnpm build` and gitignored, so a fresh
/// checkout has none; `rust-embed` still embeds HTML, CSS and fonts.
/// Language packs have their own JSON-only embedding. Release builds run
/// `pnpm build` before `cargo build` (see
/// `Dockerfile`, `.github/workflows/ci.yml`) to ship the full bundle.
#[derive(RustEmbed)]
#[folder = "web/src/static/"]
#[include = "index.html"]
#[include = "callback.html"]
#[include = "dist/**/*.js"]
#[include = "dist/**/*.css"]
#[include = "styles/**/*.css"]
#[include = "fonts/*"]
#[exclude = "**/*.map"]
#[exclude = "**/tests/**"]
struct DashboardAssets;

/// Embedded per-language JSON string packs consumed by the frontend's
/// `i18n/index.ts` `loadLang()`, served by [`serve_i18n`]. Folder path is
/// relative to this crate's `Cargo.toml` (same convention as [`DashboardAssets`]).
///
/// Only files present at compile time exist: a new `es.json` must land in
/// `web/src/static/src/i18n/` before rebuilding. Serving from disk at runtime is
/// deliberately not supported — the dashboard string contract is part of the
/// binary, not runtime config.
#[derive(RustEmbed)]
#[folder = "web/src/static/src/i18n/"]
#[include = "*.json"]
struct I18nAssets;

/// Serve the SPA shell. `include_str!` keeps this allocation-free
/// (`Html<&'static str>`).
pub async fn index_html() -> Response {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    (headers, Html(include_str!("../web/src/static/index.html"))).into_response()
}

/// Serve the OAuth callback page (static HTML that grabs the `code` query param
/// and `postMessage`s it back to the opener). Same `include_str!` as `index_html`.
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
/// Immutable caching is applied to content-addressed chunks (`dist/chunks/*`)
/// and binary fonts (`fonts/*`), while entry bundles (`dist/app.js`, `dist/app.css`)
/// are validated using SHA-256 ETags and HTTP 304 Not Modified responses.
pub async fn serve_asset(uri: Uri, req_headers: axum::http::HeaderMap) -> Response {
    let raw = uri.path();
    // `/admin/dist/app.js` → `dist/app.js`; a bare `/admin` (no slash) becomes
    // the empty path and falls through to the SPA shell below.
    let path = raw
        .strip_prefix("/admin")
        .unwrap_or(raw)
        .trim_start_matches('/');

    if path.is_empty() {
        return index_html().await;
    }

    if !is_public_asset(path) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let Some(file) = DashboardAssets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let hash = file.metadata.sha256_hash();
    let mut etag = String::with_capacity(66);
    etag.push('"');
    for byte in hash {
        use std::fmt::Write;
        let _ = write!(etag, "{byte:02x}");
    }
    etag.push('"');

    if req_headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|m| m.as_bytes() == etag.as_bytes())
    {
        let mut headers = axum::http::HeaderMap::new();
        if let Ok(val) = HeaderValue::from_str(&etag) {
            headers.insert(header::ETAG, val);
        }
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }

    let mime = from_path(path).first_or_octet_stream();
    let cache = if path.starts_with("fonts/") || path.starts_with("dist/chunks/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };

    let mut headers = axum::http::HeaderMap::new();
    if let Ok(ct) = HeaderValue::from_str(mime.as_ref()) {
        headers.insert(header::CONTENT_TYPE, ct);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(val) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, val);
    }
    let body = Body::from(file.data);
    (StatusCode::OK, headers, body).into_response()
}

fn is_public_asset(path: &str) -> bool {
    if path
        .split('/')
        .any(|segment| matches!(segment, ".." | "." | "src" | "tests"))
    {
        return false;
    }
    // Bundled asset names are lowercase build outputs; keep the allowlist
    // strict so path case differences are denied rather than served.
    // `rsplit_once('.')` is used instead of `ends_with` so dotfiles and
    // multi-dot names (e.g. `dist/app.js.map`) do not match `*.js`; the
    // comparison intentionally stays lowercase-only to preserve the denial
    // contract — outputs are `*.js`/`*.css`, never `*.JS`/`*.CSS`.
    match path {
        "index.html" | "callback.html" => true,
        _ if path.starts_with("dist/") => path
            .rsplit_once('.')
            .is_some_and(|(_, ext)| ext == "js" || ext == "css"),
        _ if path.starts_with("styles/") => {
            path.rsplit_once('.').is_some_and(|(_, ext)| ext == "css")
        }
        _ => {
            path.starts_with("fonts/") && path.rsplit_once('.').is_none_or(|(_, ext)| ext != "map")
        }
    }
}

/// `GET /admin/i18n/{lang}` — serve a language pack.
///
/// `i18n/index.ts::loadLang()` calls this at boot with `/admin/i18n/en.json`.
pub async fn serve_i18n(lang: Path<String>) -> Response {
    let lang = lang.0.strip_suffix(".json").unwrap_or(&lang.0);
    // Letters, digits, hyphen, underscore: every ISO 639-1 code plus regional
    // variants (`pt-BR`, `zh-Hans`). Anything else is rejected so the
    // embedded-tree lookup cannot be probed with a crafted path.
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
    let filename = format!("{lang}.json");
    let Some(file) = I18nAssets::get(&filename) else {
        return (
            StatusCode::NOT_FOUND,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            )],
            Body::from(r#"{"error":"Language pack not found"}"#),
        )
            .into_response();
    };

    let body = Body::from(file.data);
    (
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
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn denies_source_tests_maps_and_unknown_assets() {
        for path in [
            "src/main.ts",
            "src/i18n/en.json",
            "tests/test.js",
            "dist/app.js.map",
            "styles/index.css.map",
            "../index.html",
            "package.json",
            "missing",
        ] {
            assert!(DashboardAssets::get(path).is_none(), "embedded {path}");
            let response = serve_asset(
                format!("/admin/{path}").parse().unwrap(),
                Default::default(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[test]
    fn preserves_styles_fonts_and_bundles() {
        for path in DashboardAssets::iter() {
            assert!(is_public_asset(&path), "unexpected embedded asset: {path}");
        }
        assert!(DashboardAssets::get("styles/index.css").is_some());
        assert!(DashboardAssets::get("fonts/Ubuntu-Regular.ttf").is_some());
        assert!(I18nAssets::get("en.json").is_some());
    }
}
