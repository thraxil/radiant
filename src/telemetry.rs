//! Prometheus metrics. Every metric name is defined here.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use metrics::{
    Unit, counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram,
};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

use crate::hash::Sha1Hash;
use crate::store::{BlobName, BlobRef, BlobStore, StoreError};

pub const HTTP_REQUESTS: &str = "radiant_http_requests_total";
pub const HTTP_DURATION: &str = "radiant_http_request_duration_seconds";
pub const IMAGES_SERVED: &str = "radiant_images_served_total";
pub const SCALE_DURATION: &str = "radiant_scale_duration_seconds";
pub const SCALE_RUNNING: &str = "radiant_scale_running";
pub const SCALE_WAITING: &str = "radiant_scale_waiting";
pub const SCALE_TIMEOUTS: &str = "radiant_scale_timeouts_total";
pub const SINGLEFLIGHT_COALESCED: &str = "radiant_singleflight_coalesced_total";
pub const VARIANT_STORE_FAILURES: &str = "radiant_variant_store_failures_total";
pub const BACKEND_DURATION: &str = "radiant_backend_op_duration_seconds";
pub const UPLOADS: &str = "radiant_uploads_total";
pub const UPLOAD_SIZE: &str = "radiant_upload_size_bytes";
pub const LIMIT_REJECTIONS: &str = "radiant_limit_rejections_total";
pub const CATALOG_REPAIRS: &str = "radiant_catalog_repairs_total";
pub const CASK_NODE_FAILURES: &str = "radiant_cask_node_failures_total";
pub const BUILD_INFO: &str = "radiant_build_info";

const LATENCY_BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];
const SIZE_BUCKETS: &[f64] = &[
    10e3, 50e3, 100e3, 250e3, 500e3, 1e6, 2.5e6, 5e6, 10e6, 25e6, 50e6, 100e6,
];

/// Builds the Prometheus recorder and installs it as the global recorder.
/// The caller must call [`PrometheusHandle::run_upkeep`] periodically (see [`spawn_upkeep`]).
pub fn install() -> Result<PrometheusHandle, metrics_exporter_prometheus::BuildError> {
    let handle = PrometheusBuilder::new()
        .set_buckets_for_metric(Matcher::Suffix("_seconds".into()), LATENCY_BUCKETS)?
        .set_buckets_for_metric(Matcher::Full(UPLOAD_SIZE.into()), SIZE_BUCKETS)?
        .install_recorder()?;
    describe();
    gauge!(BUILD_INFO, "version" => crate::version()).set(1.0);
    Ok(handle)
}

/// Histogram data is compacted during upkeep. Without it, memory grows between scrapes.
pub fn spawn_upkeep(handle: PrometheusHandle) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            handle.run_upkeep();
        }
    });
}

fn describe() {
    describe_counter!(HTTP_REQUESTS, "HTTP requests by route and status.");
    describe_histogram!(
        HTTP_DURATION,
        Unit::Seconds,
        "HTTP request latency by route."
    );
    describe_counter!(
        IMAGES_SERVED,
        "Image responses by source: original, variant_stored, variant_rendered, not_modified."
    );
    describe_histogram!(
        SCALE_DURATION,
        Unit::Seconds,
        "Time to decode, scale and encode a variant, by output format and outcome."
    );
    describe_gauge!(SCALE_RUNNING, "Scaling jobs running now.");
    describe_gauge!(SCALE_WAITING, "Scaling jobs waiting for a free slot.");
    describe_counter!(
        SCALE_TIMEOUTS,
        "Requests that gave up waiting for a scale (503). The scale still finishes and is stored."
    );
    describe_counter!(
        SINGLEFLIGHT_COALESCED,
        "Requests that waited on an identical scale already in progress instead of starting one."
    );
    describe_counter!(
        VARIANT_STORE_FAILURES,
        "Rendered variants that could not be written to the backend or catalog."
    );
    describe_histogram!(
        BACKEND_DURATION,
        Unit::Seconds,
        "Storage backend operation latency by backend, operation and outcome."
    );
    describe_counter!(UPLOADS, "Uploads by outcome: created, duplicate, rejected.");
    describe_histogram!(UPLOAD_SIZE, Unit::Bytes, "Size of accepted uploads.");
    describe_counter!(
        LIMIT_REJECTIONS,
        "Requests rejected by a configured limit, by limit."
    );
    describe_counter!(
        CATALOG_REPAIRS,
        "Catalog rows rebuilt or bypassed because the catalog and backend disagreed, by kind."
    );
    describe_gauge!(
        BUILD_INFO,
        "Always 1. The version label identifies the build."
    );
}

/// Axum middleware recording request count and latency. Labels use the route *pattern*
/// (e.g. `/image/{hash}/{size}/{filename}`), so the number of label values stays small.
pub async fn track_http(req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |p| p.as_str().to_owned());
    let method = req.method().as_str().to_owned();
    let start = Instant::now();
    let res = next.run(req).await;
    let status = res.status().as_u16().to_string();
    counter!(HTTP_REQUESTS, "route" => route.clone(), "method" => method, "status" => status)
        .increment(1);
    histogram!(HTTP_DURATION, "route" => route).record(start.elapsed().as_secs_f64());
    res
}

pub fn limit_rejected(limit: &'static str) {
    counter!(LIMIT_REJECTIONS, "limit" => limit).increment(1);
}

/// Wraps a [`BlobStore`] to time every operation.
pub struct InstrumentedStore {
    inner: Arc<dyn BlobStore>,
}

impl InstrumentedStore {
    pub fn new(inner: Arc<dyn BlobStore>) -> Self {
        Self { inner }
    }

    fn record(&self, op: &'static str, outcome: &'static str, start: Instant) {
        histogram!(
            BACKEND_DURATION,
            "backend" => self.inner.backend(),
            "op" => op,
            "outcome" => outcome,
        )
        .record(start.elapsed().as_secs_f64());
    }
}

fn outcome<T>(r: &Result<Option<T>, StoreError>) -> &'static str {
    match r {
        Ok(Some(_)) => "ok",
        Ok(None) => "not_found",
        Err(_) => "error",
    }
}

#[async_trait]
impl BlobStore for InstrumentedStore {
    fn backend(&self) -> &'static str {
        self.inner.backend()
    }

    async fn put(&self, name: &BlobName, bytes: Bytes) -> Result<BlobRef, StoreError> {
        let start = Instant::now();
        let r = self.inner.put(name, bytes).await;
        self.record("put", if r.is_ok() { "ok" } else { "error" }, start);
        r
    }

    async fn get(&self, r: &BlobRef) -> Result<Option<Bytes>, StoreError> {
        let start = Instant::now();
        let res = self.inner.get(r).await;
        self.record("get", outcome(&res), start);
        res
    }

    async fn find_original(&self, hash: &Sha1Hash) -> Result<Option<(BlobRef, Bytes)>, StoreError> {
        let start = Instant::now();
        let res = self.inner.find_original(hash).await;
        self.record("find_original", outcome(&res), start);
        res
    }

    async fn ping(&self) -> Result<(), StoreError> {
        // Not timed: it runs on every readiness probe and isn't real traffic.
        self.inner.ping().await
    }
}
