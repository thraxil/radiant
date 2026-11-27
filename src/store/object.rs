//! [`BlobStore`] on top of the `object_store` crate: local filesystem, in-memory, and (from M7)
//! S3.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use super::{BlobName, BlobRef, BlobStore, StoreError};
use crate::hash::Sha1Hash;

pub struct ObjectBlobStore {
    backend: &'static str,
    inner: Arc<dyn ObjectStore>,
    /// Key prefix with no leading or trailing slash. Empty means no prefix.
    prefix: String,
}

impl ObjectBlobStore {
    /// Local filesystem rooted at `root`, which is created if it doesn't exist.
    pub fn local(root: &std::path::Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(root).map_err(|e| err("fs", e))?;
        let fs = object_store::local::LocalFileSystem::new_with_prefix(root)
            .map_err(|e| err("fs", e))?;
        Ok(Self::new("fs", Arc::new(fs), ""))
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
        self.inner
            .put(&Path::from(key.as_str()), PutPayload::from_bytes(bytes))
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
mod tests {
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
