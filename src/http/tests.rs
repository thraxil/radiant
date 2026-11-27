use std::sync::Arc;

use ::image::{DynamicImage, RgbImage};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::*;
use crate::catalog::Catalog;
use crate::service::{ImageService, ServiceConfig};
use crate::store::ObjectBlobStore;

const H: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";
const BOUNDARY: &str = "radiant-test-boundary";

async fn app_with(key: Option<&str>, max_upload_bytes: usize) -> Router {
    let service = ImageService::new(
        Arc::new(ObjectBlobStore::memory()),
        Catalog::in_memory().await.unwrap(),
        ServiceConfig::default(),
    );
    router(AppState {
        service,
        upload_key: key.map(Arc::from),
        max_upload_bytes,
        upload_permits: Arc::new(tokio::sync::Semaphore::new(4)),
        request_timeout: std::time::Duration::from_secs(60),
        metrics: Some(test_metrics()),
    })
}

/// The metrics recorder is global, so install it once for every test.
fn test_metrics() -> metrics_exporter_prometheus::PrometheusHandle {
    static HANDLE: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    HANDLE
        .get_or_init(|| crate::telemetry::install().expect("install metrics recorder"))
        .clone()
}

async fn app() -> Router {
    app_with(None, 10 << 20).await
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| {
        ::image::Rgb([x as u8, y as u8, 7])
    }));
    let mut out = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut out),
        ::image::ImageFormat::Png,
    )
    .unwrap();
    out
}

/// Builds a multipart/form-data body. `(name, filename, bytes)`. A filename makes it a file
/// part, sent as `application/octet-stream` to show that the server ignores the client's
/// content type.
fn multipart(parts: &[(&str, Option<&str>, &[u8])]) -> Request<Body> {
    let mut body = Vec::new();
    for (name, filename, data) in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match filename {
            Some(f) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n\
                     Content-Type: application/octet-stream\r\n\r\n"
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    Request::post("/")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap()
}

async fn send(app: &Router, req: Request<Body>) -> Response {
    app.clone().oneshot(req).await.unwrap()
}

async fn get(app: &Router, uri: &str) -> Response {
    send(app, Request::get(uri).body(Body::empty()).unwrap()).await
}

async fn body(res: Response) -> Vec<u8> {
    res.into_body().collect().await.unwrap().to_bytes().to_vec()
}

async fn json(res: Response) -> serde_json::Value {
    serde_json::from_slice(&body(res).await).unwrap()
}

async fn upload(app: &Router, bytes: &[u8]) -> serde_json::Value {
    let res = send(app, multipart(&[("image", Some("x.bin"), bytes)])).await;
    assert_eq!(res.status(), StatusCode::OK);
    json(res).await
}

#[tokio::test]
async fn healthz_and_readyz() {
    let app = app().await;
    assert_eq!(get(&app, "/healthz").await.status(), StatusCode::OK);
    assert_eq!(get(&app, "/readyz").await.status(), StatusCode::OK);
}

#[tokio::test]
async fn upload_form() {
    let open = body(get(&app().await, "/").await).await;
    let open = String::from_utf8(open).unwrap();
    assert!(open.contains(r#"name="image""#));
    assert!(!open.contains(r#"name="key""#));

    let keyed = body(get(&app_with(Some("s3cret"), 1 << 20).await, "/").await).await;
    assert!(String::from_utf8(keyed).unwrap().contains(r#"name="key""#));
}

#[tokio::test]
async fn upload_response_matches_reticulum_shape() {
    let app = app().await;
    let bytes = png(20, 10);
    let v = upload(&app, &bytes).await;
    let hash = crate::hash::Sha1Hash::digest(&bytes).to_string();
    assert_eq!(
        v,
        serde_json::json!({
            "hash": hash,
            "length": bytes.len(),
            "extension": "png",
            "full_url": format!("/image/{hash}/full/image.png"),
            "satisfied": true,
            "nodes": [],
        })
    );
}

#[tokio::test]
async fn upload_then_fetch_original_and_variants() {
    let app = app().await;
    let bytes = png(64, 32);
    let v = upload(&app, &bytes).await;
    let hash = v["hash"].as_str().unwrap();

    let res = get(&app, v["full_url"].as_str().unwrap()).await;
    assert_eq!(res.status(), StatusCode::OK);
    let h = res.headers();
    assert_eq!(h[header::CONTENT_TYPE], "image/png");
    assert_eq!(h[header::ETAG], format!("\"{hash}\"").as_str());
    assert_eq!(
        h[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert!(h.contains_key(header::EXPIRES));
    assert_eq!(body(res).await, bytes);

    // The filename stem is ignored. Only the extension matters.
    let res = get(&app, &format!("/image/{hash}/16w/anything.webp")).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "image/webp");
    let img = ::image::load_from_memory(&body(res).await).unwrap();
    assert_eq!((img.width(), img.height()), (16, 8));
}

#[tokio::test]
async fn conditional_get() {
    let app = app().await;
    let v = upload(&app, &png(8, 8)).await;
    let url = format!("/image/{}/4s/image.jpg", v["hash"].as_str().unwrap());
    let first = get(&app, &url).await;
    let etag = first.headers()[header::ETAG].clone();

    let res = send(
        &app,
        Request::get(&url)
            .header(header::IF_NONE_MATCH, etag.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(res.headers()[header::ETAG], etag);
    assert!(body(res).await.is_empty());
}

#[tokio::test]
async fn upload_key() {
    let app = app_with(Some("s3cret"), 1 << 20).await;
    let img = png(4, 4);
    for (key, expected) in [
        (None, StatusCode::FORBIDDEN),
        (Some("wrong"), StatusCode::FORBIDDEN),
        (Some("s3cre"), StatusCode::FORBIDDEN),
        (Some("s3cret"), StatusCode::OK),
    ] {
        let mut parts: Vec<(&str, Option<&str>, &[u8])> = vec![("image", Some("a.png"), &img)];
        if let Some(k) = key {
            parts.push(("key", None, k.as_bytes()));
        }
        assert_eq!(
            send(&app, multipart(&parts)).await.status(),
            expected,
            "{key:?}"
        );
    }
    // Field order doesn't matter.
    let res = send(
        &app,
        multipart(&[("key", None, b"s3cret"), ("image", Some("a.png"), &img)]),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn upload_rejections() {
    let app = app_with(None, 4096).await;
    let not_image = send(&app, multipart(&[("image", Some("a.txt"), b"hello")])).await;
    assert_eq!(not_image.status(), StatusCode::BAD_REQUEST);

    let missing = send(&app, multipart(&[("size_hints", None, b"100s")])).await;
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

    let big = vec![0u8; 8192];
    let too_big = send(&app, multipart(&[("image", Some("a.png"), &big)])).await;
    assert_eq!(too_big.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn size_hints_are_prerendered() {
    let app = app().await;
    let img = png(40, 40);
    let res = send(
        &app,
        multipart(&[
            ("image", Some("a.png"), &img),
            ("size_hints", None, b"10s, 20w,bogus,full,"),
        ]),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    // Behaviour is covered by the service tests. Here we only check that junk hints don't
    // break the upload.
}

#[tokio::test]
async fn error_statuses() {
    let app = app().await;
    assert_eq!(
        get(&app, &format!("/image/{H}/full/image.jpg"))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let v = upload(&app, &png(10, 400)).await;
    let hash = v["hash"].as_str().unwrap();
    // 200w of a 1:40 image would be 200x8000, over the 4096 limit.
    assert_eq!(
        get(&app, &format!("/image/{hash}/200w/image.png"))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&app, "/image/nothex/full/image.jpg").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn non_canonical_size_is_301() {
    let res = get(&app().await, &format!("/image/{H}/200h100w/image.jpeg")).await;
    assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(
        res.headers()[header::LOCATION],
        format!("/image/{H}/100w200h/image.jpg").as_str()
    );
}

#[tokio::test]
async fn percent_encoded_filename_survives_redirect() {
    let res = get(&app().await, &format!("/image/{H}/0100w/my%20photo.png")).await;
    assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(
        res.headers()[header::LOCATION],
        format!("/image/{H}/100w/my%20photo.png").as_str()
    );
}

#[tokio::test]
async fn metrics_endpoint() {
    let app = app().await;
    let v = upload(&app, &png(20, 20)).await;
    let hash = v["hash"].as_str().unwrap();
    assert_eq!(
        get(&app, &format!("/image/{hash}/10s/a.webp"))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        get(&app, &format!("/image/{hash}/full/a.png"))
            .await
            .status(),
        StatusCode::OK
    );
    let _ = get(&app, &format!("/image/{hash}/9000s/a.png")).await;
    let _ = get(&app, "/no/such/route").await;

    let res = get(&app, "/metrics").await;
    assert_eq!(res.status(), StatusCode::OK);
    let text = String::from_utf8(body(res).await).unwrap();
    // The recorder is shared by every test in this process, so check only that series exist,
    // not exact counts.
    for needle in [
        "radiant_build_info{",
        r#"radiant_http_requests_total{route="/image/{hash}/{size}/{filename}",method="GET",status="200"}"#,
        r#"route="unmatched""#,
        r#"radiant_http_request_duration_seconds_bucket{route="/",le="#,
        r#"radiant_images_served_total{source="original"}"#,
        r#"radiant_images_served_total{source="variant_rendered"}"#,
        r#"radiant_uploads_total{outcome="created"}"#,
        r#"radiant_limit_rejections_total{limit="output_dimension"}"#,
        r#"radiant_scale_duration_seconds_bucket{format="webp",outcome="ok",le="#,
        "radiant_scale_running",
        "radiant_upload_size_bytes_bucket",
    ] {
        assert!(text.contains(needle), "missing {needle} in:\n{text}");
    }
}
