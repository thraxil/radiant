# Radiant — Plan

Radiant is a single-instance image server written in Rust. It is a drop-in replacement for the
public API of [reticulum](https://github.com/thraxil/reticulum) and stores images in a pluggable
backend: **Cask**, **S3**, or the **local filesystem**. It is the successor to lumen.

```
clients ──HTTP──▶ radiant (1 instance) ──▶ Cask cluster (N nodes)
                    │  scaling in-process  └▶ or S3 bucket
                    └─ SQLite catalog      └▶ or local dir
```

## Goals

- Byte-for-byte compatible public URLs and upload API with reticulum, so existing apps and
  templates keep working unchanged.
- Originals stored durably in the backend. Scaled/converted versions ("variants") are generated
  on demand, stored in the backend, and served from there on later requests.
- Format conversion: any supported input format can be requested in any supported output format.
- Animated GIF / PNG (APNG) / WebP stay animated when scaled.
- Upscaling is allowed.
- Hard limits against abuse (upload size, decoded pixels, output dimensions, frames, CPU time).
- Prometheus metrics.
- Docker image for deployment; Nix flake for development.

## Non-goals (for now)

- Clustering, gossip, replication (that's Cask's job).
- Reticulum's internal/admin routes: `/stash/`, `/retrieve/`, `/retrieve_info/`, `/announce/`,
  `/join/`, `/status/`, `/dashboard/`, `/logs/`, `/image/<h>/debug/...`.
- Deleting images.
- Migrating existing reticulum data. This is a later milestone (M9), started once everything else is solid.

## Public API (compatibility contract)

### `GET /`
HTML upload form with fields `image`, `key` (shown when a key is configured), and `size_hints`.

### `POST /` (multipart)
| field        | meaning                                                                 |
|--------------|-------------------------------------------------------------------------|
| `image`      | the file                                                                |
| `key`        | upload secret; required if `RADIANT_UPLOAD_KEY` is set, else ignored    |
| `size_hints` | optional comma-separated size specs to pre-generate in the background   |

- The format is **sniffed from the bytes**. The `Content-Type` header is ignored. An unsupported
  format returns 400. An invalid key returns 403. An upload over the size limit returns 413.
- The hash is the lowercase hex SHA-1 of the uploaded bytes. The original is stored exactly as
  uploaded, with no re-encoding.
- The response is `application/json`, with the same fields reticulum returns:
  ```json
  {"hash": "<40 hex>", "length": 12345, "extension": "jpg",
   "full_url": "/image/<hash>/full/image.jpg", "satisfied": true, "nodes": []}
  ```
  (`satisfied` is always true and `nodes` is always empty. Both are kept for compatibility.)
- Uploading the same bytes again is idempotent.

### `GET /image/{hash}/{size}/{filename}`
- `hash`: must be 40 hex characters, otherwise 404.
- `size`: a size spec (see below). A non-canonical spec returns a **301** to the canonical one.
- `filename`: only the extension matters. The extension picks the **output format**. `.jpeg`
  returns a **301** to `.jpg`. A missing or unsupported extension returns 404, as it effectively
  did in reticulum. Reticulum redirects the size and the `.jpeg` in two hops; we send one 301
  to the same final URL.
- `full` with the original's own extension serves the original bytes untouched. Any other
  combination is a variant.
- Response headers: `Content-Type`, `ETag: "<sha1 of served bytes>"`,
  `Cache-Control: public, max-age=31536000, immutable`, and `Expires` (one year, as reticulum
  sends). `If-None-Match` returns 304, answered from the catalog without reading the backend.
- An unknown hash returns 404. Exceeding a limit (e.g. `100000w`) returns 400.

### Ops endpoints
- `GET /metrics`: Prometheus metrics.
- `GET /healthz`: liveness. `GET /readyz`: checks that the backend and the DB are reachable.
- `GET /favicon.ico`: returns an empty 200, as reticulum does.

## Size specs

The grammar and normalisation come from `thraxil/resize` `MakeSizeSpec`/`String`, and the scaling
behaviour from reticulum's current bimg worker:

| spec      | canonical | behaviour                                                        |
|-----------|-----------|------------------------------------------------------------------|
| `full`    | `full`    | no scaling (format conversion still applies)                     |
| `Ns`      | `Ns`      | centre-crop to a square, then scale to N×N                       |
| `Nw`      | `Nw`      | scale to width N, keeping aspect ratio                           |
| `Nh`      | `Nh`      | scale to height N, keeping aspect ratio                          |
| `NwMh`    | `NwMh`    | fit inside an N×M box, keeping aspect ratio, **no crop**         |

- Parsing copies the Go version's lenient regex search: `s` takes precedence, then `w`/`h` are
  matched anywhere in the string (`200h100w` → `100w200h`, `x100sx` → `100s`). The result is then
  checked against the canonical form, which triggers the 301. Strings that don't parse return 404.
- **Upscaling is allowed** in every mode (bimg didn't allow it, so this is a deliberate change).
- Zero dimensions return 404. Output dimensions above `RADIANT_MAX_OUTPUT_DIMENSION` return
  400. The check uses the *actual* output size, so `100w` on a very tall image can exceed the
  limit. `full` is exempt, because the input pixel limit already bounds it.
- All output is 8-bit RGB or RGBA. 16-bit and greyscale sources are converted.
- EXIF orientation is applied before scaling. Metadata is stripped from the output.
- Test suite: a table of spec → canonical string → output dimensions for a set of input
  dimensions, derived from the Go code, plus the 301 redirect cases.

## Formats

| format     | decode | encode | animated              | crate(s)                                  |
|------------|--------|--------|-----------------------|-------------------------------------------|
| JPEG       | ✓      | ✓      | —                     | `image` (zune-jpeg decoder)               |
| PNG / APNG | ✓      | ✓      | ✓ (decode + encode)   | `image` decode; `png` for APNG encode     |
| GIF        | ✓      | ✓      | ✓                     | `image` / `gif`                           |
| WebP       | ✓      | ✓      | ✓                     | `image` decodes stills; libwebp (`libwebp-sys`, own wrapper) decodes animations and encodes everything (lossy) |
| AVIF       | later  | later  | —                     | dav1d decode, `ravif` encode — **deferred** (native deps, slow encode) |

Resizing uses `fast_image_resize` (SIMD, Lanczos3). For animations, every frame is resized with
the same geometry, and the original frame delays and loop count are kept. Frames are decoded
already composited, then streamed through resize one at a time, so only the resized frames are
held. Each frame is written as a full canvas that replaces the previous one. That's larger than
delta frames, but transparency always comes out right.

Conversion rules:
- Animated → static format (e.g. GIF → JPEG): the first frame is used.
- Alpha → JPEG: the image is flattened onto white.
- Anything → GIF: quantised to a 256-colour palette.
- Encoder settings are fixed (JPEG quality configurable, default 90; WebP lossy q≈85). The same
  input and spec should therefore always produce identical bytes, which keeps Cask deduplication
  working.

## Architecture

Single binary crate, `radiant`, built on axum, tokio, and tracing.

```
src/
  main.rs            config, wiring, graceful shutdown
  config.rs          RADIANT_* env vars (clap with env feature)
  http/              axum routes: upload, image, form, ops; error → status mapping
  spec.rs            SizeSpec parse/canonicalise; ImageRequest {hash, spec, format}
  hash.rs            Sha1Hash newtype (validated 40-hex)
  imaging/           sniff, decode (with limits), resize, encode, animation
  store/             BlobStore trait + fs / s3 / cask impls
  catalog.rs         SQLite: originals + variants
  service.rs         the "get or generate" logic, single-flight, scaling pool
  metrics.rs
```

### Storage: `BlobStore` + `Catalog`

`BlobStore` is a dumb byte store. Each backend decides how a stored blob is addressed:

```rust
trait BlobStore {
    async fn put(&self, name: &BlobName, bytes: Bytes) -> Result<BlobRef>;
    async fn get(&self, r: &BlobRef) -> Result<Option<Bytes>>;
    async fn exists(&self, r: &BlobRef) -> Result<bool>;
}
// BlobName = Original { hash, ext } | Variant { hash, spec, ext }
```

| backend | original `BlobRef`        | variant `BlobRef`                 | crate          |
|---------|---------------------------|-----------------------------------|----------------|
| fs      | `<hash>/full.<ext>`       | `<hash>/<spec>.<ext>`             | `object_store` |
| S3      | `[prefix/]<hash>/full.<ext>` | `[prefix/]<hash>/<spec>.<ext>` | `object_store` |
| Cask    | `sha1:<hash>`             | `sha1:<sha1 of variant bytes>`    | `reqwest`      |

`object_store` gives us S3 (and S3-compatible stores such as MinIO and R2), the local filesystem,
and an in-memory store for tests from a single crate.

**Catalog (SQLite via `sqlx`) is used for every backend.** This differs from the earlier idea of
a naming convention only for S3. Now that radiant is single-instance and does format conversion,
one code path is simpler:

```sql
CREATE TABLE originals (
  hash TEXT PRIMARY KEY, format TEXT NOT NULL, bytes INTEGER NOT NULL,
  width INTEGER, height INTEGER, frames INTEGER, created_at TEXT NOT NULL
);
CREATE TABLE variants (
  hash TEXT NOT NULL, spec TEXT NOT NULL, format TEXT NOT NULL,
  blob_ref TEXT NOT NULL, etag TEXT NOT NULL, bytes INTEGER NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (hash, spec, format)
);
```

- The catalog is a **rebuildable cache, not the source of truth**.
  - If a variant row is missing, the variant is regenerated. On Cask this produces the same key
    thanks to deterministic encoding. On fs/S3 it lands at the same deterministic path.
  - If an original row is missing, the original is fetched from the backend by hash (Cask:
    `sha1:<hash>`; fs/S3: by listing the `<hash>/full.` prefix), its format is sniffed, and the
    row is recreated.
  - On fs/S3 the bucket describes itself (the paths say everything), so the DB could be rebuilt
    by listing the bucket.
- The DB can therefore live on local disk with no backups. Losing it costs only CPU.
- ETags live in the catalog, so `If-None-Match` is answered without touching the backend.

### Request flow for `GET /image/...`

1. Parse and normalise the request (301s happen here).
2. Look up the original in the catalog. If it isn't there, recover it from the backend (above).
   If it isn't there either, return 404.
3. If the request is `full` in the original's format, stream the original.
4. Look up the variant in the catalog and fetch its blob. If the row exists but the blob is
   missing, treat it as a miss.
5. On a miss, run **single-flight** keyed on `(hash, spec, format)`, so that concurrent
   requests wait on one job. The job:
   - fetches the original,
   - takes a slot from the scaling semaphore,
   - runs decode/resize/encode on `spawn_blocking` with a timeout,
   - returns the bytes to all waiters immediately,
   - writes the blob and the catalog row (the write happens in the background, so a slow
     backend doesn't block the response).

`size_hints` on upload enqueue the same job for each listed spec without waiting.

## Limits (all configurable)

| env var                          | default   | effect                                    |
|----------------------------------|-----------|-------------------------------------------|
| `RADIANT_MAX_UPLOAD_BYTES`       | 50 MiB    | 413 on upload                             |
| `RADIANT_MAX_INPUT_PIXELS`       | 100 MP    | upload rejected (400); decode refused     |
| `RADIANT_MAX_OUTPUT_DIMENSION`   | 4096      | 400 when the spec asks for more           |
| `RADIANT_MAX_FRAMES`             | 500       | for animations                            |
| `RADIANT_MAX_ANIMATION_PIXELS`   | 400 MP    | frames × w × h, decoded and output        |
| `RADIANT_SCALE_CONCURRENCY`      | #cpus     | semaphore                                 |
| `RADIANT_SCALE_TIMEOUT_SECS`     | 30        | 503 on timeout                            |
| `RADIANT_ALLOWED_SIZES`          | unset     | optional allowlist of specs (anything else returns 404). **Not built yet** |
| `RADIANT_MAX_CONCURRENT_UPLOADS` | 8         | further uploads wait (bounds upload memory) |
| `RADIANT_REQUEST_TIMEOUT_SECS`   | 120       | 408 for the whole request                 |
| `RADIANT_SHUTDOWN_DRAIN_SECS`    | 10        | wait on shutdown for in-progress renders to be stored |

Decoding enforces `image::Limits` as well, which protects against decompression bombs. Its allocation
cap is scaled to `RADIANT_MAX_INPUT_PIXELS` so it can't reject images our own limits allow.

## Configuration

Settings are read from `RADIANT_*` environment variables, 12-factor style like cask and lumen,
and each can also be passed as a CLI flag.

- `RADIANT_LISTEN` (default `0.0.0.0:8080`), `RADIANT_UPLOAD_KEY`, `RADIANT_DB_PATH`
- `RADIANT_BACKEND` = `fs` | `s3` | `cask`
  - fs: `RADIANT_FS_ROOT`
  - s3: `RADIANT_S3_BUCKET`, `RADIANT_S3_PREFIX`, `RADIANT_S3_REGION`, `RADIANT_S3_ENDPOINT`
    (for S3-compatible stores; `http://` allowed; path-style requests), plus the standard
    `AWS_*` credential variables. `RADIANT_BACKEND_TIMEOUT_SECS` (30) is the per-request timeout.
    Objects get a `Content-Type`, so the bucket can be browsed directly.
  - cask: `RADIANT_CASK_URL` (one node URL or a comma-separated list with failover),
    `RADIANT_CASK_TIMEOUT_SECS`
- `RADIANT_JPEG_QUALITY`, `RADIANT_LOG_FORMAT` (`json` | `pretty`)

## Metrics (Prometheus)

`GET /metrics` (on by default, `RADIANT_METRICS=false` turns it off). Names are defined in
`src/telemetry.rs`.

- `radiant_http_requests_total{route,method,status}`, `radiant_http_request_duration_seconds{route}`.
  `route` is the route pattern, or `unmatched`, so the number of label values stays small.
- `radiant_images_served_total{source="original|variant_stored|variant_rendered|not_modified"}`
- `radiant_scale_duration_seconds{format,outcome="ok|limit|error"}`, `radiant_scale_running`,
  `radiant_scale_waiting`, `radiant_scale_timeouts_total`, `radiant_singleflight_coalesced_total`
- `radiant_variant_store_failures_total`
- `radiant_backend_op_duration_seconds{backend,op="get|put|find_original",outcome="ok|not_found|error"}`
- `radiant_uploads_total{outcome="created|duplicate|rejected|error"}`, `radiant_upload_size_bytes`
- `radiant_limit_rejections_total{limit="input_pixels|output_dimension|frames|animation_pixels|decode_memory|upload_bytes"}`
- `radiant_catalog_repairs_total{kind="original_recovered|variant_blob_missing|original_blob_missing"}`
- `radiant_build_info{version}`

## Testing strategy

- Unit tests: spec parsing and canonicalisation tables, geometry tables, hash validation, sniffing.
- Imaging: golden fixtures (static/animated, alpha, EXIF-rotated, odd sizes) that check output
  dimensions, frame counts and delays, and determinism (encoding twice gives the same bytes).
- `BlobStore` conformance suite run against every backend:
  - in-memory and fs: always.
  - S3: `scripts/test-s3.sh` starts versitygw (an S3 gateway over a temp directory, from the
    Nix shell) and runs the S3-gated tests. CI runs it too. MinIO was the plan, but it has been
    removed from nixpkgs (abandoned upstream).
  - Cask: against a real cask node, which builds from its flake.
- HTTP tests through `axum` + `tower::ServiceExt::oneshot` against an in-memory backend.
- Compatibility check (optional, later): a script that sends the same URLs to reticulum and
  radiant and compares status, redirect target, content type, and dimensions.

## Milestones

Status: M0–M7 are done. M8 (Cask backend) is next.

- **M0 Skeleton**: `cargo init`, axum app, config, tracing, `/healthz`, CI (fmt, clippy, test),
  Nix packaging, Dockerfile.
- **M1 Specs & URLs**: `SizeSpec` + `ImageRequest` parsing, normalisation, and redirects, with
  the full test table.
- **M2 Imaging core**: sniff, decode with limits, EXIF orientation, resize geometry, static
  encode for JPEG/PNG/GIF/WebP, format conversion.
- **M3 Animation**: GIF, APNG, and animated WebP decode → resize → encode.
- **M4 Catalog + fs backend + serving**: SQLite migrations, `BlobStore` fs, the get-or-generate
  flow, single-flight, ETag/304, cache headers.
- **M5 Upload**: multipart, key check, sniffing, idempotency, `size_hints`, upload form.
- **M6 Hardening**: all limits, timeouts, metrics, graceful shutdown.
- **M7 S3 backend**: `object_store`, S3 integration tests (versitygw).
- **M8 Cask backend**: reqwest client, multi-node failover, integration tests against real cask.
- **M9 Migration tool**: `radiant import` that walks a reticulum upload directory
  (`aa/bb/…/full.<ext>`) and pushes originals into the configured backend. Existing scaled
  versions could optionally be imported into the catalog as well.
- **Later**: AVIF/JPEG XL, a small admin/dashboard page, an optional "Accept"-based format
  negotiation.

## Decisions log

- The SQLite catalog is used for every backend. It is a rebuildable cache, not the source of truth.
- `RADIANT_ALLOWED_SIZES` is **off by default**. Arbitrary sizes in URLs are central to how
  reticulum is used, and we accept the risk of unbounded variants.
- Default limits as listed above. JPEG quality defaults to 90.
- Cask: `RADIANT_CASK_URL` takes a list of nodes, with failover.
- The upload secret field is `key`. The format is sniffed from the bytes.
- Upload checks the image *header* (format, dimensions, pixel limit) but does not decode the
  whole image, as in reticulum. A truncated file is accepted and fails later when rendered (500).
- Catalog recovery for fs/S3 picks the `full.*` blob whose bytes match the hash, because a
  full-size conversion (e.g. `full.jpg` next to the original `full.png`) shares the prefix.
- Animated WebP goes through libwebp, not image-webp or the `webp` crate. image-webp
  composited some animations wrongly (a transparent frame kept showing the previous one), and
  the `webp` crate's encoder ended animations with timestamp 0, which broke the last frame's
  duration.
- `RADIANT_MAX_ANIMATION_PIXELS` (400 MP) bounds memory per animated render at about 1.6 GB
  worst case (4 bytes per output pixel). Lower it on small machines, or lower
  `RADIANT_SCALE_CONCURRENCY`.
- `/readyz` checks the catalog and the backend. The backend check LISTs a prefix that never
  exists, because S3 answers a HEAD in a missing bucket with a bare 404 that looks like
  "no such key".
- object_store's `aws` feature brings in aws-lc-rs, a C build. It works with only a C
  compiler, but it makes a clean Docker build take about 5 minutes.
- Single-flight: the first request for a missing variant spawns the render. Later requests
  wait on it. The render is stored even if every waiter times out or disconnects.
