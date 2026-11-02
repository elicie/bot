//! Embedded frontend transport. Assets are supplied by the executable's build, not by the core.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

/// Sorted, compile-time frontend files: relative URL path and immutable bytes.
pub type EmbeddedWeb = &'static [(&'static str, &'static [u8])];

fn content_hashed_asset(name: &str) -> bool {
    // Vite's default asset names end in `-<8 URL-safe hash characters>.<extension>`.
    let Some((stem, _)) = name.strip_prefix("assets/").and_then(|name| name.rsplit_once('.')) else { return false };
    let Some(suffix) = stem.as_bytes().get(stem.len().saturating_sub(9)..) else { return false };
    suffix.len() == 9 && suffix[0] == b'-' && suffix[1..].iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

pub(crate) fn embedded_response(request: Request, assets: EmbeddedWeb) -> Response {
    let path = request.uri().path();
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(decoded) = percent_encoding::percent_decode_str(path).decode_utf8() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if decoded.contains(['\\', '\0']) || decoded.split('/').any(|part| matches!(part, "." | "..")) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = decoded.trim_start_matches('/');
    // Axum matches API routes before URL decoding; encoded reserved paths must not become SPA routes.
    if path == "api" || path.starts_with("api/") || path == "internal" || path.starts_with("internal/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let lookup = |name: &str| assets.binary_search_by_key(&name, |(name, _)| name).ok().map(|index| assets[index]);
    let asset = lookup(if path.is_empty() { "index.html" } else { path }).or_else(|| {
        // Browser routes may use the app shell; a missing hashed asset must not return HTML.
        (!path.starts_with("assets/")).then(|| lookup("index.html")).flatten()
    });
    let Some((name, bytes)) = asset else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag = format!("\"{}\"", hex::encode(Sha256::digest(bytes)));
    let unchanged = request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').map(str::trim).any(|value| value == "*" || value.strip_prefix("W/").unwrap_or(value) == etag));
    let mut response = if unchanged { StatusCode::NOT_MODIFIED.into_response() } else { Body::from(bytes).into_response() };
    response.headers_mut().insert(header::ETAG, etag.parse().unwrap());
    let cache = if content_hashed_asset(name) { "public, max-age=31536000, immutable" } else { "no-cache" };
    response.headers_mut().insert(header::CACHE_CONTROL, cache.parse().unwrap());
    if !unchanged {
        response.headers_mut().insert(header::CONTENT_TYPE, mime_guess::from_path(name).first_or_octet_stream().as_ref().parse().unwrap());
        response.headers_mut().insert(header::CONTENT_LENGTH, bytes.len().into());
    }
    if *request.method() == Method::HEAD {
        *response.body_mut() = Body::empty();
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML: &[u8] = b"<!doctype html><title>App</title>";
    const ASSETS: EmbeddedWeb =
        &[("assets/app-1234_-78.js", b"console.log('app')"), ("assets/app.js", b"unhashed"), ("assets/font-12345678.woff2", b"font"), ("index.html", HTML)];

    fn response(method: &str, path: &str) -> Response {
        embedded_response(Request::builder().method(method).uri(path).body(Body::empty()).unwrap(), ASSETS)
    }

    #[tokio::test]
    async fn index_routes_query_strings_and_percent_encoded_assets() {
        for path in ["/", "/index.html", "/chat?bot=1", "/internals", "/internal-preview"] {
            let response = response("GET", path);
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
            assert_eq!(axum::body::to_bytes(response.into_body(), 1024).await.unwrap(), HTML);
        }
        let asset = response("GET", "/assets/%61pp-1234_-78.js?v=1");
        assert_eq!(asset.status(), StatusCode::OK);
        assert!(asset.headers()[header::CONTENT_TYPE].to_str().unwrap().contains("javascript"));
        assert_eq!(asset.headers()[header::CONTENT_LENGTH], "18");
        assert_eq!(asset.headers()[header::CACHE_CONTROL], "public, max-age=31536000, immutable");
        assert_eq!(response("GET", "/assets/app.js").headers()[header::CACHE_CONTROL], "no-cache");
    }

    #[tokio::test]
    async fn head_has_the_same_headers_and_no_body() {
        let get = response("GET", "/assets/font-12345678.woff2");
        let head = response("HEAD", "/assets/font-12345678.woff2");
        assert_eq!(head.status(), get.status());
        assert_eq!(head.headers(), get.headers());
        assert!(axum::body::to_bytes(head.into_body(), 1024).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn etags_revalidate_index_and_assets_without_a_body() {
        for path in ["/", "/assets/app-1234_-78.js"] {
            let original = response("GET", path);
            let etag = original.headers()[header::ETAG].to_str().unwrap();
            for value in [etag.to_owned(), format!("W/{etag}"), format!("\"older\", {etag}"), "*".into()] {
                for method in ["GET", "HEAD"] {
                    let request = Request::builder().method(method).uri(path).header(header::IF_NONE_MATCH, &value).body(Body::empty()).unwrap();
                    let response = embedded_response(request, ASSETS);
                    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
                    assert_eq!(response.headers()[header::ETAG], etag);
                    assert_eq!(response.headers()[header::CACHE_CONTROL], original.headers()[header::CACHE_CONTROL]);
                    assert!(axum::body::to_bytes(response.into_body(), 1024).await.unwrap().is_empty());
                }
            }
            let request = Request::builder().uri(path).header(header::IF_NONE_MATCH, "\"older\"").body(Body::empty()).unwrap();
            assert_eq!(embedded_response(request, ASSETS).status(), StatusCode::OK);
        }
    }

    #[test]
    fn missing_assets_internal_paths_traversal_and_writes_are_not_spa_routes() {
        for path in [
            "/assets/missing.js",
            "/internal",
            "/internal/missing",
            "/%69nternal/missing",
            "/internal%2fmissing",
            "/api",
            "/api/missing",
            "/%61pi/missing",
            "/api%2fmissing",
            "/%2e%2e/secrets",
            "/assets/../index.html",
            "/assets/%5cfile",
            "/%00",
        ] {
            assert_eq!(response("GET", path).status(), StatusCode::NOT_FOUND, "{path}");
        }
        assert_eq!(response("POST", "/").status(), StatusCode::NOT_FOUND);
        assert_eq!(response("GET", "/%ff").status(), StatusCode::BAD_REQUEST);
    }
}
