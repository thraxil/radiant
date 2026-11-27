//! Serving and uploading images: the "get or render" logic that sits between the HTTP layer,
//! the catalog, the blob store and the imaging code.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{Semaphore, watch};

use metrics::{counter, gauge, histogram};

use crate::catalog::{Catalog, CatalogError, OriginalRow, VariantRow};
use crate::format::Format;
use crate::hash::Sha1Hash;
use crate::imaging::{self, EncodeSettings, ImagingError, Limits};
use crate::request::ImageRequest;
use crate::spec::SizeSpec;
use crate::store::{BlobName, BlobStore, StoreError};
use crate::telemetry::{self as m, limit_rejected};

#[derive(Clone, Debug)]
pub struct ServiceConfig {
    pub limits: Limits,
    pub encode: EncodeSettings,
    /// Maximum number of renders running at once.
    pub scale_concurrency: usize,
    /// How long a request waits for a render before returning 503. The render itself carries
    /// on, so a later request finds the result stored.
    pub scale_timeout: Duration,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            encode: EncodeSettings::default(),
            scale_concurrency: std::thread::available_parallelism().map_or(4, |n| n.get()),
            scale_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("image not found")]
    NotFound,
    /// The request is over a limit (e.g. the output would be too large).
    #[error("{0}")]
    TooLarge(ImagingError),
    /// The upload isn't an image we accept.
    #[error("{0}")]
    BadUpload(ImagingError),
    #[error("timed out waiting for image to be scaled")]
    Timeout,
    #[error("rendering failed: {0}")]
    Render(ImagingError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Catalog(Arc<CatalogError>),
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<CatalogError> for ServiceError {
    fn from(e: CatalogError) -> Self {
        ServiceError::Catalog(Arc::new(e))
    }
}

#[derive(Debug)]
pub enum Served {
    /// The client's `If-None-Match` matched.
    NotModified { etag: String },
    Image {
        bytes: Bytes,
        format: Format,
        /// Unquoted sha1 hex of `bytes`.
        etag: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uploaded {
    pub hash: Sha1Hash,
    pub format: Format,
    pub length: usize,
    /// False if the image was already stored.
    pub created: bool,
}

type VariantKey = (Sha1Hash, SizeSpec, Format);

#[derive(Clone)]
struct Rendered {
    bytes: Bytes,
    etag: String,
}

type RenderResult = Result<Rendered, ServiceError>;

#[derive(Clone)]
pub struct ImageService {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<dyn BlobStore>,
    catalog: Catalog,
    cfg: ServiceConfig,
    scale_permits: Arc<Semaphore>,
    /// Renders in progress, so concurrent requests for the same variant share one render.
    inflight: Mutex<HashMap<VariantKey, watch::Receiver<Option<RenderResult>>>>,
}

/// Does an `If-None-Match` header value match `etag`? Handles `*`, lists, weak validators, and
/// unquoted values (reticulum sent unquoted ETags, so clients may echo them back that way).
pub fn etag_matches(if_none_match: Option<&str>, etag: &str) -> bool {
    let Some(header) = if_none_match else {
        return false;
    };
    header.split(',').map(str::trim).any(|candidate| {
        candidate == "*"
            || candidate
                .trim_start_matches("W/")
                .trim_matches('"')
                .eq(etag)
    })
}

impl ImageService {
    pub fn new(store: Arc<dyn BlobStore>, catalog: Catalog, cfg: ServiceConfig) -> Self {
        let permits = Arc::new(Semaphore::new(cfg.scale_concurrency.max(1)));
        Self {
            inner: Arc::new(Inner {
                store,
                catalog,
                cfg,
                scale_permits: permits,
                inflight: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn catalog(&self) -> &Catalog {
        &self.inner.catalog
    }

    pub fn limits(&self) -> &Limits {
        &self.inner.cfg.limits
    }

    /// Serves a request: the original, a stored variant, or a freshly rendered one.
    pub async fn get(
        &self,
        req: &ImageRequest,
        if_none_match: Option<&str>,
    ) -> Result<Served, ServiceError> {
        let result = self.get_inner(req, if_none_match).await;
        match &result {
            Ok((Served::NotModified { .. }, _)) => {
                counter!(m::IMAGES_SERVED, "source" => "not_modified").increment(1);
            }
            Ok((Served::Image { .. }, source)) => {
                counter!(m::IMAGES_SERVED, "source" => *source).increment(1);
            }
            Err(ServiceError::TooLarge(e)) => {
                if let Some(limit) = e.limit_name() {
                    limit_rejected(limit);
                }
            }
            Err(ServiceError::Timeout) => counter!(m::SCALE_TIMEOUTS).increment(1),
            Err(_) => {}
        }
        result.map(|(served, _)| served)
    }

    /// Like [`Self::get`], but also says where the image came from (a metrics label).
    async fn get_inner(
        &self,
        req: &ImageRequest,
        if_none_match: Option<&str>,
    ) -> Result<(Served, &'static str), ServiceError> {
        let Some((orig, fetched)) = self.original(&req.hash).await? else {
            return Err(ServiceError::NotFound);
        };

        if req.spec.is_full() && req.format == orig.format {
            // The original itself. Its sha1 is its hash, so it serves as the ETag.
            let etag = req.hash.to_string();
            if etag_matches(if_none_match, &etag) {
                return Ok((Served::NotModified { etag }, "not_modified"));
            }
            let bytes = match fetched {
                Some(b) => b,
                None => self.fetch_original(&orig).await?,
            };
            return Ok((
                Served::Image {
                    bytes,
                    format: orig.format,
                    etag,
                },
                "original",
            ));
        }

        // Reject over-limit requests before fetching or decoding anything.
        imaging::planned_output(&req.spec, orig.width, orig.height, self.limits())
            .map_err(ServiceError::TooLarge)?;

        if let Some(row) = self
            .inner
            .catalog
            .get_variant(&req.hash, &req.spec, req.format)
            .await?
        {
            if etag_matches(if_none_match, &row.etag) {
                return Ok((Served::NotModified { etag: row.etag }, "not_modified"));
            }
            match self.inner.store.get(&row.blob_ref).await? {
                Some(bytes) => {
                    return Ok((
                        Served::Image {
                            bytes,
                            format: req.format,
                            etag: row.etag,
                        },
                        "variant_stored",
                    ));
                }
                None => {
                    counter!(m::CATALOG_REPAIRS, "kind" => "variant_blob_missing").increment(1);
                    tracing::warn!(
                        hash = %req.hash, spec = %req.spec, format = %req.format, blob = %row.blob_ref,
                        "variant in catalog but missing from store; re-rendering"
                    );
                }
            }
        }

        let rendered = self.render_shared(req, orig, fetched).await?;
        if etag_matches(if_none_match, &rendered.etag) {
            return Ok((
                Served::NotModified {
                    etag: rendered.etag,
                },
                "not_modified",
            ));
        }
        Ok((
            Served::Image {
                bytes: rendered.bytes,
                format: req.format,
                etag: rendered.etag,
            },
            "variant_rendered",
        ))
    }

    /// Stores an uploaded image. Uploading the same bytes again is a no-op.
    pub async fn upload(&self, bytes: Bytes) -> Result<Uploaded, ServiceError> {
        let result = self.upload_inner(bytes).await;
        match &result {
            Ok(u) => {
                let outcome = if u.created { "created" } else { "duplicate" };
                counter!(m::UPLOADS, "outcome" => outcome).increment(1);
                histogram!(m::UPLOAD_SIZE).record(u.length as f64);
            }
            Err(ServiceError::BadUpload(e)) => {
                counter!(m::UPLOADS, "outcome" => "rejected").increment(1);
                if let Some(limit) = e.limit_name() {
                    limit_rejected(limit);
                }
            }
            Err(_) => counter!(m::UPLOADS, "outcome" => "error").increment(1),
        }
        result
    }

    async fn upload_inner(&self, bytes: Bytes) -> Result<Uploaded, ServiceError> {
        let limits = *self.limits();
        let (hash, info, bytes) = tokio::task::spawn_blocking(move || {
            let info = imaging::probe(&bytes, &limits);
            (Sha1Hash::digest(&bytes), info, bytes)
        })
        .await
        .map_err(|e| ServiceError::Internal(e.to_string()))?;
        let info = info.map_err(ServiceError::BadUpload)?;
        let uploaded = Uploaded {
            hash,
            format: info.format,
            length: bytes.len(),
            created: false,
        };

        if self.inner.catalog.get_original(&hash).await?.is_some() {
            return Ok(uploaded);
        }
        let blob_ref = self
            .inner
            .store
            .put(
                &BlobName::Original {
                    hash,
                    format: info.format,
                },
                bytes.clone(),
            )
            .await?;
        self.inner
            .catalog
            .put_original(&OriginalRow {
                hash,
                format: info.format,
                blob_ref,
                bytes: bytes.len() as u64,
                width: info.width,
                height: info.height,
            })
            .await?;
        Ok(Uploaded {
            created: true,
            ..uploaded
        })
    }

    /// Renders and stores variants in the background (upload's `size_hints`), in the
    /// original's format as reticulum did.
    pub fn prewarm(&self, hash: Sha1Hash, format: Format, specs: Vec<SizeSpec>) {
        for spec in specs {
            let svc = self.clone();
            tokio::spawn(async move {
                let req = ImageRequest { hash, spec, format };
                if let Err(e) = svc.get(&req, None).await {
                    tracing::warn!(%hash, %spec, error = %e, "size hint pre-render failed");
                }
            });
        }
    }

    /// Looks up an original in the catalog. If it isn't there, tries to recover it from the
    /// store and rebuild the catalog row. Bytes are returned only when they were fetched.
    async fn original(
        &self,
        hash: &Sha1Hash,
    ) -> Result<Option<(OriginalRow, Option<Bytes>)>, ServiceError> {
        if let Some(row) = self.inner.catalog.get_original(hash).await? {
            return Ok(Some((row, None)));
        }
        let Some((blob_ref, bytes)) = self.inner.store.find_original(hash).await? else {
            return Ok(None);
        };
        let limits = *self.limits();
        let hash = *hash;
        let (actual, info, bytes) = tokio::task::spawn_blocking(move || {
            let info = imaging::probe(&bytes, &limits);
            (Sha1Hash::digest(&bytes), info, bytes)
        })
        .await
        .map_err(|e| ServiceError::Internal(e.to_string()))?;
        if actual != hash {
            tracing::error!(%hash, %actual, blob = %blob_ref, "stored original is corrupt");
            return Err(ServiceError::Internal(format!(
                "stored original {blob_ref} does not match its hash"
            )));
        }
        let info = info.map_err(ServiceError::Render)?;
        let row = OriginalRow {
            hash,
            format: info.format,
            blob_ref,
            bytes: bytes.len() as u64,
            width: info.width,
            height: info.height,
        };
        self.inner.catalog.put_original(&row).await?;
        counter!(m::CATALOG_REPAIRS, "kind" => "original_recovered").increment(1);
        tracing::info!(%hash, format = %info.format, "recovered catalog row for original");
        Ok(Some((row, Some(bytes))))
    }

    async fn fetch_original(&self, orig: &OriginalRow) -> Result<Bytes, ServiceError> {
        self.inner.store.get(&orig.blob_ref).await?.ok_or_else(|| {
            counter!(m::CATALOG_REPAIRS, "kind" => "original_blob_missing").increment(1);
            tracing::error!(hash = %orig.hash, blob = %orig.blob_ref, "original in catalog but missing from store");
            ServiceError::NotFound
        })
    }

    /// Renders a variant, sharing one render among concurrent requests for it.
    async fn render_shared(
        &self,
        req: &ImageRequest,
        orig: OriginalRow,
        fetched: Option<Bytes>,
    ) -> Result<Rendered, ServiceError> {
        let key = (req.hash, req.spec, req.format);
        let mut rx = {
            let mut inflight = self.inner.inflight.lock().expect("inflight lock");
            match inflight.get(&key) {
                Some(rx) => {
                    counter!(m::SINGLEFLIGHT_COALESCED).increment(1);
                    rx.clone()
                }
                None => {
                    let (tx, rx) = watch::channel(None);
                    inflight.insert(key, rx.clone());
                    // Spawned, not awaited: the render finishes and is stored even if every
                    // waiting client disconnects or times out.
                    tokio::spawn(self.clone().render_job(key, orig, fetched, tx));
                    rx
                }
            }
        };
        let waited =
            tokio::time::timeout(self.inner.cfg.scale_timeout, rx.wait_for(Option::is_some)).await;
        match waited {
            Err(_elapsed) => Err(ServiceError::Timeout),
            Ok(Err(_closed)) => Err(ServiceError::Internal(
                "render task ended without a result".into(),
            )),
            Ok(Ok(result)) => result.clone().expect("wait_for guarantees Some"),
        }
    }

    async fn render_job(
        self,
        key: VariantKey,
        orig: OriginalRow,
        fetched: Option<Bytes>,
        tx: watch::Sender<Option<RenderResult>>,
    ) {
        // Removes the in-flight entry however this task ends, panics included.
        let _guard = InflightGuard { svc: &self, key };

        // A request can check the catalog, miss, and get here just after another render of the
        // same variant finished and left the in-flight map. Check again rather than re-render.
        if let Some(stored) = self.stored_variant(key).await {
            tx.send_replace(Some(Ok(stored)));
            return;
        }

        let result = self.render(key, &orig, fetched).await;
        tx.send_replace(Some(result.clone()));
        if let Ok(rendered) = result
            && let Err(e) = self.persist(key, &rendered).await
        {
            // Not fatal: the next request re-renders.
            counter!(m::VARIANT_STORE_FAILURES).increment(1);
            tracing::warn!(hash = %key.0, spec = %key.1, format = %key.2, error = %e, "failed to store variant");
        }
    }

    /// The variant, if the catalog has it and the blob is in the store. Errors count as a miss.
    async fn stored_variant(&self, (hash, spec, format): VariantKey) -> Option<Rendered> {
        let row = self
            .inner
            .catalog
            .get_variant(&hash, &spec, format)
            .await
            .ok()??;
        let bytes = self.inner.store.get(&row.blob_ref).await.ok()??;
        Some(Rendered {
            bytes,
            etag: row.etag,
        })
    }

    /// Waits up to `timeout` for in-progress renders to finish and be stored. Call this on
    /// shutdown. Returns false if renders were still running at the deadline.
    pub async fn drain(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = self.inner.inflight.lock().map_or(0, |m| m.len());
            if remaining == 0 {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(remaining, "shutting down with renders still in progress");
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn render(
        &self,
        (hash, spec, format): VariantKey,
        orig: &OriginalRow,
        fetched: Option<Bytes>,
    ) -> RenderResult {
        let src = match fetched {
            Some(b) => b,
            None => self.fetch_original(orig).await?,
        };
        let permit = {
            let _waiting = GaugeGuard::new(m::SCALE_WAITING);
            self.inner
                .scale_permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| ServiceError::Internal(e.to_string()))?
        };
        let limits = self.inner.cfg.limits;
        let encode = self.inner.cfg.encode;
        let src_format = orig.format;
        let started = std::time::Instant::now();
        let out = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _running = GaugeGuard::new(m::SCALE_RUNNING);
            imaging::render(&src, src_format, &spec, format, &limits, &encode)
        })
        .await
        .map_err(|e| ServiceError::Internal(format!("render task failed: {e}")))?;
        let outcome = match &out {
            Ok(_) => "ok",
            Err(e) if e.is_limit() => "limit",
            Err(_) => "error",
        };
        histogram!(m::SCALE_DURATION, "format" => format.extension(), "outcome" => outcome)
            .record(started.elapsed().as_secs_f64());
        let out = out.map_err(|e| {
            if e.is_limit() {
                ServiceError::TooLarge(e)
            } else {
                ServiceError::Render(e)
            }
        })?;
        tracing::debug!(%hash, %spec, %format, bytes = out.len(), elapsed = ?started.elapsed(), "rendered variant");
        let etag = Sha1Hash::digest(&out).to_string();
        Ok(Rendered {
            bytes: Bytes::from(out),
            etag,
        })
    }

    async fn persist(
        &self,
        (hash, spec, format): VariantKey,
        rendered: &Rendered,
    ) -> Result<(), ServiceError> {
        let blob_ref = self
            .inner
            .store
            .put(
                &BlobName::Variant { hash, spec, format },
                rendered.bytes.clone(),
            )
            .await?;
        self.inner
            .catalog
            .put_variant(&VariantRow {
                hash,
                spec,
                format,
                blob_ref,
                etag: rendered.etag.clone(),
                bytes: rendered.bytes.len() as u64,
            })
            .await?;
        Ok(())
    }
}

/// Increments a gauge now and decrements it when dropped, even on error or panic.
struct GaugeGuard(&'static str);

impl GaugeGuard {
    fn new(name: &'static str) -> Self {
        gauge!(name).increment(1.0);
        Self(name)
    }
}

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        gauge!(self.0).decrement(1.0);
    }
}

struct InflightGuard<'a> {
    svc: &'a ImageService,
    key: VariantKey,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut inflight) = self.svc.inner.inflight.lock() {
            inflight.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests;
