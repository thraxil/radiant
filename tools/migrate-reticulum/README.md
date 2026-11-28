# migrate-reticulum

This script copies the original images from reticulum's upload directories into radiant. It
does this through radiant's normal upload API (`POST /`). Radiant re-renders the scaled
versions on demand, so the script skips them.

Each upload goes to radiant, which hashes the bytes. A migrated image therefore keeps its
hash, and every existing `/image/<hash>/...` URL keeps working.

The script uses only the Python 3.9+ standard library, so you can copy it onto a reticulum node
and run it there. There is nothing to install.

## How it works

```
scan  ──► state.db ──► run ──► radiant
```

- **`scan UPLOAD_DIR...`** walks reticulum's `aa/bb/…/full.<ext>` tree. It records each
  original in a SQLite state file, keyed by hash.
  - One hash can have several copies, from several nodes' directories, or as both
    `full.jpg` and `full.png`. All of them are recorded.
  - Progress is saved for each top-level prefix (`00`–`ff`), so an interrupted scan
    carries on where it stopped.
  - Anything that doesn't fit the layout is logged and kept in the state file as an
    *anomaly*.
- **`run`** takes each image that isn't done yet:
  1. Asks radiant whether it already has the image. This is a conditional GET, which
     radiant answers from its catalog with a 304. If radiant has it, the image is marked
     `present`.
  2. Reads a copy and checks that its SHA-1 matches the hash. A truncated or corrupt copy is
     logged and skipped, and the next copy is tried.
  3. Uploads it, then checks that radiant reports the same hash and length.
- Everything is recorded in the state file as it goes. You can stop a run at any time and
  start it again later.
  - The first Ctrl-C or SIGTERM lets in-flight uploads finish. A second one exits
    immediately.
  - Uploading again is harmless, because uploads to radiant are idempotent.

### Image statuses

| status     | meaning                                                                          | `run` retries it?   |
|------------|----------------------------------------------------------------------------------|---------------------|
| `pending`  | found by a scan, not tried yet                                                   | yes                 |
| `migrated` | uploaded by this script                                                          | no                  |
| `present`  | radiant already had it (e.g. another node uploaded it)                           | no                  |
| `retry`    | temporary error (network, timeout, 5xx) after `--retries` attempts with backoff  | yes                 |
| `failed`   | permanent error (see below)                                                      | with `--retry-failed` |

Permanent failures have an `error_kind`:

- **`rejected`**: radiant refused the image (400/413), for example an unsupported format,
  a file over the upload size limit, or one over the pixel limit.
- **`corrupt`**: no copy hashes to the expected value.
- **`unreadable`**: no copy could be read.
- **`mismatch`**: radiant hashed the upload differently. This should never happen.

The run stops by itself in two cases:
- Radiant refuses the upload key (403). The exit code is 2.
- `--max-consecutive-errors` images in a row (default 20) end with temporary errors, which
  usually means radiant is down. The exit code is 1.

In both cases, fix the problem and run the same command again.

### Logs

Every command except `status` and `failures` logs to stderr. It also appends to
`<state>.log`, which you can change with `--log-file`. Log lines are `key=value`, so they're
easy to grep:

```
2026-11-28 09:33:25,607 WARNING bad copy hash=f1b6… /…/node1/f1/b6/…/full.png: corrupt: 100 bytes hashing to 4e1…
2026-11-28 09:33:25,628 WARNING failed hash=3c89… kind=rejected error=HTTP 400: unsupported or unrecognised image format (from /…/full.jpg)
2026-11-28 09:34:25,629 INFO progress processed=41200/310000 rate=11.9/s eta=6h17m this_run={…} totals={…}
```

A progress line is written every `--progress-interval` seconds (default 60). Add `-v` to log
every image.

## Usage

```sh
M=migrate_reticulum.py
STATE=/var/lib/radiant-migrate/state.db    # keep on local disk (SQLite), not NFS

python3 $M --state $STATE scan /var/data/reticulum/uploads

export RADIANT_UPLOAD_KEY=…                 # or --key-file /path/to/key
python3 $M --state $STATE run --radiant http://radiant.lan:8080

python3 $M --state $STATE status            # any time, also while run is going
python3 $M --state $STATE failures          # TSV: hash, status, kind, attempts, error, paths
python3 $M --state $STATE failures --anomalies
python3 $M --state $STATE verify --radiant http://radiant.lan:8080
```

The upload key is read from `$RADIANT_UPLOAD_KEY` or from `--key-file`, never from a
command-line argument, so it doesn't show up in `ps`.

Useful `run` options (`--help` lists all):

- **`--workers N`**: concurrent uploads (default 4).
- **`--rate R`**: start at most R images per second, to stay gentle on radiant and Cask.
- **`--limit N`**: process only N images. Good for a first trial run.
- **`--retries N`** and **`--backoff S`**: how hard to retry temporary errors before moving on.
- **`--no-check-existing`**: skip the "does radiant already have it?" request.

For a run that lasts days, start it in `tmux`/`screen`, or as a transient unit:

```sh
systemd-run --user --unit radiant-migrate -E RADIANT_UPLOAD_KEY=… \
  python3 $PWD/$M --state $STATE run --radiant http://radiant.lan:8080
```

## Migrating a multi-node reticulum

Reticulum replicates each image to several of its nodes. There are two ways to handle that.

**Run on every node (recommended).** Copy the script to each node and give each one its own
state file.
- Each node reads its own disk, and the nodes work in parallel.
- Replicas need no coordination. Whichever node gets to an image first uploads it, and the
  others then find it in radiant and mark it `present`. That costs one cheap 304 each.
- A copy that is corrupt on one node is no problem if another node has a good copy.
  - The first node records `failed`/`corrupt` for the image.
  - After all nodes have finished, run `run --retry-failed` once more on each node.
  - Each node checks radiant before reading its own copy, so any image another node uploaded
    becomes `present`.
  - What's left after that is really missing everywhere.
- Radiant handles at most 8 uploads at a time (`RADIANT_MAX_CONCURRENT_UPLOADS`). Other
  uploads wait for a slot, up to radiant's 120s request timeout. With 10 nodes, use
  `--workers 1` or `2` per node, or raise radiant's limit.

**Run once over all nodes.** Mount every node's upload directory on one host and scan them all
into one state file:

```sh
python3 $M --state $STATE scan /mnt/node01/uploads /mnt/node02/uploads …
```

Each image is then tried once. Corrupt copies fall back to other nodes' copies automatically,
and one `failures` list covers the whole cluster. The catch is that the reads go over the
network.

### Cutting over

1. Run the bulk migration while reticulum keeps serving and taking uploads.
2. Point uploads at radiant.
3. On each node, run `scan --rescan`, then `run`. This picks up images uploaded to reticulum
   during the bulk run. It walks the whole tree again, but only the new images get uploaded.
4. Run `verify`, check `failures`, then point image traffic at radiant.

## Tests

```sh
python3 -m unittest discover -s tools/migrate-reticulum -v
```

The tests run against a fake radiant that can inject failures (500s, 400s, a wrong key, being
down).
