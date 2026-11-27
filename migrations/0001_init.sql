-- The catalog is a cache that can be rebuilt; the blob store is the source of truth. See PLAN.md.

CREATE TABLE originals (
    hash       TEXT PRIMARY KEY NOT NULL,  -- sha1 hex of the uploaded bytes
    format     TEXT NOT NULL,              -- canonical extension: jpg, png, gif, webp
    blob_ref   TEXT NOT NULL,
    bytes      INTEGER NOT NULL,
    width      INTEGER NOT NULL,           -- after EXIF orientation
    height     INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE TABLE variants (
    hash       TEXT NOT NULL REFERENCES originals (hash),
    spec       TEXT NOT NULL,              -- canonical size spec, e.g. 100s, 200w100h
    format     TEXT NOT NULL,
    blob_ref   TEXT NOT NULL,
    etag       TEXT NOT NULL,              -- sha1 hex of the variant bytes
    bytes      INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (hash, spec, format)
) STRICT;
