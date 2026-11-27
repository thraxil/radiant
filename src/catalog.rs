//! SQLite catalog of originals and the variants rendered from them.
//!
//! This is a cache, not the source of truth. Losing a row costs a re-render (variants) or a
//! lookup in the backend (originals), never data.

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use crate::format::Format;
use crate::hash::Sha1Hash;
use crate::spec::SizeSpec;
use crate::store::BlobRef;

#[derive(Clone)]
pub struct Catalog {
    pool: SqlitePool,
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("catalog database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("catalog migration: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("catalog has a corrupt row: {0}")]
    Corrupt(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OriginalRow {
    pub hash: Sha1Hash,
    pub format: Format,
    pub blob_ref: BlobRef,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariantRow {
    pub hash: Sha1Hash,
    pub spec: SizeSpec,
    pub format: Format,
    pub blob_ref: BlobRef,
    pub etag: String,
    pub bytes: u64,
}

fn format_from_db(s: &str) -> Result<Format, CatalogError> {
    Format::from_extension(s).ok_or_else(|| CatalogError::Corrupt(format!("unknown format {s:?}")))
}

impl Catalog {
    /// Opens (creating if needed) the database at `path` and runs migrations.
    pub async fn open(path: &Path) -> Result<Self, CatalogError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| CatalogError::Db(sqlx::Error::Io(e)))?;
        }
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        Self::migrate(pool).await
    }

    /// A private in-memory database, for tests.
    pub async fn in_memory() -> Result<Self, CatalogError> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        // Each connection to `:memory:` is a separate database, so allow only one.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        Self::migrate(pool).await
    }

    async fn migrate(pool: SqlitePool) -> Result<Self, CatalogError> {
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn ping(&self) -> Result<(), CatalogError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn get_original(&self, hash: &Sha1Hash) -> Result<Option<OriginalRow>, CatalogError> {
        let row: Option<(String, String, i64, i64, i64)> = sqlx::query_as(
            "SELECT format, blob_ref, bytes, width, height FROM originals WHERE hash = ?",
        )
        .bind(hash.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(format, blob_ref, bytes, width, height)| {
            Ok(OriginalRow {
                hash: *hash,
                format: format_from_db(&format)?,
                blob_ref: BlobRef(blob_ref),
                bytes: bytes as u64,
                width: width as u32,
                height: height as u32,
            })
        })
        .transpose()
    }

    /// Inserts an original, leaving an existing row unchanged.
    pub async fn put_original(&self, row: &OriginalRow) -> Result<(), CatalogError> {
        sqlx::query(
            "INSERT INTO originals (hash, format, blob_ref, bytes, width, height)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (hash) DO NOTHING",
        )
        .bind(row.hash.as_str())
        .bind(row.format.extension())
        .bind(&row.blob_ref.0)
        .bind(row.bytes as i64)
        .bind(i64::from(row.width))
        .bind(i64::from(row.height))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_variant(
        &self,
        hash: &Sha1Hash,
        spec: &SizeSpec,
        format: Format,
    ) -> Result<Option<VariantRow>, CatalogError> {
        let row: Option<(String, String, i64)> = sqlx::query_as(
            "SELECT blob_ref, etag, bytes FROM variants WHERE hash = ? AND spec = ? AND format = ?",
        )
        .bind(hash.as_str())
        .bind(spec.to_string())
        .bind(format.extension())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(blob_ref, etag, bytes)| VariantRow {
            hash: *hash,
            spec: *spec,
            format,
            blob_ref: BlobRef(blob_ref),
            etag,
            bytes: bytes as u64,
        }))
    }

    /// Inserts or replaces a variant. Replacing is correct: a re-render after the blob went
    /// missing gives the same key on fs/S3, and on Cask the same bytes give the same key.
    pub async fn put_variant(&self, row: &VariantRow) -> Result<(), CatalogError> {
        sqlx::query(
            "INSERT INTO variants (hash, spec, format, blob_ref, etag, bytes)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (hash, spec, format) DO UPDATE SET
               blob_ref = excluded.blob_ref, etag = excluded.etag, bytes = excluded.bytes",
        )
        .bind(row.hash.as_str())
        .bind(row.spec.to_string())
        .bind(row.format.extension())
        .bind(&row.blob_ref.0)
        .bind(&row.etag)
        .bind(row.bytes as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn original(hash: Sha1Hash) -> OriginalRow {
        OriginalRow {
            hash,
            format: Format::Jpeg,
            blob_ref: BlobRef(format!("{hash}/full.jpg")),
            bytes: 1234,
            width: 640,
            height: 480,
        }
    }

    #[tokio::test]
    async fn originals_round_trip_and_insert_is_idempotent() {
        let c = Catalog::in_memory().await.unwrap();
        let h = Sha1Hash::digest(b"a");
        assert_eq!(c.get_original(&h).await.unwrap(), None);
        let row = original(h);
        c.put_original(&row).await.unwrap();
        c.put_original(&OriginalRow {
            bytes: 1,
            ..row.clone()
        })
        .await
        .unwrap();
        assert_eq!(c.get_original(&h).await.unwrap(), Some(row));
    }

    #[tokio::test]
    async fn variants_round_trip_and_upsert() {
        let c = Catalog::in_memory().await.unwrap();
        let h = Sha1Hash::digest(b"a");
        c.put_original(&original(h)).await.unwrap();
        let spec: SizeSpec = "100s".parse().unwrap();
        let v = VariantRow {
            hash: h,
            spec,
            format: Format::Webp,
            blob_ref: BlobRef("r1".into()),
            etag: "e1".into(),
            bytes: 10,
        };
        assert_eq!(c.get_variant(&h, &spec, Format::Webp).await.unwrap(), None);
        c.put_variant(&v).await.unwrap();
        assert_eq!(
            c.get_variant(&h, &spec, Format::Webp).await.unwrap(),
            Some(v.clone())
        );
        assert_eq!(c.get_variant(&h, &spec, Format::Png).await.unwrap(), None);

        let v2 = VariantRow {
            blob_ref: BlobRef("r2".into()),
            etag: "e2".into(),
            ..v
        };
        c.put_variant(&v2).await.unwrap();
        assert_eq!(
            c.get_variant(&h, &spec, Format::Webp).await.unwrap(),
            Some(v2)
        );
    }

    #[tokio::test]
    async fn variant_requires_original() {
        let c = Catalog::in_memory().await.unwrap();
        let h = Sha1Hash::digest(b"a");
        let v = VariantRow {
            hash: h,
            spec: "100s".parse().unwrap(),
            format: Format::Webp,
            blob_ref: BlobRef("r".into()),
            etag: "e".into(),
            bytes: 1,
        };
        assert!(c.put_variant(&v).await.is_err());
    }

    #[tokio::test]
    async fn file_database_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/radiant.db");
        let h = Sha1Hash::digest(b"a");
        Catalog::open(&path)
            .await
            .unwrap()
            .put_original(&original(h))
            .await
            .unwrap();
        let c = Catalog::open(&path).await.unwrap();
        assert!(c.get_original(&h).await.unwrap().is_some());
    }
}
