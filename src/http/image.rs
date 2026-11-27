use std::time::{Duration, SystemTime};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::AppState;
use crate::request::{ParseOutcome, parse_image_request};
use crate::service::Served;

const ONE_YEAR: Duration = Duration::from_secs(365 * 24 * 60 * 60);

pub(super) async fn serve(
    State(state): State<AppState>,
    Path((hash, size, filename)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let req = match parse_image_request(&hash, &size, &filename) {
        ParseOutcome::Ok(req) => req,
        // 301 to match reticulum. axum's `Redirect::permanent` would send 308.
        ParseOutcome::Redirect(to) => {
            return (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, to)]).into_response();
        }
        ParseOutcome::NotFound(why) => return (StatusCode::NOT_FOUND, why).into_response(),
    };
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());

    match state.service.get(&req, if_none_match).await {
        Ok(Served::NotModified { etag }) => {
            let mut res = StatusCode::NOT_MODIFIED.into_response();
            set_cache_headers(res.headers_mut(), &etag);
            res
        }
        Ok(Served::Image {
            bytes,
            format,
            etag,
        }) => {
            let mut res = bytes.into_response();
            let h = res.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(format.mime_type()),
            );
            set_cache_headers(h, &etag);
            res
        }
        Err(e) => e.into_response(),
    }
}

/// Everything under a hash is immutable, so it can be cached forever.
fn set_cache_headers(h: &mut HeaderMap, etag: &str) {
    if let Ok(v) = HeaderValue::from_str(&format!("\"{etag}\"")) {
        h.insert(header::ETAG, v);
    }
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    // Reticulum sent Expires; keep it for old caches.
    if let Ok(v) = HeaderValue::from_str(&httpdate::fmt_http_date(SystemTime::now() + ONE_YEAR)) {
        h.insert(header::EXPIRES, v);
    }
}
