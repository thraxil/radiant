//! Blob storage backends.
//!
//! A [`BlobStore`] is a plain byte store. It knows nothing about images beyond the
//! [`BlobName`] it is given, and each backend decides how blobs are addressed:
//!
//! | backend         | original                     | variant                        |
//! |-----------------|------------------------------|--------------------------------|
//! | fs / S3 / memory| `[prefix/]<hash>/full.<ext>` | `[prefix/]<hash>/<spec>.<ext>` |
//! | Cask (M8)       | `sha1:<hash>`                | `sha1:<sha1 of variant bytes>` |
//!
//! The catalog records the [`BlobRef`] returned by [`BlobStore::put`], so reads never need to
//! know the addressing scheme.

mod object;

use std::fmt;

use async_trait::async_trait;
use bytes::Bytes;

pub use object::ObjectBlobStore;

use crate::format::Format;
use crate::hash::Sha1Hash;
use crate::spec::SizeSpec;

/// What a blob is, which backends with predictable names use to build its path.
#[derive(Clone, Copy, Debug)]
pub enum BlobName {
    Original {
        hash: Sha1Hash,
        format: Format,
    },
    Variant {
        hash: Sha1Hash,
        spec: SizeSpec,
        format: Format,
    },
}

/// Backend-specific address of a stored blob, as recorded in the catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobRef(pub String);

impl fmt::Display for BlobRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{backend} backend: {message}")]
pub struct StoreError {
    pub backend: &'static str,
    pub message: String,
}

#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    /// Short name for logs and metrics.
    fn backend(&self) -> &'static str;

    /// Stores a blob. Must be atomic (readers never see a partial blob) and idempotent.
    async fn put(&self, name: &BlobName, bytes: Bytes) -> Result<BlobRef, StoreError>;

    /// Fetches a blob. `Ok(None)` means it does not exist.
    async fn get(&self, r: &BlobRef) -> Result<Option<Bytes>, StoreError>;

    /// Finds an original that the catalog has no row for, given only its hash, so the row can
    /// be rebuilt. Returns the blob's ref and bytes, which must hash to `hash`. The caller
    /// sniffs the format.
    async fn find_original(&self, hash: &Sha1Hash) -> Result<Option<(BlobRef, Bytes)>, StoreError>;
}

#[cfg(test)]
pub(crate) mod conformance;
