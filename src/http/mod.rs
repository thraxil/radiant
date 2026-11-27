//! HTTP routes.

use axum::Router;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::request::{ParseOutcome, parse_image_request};

pub fn router() -> Router {
    Router::new()
        .route("/image/{hash}/{size}/{filename}", get(serve_image))
        .route("/healthz", get(healthz))
        .route("/favicon.ico", get(favicon))
}

async fn serve_image(Path((hash, size, filename)): Path<(String, String, String)>) -> Response {
    match parse_image_request(&hash, &size, &filename) {
        // 301 to match reticulum. axum's `Redirect::permanent` would send 308.
        ParseOutcome::Redirect(to) => {
            (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, to)]).into_response()
        }
        ParseOutcome::NotFound(why) => (StatusCode::NOT_FOUND, why).into_response(),
        // Serving arrives in M4.
        ParseOutcome::Ok(_req) => StatusCode::NOT_IMPLEMENTED.into_response(),
    }
}

async fn healthz() -> &'static str {
    "ok"
}

/// Reticulum returns an empty 200 here, and so do we.
async fn favicon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/x-icon")], "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const H: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";

    async fn get(uri: &str) -> Response {
        router()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn healthz_ok() {
        assert_eq!(get("/healthz").await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn non_canonical_size_is_permanent_redirect() {
        let res = get(&format!("/image/{H}/200h100w/image.jpeg")).await;
        assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(
            res.headers()[header::LOCATION],
            format!("/image/{H}/100w200h/image.jpg").as_str()
        );
    }

    #[tokio::test]
    async fn percent_encoded_filename_survives_redirect() {
        let res = get(&format!("/image/{H}/0100w/my%20photo.png")).await;
        assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(
            res.headers()[header::LOCATION],
            format!("/image/{H}/100w/my%20photo.png").as_str()
        );
    }

    #[tokio::test]
    async fn bad_hash_is_404() {
        assert_eq!(
            get("/image/nothex/full/image.jpg").await.status(),
            StatusCode::NOT_FOUND
        );
    }
}
