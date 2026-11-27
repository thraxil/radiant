//! [`BlobStore`] backed by a [Cask](https://github.com/thraxil/cask) cluster.
//!
//! Cask stores blobs under the SHA-1 of their content, and any node serves any key (it fetches
//! from the rest of the cluster when it has no local copy). So:
//!
//! * an original's ref is `sha1:<hash>`, the same hash radiant uses, and needs no lookup;
//! * a variant's ref is `sha1:<sha1 of the variant bytes>`, recorded in the catalog;
//! * every blob read is checked against its key, which costs nothing extra because the key is
//!   the hash.
//!
//! Radiant is given a list of nodes. Each request starts at the next node in turn, and moves to
//! the next node on a connection error, a timeout, a 5xx, or a bad response. A 404 from a
//! healthy node means the cluster doesn't have the key, so it is returned without asking the
//! others.
//!
//! Cask can't delete, so a variant that is re-rendered with different bytes (e.g. after an
//! encoder setting change) leaves the old blob behind.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use reqwest::StatusCode;
use serde::Deserialize;

use super::{BlobName, BlobRef, BlobStore, StoreError};
use crate::hash::Sha1Hash;

#[derive(Clone, Debug)]
pub struct CaskConfig {
    /// Base URLs of the nodes to talk to, e.g. `http://cask1:8080`.
    pub urls: Vec<String>,
    /// Per-request timeout.
    pub timeout: Duration,
}

pub struct CaskStore {
    client: reqwest::Client,
    nodes: Vec<String>,
    next: AtomicUsize,
}

#[derive(Deserialize)]
struct PostResponse {
    key: String,
    success: bool,
}

fn err(message: impl Into<String>) -> StoreError {
    StoreError {
        backend: "cask",
        message: message.into(),
    }
}

fn key_for(hash: &Sha1Hash) -> String {
    format!("sha1:{hash}")
}

/// Parses `sha1:<40 hex>`, the only key format Cask uses.
fn hash_of_ref(r: &BlobRef) -> Result<Sha1Hash, StoreError> {
    r.0.strip_prefix("sha1:")
        .and_then(|h| h.parse().ok())
        .ok_or_else(|| err(format!("not a cask key: {r}")))
}

async fn sha1_of(bytes: Bytes) -> (Sha1Hash, Bytes) {
    tokio::task::spawn_blocking(move || (Sha1Hash::digest(&bytes), bytes))
        .await
        .expect("hashing task panicked")
}

/// Records why one node couldn't serve a request (the next node is tried after this) and
/// returns the reason for the error message if every node fails. Each failure is logged and
/// counted, so a dead node shows up even while failover hides it from clients.
fn node_failure(op: &'static str, node: &str, what: impl std::fmt::Display) -> String {
    tracing::warn!(node, op, reason = %what, "cask node failed; trying next node");
    metrics::counter!(
        crate::telemetry::CASK_NODE_FAILURES,
        "node" => node.to_owned(),
        "op" => op
    )
    .increment(1);
    format!("{node}: {what}")
}

impl CaskStore {
    pub fn new(cfg: &CaskConfig) -> Result<Self, StoreError> {
        let nodes: Vec<String> = cfg
            .urls
            .iter()
            .map(|u| u.trim().trim_end_matches('/').to_owned())
            .filter(|u| !u.is_empty())
            .collect();
        if nodes.is_empty() {
            return Err(err("no cask node URLs configured"));
        }
        for n in &nodes {
            if !(n.starts_with("http://") || n.starts_with("https://")) {
                return Err(err(format!(
                    "cask node URL must start with http(s)://: {n}"
                )));
            }
        }
        let client = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| err(e.to_string()))?;
        Ok(Self {
            client,
            nodes,
            next: AtomicUsize::new(0),
        })
    }

    /// All nodes, starting from the next one in turn, so load spreads across them.
    fn nodes_in_turn(&self) -> impl Iterator<Item = &str> {
        let start = self.next.fetch_add(1, Ordering::Relaxed) % self.nodes.len();
        self.nodes[start..]
            .iter()
            .chain(&self.nodes[..start])
            .map(String::as_str)
    }

    fn all_failed(&self, op: &str, failures: &[String]) -> StoreError {
        err(format!(
            "{op} failed on all {} nodes: {}",
            self.nodes.len(),
            failures.join("; ")
        ))
    }

    /// POSTs a blob to the cluster and returns its key, which must equal `expected`.
    async fn post(&self, bytes: Bytes, expected: &str) -> Result<String, StoreError> {
        let mut failures = Vec::new();
        for node in self.nodes_in_turn() {
            let len = bytes.len() as u64;
            let form = reqwest::multipart::Form::new().part(
                "file",
                reqwest::multipart::Part::stream_with_length(bytes.clone(), len).file_name("blob"),
            );
            let res = match self
                .client
                .post(format!("{node}/"))
                .multipart(form)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    failures.push(node_failure("write", node, e));
                    continue;
                }
            };
            let status = res.status();
            if status != StatusCode::OK {
                failures.push(node_failure("write", node, format!("HTTP {status}")));
                continue;
            }
            let body: PostResponse =
                match res.bytes().await.map_err(|e| e.to_string()).and_then(|b| {
                    serde_json::from_slice(&b).map_err(|e| format!("bad response: {e}"))
                }) {
                    Ok(b) => b,
                    Err(e) => {
                        failures.push(node_failure("write", node, e));
                        continue;
                    }
                };
            if !body.success {
                // The node couldn't place a copy anywhere (e.g. every node is read-only or full).
                failures.push(node_failure(
                    "write",
                    node,
                    "cluster did not accept the write",
                ));
                continue;
            }
            if body.key != expected {
                failures.push(node_failure(
                    "write",
                    node,
                    format!("returned key {} for content with key {expected}", body.key),
                ));
                continue;
            }
            return Ok(body.key);
        }
        Err(self.all_failed("write", &failures))
    }
}

#[async_trait]
impl BlobStore for CaskStore {
    fn backend(&self) -> &'static str {
        "cask"
    }

    async fn put(&self, name: &BlobName, bytes: Bytes) -> Result<BlobRef, StoreError> {
        let (actual, bytes) = sha1_of(bytes).await;
        if let BlobName::Original { hash, .. } = name
            && *hash != actual
        {
            return Err(err(format!(
                "original {hash} has content hashing to {actual}"
            )));
        }
        let key = self.post(bytes, &key_for(&actual)).await?;
        Ok(BlobRef(key))
    }

    async fn get(&self, r: &BlobRef) -> Result<Option<Bytes>, StoreError> {
        let expected = hash_of_ref(r)?;
        let mut failures = Vec::new();
        for node in self.nodes_in_turn() {
            let res = match self.client.get(format!("{node}/file/{r}/")).send().await {
                Ok(res) => res,
                Err(e) => {
                    failures.push(node_failure("read", node, e));
                    continue;
                }
            };
            match res.status() {
                StatusCode::OK => {}
                // The node looked across the cluster and nobody has it.
                StatusCode::NOT_FOUND => return Ok(None),
                StatusCode::BAD_REQUEST => return Err(err(format!("cask rejected key {r}"))),
                status => {
                    failures.push(node_failure("read", node, format!("HTTP {status}")));
                    continue;
                }
            }
            let bytes = match res.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    failures.push(node_failure("read", node, e));
                    continue;
                }
            };
            let (actual, bytes) = sha1_of(bytes).await;
            if actual != expected {
                // Cask verifies on read too, so this is unlikely. Another node may have a good copy.
                tracing::error!(node, key = %r, %actual, "cask returned corrupt data");
                failures.push(node_failure(
                    "read",
                    node,
                    format!("corrupt data (hashes to {actual})"),
                ));
                continue;
            }
            return Ok(Some(bytes));
        }
        Err(self.all_failed("read", &failures))
    }

    async fn find_original(&self, hash: &Sha1Hash) -> Result<Option<(BlobRef, Bytes)>, StoreError> {
        let r = BlobRef(key_for(hash));
        Ok(self.get(&r).await?.map(|bytes| (r, bytes)))
    }

    async fn ping(&self) -> Result<(), StoreError> {
        // Ready if any node answers: the others are failover targets.
        let mut failures = Vec::new();
        for node in &self.nodes {
            match self.client.get(format!("{node}/config/")).send().await {
                Ok(res) if res.status().is_success() => return Ok(()),
                Ok(res) => {
                    failures.push(node_failure("ping", node, format!("HTTP {}", res.status())))
                }
                Err(e) => failures.push(node_failure("ping", node, e)),
            }
        }
        Err(self.all_failed("ping", &failures))
    }
}

#[cfg(test)]
pub(crate) mod tests;
