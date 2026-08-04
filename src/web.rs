//! Web application delivery.

use std::borrow::Cow;

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::Response,
};
use rust_embed::RustEmbed;
use thiserror::Error;

const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";
const CACHE_INDEX: &str = "no-store";
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; connect-src 'self'; script-src 'self'; style-src 'self'";

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct EmbeddedAssets;

pub(crate) trait AssetProvider: Clone + Send + Sync + 'static {
    fn get(&self, path: &str) -> Option<Cow<'static, [u8]>>;
}

#[derive(Clone)]
struct ProductionAssets;

impl AssetProvider for ProductionAssets {
    fn get(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        EmbeddedAssets::get(path).map(|asset| asset.data)
    }
}

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[error("embedded web application is unavailable")]
pub enum WebBuildError {
    MissingIndex,
}

/// Builds the embedded web router, failing safely when the frontend has not
/// been built before the Rust binary.
pub fn web_router() -> Result<Router, WebBuildError> {
    router_with_assets(ProductionAssets)
}

pub(crate) fn router_with_assets<P>(assets: P) -> Result<Router, WebBuildError>
where
    P: AssetProvider,
{
    if assets
        .get("index.html")
        .is_none_or(|index| index.is_empty())
    {
        return Err(WebBuildError::MissingIndex);
    }

    Ok(with_security_headers(
        Router::new().fallback(web_fallback::<P>).with_state(assets),
    ))
}

/// Installs the response-header contract without inspecting request content.
pub fn with_security_headers(router: Router) -> Router {
    router.layer(middleware::from_fn(security_headers_middleware))
}

async fn security_headers_middleware(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

async fn web_fallback<P>(State(assets): State<P>, request: Request) -> Response
where
    P: AssetProvider,
{
    let method = request.method().clone();
    if !matches!(method, Method::GET | Method::HEAD) {
        return empty_response(StatusCode::NOT_FOUND);
    }

    let path = request.uri().path().trim_start_matches('/');
    if !path.is_empty()
        && safe_asset_path(path)
        && let Some(asset) = assets.get(path)
    {
        return asset_response(path, asset, method == Method::HEAD);
    }

    if path.is_empty() || is_spa_route(path) {
        let index = assets
            .get("index.html")
            .expect("web router validated the embedded index");
        return asset_response("index.html", index, method == Method::HEAD);
    }

    empty_response(StatusCode::NOT_FOUND)
}

fn safe_asset_path(path: &str) -> bool {
    if path.contains('\\') {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    if lower.contains("%2e") || lower.contains("%2f") || lower.contains("%5c") {
        return false;
    }
    path.split('/')
        .all(|component| !component.is_empty() && !matches!(component, "." | ".."))
}

fn is_spa_route(path: &str) -> bool {
    safe_asset_path(path) && !path.starts_with("assets/")
}

fn asset_response(path: &str, bytes: Cow<'static, [u8]>, head: bool) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let content_type = match mime.essence_str() {
        "text/html" => "text/html; charset=utf-8",
        "text/css" => "text/css; charset=utf-8",
        "text/javascript" | "application/javascript" => "text/javascript; charset=utf-8",
        other => other,
    };
    let cache_control = if path == "index.html" {
        CACHE_INDEX
    } else if is_hashed_asset(path) {
        CACHE_IMMUTABLE
    } else {
        CACHE_INDEX
    };
    let body = if head {
        Body::empty()
    } else {
        Body::from(bytes.into_owned())
    };
    let mut response = Response::new(body);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    response
}

fn is_hashed_asset(path: &str) -> bool {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let stem = file_name.split('.').next().unwrap_or(file_name);
    stem.rsplit_once('-').is_some_and(|(_, hash)| {
        hash.len() >= 8 && hash.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

fn empty_response(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, collections::HashMap};

    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode, header},
    };
    use tower::ServiceExt;

    use super::{AssetProvider, WebBuildError, router_with_assets};

    const CSP: &str = "default-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; connect-src 'self'; script-src 'self'; style-src 'self'";

    #[derive(Clone)]
    struct FixtureAssets(HashMap<&'static str, &'static [u8]>);

    impl FixtureAssets {
        fn complete() -> Self {
            Self(HashMap::from([
                (
                    "index.html",
                    b"<!doctype html><div id=\"root\"></div>".as_slice(),
                ),
                (
                    "assets/index-AbC123xy.js",
                    b"console.log('cellar')".as_slice(),
                ),
                ("assets/index-ZyX987ab.css", b"body{}".as_slice()),
            ]))
        }
    }

    impl AssetProvider for FixtureAssets {
        fn get(&self, path: &str) -> Option<Cow<'static, [u8]>> {
            self.0.get(path).map(|bytes| Cow::Borrowed(*bytes))
        }
    }

    async fn response(method: Method, uri: &str) -> axum::response::Response {
        router_with_assets(FixtureAssets::complete())
            .unwrap()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    fn assert_security_headers(response: &axum::response::Response) {
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .unwrap(),
            CSP
        );
        assert_eq!(
            response
                .headers()
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .unwrap(),
            "nosniff"
        );
        assert_eq!(
            response.headers().get(header::REFERRER_POLICY).unwrap(),
            "no-referrer"
        );
    }

    #[tokio::test]
    async fn root_serves_index_without_caching_and_with_security_headers() {
        let response = response(Method::GET, "/").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_security_headers(&response);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "<!doctype html><div id=\"root\"></div>"
        );
    }

    #[tokio::test]
    async fn hashed_assets_have_mime_types_immutable_cache_and_security_headers() {
        let javascript = response(Method::GET, "/assets/index-AbC123xy.js").await;
        assert_eq!(javascript.status(), StatusCode::OK);
        assert_eq!(
            javascript.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            javascript.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable"
        );
        assert_security_headers(&javascript);

        let stylesheet = response(Method::GET, "/assets/index-ZyX987ab.css").await;
        assert_eq!(stylesheet.status(), StatusCode::OK);
        assert_eq!(
            stylesheet.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            stylesheet.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable"
        );
    }

    #[tokio::test]
    async fn safe_get_and_head_routes_fall_back_to_bodyless_or_full_index() {
        let get = response(Method::GET, "/projects/0198f67e").await;
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(
            get.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_eq!(
            to_bytes(get.into_body(), usize::MAX).await.unwrap(),
            "<!doctype html><div id=\"root\"></div>"
        );

        let head = response(Method::HEAD, "/projects/0198f67e").await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(
            head.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            head.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_security_headers(&head);
        assert!(
            to_bytes(head.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn missing_asset_and_path_tricks_are_not_spa_routes() {
        for path in [
            "/assets/missing.js",
            "/assets/../index.html",
            "/assets/%2e%2e/index.html",
            "/assets\\index-AbC123xy.js",
        ] {
            let response = response(Method::GET, path).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert_security_headers(&response);
            assert!(
                to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .is_empty(),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn safe_non_asset_path_with_extension_falls_back_to_index() {
        let response = response(Method::GET, "/missing.txt").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_security_headers(&response);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "<!doctype html><div id=\"root\"></div>"
        );
    }

    #[tokio::test]
    async fn unsafe_fallback_methods_never_receive_spa_html() {
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            let response = response(method.clone(), "/projects/new").await;
            assert!(matches!(
                response.status(),
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ));
            assert_security_headers(&response);
            assert!(
                to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn head_asset_has_get_headers_and_no_body() {
        let head = response(Method::HEAD, "/assets/index-AbC123xy.js").await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(
            head.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            head.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable"
        );
        assert!(
            to_bytes(head.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn missing_or_empty_index_is_a_safe_build_error() {
        let missing = router_with_assets(FixtureAssets(HashMap::new())).unwrap_err();
        let empty = router_with_assets(FixtureAssets(HashMap::from([(
            "index.html",
            b"".as_slice(),
        )])))
        .unwrap_err();

        assert_eq!(missing, WebBuildError::MissingIndex);
        assert_eq!(empty, WebBuildError::MissingIndex);
        assert_eq!(
            missing.to_string(),
            "embedded web application is unavailable"
        );
        assert!(!missing.to_string().contains("dist"));
    }
}
