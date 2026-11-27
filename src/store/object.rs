//! [`BlobStore`] on top of the `object_store` crate: local filesystem, in-memory, and S3
//! (including S3-compatible stores such as MinIO, R2 and Ceph RGW).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::{
    Attribute, Attributes, ClientOptions, ObjectStore, ObjectStoreExt, PutOptions, PutPayload,
};

use super::{BlobName, BlobRef, BlobStore, StoreError};
use crate::hash::Sha1Hash;

pub struct ObjectBlobStore {
    backend: &'static str,
    inner: Arc<dyn ObjectStore>,
    /// Key prefix with no leading or trailing slash. Empty means no prefix.
    prefix: String,
    /// Set `Content-Type` on stored objects, so the bucket is browsable on its own.
    /// The local filesystem can't store attributes.
    content_type: bool,
}

/// S3 settings. Credentials come from the standard `AWS_*` environment variables
/// (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, ...), which `object_store` reads.
#[derive(Clone, Debug, Default)]
pub struct S3Config {
    pub bucket: String,
    /// Key prefix inside the bucket, so radiant can share a bucket.
    pub prefix: String,
    /// Overrides `AWS_REGION`/`AWS_DEFAULT_REGION`.
    pub region: Option<String>,
    /// For S3-compatible stores, e.g. `http://localhost:9000`. Plain `http://` endpoints are
    /// allowed. Path-style requests are used unless `AWS_VIRTUAL_HOSTED_STYLE_REQUEST=true`.
    pub endpoint: Option<String>,
    /// Per-request timeout.
    pub timeout: Duration,
}

impl ObjectBlobStore {
    /// Local filesystem rooted at `root`, which is created if it doesn't exist.
    pub fn local(root: &std::path::Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(root).map_err(|e| err("fs", e))?;
        let fs = object_store::local::LocalFileSystem::new_with_prefix(root)
            .map_err(|e| err("fs", e))?;
        Ok(Self::new("fs", Arc::new(fs), ""))
    }

    /// An S3 bucket. This doesn't contact S3; a bad bucket or bad credentials show up on the
    /// first request (and in `/readyz`).
    pub fn s3(cfg: &S3Config) -> Result<Self, StoreError> {
        let mut b = AmazonS3Builder::from_env()
            .with_bucket_name(&cfg.bucket)
            .with_client_options(
                ClientOptions::new()
                    .with_timeout(cfg.timeout)
                    .with_connect_timeout(Duration::from_secs(5)),
            );
        if let Some(region) = &cfg.region {
            b = b.with_region(region);
        }
        if let Some(endpoint) = &cfg.endpoint {
            b = b
                .with_endpoint(endpoint)
                .with_allow_http(endpoint.starts_with("http://"));
        }
        let s3 = b.build().map_err(|e| err("s3", e))?;
        let mut store = Self::new("s3", Arc::new(s3), &cfg.prefix);
        store.content_type = true;
        Ok(store)
    }

    /// In-memory, for tests.
    pub fn memory() -> Self {
        Self::new(
            "memory",
            Arc::new(object_store::memory::InMemory::new()),
            "",
        )
    }

    pub fn new(backend: &'static str, inner: Arc<dyn ObjectStore>, prefix: &str) -> Self {
        Self {
            backend,
            inner,
            prefix: prefix.trim_matches('/').to_owned(),
            content_type: false,
        }
    }

    fn key(&self, name: &BlobName) -> String {
        let rel = match name {
            BlobName::Original { hash, format } => format!("{hash}/full.{format}"),
            BlobName::Variant { hash, spec, format } => format!("{hash}/{spec}.{format}"),
        };
        self.prefixed(&rel)
    }

    fn prefixed(&self, rel: &str) -> String {
        if self.prefix.is_empty() {
            rel.to_owned()
        } else {
            format!("{}/{rel}", self.prefix)
        }
    }

    fn err(&self, e: impl std::fmt::Display) -> StoreError {
        err(self.backend, e)
    }
}

fn err(backend: &'static str, e: impl std::fmt::Display) -> StoreError {
    StoreError {
        backend,
        message: e.to_string(),
    }
}

#[async_trait]
impl BlobStore for ObjectBlobStore {
    fn backend(&self) -> &'static str {
        self.backend
    }

    async fn put(&self, name: &BlobName, bytes: Bytes) -> Result<BlobRef, StoreError> {
        let key = self.key(name);
        let mut opts = PutOptions::default();
        if self.content_type {
            let format = match name {
                BlobName::Original { format, .. } | BlobName::Variant { format, .. } => format,
            };
            let mut attrs = Attributes::new();
            attrs.insert(Attribute::ContentType, format.mime_type().into());
            opts.attributes = attrs;
        }
        self.inner
            .put_opts(
                &Path::from(key.as_str()),
                PutPayload::from_bytes(bytes),
                opts,
            )
            .await
            .map_err(|e| self.err(e))?;
        Ok(BlobRef(key))
    }

    async fn get(&self, r: &BlobRef) -> Result<Option<Bytes>, StoreError> {
        match self.inner.get(&Path::from(r.0.as_str())).await {
            Ok(res) => Ok(Some(res.bytes().await.map_err(|e| self.err(e))?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(self.err(e)),
        }
    }

    async fn ping(&self) -> Result<(), StoreError> {
        // LIST a prefix that never exists. An existing bucket returns an empty listing.
        // Don't use HEAD: S3 answers a HEAD in a missing bucket with a bare 404, which looks
        // the same as "no such key", whereas a LIST error names the bucket problem.
        let probe = Path::from(self.prefixed(".radiant-ping").as_str());
        self.inner
            .list_with_delimiter(Some(&probe))
            .await
            .map(|_| ())
            .map_err(|e| self.err(e))
    }

    async fn find_original(&self, hash: &Sha1Hash) -> Result<Option<(BlobRef, Bytes)>, StoreError> {
        let dir = Path::from(self.prefixed(hash.as_str()).as_str());
        let listing = self
            .inner
            .list_with_delimiter(Some(&dir))
            .await
            .map_err(|e| self.err(e))?;
        // `full.<ext>` is the original, but a full-size conversion to another format is also
        // `full.<other ext>`. The original is the one whose bytes match the hash.
        for meta in listing.objects.into_iter().filter(|m| {
            m.location
                .filename()
                .is_some_and(|f| f.starts_with("full."))
        }) {
            let r = BlobRef(meta.location.to_string());
            if let Some(bytes) = self.get(&r).await?
                && Sha1Hash::digest(&bytes) == *hash
            {
                return Ok(Some((r, bytes)));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::store::conformance;

    #[tokio::test]
    async fn memory_conforms() {
        conformance::run(&ObjectBlobStore::memory()).await;
    }

    #[tokio::test]
    async fn memory_with_prefix_conforms() {
        let s = ObjectBlobStore::new(
            "memory",
            Arc::new(object_store::memory::InMemory::new()),
            "/some/prefix/",
        );
        conformance::run(&s).await;
    }

    #[tokio::test]
    async fn local_fs_conforms() {
        let dir = tempfile::tempdir().unwrap();
        let s = ObjectBlobStore::local(&dir.path().join("blobs")).unwrap();
        conformance::run(&s).await;
    }

    /// Runs only when `RADIANT_TEST_S3_BUCKET` is set (see `scripts/test-s3.sh`, which starts a
    /// throwaway MinIO). Each call gets its own prefix, so tests don't see each other's keys.
    pub(crate) fn test_s3() -> Option<ObjectBlobStore> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let bucket = std::env::var("RADIANT_TEST_S3_BUCKET").ok()?;
        let prefix = format!(
            "test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        Some(
            ObjectBlobStore::s3(&S3Config {
                bucket,
                prefix,
                region: Some("us-east-1".into()),
                endpoint: std::env::var("RADIANT_TEST_S3_ENDPOINT").ok(),
                timeout: Duration::from_secs(10),
            })
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn s3_conforms() {
        let Some(s) = test_s3() else {
            eprintln!("skipping: RADIANT_TEST_S3_BUCKET not set (run scripts/test-s3.sh)");
            return;
        };
        conformance::run(&s).await;
    }

    #[tokio::test]
    async fn s3_ping_fails_for_missing_bucket() {
        if test_s3().is_none() {
            return;
        }
        let s = ObjectBlobStore::s3(&S3Config {
            bucket: "radiant-no-such-bucket".into(),
            prefix: String::new(),
            region: Some("us-east-1".into()),
            endpoint: std::env::var("RADIANT_TEST_S3_ENDPOINT").ok(),
            timeout: Duration::from_secs(10),
        })
        .unwrap();
        assert!(s.ping().await.is_err());
    }

    #[tokio::test]
    async fn s3_sets_content_type() {
        let Some(s) = test_s3() else {
            return;
        };
        let r = s
            .put(
                &BlobName::Original {
                    hash: Sha1Hash::digest(b"ct"),
                    format: crate::format::Format::Webp,
                },
                Bytes::from_static(b"ct"),
            )
            .await
            .unwrap();
        let got = s.inner.get(&Path::from(r.0.as_str())).await.unwrap();
        assert_eq!(
            got.attributes
                .get(&Attribute::ContentType)
                .map(|v| v.as_ref()),
            Some("image/webp")
        );
    }

    #[tokio::test]
    async fn local_fs_layout() {
        let dir = tempfile::tempdir().unwrap();
        let s = ObjectBlobStore::local(dir.path()).unwrap();
        let hash = Sha1Hash::digest(b"x");
        s.put(
            &BlobName::Variant {
                hash,
                spec: "100s".parse().unwrap(),
                format: crate::format::Format::Webp,
            },
            Bytes::from_static(b"v"),
        )
        .await
        .unwrap();
        assert!(dir.path().join(hash.as_str()).join("100s.webp").is_file());
    }
}
