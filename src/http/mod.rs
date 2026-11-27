//! HTTP routes.

mod image;
mod upload;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::sync::Semaphore;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::service::{ImageService, ServiceError};
use crate::telemetry;

#[derive(Clone)]
pub struct AppState {
    pub service: ImageService,
    /// If set, uploads must send it as the `key` form field.
    pub upload_key: Option<Arc<str>>,
    pub max_upload_bytes: usize,
    /// Caps concurrent uploads, each of which holds up to `max_upload_bytes` in memory.
    /// Extra uploads wait their turn.
    pub upload_permits: Arc<Semaphore>,
    /// Whole-request timeout (408).
    pub request_timeout: Duration,
    /// Serves `/metrics` when set.
    pub metrics: Option<PrometheusHandle>,
}

pub fn router(state: AppState) -> Router {
    let mut router = Router::new()
        .route(
            "/",
            get(upload::form)
                .post(upload::upload)
                .layer(DefaultBodyLimit::max(state.max_upload_bytes)),
        )
        .route("/image/{hash}/{size}/{filename}", get(image::serve))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/favicon.ico", get(favicon));
    if state.metrics.is_some() {
        router = router.route("/metrics", get(metrics));
    }
    router
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            state.request_timeout,
        ))
        .layer(middleware::from_fn(telemetry::track_http))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn metrics(State(state): State<AppState>) -> Response {
    match &state.metrics {
        Some(handle) => (
            [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
            handle.render(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(state): State<AppState>) -> Response {
    match state.service.catalog().ping().await {
        Ok(()) => "ok".into_response(),
        Err(e) => {
            tracing::error!(error = %e, "readiness check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "catalog unavailable").into_response()
        }
    }
}

/// Reticulum returns an empty 200 here, and so do we.
async fn favicon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/x-icon")], "")
}

impl IntoResponse for ServiceError {
    fn into_response(self) -> Response {
        let status = match &self {
            ServiceError::NotFound => StatusCode::NOT_FOUND,
            ServiceError::TooLarge(_) | ServiceError::BadUpload(_) => StatusCode::BAD_REQUEST,
            ServiceError::Timeout => StatusCode::SERVICE_UNAVAILABLE,
            ServiceError::Render(_)
            | ServiceError::Store(_)
            | ServiceError::Catalog(_)
            | ServiceError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
            // Internal details go to the log, not the client.
            let msg = match self {
                ServiceError::Timeout => "timed out scaling image, try again shortly",
                _ => "internal error",
            };
            (status, msg).into_response()
        } else {
            (status, self.to_string()).into_response()
        }
    }
}

#[cfg(test)]
mod tests;
