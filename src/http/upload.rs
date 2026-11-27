use axum::Json;
use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use bytes::Bytes;
use serde::Serialize;

use super::AppState;
use crate::spec::SizeSpec;

/// The upload response. Field names and shape match reticulum.
#[derive(Debug, Serialize)]
pub(super) struct UploadResponse {
    hash: String,
    length: usize,
    /// Without the leading dot, e.g. `jpg`.
    extension: &'static str,
    full_url: String,
    /// Always true. Kept for compatibility (reticulum: "replicated to enough nodes").
    satisfied: bool,
    /// Always empty. Kept for compatibility (reticulum: nodes holding a copy).
    nodes: Vec<String>,
}

pub(super) async fn form(State(state): State<AppState>) -> Html<String> {
    let key_field = if state.upload_key.is_some() {
        r#"<label>Upload key <input type="password" name="key" required></label>"#
    } else {
        ""
    };
    Html(include_str!("form.html").replace("{{KEY_FIELD}}", key_field))
}

fn bad_request(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, msg.into()).into_response()
}

/// Compares in time that doesn't depend on where the first difference is.
fn key_matches(expected: &str, given: &str) -> bool {
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(super) async fn upload(State(state): State<AppState>, mut multipart: Multipart) -> Response {
    let mut image: Option<Bytes> = None;
    let mut key: Option<String> = None;
    let mut size_hints = String::new();

    // Fields can come in any order, so read them all before acting.
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return (e.status(), e.body_text()).into_response(),
        };
        let name = field.name().unwrap_or_default().to_owned();
        let read = match name.as_str() {
            "image" => field.bytes().await.map(|b| image = Some(b)),
            "key" => field.text().await.map(|t| key = Some(t)),
            "size_hints" => field.text().await.map(|t| size_hints = t),
            _ => Ok(()), // ignore unknown fields
        };
        if let Err(e) = read {
            return (e.status(), e.body_text()).into_response();
        }
    }

    if let Some(expected) = &state.upload_key
        && !key.as_deref().is_some_and(|k| key_matches(expected, k))
    {
        return (StatusCode::FORBIDDEN, "invalid upload key").into_response();
    }
    let Some(image) = image.filter(|b| !b.is_empty()) else {
        return bad_request("missing image file");
    };

    let uploaded = match state.service.upload(image).await {
        Ok(u) => u,
        Err(e) => return e.into_response(),
    };
    tracing::info!(hash = %uploaded.hash, format = %uploaded.format, bytes = uploaded.length, created = uploaded.created, "upload");

    // Reticulum ignored hints it couldn't parse, and so do we.
    let specs: Vec<SizeSpec> = size_hints
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .filter(|s: &SizeSpec| !s.is_full())
        .collect();
    if !specs.is_empty() {
        state.service.prewarm(uploaded.hash, uploaded.format, specs);
    }

    let ext = uploaded.format.extension();
    Json(UploadResponse {
        hash: uploaded.hash.to_string(),
        length: uploaded.length,
        extension: ext,
        full_url: format!("/image/{}/full/image.{ext}", uploaded.hash),
        satisfied: true,
        nodes: Vec::new(),
    })
    .into_response()
}
