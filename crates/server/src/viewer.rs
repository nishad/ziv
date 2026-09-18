//! Embedded OpenSeadragon viewer. Serves the vendored `crates/viewer-assets/assets/viewer/` tree
//! (`index.html`, the `viewer.js` and `viewer.css` it loads, the nav buttons and the
//! OpenSeadragon build), embedded once by `ziv-viewer-assets`, so the binary is a zero-install
//! viewer: `/viewer/` returns the page, `/viewer/{*file}` returns any other embedded asset (JS,
//! CSS, images).
use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use viewer_assets::ViewerAssets;

pub fn viewer_router() -> Router {
    Router::new()
        .route("/viewer/", get(index))
        .route("/viewer/{*file}", get(asset))
}

async fn index() -> Response {
    serve("index.html")
}

async fn asset(Path(file): Path<String>) -> Response {
    serve(&file)
}

/// Serves one embedded asset by path, or 404.
///
/// `pub(crate)` because the `/i/{name}/viewer/…` mount serves the same assets through
/// `routes::mount_handler` rather than through this module's own router. An empty path is the
/// index, which is what makes `/i/{name}/viewer/` work with and without a trailing slash.
pub(crate) fn serve_asset(path: &str) -> Response {
    serve(if path.is_empty() { "index.html" } else { path })
}

fn serve(path: &str) -> Response {
    match ViewerAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref())], content.data).into_response()
        }
        None => (StatusCode::NOT_FOUND, "404").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn serves_index_html_which_loads_the_viewer_files() {
        let html = body_text("/viewer/").await;
        for file in [
            "openseadragon/openseadragon.min.js",
            "nav/nav.js",
            "viewer.js",
            "viewer.css",
        ] {
            assert!(html.contains(file), "index.html must load {file}");
        }
    }

    /// The exporter turns this marker into static mode (see `exporter::viewer`). Exactly one, so
    /// the substitution can neither miss nor hit twice.
    #[tokio::test]
    async fn index_html_carries_the_server_mode_marker_exactly_once() {
        let html = body_text("/viewer/").await;
        assert_eq!(
            html.matches(r#"<meta name="ziv-mode" content="server">"#)
                .count(),
            1
        );
    }

    /// The controls are built from `/ziv/dimensions.json`. That wiring is JavaScript no Rust test
    /// can execute, so this pins the strings whose removal would break it silently. Behaviour is
    /// covered by the browser suite in `e2e/`.
    #[tokio::test]
    async fn viewer_js_wires_the_controls_to_the_dimensions_endpoint() {
        let js = body_text("/viewer/viewer.js").await;
        assert!(js.contains("/ziv/dimensions.json"));
        for token in ["\"z=\"", "\"t=\"", "\"c=\"", "\"label=\"", "\"overlay=\""] {
            assert!(
                js.contains(token),
                "viewer.js must build the {token} identifier part"
            );
        }
        // Inlined, or the browser requests /favicon.ico and logs a 404 on every page.
        let html = body_text("/viewer/").await;
        assert!(html.contains("rel=\"icon\"") && html.contains("data:image/svg+xml"));
    }

    /// `viewer.js` is the same file in both modes, so the only thing telling it where its assets
    /// live is what this page resolves. `ziv export` rewrites the marker that choice reads, and a
    /// server-mode page resolving to `./viewer/` would look for its files one level deep. The
    /// parts are pinned rather than the expression, so rewriting it stays free and losing half of
    /// it does not.
    #[tokio::test]
    async fn index_html_resolves_the_asset_base_from_the_mode_marker() {
        let html = body_text("/viewer/").await;
        for part in [
            "window.zivAssetBase",
            r#"meta[name="ziv-mode"]"#,
            r#""./viewer/""#,
            r#""/viewer/""#,
        ] {
            assert!(
                html.contains(part),
                "index.html must resolve its asset base from {part}"
            );
        }
    }

    #[tokio::test]
    async fn viewer_js_draws_the_nav_buttons_instead_of_the_sprites() {
        let js = body_text("/viewer/viewer.js").await;
        assert!(js.contains("showNavigationControl: false"));
        assert!(js.contains("zivNavigation(viewer)"));
        let nav = body_text("/viewer/nav/nav.js").await;
        assert!(nav.contains("window.zivNavigation"));
    }

    #[tokio::test]
    async fn serves_viewer_css_as_css() {
        let res = viewer_router()
            .oneshot(
                Request::builder()
                    .uri("/viewer/viewer.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let ct = res
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(ct.starts_with("text/css"), "{ct}");
    }

    /// GETs `uri` from the viewer router, asserting 200, and returns the body as text.
    async fn body_text(uri: &str) -> String {
        let res = viewer_router()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "GET {uri}");
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn serves_vendored_openseadragon_js() {
        let app = viewer_router();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/viewer/openseadragon/openseadragon.min.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let content_type = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.contains("javascript"),
            "unexpected content-type: {content_type}"
        );
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!bytes.is_empty());
    }

    /// `mount::split_mount` splits on the LAST marker segment, which is correct for every image
    /// name but relies on this server never generating a path that contains a marker segment
    /// AFTER the marker. Viewer asset paths are the only generated paths, so this is the whole
    /// invariant. Adding an asset under a directory called `iiif`, `ziv` or `viewer` would break
    /// image routing in a way no routing test would catch, so it is pinned here at the source.
    #[test]
    fn no_embedded_asset_path_contains_a_marker_segment() {
        for path in ViewerAssets::iter() {
            for segment in path.split('/') {
                assert!(
                    !crate::mount::MARKER_SEGMENTS.contains(&segment),
                    "asset {path} contains reserved segment {segment:?}; \
                     it would break /i/{{name}}/viewer/ routing"
                );
            }
        }
    }
}
