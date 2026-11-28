#!/usr/bin/env python3
"""Migrate originals from reticulum upload directories into radiant.

Reticulum stores each original at <upload dir>/aa/bb/.../tt/full.<ext>, where the 20 directory
names are the SHA-1 of the file split into pairs. Scaled versions sit next to it as
<size>.<ext>. They are not migrated, because radiant re-renders them on demand.

Both steps are resumable, and all progress is kept in a SQLite state file:

  scan      walk upload directories and record every original found
  run       upload the recorded originals to radiant (POST /), skipping any radiant already has
  status    print counts (safe to run while `run` is going)
  failures  list the images that couldn't be migrated, and why
  verify    check that radiant has every image marked as migrated

Uses only the Python standard library (3.9+), so it can run directly on a reticulum node.
See README.md next to this file.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import http.client
import json
import logging
import os
import signal
import sqlite3
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid
from dataclasses import dataclass
from typing import Iterator, List, Optional, Sequence, Tuple, Union

log = logging.getLogger("migrate")

HASH_DEPTH = 20  # a 40-character SHA-1 split into 20 directory names
HEX = frozenset("0123456789abcdef")

# Image statuses:
#   pending   found by a scan, not tried yet
#   migrated  uploaded by this script
#   present   radiant already had it
#   retry     temporary failure (network, 5xx); `run` tries these again
#   failed    permanent failure (rejected by radiant, every copy corrupt or unreadable);
#             `run --retry-failed` tries these again
DONE = ("migrated", "present")

SCHEMA = """
CREATE TABLE IF NOT EXISTS images (
    hash        TEXT PRIMARY KEY,
    status      TEXT NOT NULL DEFAULT 'pending',
    attempts    INTEGER NOT NULL DEFAULT 0,
    error_kind  TEXT,
    error       TEXT,
    radiant_ext TEXT,
    bytes       INTEGER,
    updated_at  REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS images_status ON images(status, hash);

-- Every file found for a hash. Reticulum replicates, so one hash can have several copies
-- (one per mounted node), and the same bytes uploaded with two content types end up as
-- both full.jpg and full.png.
CREATE TABLE IF NOT EXISTS copies (
    path TEXT PRIMARY KEY,
    hash TEXT NOT NULL REFERENCES images(hash),
    ext  TEXT NOT NULL,
    size INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS copies_hash ON copies(hash);

-- Top-level prefix directories (00..ff) fully scanned, so an interrupted scan resumes.
CREATE TABLE IF NOT EXISTS scanned (
    root        TEXT NOT NULL,
    prefix      TEXT NOT NULL,
    originals   INTEGER NOT NULL,
    finished_at REAL NOT NULL,
    PRIMARY KEY (root, prefix)
);

-- Things in an upload directory that don't fit reticulum's layout.
CREATE TABLE IF NOT EXISTS anomalies (
    path    TEXT PRIMARY KEY,
    reason  TEXT NOT NULL,
    seen_at REAL NOT NULL
);
"""


def open_db(path: str) -> sqlite3.Connection:
    db = sqlite3.connect(path, timeout=60, isolation_level=None)
    db.execute("PRAGMA journal_mode=WAL")
    db.execute("PRAGMA synchronous=NORMAL")
    db.execute("PRAGMA foreign_keys=ON")
    db.executescript(SCHEMA)
    return db


class Tx:
    """`with Tx(db):` runs the block in one transaction (the connection is in autocommit mode)."""

    def __init__(self, db: sqlite3.Connection):
        self.db = db

    def __enter__(self) -> sqlite3.Connection:
        self.db.execute("BEGIN IMMEDIATE")
        return self.db

    def __exit__(self, exc_type, exc, tb) -> None:
        self.db.execute("ROLLBACK" if exc_type else "COMMIT")


# ---------------------------------------------------------------------------------------------
# scan


@dataclass
class Original:
    hash: str
    path: str
    ext: str
    size: int


@dataclass
class Anomaly:
    path: str
    reason: str


def is_pair(name: str) -> bool:
    return len(name) == 2 and name[0] in HEX and name[1] in HEX


def walk_prefix(root: str, prefix: str) -> Iterator[Union[Original, Anomaly]]:
    """Yields every original under root/prefix, plus anything that doesn't fit the layout."""
    stack: List[Tuple[str, List[str]]] = [(os.path.join(root, prefix), [prefix])]
    while stack:
        directory, parts = stack.pop()
        try:
            entries = list(os.scandir(directory))
        except OSError as e:
            yield Anomaly(directory, f"can't list directory: {e}")
            continue
        for entry in entries:
            try:
                if entry.is_dir(follow_symlinks=False):
                    if len(parts) < HASH_DEPTH and is_pair(entry.name):
                        stack.append((entry.path, parts + [entry.name]))
                    else:
                        yield Anomaly(entry.path, "unexpected directory")
                elif entry.name.startswith("full."):
                    if len(parts) != HASH_DEPTH:
                        yield Anomaly(entry.path, "original outside a hash directory")
                    elif not entry.is_file(follow_symlinks=False):
                        yield Anomaly(entry.path, "original is not a regular file")
                    else:
                        yield Original(
                            hash="".join(parts),
                            path=entry.path,
                            ext=entry.name[len("full."):].lower(),
                            size=entry.stat(follow_symlinks=False).st_size,
                        )
                # Anything else is a scaled version (<size>.<ext>) and is skipped.
            except OSError as e:
                yield Anomaly(entry.path, f"can't stat: {e}")


def scan(db: sqlite3.Connection, roots: Sequence[str], rescan: bool) -> int:
    total_new = 0
    for given in roots:
        root = os.path.abspath(given)
        if not os.path.isdir(root):
            raise SystemExit(f"not a directory: {root}")
        if rescan:
            db.execute("DELETE FROM scanned WHERE root = ?", (root,))
        done = {r[0] for r in db.execute("SELECT prefix FROM scanned WHERE root = ?", (root,))}
        prefixes = sorted(e.name for e in os.scandir(root) if e.is_dir() and is_pair(e.name))
        todo = [p for p in prefixes if p not in done]
        log.info(
            "scan root=%s prefixes=%d already_scanned=%d to_scan=%d",
            root, len(prefixes), len(prefixes) - len(todo), len(todo),
        )
        started = time.monotonic()
        for i, prefix in enumerate(todo, 1):
            originals = new = anomalies = 0
            with Tx(db):
                for item in walk_prefix(root, prefix):
                    if isinstance(item, Anomaly):
                        anomalies += 1
                        log.warning("anomaly path=%s reason=%s", item.path, item.reason)
                        db.execute(
                            "INSERT INTO anomalies(path, reason, seen_at) VALUES (?, ?, ?) "
                            "ON CONFLICT(path) DO UPDATE SET reason = excluded.reason, "
                            "seen_at = excluded.seen_at",
                            (item.path, item.reason, time.time()),
                        )
                        continue
                    originals += 1
                    cur = db.execute(
                        "INSERT OR IGNORE INTO images(hash, updated_at) VALUES (?, ?)",
                        (item.hash, time.time()),
                    )
                    new += cur.rowcount
                    db.execute(
                        "INSERT INTO copies(path, hash, ext, size) VALUES (?, ?, ?, ?) "
                        "ON CONFLICT(path) DO UPDATE SET hash = excluded.hash, "
                        "ext = excluded.ext, size = excluded.size",
                        (item.path, item.hash, item.ext, item.size),
                    )
                db.execute(
                    "INSERT OR REPLACE INTO scanned(root, prefix, originals, finished_at) "
                    "VALUES (?, ?, ?, ?)",
                    (root, prefix, originals, time.time()),
                )
            total_new += new
            elapsed = time.monotonic() - started
            eta = elapsed / i * (len(todo) - i)
            log.info(
                "scanned root=%s prefix=%s (%d/%d) originals=%d new_images=%d anomalies=%d "
                "eta=%s",
                root, prefix, i, len(todo), originals, new, anomalies, fmt_duration(eta),
            )
    log.info("scan finished new_images=%d", total_new)
    return total_new


# ---------------------------------------------------------------------------------------------
# radiant client


class Transient(Exception):
    """Worth retrying: network errors, timeouts, 5xx, 408, 429."""

    def __init__(self, kind: str, message: str):
        super().__init__(message)
        self.kind = kind


class Rejected(Exception):
    """Radiant refused the image (400, 413, ...). Retrying won't help."""


class Fatal(Exception):
    """Stops the whole run, e.g. a wrong upload key."""


MIME = {"jpg": "image/jpeg", "jpeg": "image/jpeg", "png": "image/png", "gif": "image/gif",
        "webp": "image/webp"}


class Radiant:
    def __init__(self, base_url: str, key: Optional[str], timeout: float):
        self.base = base_url.rstrip("/")
        self.key = key
        self.timeout = timeout

    def _request(self, method: str, path: str, body: Optional[bytes] = None,
                 headers: Optional[dict] = None) -> Tuple[int, bytes]:
        req = urllib.request.Request(self.base + path, data=body, method=method,
                                     headers=headers or {})
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as res:
                return res.status, res.read()
        except urllib.error.HTTPError as e:
            with e:
                return e.code, e.read()
        except (urllib.error.URLError, http.client.HTTPException, OSError) as e:
            reason = getattr(e, "reason", e)
            raise Transient("network", f"{method} {path}: {reason}") from e

    def has(self, hash: str, ext: str) -> bool:
        """Whether radiant has the original. The ETag of an original is its hash, so a
        conditional GET is answered with a 304 from radiant's catalog, without reading the
        backend."""
        ext = "jpg" if ext == "jpeg" else ext
        status, body = self._request("GET", f"/image/{hash}/full/image.{ext}",
                                     headers={"If-None-Match": f'"{hash}"'})
        if status in (200, 304):
            return True
        if status == 404:
            return False
        raise classify(status, body)

    def upload(self, data: bytes, ext: str) -> dict:
        boundary = uuid.uuid4().hex
        parts = []
        if self.key is not None:
            parts.append(
                f'--{boundary}\r\nContent-Disposition: form-data; name="key"\r\n\r\n'
                f"{self.key}\r\n".encode()
            )
        parts.append(
            f"--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; "
            f"filename=\"image.{ext}\"\r\n"
            f"Content-Type: {MIME.get(ext, 'application/octet-stream')}\r\n\r\n".encode()
        )
        body = b"".join(parts) + data + f"\r\n--{boundary}--\r\n".encode()
        status, res = self._request(
            "POST", "/", body,
            {"Content-Type": f"multipart/form-data; boundary={boundary}"},
        )
        if status == 200:
            try:
                return json.loads(res)
            except ValueError as e:
                raise Transient("bad-response", f"upload response isn't JSON: {e}") from e
        raise classify(status, res)


def classify(status: int, body: bytes) -> Exception:
    text = body[:300].decode("utf-8", "replace").strip()
    msg = f"HTTP {status}: {text}" if text else f"HTTP {status}"
    if status == 403:
        return Fatal(f"radiant refused the upload key ({msg})")
    if status >= 500 or status in (408, 429):
        return Transient("http", msg)
    return Rejected(msg)


# ---------------------------------------------------------------------------------------------
# run


@dataclass
class Copy:
    path: str
    ext: str


@dataclass
class Outcome:
    hash: str
    status: str
    attempts: int
    kind: Optional[str] = None
    error: Optional[str] = None
    radiant_ext: Optional[str] = None
    size: Optional[int] = None
    fatal: bool = False


@dataclass
class RunOptions:
    retries: int = 5
    backoff: float = 2.0
    check_existing: bool = True


def read_good_copy(hash: str, copies: Sequence[Copy]) -> Tuple[Optional[Tuple[Copy, bytes]], List[str]]:
    """Returns the first copy whose bytes hash to `hash`, plus a note for every bad copy."""
    problems = []
    for c in copies:
        try:
            with open(c.path, "rb") as f:
                data = f.read()
        except OSError as e:
            problems.append(f"{c.path}: unreadable: {e}")
            continue
        actual = hashlib.sha1(data).hexdigest()
        if actual != hash:
            problems.append(f"{c.path}: corrupt: {len(data)} bytes hashing to {actual}")
            continue
        return (c, data), problems
    return None, problems


def attempt(client: Radiant, hash: str, copies: Sequence[Copy], opts: RunOptions,
            attempts: int) -> Outcome:
    if opts.check_existing and client.has(hash, copies[0].ext):
        return Outcome(hash, "present", attempts)

    good, problems = read_good_copy(hash, copies)
    for p in problems:
        log.warning("bad copy hash=%s %s", hash, p)
    if good is None:
        kind = "corrupt" if any(": corrupt:" in p for p in problems) else "unreadable"
        return Outcome(hash, "failed", attempts, kind,
                       f"no good copy among {len(copies)}: " + "; ".join(problems))
    copy, data = good

    try:
        res = client.upload(data, copy.ext)
    except Rejected as e:
        return Outcome(hash, "failed", attempts, "rejected", f"{e} (from {copy.path})",
                       size=len(data))

    if res.get("hash") != hash or res.get("length") != len(data):
        # Radiant hashes what it received, so this means the bytes were changed in transit.
        return Outcome(hash, "failed", attempts, "mismatch",
                       f"radiant stored hash={res.get('hash')} length={res.get('length')}, "
                       f"sent {len(data)} bytes from {copy.path}", size=len(data))
    ext = res.get("extension")
    if ext and ext != copy.ext:
        log.info("format differs hash=%s reticulum_ext=%s radiant_ext=%s", hash, copy.ext, ext)
    return Outcome(hash, "migrated", attempts, radiant_ext=ext, size=len(data))


def migrate_one(client: Radiant, hash: str, copies: Sequence[Copy], opts: RunOptions,
                stop: threading.Event) -> Outcome:
    attempts = 0
    while True:
        attempts += 1
        try:
            return attempt(client, hash, copies, opts, attempts)
        except Fatal as e:
            return Outcome(hash, "retry", attempts, "fatal", str(e), fatal=True)
        except Transient as e:
            if attempts > opts.retries or stop.is_set():
                return Outcome(hash, "retry", attempts, e.kind, str(e))
            delay = min(60.0, opts.backoff * 2 ** (attempts - 1))
            log.info("retrying hash=%s attempt=%d in=%.0fs error=%s", hash, attempts, delay, e)
            stop.wait(delay)
        except Exception as e:  # a bug here shouldn't kill the run; record it and move on
            log.exception("unexpected error hash=%s", hash)
            return Outcome(hash, "retry", attempts, "internal", f"{type(e).__name__}: {e}")


def record(db: sqlite3.Connection, o: Outcome) -> None:
    db.execute(
        "UPDATE images SET status = ?, attempts = attempts + ?, error_kind = ?, error = ?, "
        "radiant_ext = COALESCE(?, radiant_ext), bytes = COALESCE(?, bytes), updated_at = ? "
        "WHERE hash = ?",
        (o.status, o.attempts, o.kind, o.error, o.radiant_ext, o.size, time.time(), o.hash),
    )


def counts(db: sqlite3.Connection) -> dict:
    return dict(db.execute("SELECT status, count(*) FROM images GROUP BY status").fetchall())


class Stopper:
    """First SIGINT/SIGTERM: stop taking new work and finish what's in flight. Second: exit."""

    def __init__(self) -> None:
        self.event = threading.Event()

    def install(self) -> None:
        signal.signal(signal.SIGINT, self._handle)
        signal.signal(signal.SIGTERM, self._handle)

    def _handle(self, signum, frame) -> None:
        if self.event.is_set():
            log.error("second signal; exiting now (in-flight images stay pending)")
            os._exit(130)
        log.warning("stopping: finishing in-flight uploads (signal again to exit now)")
        self.event.set()


def run(db: sqlite3.Connection, client: Radiant, args: argparse.Namespace,
        stop: threading.Event) -> int:
    statuses = ["pending", "retry"] + (["failed"] if args.retry_failed else [])
    opts = RunOptions(retries=args.retries, backoff=args.backoff,
                      check_existing=not args.no_check_existing)
    before = counts(db)
    queued = sum(before.get(s, 0) for s in statuses)
    log.info("run radiant=%s workers=%d to_process=%d statuses=%s counts=%s",
             client.base, args.workers, queued, ",".join(statuses), json.dumps(before))
    if args.limit:
        queued = min(queued, args.limit)

    placeholders = ",".join("?" * len(statuses))
    cursor = ""  # hashes are processed in order, so a retried image isn't picked up again
    batch: List[str] = []
    exhausted = False
    submitted = 0
    this_run = {"migrated": 0, "present": 0, "retry": 0, "failed": 0}
    consecutive_transient = 0
    fatal = False
    started = last_progress = time.monotonic()
    next_submit = started
    min_interval = 1.0 / args.rate if args.rate else 0.0

    def next_hash() -> Optional[str]:
        nonlocal cursor, exhausted, batch
        if not batch and not exhausted:
            batch = [r[0] for r in db.execute(
                f"SELECT hash FROM images WHERE status IN ({placeholders}) AND hash > ? "
                f"ORDER BY hash LIMIT 1000", (*statuses, cursor))]
            batch.reverse()
            exhausted = not batch
        if not batch:
            return None
        cursor = batch.pop()
        return cursor

    with concurrent.futures.ThreadPoolExecutor(args.workers) as pool:
        inflight = {}
        while True:
            now = time.monotonic()
            while (not stop.is_set() and len(inflight) < args.workers * 2
                   and not (args.limit and submitted >= args.limit) and now >= next_submit):
                h = next_hash()
                if h is None:
                    break
                copies = [Copy(p, e) for p, e in db.execute(
                    "SELECT path, ext FROM copies WHERE hash = ? ORDER BY path", (h,))]
                if not copies:
                    record(db, Outcome(h, "failed", 0, "no-copies", "no files recorded"))
                    continue
                inflight[pool.submit(migrate_one, client, h, copies, opts, stop)] = h
                submitted += 1
                next_submit = max(next_submit + min_interval, now) if min_interval else now
            if not inflight:
                break
            done, _ = concurrent.futures.wait(
                inflight, timeout=1.0, return_when=concurrent.futures.FIRST_COMPLETED)
            for fut in done:
                del inflight[fut]
                o = fut.result()
                record(db, o)
                this_run[o.status] += 1
                if o.status == "failed":
                    log.warning("failed hash=%s kind=%s error=%s", o.hash, o.kind, o.error)
                elif o.status == "retry":
                    log.warning("gave up for now hash=%s kind=%s attempts=%d error=%s",
                                o.hash, o.kind, o.attempts, o.error)
                else:
                    log.debug("%s hash=%s", o.status, o.hash)
                if o.fatal:
                    if not fatal:
                        log.error("stopping: %s", o.error)
                    fatal = True
                    stop.set()
                if o.status == "retry":
                    consecutive_transient += 1
                    if consecutive_transient == args.max_consecutive_errors:
                        log.error("stopping: %d images in a row failed with temporary errors; "
                                  "is radiant up? last error: %s",
                                  consecutive_transient, o.error)
                        stop.set()
                else:
                    consecutive_transient = 0

            now = time.monotonic()
            if now - last_progress >= args.progress_interval:
                last_progress = now
                log_progress(db, this_run, started, queued)
    log_progress(db, this_run, started, queued)

    if fatal:
        return 2
    if stop.is_set():
        log.warning("stopped early; run again to continue")
        return 1
    return 0


def log_progress(db: sqlite3.Connection, this_run: dict, started: float, queued: int) -> None:
    elapsed = time.monotonic() - started
    processed = sum(this_run.values())
    rate = processed / elapsed if elapsed > 0 else 0.0
    left = max(queued - processed, 0)
    eta = fmt_duration(left / rate) if rate > 0 else "?"
    log.info("progress processed=%d/%d rate=%.1f/s eta=%s this_run=%s totals=%s",
             processed, queued, rate, eta, json.dumps(this_run), json.dumps(counts(db)))


def fmt_duration(seconds: float) -> str:
    seconds = int(seconds)
    d, rem = divmod(seconds, 86400)
    h, rem = divmod(rem, 3600)
    m, s = divmod(rem, 60)
    if d:
        return f"{d}d{h:02d}h{m:02d}m"
    if h:
        return f"{h}h{m:02d}m"
    return f"{m}m{s:02d}s"


# ---------------------------------------------------------------------------------------------
# verify, status, failures


def verify(db: sqlite3.Connection, client: Radiant, workers: int, stop: threading.Event) -> int:
    rows = db.execute(
        "SELECT i.hash, (SELECT ext FROM copies c WHERE c.hash = i.hash ORDER BY path LIMIT 1) "
        "FROM images i WHERE i.status IN ('migrated', 'present') ORDER BY i.hash").fetchall()
    log.info("verify checking=%d", len(rows))

    def check(hash: str, ext: str) -> Tuple[str, Optional[str]]:
        if stop.is_set():
            return hash, "skipped"
        try:
            return hash, None if client.has(hash, ext or "jpg") else "missing"
        except (Transient, Rejected, Fatal) as e:
            return hash, f"error: {e}"

    missing = errors = checked = 0
    with concurrent.futures.ThreadPoolExecutor(workers) as pool:
        for hash, problem in pool.map(lambda r: check(*r), rows):
            checked += 1
            if problem == "missing":
                missing += 1
                log.warning("missing from radiant hash=%s; marked pending", hash)
                db.execute(
                    "UPDATE images SET status = 'pending', error_kind = 'missing', "
                    "error = 'verify: radiant returned 404', updated_at = ? WHERE hash = ?",
                    (time.time(), hash))
            elif problem and problem != "skipped":
                errors += 1
                log.warning("couldn't verify hash=%s %s", hash, problem)
            if checked % 10000 == 0:
                log.info("verify progress checked=%d/%d missing=%d errors=%d",
                         checked, len(rows), missing, errors)
    log.info("verify finished checked=%d missing=%d errors=%d", checked, missing, errors)
    if stop.is_set():
        return 1
    return 0 if missing == errors == 0 else 1


def status(db: sqlite3.Connection, out=sys.stdout) -> None:
    c = counts(db)
    total = sum(c.values())
    done = sum(c.get(s, 0) for s in DONE)
    print(f"images:      {total}", file=out)
    for s in ("migrated", "present", "pending", "retry", "failed"):
        print(f"  {s:<10} {c.get(s, 0)}", file=out)
    if total:
        print(f"  done       {done / total:.1%}", file=out)
    (copies,) = db.execute("SELECT count(*) FROM copies").fetchone()
    print(f"copies:      {copies}", file=out)
    (migrated_bytes,) = db.execute(
        "SELECT coalesce(sum(bytes), 0) FROM images WHERE status = 'migrated'").fetchone()
    print(f"uploaded:    {migrated_bytes / 2**30:.2f} GiB", file=out)
    (converted,) = db.execute(
        "SELECT count(*) FROM images i WHERE radiant_ext IS NOT NULL AND NOT EXISTS "
        "(SELECT 1 FROM copies c WHERE c.hash = i.hash AND c.ext = i.radiant_ext)").fetchone()
    if converted:
        print(f"format differs from reticulum's extension: {converted}", file=out)
    for root, n, originals in db.execute(
            "SELECT root, count(*), sum(originals) FROM scanned GROUP BY root ORDER BY root"):
        print(f"scanned:     {root}: {n} prefixes, {originals} originals", file=out)
    (anomalies,) = db.execute("SELECT count(*) FROM anomalies").fetchone()
    print(f"anomalies:   {anomalies}", file=out)
    kinds = db.execute(
        "SELECT status, error_kind, count(*) FROM images WHERE status IN ('retry', 'failed') "
        "GROUP BY status, error_kind ORDER BY status, count(*) DESC").fetchall()
    if kinds:
        print("problems:", file=out)
        for s, kind, n in kinds:
            print(f"  {s:<7} {kind or '?':<12} {n}", file=out)


def failures(db: sqlite3.Connection, which: str, anomalies: bool, out=sys.stdout) -> None:
    if anomalies:
        for path, reason in db.execute("SELECT path, reason FROM anomalies ORDER BY path"):
            print(f"{path}\t{reason}", file=out)
        return
    statuses = ("retry", "failed") if which == "all" else (which,)
    rows = db.execute(
        f"SELECT i.hash, i.status, i.error_kind, i.attempts, i.error, "
        f"(SELECT group_concat(path, ' ') FROM copies c WHERE c.hash = i.hash) "
        f"FROM images i WHERE i.status IN ({','.join('?' * len(statuses))}) ORDER BY i.hash",
        statuses)
    print("hash\tstatus\tkind\tattempts\terror\tpaths", file=out)
    for row in rows:
        print("\t".join("" if v is None else str(v).replace("\t", " ") for v in row), file=out)


# ---------------------------------------------------------------------------------------------
# CLI


def setup_logging(log_file: Optional[str], verbose: bool) -> None:
    fmt = logging.Formatter("%(asctime)s %(levelname)s %(message)s")
    root = logging.getLogger()
    root.setLevel(logging.DEBUG if verbose else logging.INFO)
    handlers: List[logging.Handler] = [logging.StreamHandler(sys.stderr)]
    if log_file:
        handlers.append(logging.FileHandler(log_file))
    for h in handlers:
        h.setFormatter(fmt)
        root.addHandler(h)


def upload_key(args: argparse.Namespace) -> Optional[str]:
    if args.key_file:
        with open(args.key_file) as f:
            return f.read().strip()
    return os.environ.get("RADIANT_UPLOAD_KEY")


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Migrate reticulum originals into radiant.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter)
    p.add_argument("--state", required=True, help="SQLite state file (created if missing)")
    p.add_argument("--log-file", help="also append logs here (default: <state>.log)")
    p.add_argument("--no-log-file", action="store_true", help="log to stderr only")
    p.add_argument("-v", "--verbose", action="store_true", help="log every image")
    sub = p.add_subparsers(dest="command", required=True)

    s = sub.add_parser("scan", help="record the originals in reticulum upload directories")
    s.add_argument("roots", nargs="+", metavar="UPLOAD_DIR")
    s.add_argument("--rescan", action="store_true",
                   help="walk prefixes already scanned (to find images uploaded since)")

    def radiant_args(sp: argparse.ArgumentParser) -> None:
        sp.add_argument("--radiant", required=True, help="radiant base URL")
        sp.add_argument("--timeout", type=float, default=120, help="per-request timeout (s)")
        sp.add_argument("--workers", type=int, default=4, help="concurrent requests")

    r = sub.add_parser("run", help="upload recorded originals to radiant")
    radiant_args(r)
    r.add_argument("--key-file", help="file holding the upload key "
                   "(default: $RADIANT_UPLOAD_KEY, if set)")
    r.add_argument("--retry-failed", action="store_true",
                   help="also retry images that failed permanently")
    r.add_argument("--no-check-existing", action="store_true",
                   help="upload without asking radiant whether it already has the image")
    r.add_argument("--retries", type=int, default=5,
                   help="retries per image for temporary errors, before moving on")
    r.add_argument("--backoff", type=float, default=2.0,
                   help="first retry delay (s); doubles each retry, up to 60")
    r.add_argument("--rate", type=float, default=0, help="max images started per second (0: no limit)")
    r.add_argument("--limit", type=int, default=0, help="stop after this many images (0: all)")
    r.add_argument("--max-consecutive-errors", type=int, default=20,
                   help="stop when this many images in a row fail with temporary errors")
    r.add_argument("--progress-interval", type=float, default=60, help="seconds between progress lines")

    v = sub.add_parser("verify", help="check that radiant has every migrated image")
    radiant_args(v)

    sub.add_parser("status", help="print counts")

    f = sub.add_parser("failures", help="list images that couldn't be migrated (TSV)")
    f.add_argument("--status", choices=("retry", "failed", "all"), default="all")
    f.add_argument("--anomalies", action="store_true",
                   help="list layout anomalies found by scan instead")
    return p.parse_args(argv)


def main(argv: Optional[Sequence[str]] = None, stopper: Optional[Stopper] = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    if args.command in ("scan", "run", "verify"):
        log_file = None if args.no_log_file else (args.log_file or args.state + ".log")
        setup_logging(log_file, args.verbose)
    db = open_db(args.state)
    try:
        if args.command == "status":
            status(db)
            return 0
        if args.command == "failures":
            failures(db, args.status, args.anomalies)
            return 0
        if args.command == "scan":
            scan(db, args.roots, args.rescan)
            return 0
        if stopper is None:
            stopper = Stopper()
            stopper.install()
        if args.command == "run":
            client = Radiant(args.radiant, upload_key(args), args.timeout)
            return run(db, client, args, stopper.event)
        client = Radiant(args.radiant, None, args.timeout)
        return verify(db, client, args.workers, stopper.event)
    finally:
        db.close()


if __name__ == "__main__":
    sys.exit(main())
