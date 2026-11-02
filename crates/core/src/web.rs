//! Embedded frontend transport. Assets are supplied by the executable's build, not by the core.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Sorted, compile-time frontend files: relative URL path and immutable bytes.
pub type EmbeddedWeb = &'static [(&'static str, &'static [u8])];

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
    if path == "api" || path.starts_with("api/") || path.starts_with("internal") {
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
    let mut response = Body::from(bytes).into_response();
    response.headers_mut().insert(header::CONTENT_TYPE, mime_guess::from_path(name).first_or_octet_stream().as_ref().parse().unwrap());
    response.headers_mut().insert(header::CONTENT_LENGTH, bytes.len().into());
    response.headers_mut().insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    if *request.method() == Method::HEAD {
        *response.body_mut() = Body::empty();
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASSETS: EmbeddedWeb =
        &[("assets/app.js", b"console.log('app')"), ("assets/font.woff2", b"font"), ("index.html", b"<!doctype html><title>App</title>")];

    fn response(method: &str, path: &str) -> Response {
        embedded_response(Request::builder().method(method).uri(path).body(Body::empty()).unwrap(), ASSETS)
    }

    #[tokio::test]
    async fn index_routes_query_strings_and_percent_encoded_assets() {
        for path in ["/", "/index.html", "/chat?bot=1"] {
            let response = response("GET", path);
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
            assert_eq!(axum::body::to_bytes(response.into_body(), 1024).await.unwrap(), ASSETS[2].1);
        }
        let response = response("GET", "/assets/%61pp.js?v=1");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers()[header::CONTENT_TYPE].to_str().unwrap().contains("javascript"));
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "18");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
    }

    #[tokio::test]
    async fn head_has_the_same_headers_and_no_body() {
        let get = response("GET", "/assets/font.woff2");
        let head = response("HEAD", "/assets/font.woff2");
        assert_eq!(head.status(), get.status());
        assert_eq!(head.headers(), get.headers());
        assert!(axum::body::to_bytes(head.into_body(), 1024).await.unwrap().is_empty());
    }

    #[test]
    fn missing_assets_internal_paths_traversal_and_writes_are_not_spa_routes() {
        for path in [
            "/assets/missing.js",
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
