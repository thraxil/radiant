"""Tests for migrate_reticulum.py, against a fake radiant. Run with
`python3 -m unittest discover tools/migrate-reticulum`."""

from __future__ import annotations

import contextlib
import hashlib
import io
import json
import logging
import os
import sqlite3
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Dict, List, Optional

import migrate_reticulum as m

# Keep test output quiet; set MIGRATE_TEST_LOGS=1 to see the script's logs.
if not os.environ.get("MIGRATE_TEST_LOGS"):
    m.setup_logging = lambda *args: None
    logging.getLogger().addHandler(logging.NullHandler())


def sha1(data: bytes) -> str:
    return hashlib.sha1(data).hexdigest()


def hash_dir(root: str, h: str) -> str:
    return os.path.join(root, *[h[i:i + 2] for i in range(0, 40, 2)])


def put_original(root: str, data: bytes, ext: str = "jpg", variants=("100s",)) -> str:
    h = sha1(data)
    d = hash_dir(root, h)
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, f"full.{ext}"), "wb") as f:
        f.write(data)
    for v in variants:
        with open(os.path.join(d, f"{v}.{ext}"), "wb") as f:
            f.write(b"scaled")
    return h


class FakeRadiant:
    """Just enough of radiant's API: conditional GET of originals, and POST / uploads."""

    def __init__(self, key: Optional[str] = None):
        self.key = key
        self.images: Dict[str, bytes] = {}
        self.upload_failures = 0  # answer this many uploads with 500 first
        self.reject: set = set()  # hashes to answer with 400
        self.uploads: List[str] = []
        self.gets = 0
        self.lock = threading.Lock()
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def reply(self, status: int, body: bytes = b"", ctype="text/plain"):
                self.send_response(status)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                with fake.lock:
                    fake.gets += 1
                parts = self.path.split("/")  # ['', 'image', hash, 'full', 'image.jpg']
                h = parts[2]
                if h not in fake.images:
                    return self.reply(404, b"not found")
                if self.headers.get("If-None-Match") == f'"{h}"':
                    return self.reply(304)
                self.reply(200, fake.images[h], "image/jpeg")

            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                boundary = self.headers["Content-Type"].split("boundary=")[1].encode()
                fields = {}
                for part in body.split(b"--" + boundary)[1:-1]:
                    head, _, value = part[2:-2].partition(b"\r\n\r\n")
                    name = head.split(b'name="')[1].split(b'"')[0].decode()
                    fields[name] = value
                if fake.key is not None and fields.get("key", b"").decode() != fake.key:
                    return self.reply(403, b"invalid upload key")
                data = fields["image"]
                h = sha1(data)
                with fake.lock:
                    if fake.upload_failures > 0:
                        fake.upload_failures -= 1
                        return self.reply(500, b"internal error")
                    if h in fake.reject:
                        return self.reply(400, b"unsupported image format")
                    fake.uploads.append(h)
                    fake.images[h] = data
                self.reply(200, json.dumps({
                    "hash": h, "length": len(data), "extension": "png",
                    "full_url": f"/image/{h}/full/image.png", "satisfied": True, "nodes": [],
                }).encode(), "application/json")

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, args=(0.05,), daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class MigrateTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = os.path.join(self.tmp.name, "uploads")
        os.makedirs(self.root)
        self.state = os.path.join(self.tmp.name, "state.db")
        self.radiant = FakeRadiant()
        self.addCleanup(self.radiant.close)

    def main(self, *argv: str, stop: Optional[threading.Event] = None) -> int:
        stopper = m.Stopper()
        if stop is not None:
            stopper.event = stop
        return m.main(["--state", self.state, "--no-log-file", *argv], stopper=stopper)

    def migrate(self, *extra: str) -> int:
        return self.main("run", "--radiant", self.radiant.url, "--backoff", "0",
                         "--retries", "2", *extra)

    def statuses(self) -> Dict[str, str]:
        db = sqlite3.connect(self.state)
        try:
            return dict(db.execute("SELECT hash, status FROM images"))
        finally:
            db.close()

    def row(self, h: str) -> sqlite3.Row:
        db = sqlite3.connect(self.state)
        db.row_factory = sqlite3.Row
        try:
            return db.execute("SELECT * FROM images WHERE hash = ?", (h,)).fetchone()
        finally:
            db.close()

    def test_scan_records_originals_and_skips_variants(self):
        a = put_original(self.root, b"image a", "jpg")
        b = put_original(self.root, b"image b", "png")
        self.assertEqual(self.main("scan", self.root), 0)
        self.assertEqual(self.statuses(), {a: "pending", b: "pending"})
        db = sqlite3.connect(self.state)
        paths = [r[0] for r in db.execute("SELECT path FROM copies ORDER BY path")]
        db.close()
        self.assertEqual(len(paths), 2)
        self.assertTrue(all(os.path.basename(p).startswith("full.") for p in paths))

    def test_scan_records_anomalies(self):
        put_original(self.root, b"fine")
        os.makedirs(os.path.join(self.root, "ab", "cd"))
        open(os.path.join(self.root, "ab", "cd", "full.jpg"), "wb").close()
        os.makedirs(os.path.join(self.root, "ab", "zz"))
        self.main("scan", self.root)
        out = io.StringIO()
        db = m.open_db(self.state)
        m.failures(db, "all", anomalies=True, out=out)
        db.close()
        reasons = sorted(line.split("\t")[1] for line in out.getvalue().splitlines())
        self.assertEqual(reasons, ["original outside a hash directory", "unexpected directory"])

    def test_scan_resumes_by_prefix_and_rescan_finds_new_images(self):
        put_original(self.root, b"first")
        self.main("scan", self.root)
        later = put_original(self.root, b"second")
        # The new image's prefix may already be marked as scanned, so only --rescan is sure
        # to find it.
        self.main("scan", self.root, "--rescan")
        self.assertIn(later, self.statuses())

    def test_same_hash_in_several_roots_is_one_image_with_several_copies(self):
        other = os.path.join(self.tmp.name, "node2")
        h = put_original(self.root, b"replicated")
        put_original(other, b"replicated")
        self.main("scan", self.root, other)
        self.assertEqual(self.statuses(), {h: "pending"})
        db = sqlite3.connect(self.state)
        (copies,) = db.execute("SELECT count(*) FROM copies WHERE hash = ?", (h,)).fetchone()
        db.close()
        self.assertEqual(copies, 2)

    def test_run_uploads_everything(self):
        hashes = {put_original(self.root, f"img {i}".encode()) for i in range(25)}
        self.main("scan", self.root)
        self.assertEqual(self.migrate(), 0)
        self.assertEqual(set(self.radiant.uploads), hashes)
        self.assertEqual(set(self.statuses().values()), {"migrated"})
        row = self.row(next(iter(hashes)))
        self.assertEqual(row["radiant_ext"], "png")
        self.assertIsNotNone(row["bytes"])

    def test_images_radiant_has_are_not_uploaded(self):
        h = put_original(self.root, b"already there")
        self.radiant.images[h] = b"already there"
        self.main("scan", self.root)
        self.migrate()
        self.assertEqual(self.radiant.uploads, [])
        self.assertEqual(self.statuses(), {h: "present"})

    def test_a_second_run_does_nothing(self):
        put_original(self.root, b"x")
        self.main("scan", self.root)
        self.migrate()
        gets = self.radiant.gets
        self.assertEqual(self.migrate(), 0)
        self.assertEqual(self.radiant.gets, gets)

    def test_corrupt_copy_falls_back_to_a_good_one(self):
        other = os.path.join(self.tmp.name, "node2")
        h = put_original(self.root, b"good bytes")
        put_original(other, b"good bytes")
        # Corrupt the copy that sorts first.
        first = sorted([os.path.join(hash_dir(r, h), "full.jpg") for r in (self.root, other)])[0]
        with open(first, "wb") as f:
            f.write(b"truncated")
        self.main("scan", self.root, other)
        self.migrate()
        self.assertEqual(self.statuses(), {h: "migrated"})
        self.assertEqual(self.radiant.images[h], b"good bytes")

    def test_every_copy_corrupt_is_a_permanent_failure(self):
        h = put_original(self.root, b"good bytes")
        with open(os.path.join(hash_dir(self.root, h), "full.jpg"), "wb") as f:
            f.write(b"bad")
        self.main("scan", self.root)
        self.assertEqual(self.migrate(), 0)
        row = self.row(h)
        self.assertEqual((row["status"], row["error_kind"]), ("failed", "corrupt"))
        self.assertEqual(self.radiant.uploads, [])

    def test_missing_file_is_unreadable(self):
        h = put_original(self.root, b"will vanish")
        self.main("scan", self.root)
        os.remove(os.path.join(hash_dir(self.root, h), "full.jpg"))
        self.migrate()
        row = self.row(h)
        self.assertEqual((row["status"], row["error_kind"]), ("failed", "unreadable"))

    def test_rejected_image_fails_permanently_and_retry_failed_retries_it(self):
        h = put_original(self.root, b"not really an image")
        self.radiant.reject.add(h)
        self.main("scan", self.root)
        self.migrate()
        row = self.row(h)
        self.assertEqual((row["status"], row["error_kind"]), ("failed", "rejected"))
        self.assertIn("HTTP 400", row["error"])
        # A plain run leaves it alone; --retry-failed tries again.
        self.migrate()
        self.assertEqual(self.radiant.uploads, [])
        self.radiant.reject.clear()
        self.migrate("--retry-failed")
        self.assertEqual(self.statuses(), {h: "migrated"})

    def test_temporary_errors_are_retried(self):
        h = put_original(self.root, b"flaky")
        self.radiant.upload_failures = 2
        self.main("scan", self.root)
        self.assertEqual(self.migrate(), 0)
        self.assertEqual(self.statuses(), {h: "migrated"})
        self.assertEqual(self.row(h)["attempts"], 3)

    def test_persistent_temporary_errors_are_left_for_the_next_run(self):
        h = put_original(self.root, b"flaky")
        self.radiant.upload_failures = 3  # more than --retries 2 allows
        self.main("scan", self.root)
        self.migrate()
        row = self.row(h)
        self.assertEqual((row["status"], row["error_kind"]), ("retry", "http"))
        self.migrate()
        self.assertEqual(self.statuses(), {h: "migrated"})

    def test_stops_when_radiant_is_down(self):
        for i in range(10):
            put_original(self.root, f"img {i}".encode())
        self.main("scan", self.root)
        url = self.radiant.url
        self.radiant.close()
        rc = self.main("run", "--radiant", url, "--backoff", "0", "--retries", "0",
                       "--workers", "1", "--max-consecutive-errors", "3")
        self.assertEqual(rc, 1)
        statuses = list(self.statuses().values())
        self.assertGreaterEqual(statuses.count("retry"), 3)
        self.assertGreater(statuses.count("pending"), 0)
        self.radiant = FakeRadiant()
        self.addCleanup(self.radiant.close)

    def test_wrong_key_stops_the_run(self):
        self.radiant.key = "secret"
        for i in range(5):
            put_original(self.root, f"img {i}".encode())
        self.main("scan", self.root)
        os.environ["RADIANT_UPLOAD_KEY"] = "wrong"
        self.addCleanup(os.environ.pop, "RADIANT_UPLOAD_KEY")
        self.assertEqual(self.migrate("--workers", "1"), 2)
        self.assertEqual(self.radiant.uploads, [])
        self.assertNotIn("migrated", self.statuses().values())

    def test_key_from_file(self):
        self.radiant.key = "secret"
        h = put_original(self.root, b"keyed")
        key_file = os.path.join(self.tmp.name, "key")
        with open(key_file, "w") as f:
            f.write("secret\n")
        self.main("scan", self.root)
        self.assertEqual(self.migrate("--key-file", key_file), 0)
        self.assertEqual(self.statuses(), {h: "migrated"})

    def test_stop_leaves_the_rest_pending(self):
        for i in range(10):
            put_original(self.root, f"img {i}".encode())
        self.main("scan", self.root)
        stop = threading.Event()
        stop.set()
        rc = self.main("run", "--radiant", self.radiant.url, stop=stop)
        self.assertEqual(rc, 1)
        self.assertEqual(set(self.statuses().values()), {"pending"})

    def test_limit(self):
        for i in range(10):
            put_original(self.root, f"img {i}".encode())
        self.main("scan", self.root)
        self.migrate("--limit", "4")
        self.assertEqual(len(self.radiant.uploads), 4)

    def test_verify_marks_missing_images_pending(self):
        a = put_original(self.root, b"kept")
        b = put_original(self.root, b"lost")
        self.main("scan", self.root)
        self.migrate()
        del self.radiant.images[b]
        rc = self.main("verify", "--radiant", self.radiant.url)
        self.assertEqual(rc, 1)
        self.assertEqual(self.statuses(), {a: "migrated", b: "pending"})
        self.migrate()
        self.assertEqual(self.statuses(), {a: "migrated", b: "migrated"})

    def test_status_and_failures_output(self):
        ok = put_original(self.root, b"ok")
        bad = put_original(self.root, b"bad")
        self.radiant.reject.add(bad)
        self.main("scan", self.root)
        self.migrate()
        db = m.open_db(self.state)
        out = io.StringIO()
        m.status(db, out)
        self.assertIn("migrated   1", out.getvalue())
        self.assertIn("failed     1", out.getvalue())
        self.assertIn("rejected", out.getvalue())
        out = io.StringIO()
        m.failures(db, "all", anomalies=False, out=out)
        db.close()
        lines = out.getvalue().splitlines()
        self.assertEqual(len(lines), 2)
        self.assertTrue(lines[1].startswith(bad + "\tfailed\trejected\t1\tHTTP 400"))
        self.assertNotIn(ok, out.getvalue())


    def test_options_in_any_order_and_run_scans_directories_first(self):
        h = put_original(self.root, b"one step")
        # The command line from the bug report: options before the command, directory after.
        rc = m.main(["--radiant", self.radiant.url, "--state", self.state, "--no-log-file",
                     "run", self.root, "--limit", "1000"], stopper=m.Stopper())
        self.assertEqual(rc, 0)
        self.assertEqual(self.statuses(), {h: "migrated"})


class ArgsTest(unittest.TestCase):
    def parse(self, *argv):
        return m.parse_args(list(argv))

    def error(self, *argv) -> str:
        err = io.StringIO()
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(err):
            self.parse(*argv)
        return err.getvalue()

    def test_order_does_not_matter(self):
        forms = [
            ("--state", "s.db", "run", "--radiant", "http://r", "--workers", "2", "/up"),
            ("run", "/up", "--radiant", "http://r", "--state", "s.db", "--workers", "2"),
            ("--workers", "2", "--radiant", "http://r", "--state", "s.db", "run", "/up"),
        ]
        for argv in forms:
            a = self.parse(*argv)
            self.assertEqual((a.command, a.roots, a.radiant, a.workers, a.state),
                             ("run", ["/up"], "http://r", 2, "s.db"), argv)

    def test_defaults(self):
        a = self.parse("run", "--state", "s.db", "--radiant", "http://r")
        self.assertEqual((a.workers, a.retries, a.timeout, a.limit, a.retry_failed),
                         (4, 5, 120.0, 0, False))
        self.assertEqual(self.parse("failures", "--state", "s.db").which, "all")

    def test_errors(self):
        self.assertIn("needs --radiant", self.error("run", "--state", "s.db"))
        self.assertIn("needs at least one UPLOAD_DIR", self.error("scan", "--state", "s.db"))
        self.assertIn("--limit doesn't apply to scan",
                      self.error("scan", "/up", "--state", "s.db", "--limit", "3"))
        self.assertIn("--radiant doesn't apply to status",
                      self.error("status", "--state", "s.db", "--radiant", "http://r"))
        self.assertIn("doesn't take upload directories",
                      self.error("verify", "/up", "--state", "s.db", "--radiant", "http://r"))
        self.assertIn("invalid choice", self.error("upload", "--state", "s.db"))


class HelpersTest(unittest.TestCase):
    def test_fmt_duration(self):
        self.assertEqual(m.fmt_duration(59), "0m59s")
        self.assertEqual(m.fmt_duration(3 * 3600 + 120), "3h02m")
        self.assertEqual(m.fmt_duration(2 * 86400 + 3600), "2d01h00m")

    def test_classify(self):
        self.assertIsInstance(m.classify(403, b""), m.Fatal)
        self.assertIsInstance(m.classify(400, b"nope"), m.Rejected)
        self.assertIsInstance(m.classify(413, b""), m.Rejected)
        for s in (408, 429, 500, 503):
            self.assertIsInstance(m.classify(s, b""), m.Transient)


if __name__ == "__main__":
    unittest.main()
