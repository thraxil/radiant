use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use super::*;
use crate::format::Format;
use crate::store::conformance;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Healthy,
    /// Every request gets a 500.
    ServerError,
    /// Reads return the wrong bytes.
    Corrupt,
    /// Writes answer `"success": false`.
    RejectWrites,
}

type Blobs = Arc<Mutex<HashMap<String, Bytes>>>;

#[derive(Clone)]
struct Fake {
    mode: Mode,
    blobs: Blobs,
    hits: Arc<AtomicUsize>,
}

async fn fake_post(State(f): State<Fake>, mut mp: Multipart) -> Response {
    f.hits.fetch_add(1, Ordering::SeqCst);
    if f.mode == Mode::ServerError {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let mut data = None;
    while let Some(field) = mp.next_field().await.unwrap() {
        if field.name() == Some("file") {
            data = Some(field.bytes().await.unwrap());
        }
    }
    let data = data.expect("file field");
    let key = format!("sha1:{}", Sha1Hash::digest(&data));
    let success = f.mode != Mode::RejectWrites;
    if success {
        f.blobs.lock().unwrap().insert(key.clone(), data);
    }
    format!(r#"{{"key":"{key}","success":{success}}}"#).into_response()
}

async fn fake_get(State(f): State<Fake>, Path(key): Path<String>) -> Response {
    f.hits.fetch_add(1, Ordering::SeqCst);
    match f.mode {
        Mode::ServerError => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Mode::Corrupt => return "garbage".into_response(),
        _ => {}
    }
    if !key.starts_with("sha1:") {
        return (StatusCode::BAD_REQUEST, "invalid key").into_response();
    }
    match f.blobs.lock().unwrap().get(&key) {
        Some(b) => b.clone().into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn fake_config(State(f): State<Fake>) -> Response {
    if f.mode == Mode::ServerError {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    "{}".into_response()
}

/// Starts a fake node and returns its URL and request counter. Nodes given the same `blobs`
/// behave like one cluster.
async fn fake_node(mode: Mode, blobs: &Blobs) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/", post(fake_post))
        .route("/file/{key}/", get(fake_get))
        .route("/config/", get(fake_config))
        .with_state(Fake {
            mode,
            blobs: blobs.clone(),
            hits: hits.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, hits)
}

/// A URL where nothing is listening.
async fn down_node() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    url
}

fn store(urls: &[&str]) -> CaskStore {
    CaskStore::new(&CaskConfig {
        urls: urls.iter().map(|u| u.to_string()).collect(),
        timeout: Duration::from_secs(5),
    })
    .unwrap()
}

fn blobs() -> Blobs {
    Arc::default()
}

fn original(content: &'static [u8]) -> (BlobName, Bytes) {
    (
        BlobName::Original {
            hash: Sha1Hash::digest(content),
            format: Format::Png,
        },
        Bytes::from_static(content),
    )
}

#[tokio::test]
async fn conforms_against_fake_node() {
    let b = blobs();
    let (url, _) = fake_node(Mode::Healthy, &b).await;
    conformance::run(&store(&[&url])).await;
}

#[tokio::test]
async fn refs_are_content_keys() {
    let b = blobs();
    let (url, _) = fake_node(Mode::Healthy, &b).await;
    let s = store(&[&url]);
    let (name, bytes) = original(b"abc");
    assert_eq!(
        s.put(&name, bytes).await.unwrap().0,
        "sha1:a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    let v = s
        .put(
            &BlobName::Variant {
                hash: Sha1Hash::digest(b"abc"),
                spec: "10s".parse().unwrap(),
                format: Format::Webp,
            },
            Bytes::from_static(b"variant"),
        )
        .await
        .unwrap();
    assert_eq!(v.0, format!("sha1:{}", Sha1Hash::digest(b"variant")));
}

#[tokio::test]
async fn put_rejects_original_whose_bytes_do_not_match_its_hash() {
    let b = blobs();
    let (url, hits) = fake_node(Mode::Healthy, &b).await;
    let s = store(&[&url]);
    let name = BlobName::Original {
        hash: Sha1Hash::digest(b"one thing"),
        format: Format::Png,
    };
    assert!(s.put(&name, Bytes::from_static(b"another")).await.is_err());
    assert_eq!(hits.load(Ordering::SeqCst), 0, "nothing should be sent");
}

#[tokio::test]
async fn fails_over_from_down_and_erroring_nodes() {
    let b = blobs();
    let down = down_node().await;
    let (broken, _) = fake_node(Mode::ServerError, &b).await;
    let (good, _) = fake_node(Mode::Healthy, &b).await;
    let s = store(&[&down, &broken, &good]);
    // Several rounds, so every starting node in the rotation is exercised.
    for i in 0..6u8 {
        let content = Bytes::from(vec![i; 10]);
        let name = BlobName::Original {
            hash: Sha1Hash::digest(&content),
            format: Format::Png,
        };
        let r = s.put(&name, content.clone()).await.unwrap();
        assert_eq!(s.get(&r).await.unwrap(), Some(content));
    }
    s.ping().await.unwrap();
}

#[tokio::test]
async fn corrupt_node_is_skipped() {
    let b = blobs();
    let (corrupt, _) = fake_node(Mode::Corrupt, &b).await;
    let (good, _) = fake_node(Mode::Healthy, &b).await;
    let s = store(&[&corrupt, &good]);
    let (name, bytes) = original(b"precious");
    let r = s.put(&name, bytes.clone()).await.unwrap();
    for _ in 0..4 {
        assert_eq!(s.get(&r).await.unwrap(), Some(bytes.clone()));
    }
}

#[tokio::test]
async fn not_found_is_trusted_without_asking_other_nodes() {
    let b = blobs();
    let (first, first_hits) = fake_node(Mode::Healthy, &b).await;
    let (second, second_hits) = fake_node(Mode::Healthy, &b).await;
    let s = store(&[&first, &second]);
    let missing = BlobRef(format!("sha1:{}", Sha1Hash::digest(b"missing")));
    assert_eq!(s.get(&missing).await.unwrap(), None);
    assert_eq!(
        first_hits.load(Ordering::SeqCst) + second_hits.load(Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn requests_rotate_across_nodes() {
    let b = blobs();
    let (n1, h1) = fake_node(Mode::Healthy, &b).await;
    let (n2, h2) = fake_node(Mode::Healthy, &b).await;
    let s = store(&[&n1, &n2]);
    let (name, bytes) = original(b"spread me");
    let r = s.put(&name, bytes).await.unwrap();
    for _ in 0..9 {
        s.get(&r).await.unwrap().unwrap();
    }
    assert_eq!(h1.load(Ordering::SeqCst), 5);
    assert_eq!(h2.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn errors_when_every_node_fails() {
    let b = blobs();
    let down = down_node().await;
    let (broken, _) = fake_node(Mode::ServerError, &b).await;
    let s = store(&[&down, &broken]);
    let (name, bytes) = original(b"x");
    let e = s.put(&name, bytes).await.unwrap_err();
    assert!(e.message.contains("all 2 nodes"), "{e}");
    let r = BlobRef(format!("sha1:{}", Sha1Hash::digest(b"x")));
    assert!(s.get(&r).await.is_err());
    assert!(s.ping().await.is_err());
}

#[tokio::test]
async fn rejected_write_is_an_error() {
    let b = blobs();
    let (url, _) = fake_node(Mode::RejectWrites, &b).await;
    let s = store(&[&url]);
    let (name, bytes) = original(b"nowhere to go");
    let e = s.put(&name, bytes).await.unwrap_err();
    assert!(e.message.contains("did not accept"), "{e}");
}

#[tokio::test]
async fn bad_refs_are_rejected_locally() {
    let s = store(&["http://127.0.0.1:1"]);
    assert!(s.get(&BlobRef("abc/full.png".into())).await.is_err());
    assert!(s.get(&BlobRef("sha1:nothex".into())).await.is_err());
}

#[test]
fn config_validation() {
    let cfg = |urls: &[&str]| CaskConfig {
        urls: urls.iter().map(|u| u.to_string()).collect(),
        timeout: Duration::from_secs(1),
    };
    assert!(CaskStore::new(&cfg(&[])).is_err());
    assert!(CaskStore::new(&cfg(&["", " "])).is_err());
    assert!(CaskStore::new(&cfg(&["cask1:8080"])).is_err());
    let s = CaskStore::new(&cfg(&["http://a:1/", " http://b:2 "])).unwrap();
    assert_eq!(s.nodes, vec!["http://a:1", "http://b:2"]);
}

/// Runs only under `scripts/cask-cluster.sh`, which sets `RADIANT_TEST_CASK_URLS`.
pub(crate) fn test_cask() -> Option<CaskStore> {
    let urls = std::env::var("RADIANT_TEST_CASK_URLS").ok()?;
    Some(
        CaskStore::new(&CaskConfig {
            urls: urls.split(',').map(str::to_owned).collect(),
            timeout: Duration::from_secs(10),
        })
        .unwrap(),
    )
}

#[tokio::test]
async fn cask_cluster_conforms() {
    let Some(s) = test_cask() else {
        eprintln!("skipping: RADIANT_TEST_CASK_URLS not set (run scripts/test-cask.sh)");
        return;
    };
    conformance::run(&s).await;
}

#[tokio::test]
async fn cask_cluster_fails_over_from_a_dead_node() {
    let Some(real) = test_cask() else {
        return;
    };
    let mut urls = vec![down_node().await];
    urls.extend(real.nodes.iter().cloned());
    let s = CaskStore::new(&CaskConfig {
        urls,
        timeout: Duration::from_secs(10),
    })
    .unwrap();
    let content = Bytes::from(format!("failover {:?}", std::time::SystemTime::now()));
    let name = BlobName::Original {
        hash: Sha1Hash::digest(&content),
        format: Format::Png,
    };
    for _ in 0..4 {
        let r = s.put(&name, content.clone()).await.unwrap();
        assert_eq!(s.get(&r).await.unwrap(), Some(content.clone()));
    }
    s.ping().await.unwrap();
}
