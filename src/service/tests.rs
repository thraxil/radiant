use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use image::{DynamicImage, RgbImage};

use super::*;
use crate::store::{BlobRef, ObjectBlobStore};

/// Wraps a store and counts variant writes, to check single-flight.
struct CountingStore {
    inner: ObjectBlobStore,
    variant_puts: AtomicUsize,
}

#[async_trait]
impl BlobStore for CountingStore {
    fn backend(&self) -> &'static str {
        "counting"
    }
    async fn put(&self, name: &BlobName, bytes: Bytes) -> Result<BlobRef, StoreError> {
        if matches!(name, BlobName::Variant { .. }) {
            self.variant_puts.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.put(name, bytes).await
    }
    async fn get(&self, r: &BlobRef) -> Result<Option<Bytes>, StoreError> {
        self.inner.get(r).await
    }
    async fn find_original(&self, hash: &Sha1Hash) -> Result<Option<(BlobRef, Bytes)>, StoreError> {
        self.inner.find_original(hash).await
    }
}

fn counting() -> Arc<CountingStore> {
    Arc::new(CountingStore {
        inner: ObjectBlobStore::memory(),
        variant_puts: AtomicUsize::new(0),
    })
}

async fn service_with(store: Arc<dyn BlobStore>, cfg: ServiceConfig) -> ImageService {
    ImageService::new(store, Catalog::in_memory().await.unwrap(), cfg)
}

async fn service() -> ImageService {
    service_with(
        Arc::new(ObjectBlobStore::memory()),
        ServiceConfig::default(),
    )
    .await
}

fn jpeg(w: u32, h: u32) -> Bytes {
    let img = DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([x as u8, y as u8, 99])
    }));
    let mut out = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut out),
        image::ImageFormat::Jpeg,
    )
    .unwrap();
    Bytes::from(out)
}

fn req(hash: Sha1Hash, spec: &str, format: Format) -> ImageRequest {
    ImageRequest {
        hash,
        spec: spec.parse().unwrap(),
        format,
    }
}

fn image(served: Served) -> (Bytes, Format, String) {
    match served {
        Served::Image {
            bytes,
            format,
            etag,
        } => (bytes, format, etag),
        Served::NotModified { .. } => panic!("expected an image, got 304"),
    }
}

/// Waits for a background write to show up in the catalog.
async fn wait_for_variant(
    svc: &ImageService,
    hash: &Sha1Hash,
    spec: &str,
    format: Format,
) -> VariantRow {
    let spec: SizeSpec = spec.parse().unwrap();
    for _ in 0..200 {
        if let Some(row) = svc
            .catalog()
            .get_variant(hash, &spec, format)
            .await
            .unwrap()
        {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("variant {hash}/{spec}.{format} never stored");
}

#[tokio::test]
async fn upload_then_get_original() {
    let svc = service().await;
    let bytes = jpeg(64, 48);
    let up = svc.upload(bytes.clone()).await.unwrap();
    assert_eq!(up.hash, Sha1Hash::digest(&bytes));
    assert_eq!(up.format, Format::Jpeg);
    assert_eq!(up.length, bytes.len());
    assert!(up.created);

    let (got, format, etag) = image(
        svc.get(&req(up.hash, "full", Format::Jpeg), None)
            .await
            .unwrap(),
    );
    assert_eq!(got, bytes, "original is served byte-for-byte");
    assert_eq!(format, Format::Jpeg);
    assert_eq!(etag, up.hash.to_string());
}

#[tokio::test]
async fn upload_is_idempotent() {
    let svc = service().await;
    let bytes = jpeg(8, 8);
    assert!(svc.upload(bytes.clone()).await.unwrap().created);
    assert!(!svc.upload(bytes).await.unwrap().created);
}

#[tokio::test]
async fn upload_rejects_non_images() {
    let svc = service().await;
    assert!(matches!(
        svc.upload(Bytes::from_static(b"not an image")).await,
        Err(ServiceError::BadUpload(ImagingError::Unsupported))
    ));
}

#[tokio::test]
async fn upload_rejects_too_many_pixels() {
    let cfg = ServiceConfig {
        limits: Limits {
            max_input_pixels: 100,
            ..Limits::default()
        },
        ..ServiceConfig::default()
    };
    let svc = service_with(Arc::new(ObjectBlobStore::memory()), cfg).await;
    assert!(matches!(
        svc.upload(jpeg(20, 20)).await,
        Err(ServiceError::BadUpload(ImagingError::InputTooLarge { .. }))
    ));
}

#[tokio::test]
async fn unknown_hash_is_not_found() {
    let svc = service().await;
    assert!(matches!(
        svc.get(&req(Sha1Hash::digest(b"nope"), "100s", Format::Jpeg), None)
            .await,
        Err(ServiceError::NotFound)
    ));
}

#[tokio::test]
async fn variant_is_rendered_then_served_from_store() {
    let store = counting();
    let svc = service_with(store.clone(), ServiceConfig::default()).await;
    let up = svc.upload(jpeg(64, 48)).await.unwrap();
    let r = req(up.hash, "32w", Format::Webp);

    let (first, format, etag) = image(svc.get(&r, None).await.unwrap());
    assert_eq!(format, Format::Webp);
    assert_eq!(imaging::sniff(&first), Some(Format::Webp));
    assert_eq!(etag, Sha1Hash::digest(&first).to_string());

    let row = wait_for_variant(&svc, &up.hash, "32w", Format::Webp).await;
    assert_eq!(row.etag, etag);

    let (second, _, etag2) = image(svc.get(&r, None).await.unwrap());
    assert_eq!(second, first);
    assert_eq!(etag2, etag);
    assert_eq!(
        store.variant_puts.load(Ordering::SeqCst),
        1,
        "second request didn't re-render"
    );
}

#[tokio::test]
async fn full_in_another_format_is_a_variant() {
    let svc = service().await;
    let up = svc.upload(jpeg(30, 20)).await.unwrap();
    let (bytes, format, _) = image(
        svc.get(&req(up.hash, "full", Format::Png), None)
            .await
            .unwrap(),
    );
    assert_eq!(format, Format::Png);
    assert_eq!(imaging::sniff(&bytes), Some(Format::Png));
}

#[tokio::test]
async fn if_none_match() {
    let svc = service().await;
    let up = svc.upload(jpeg(40, 40)).await.unwrap();
    let h = up.hash.to_string();

    // Original: matches quoted, unquoted and weak forms, and lists.
    for inm in [
        format!("\"{h}\""),
        h.clone(),
        format!("W/\"{h}\""),
        format!("\"x\", \"{h}\""),
        "*".into(),
    ] {
        assert!(
            matches!(
                svc.get(&req(up.hash, "full", Format::Jpeg), Some(&inm))
                    .await
                    .unwrap(),
                Served::NotModified { .. }
            ),
            "{inm}"
        );
    }
    assert!(matches!(
        svc.get(&req(up.hash, "full", Format::Jpeg), Some("\"other\""))
            .await
            .unwrap(),
        Served::Image { .. }
    ));

    // Variant: after the first render, a matching ETag gets a 304.
    let r = req(up.hash, "10s", Format::Png);
    let (_, _, etag) = image(svc.get(&r, None).await.unwrap());
    wait_for_variant(&svc, &up.hash, "10s", Format::Png).await;
    assert!(matches!(
        svc.get(&r, Some(&format!("\"{etag}\""))).await.unwrap(),
        Served::NotModified { .. }
    ));
}

#[tokio::test]
async fn output_limit_is_enforced_before_rendering() {
    let store = counting();
    let svc = service_with(store.clone(), ServiceConfig::default()).await;
    let up = svc.upload(jpeg(10, 400)).await.unwrap();
    // 200w on a 1:40 image would be 200x8000.
    assert!(matches!(
        svc.get(&req(up.hash, "200w", Format::Jpeg), None).await,
        Err(ServiceError::TooLarge(ImagingError::OutputTooLarge { .. }))
    ));
    assert!(matches!(
        svc.get(&req(up.hash, "5000s", Format::Jpeg), None).await,
        Err(ServiceError::TooLarge(_))
    ));
    assert_eq!(store.variant_puts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn catalog_is_rebuilt_from_store() {
    let store: Arc<dyn BlobStore> = Arc::new(ObjectBlobStore::memory());
    let bytes = jpeg(64, 48);
    let hash = service_with(store.clone(), ServiceConfig::default())
        .await
        .upload(bytes.clone())
        .await
        .unwrap()
        .hash;

    // A new service with an empty catalog, but the same store.
    let svc = service_with(store, ServiceConfig::default()).await;
    assert!(svc.catalog().get_original(&hash).await.unwrap().is_none());

    let (got, _, _) = image(
        svc.get(&req(hash, "full", Format::Jpeg), None)
            .await
            .unwrap(),
    );
    assert_eq!(got, bytes);
    let row = svc.catalog().get_original(&hash).await.unwrap().unwrap();
    assert_eq!((row.format, row.width, row.height), (Format::Jpeg, 64, 48));

    // Variants work too after recovery.
    image(svc.get(&req(hash, "16s", Format::Png), None).await.unwrap());
}

#[tokio::test]
async fn missing_variant_blob_is_re_rendered() {
    let svc = service().await;
    let up = svc.upload(jpeg(32, 32)).await.unwrap();
    let spec: SizeSpec = "8s".parse().unwrap();
    svc.catalog()
        .put_variant(&VariantRow {
            hash: up.hash,
            spec,
            format: Format::Png,
            blob_ref: BlobRef("does/not/exist.png".into()),
            etag: "stale".into(),
            bytes: 1,
        })
        .await
        .unwrap();
    let (bytes, _, etag) = image(
        svc.get(&req(up.hash, "8s", Format::Png), None)
            .await
            .unwrap(),
    );
    assert_eq!(imaging::sniff(&bytes), Some(Format::Png));
    assert_ne!(etag, "stale");
}

#[tokio::test]
async fn concurrent_requests_share_one_render() {
    let store = counting();
    let svc = service_with(store.clone(), ServiceConfig::default()).await;
    let up = svc.upload(jpeg(400, 300)).await.unwrap();
    let r = req(up.hash, "123w", Format::Webp);

    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let svc = svc.clone();
            tokio::spawn(async move { image(svc.get(&r, None).await.unwrap()) })
        })
        .collect();
    let mut etags = Vec::new();
    for t in tasks {
        etags.push(t.await.unwrap().2);
    }
    etags.dedup();
    assert_eq!(etags.len(), 1);
    wait_for_variant(&svc, &up.hash, "123w", Format::Webp).await;
    assert_eq!(store.variant_puts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn timed_out_render_still_completes_and_is_stored() {
    let cfg = ServiceConfig {
        scale_timeout: Duration::ZERO,
        ..ServiceConfig::default()
    };
    let svc = service_with(Arc::new(ObjectBlobStore::memory()), cfg).await;
    let up = svc.upload(jpeg(800, 600)).await.unwrap();
    assert!(matches!(
        svc.get(&req(up.hash, "300w", Format::Webp), None).await,
        Err(ServiceError::Timeout)
    ));
    // The render carried on in the background and was stored, so this is served from the store
    // even with a zero timeout.
    wait_for_variant(&svc, &up.hash, "300w", Format::Webp).await;
    image(
        svc.get(&req(up.hash, "300w", Format::Webp), None)
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn prewarm_renders_in_original_format() {
    let svc = service().await;
    let up = svc.upload(jpeg(50, 50)).await.unwrap();
    svc.prewarm(
        up.hash,
        up.format,
        vec!["10s".parse().unwrap(), "20w".parse().unwrap()],
    );
    wait_for_variant(&svc, &up.hash, "10s", Format::Jpeg).await;
    wait_for_variant(&svc, &up.hash, "20w", Format::Jpeg).await;
}

#[test]
fn etag_matching() {
    assert!(!etag_matches(None, "abc"));
    assert!(etag_matches(Some("abc"), "abc"));
    assert!(etag_matches(Some("\"abc\""), "abc"));
    assert!(etag_matches(Some("W/\"abc\""), "abc"));
    assert!(etag_matches(Some("\"x\" , \"abc\""), "abc"));
    assert!(etag_matches(Some("*"), "abc"));
    assert!(!etag_matches(Some("\"abcd\""), "abc"));
    assert!(!etag_matches(Some(""), "abc"));
}
