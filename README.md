# breadcloud

A scoped Rust replacement for a self-hosted Nextcloud instance: file sync
(WebDAV), calendar (CalDAV), contacts (CardDAV), notes, multi-user auth, and a
minimal web UI, built as a series of breadboard issues (CLOUD-2 through
CLOUD-14). CLOUD-2 built the WebDAV core; CLOUD-3 (this one) made it actually
work as a real Nextcloud desktop client (mirall) sync backend.

## Workspace layout

A Cargo workspace, not a single binary, so later issues (CalDAV, CardDAV, the
web UI) can add crates that reuse the storage and HTTP layers without a
rewrite.

```
crates/
  breadcloud-storage/   filesystem-backed storage primitives, no HTTP knowledge
  breadcloud-webdav/    WebDAV (RFC 4918) HTTP layer, built on axum
  breadcloud-server/    binary: wires storage + webdav into a runnable server
```

### breadcloud-storage

Everything here is `tokio::fs` plus path safety, with zero knowledge of HTTP
or WebDAV. The central type is `Storage`, which is sandboxed to a root
directory: every operation takes a `DavPath` (a validated, storage-relative
path that cannot contain `..` or otherwise escape the root) rather than a raw
string.

Key operations: `metadata`, `open` (streamed reads), `list_dir` /
`list_recursive`, `write_bytes`, `mkcol`, `delete`, `move_to`, `copy_to`,
`assemble`, `quota`, `etag` / `etag_for`. Each write returns enough
information (`WriteOutcome::Created` vs. `Overwritten`, `StorageError::Conflict`
vs. `NotFound` vs. `AlreadyExists`) for the HTTP layer to pick the RFC-correct
status code without re-deriving it from raw filesystem errors.

Etags are content-derived (a cached BLAKE3 hash), not just mtime/size: a plain
mtime-and-length etag has real false-negative collisions at typical filesystem
mtime granularity, which is exactly the failure mode a sync client's conflict
detection depends on not having. The hash is computed once per write (free,
from bytes already in memory) or lazily and cached on first read for content
written outside this crate; `write_bytes` and `assemble` both return the new
etag directly so callers do not need a second read to learn it. Directories
keep the cheaper mtime/len signature (`Metadata::etag`) -- there is no
meaningful "content" to hash for a collection.

`assemble` streams N pre-uploaded chunk files (from a separate `Storage`, not
necessarily this one) into a destination in order, hashing as it goes, and
optionally validates the total length against a caller-supplied expectation
before making the result visible -- the storage-layer half of chunked upload
finalization (see `breadcloud-webdav::handlers::uploads`).

This crate is the one to extend for file versioning (CLOUD-*) and trash /
soft-delete (CLOUD-*): both are storage-layer concerns, not WebDAV-layer ones.

### breadcloud-webdav

Turns a `Storage` into an `axum::Router` implementing RFC 4918: `PROPFIND`,
`GET`/`HEAD`, `PUT`, `DELETE`, `MKCOL`, `MOVE`, `COPY`, `OPTIONS`. All non-GET
methods route through a single `fallback` handler (`dispatch` in `lib.rs`)
rather than axum's per-method routing, because `PROPFIND`/`MKCOL`/`MOVE`/`COPY`
aren't in axum's built-in `MethodFilter`.

Notable pieces:

- `xml::request` parses a PROPFIND request body (`allprop` / `propname` /
  explicit `prop` list) by local element name only, ignoring XML namespace
  prefixes -- real clients bind the `DAV:` namespace to different prefixes,
  and every property this server understands lives in that one namespace.
- `xml::response` builds the `multistatus` XML, declaring both the `DAV:`
  (`d:`) and Nextcloud's `http://owncloud.org/ns` (`oc:`) namespaces.
  Properties outside its supported set correctly come back as 404 inside
  their own `propstat` -- that's how PROPFIND is supposed to work, not a bug.
  `oc:fileid`/`oc:id` and `oc:permissions` are always answered (see
  `fileid.rs`): confirmed against the real desktop client that it refuses a
  file as "not accessible on the server" on any sync after the first without
  them, regardless of etag/size/displayname all being correct.
- `handlers::discovery` serves `status.php` (unauthenticated -- see `auth.rs`)
  and the `ocs/*/cloud/{capabilities,user}` endpoints. A real Nextcloud client
  refuses to even attempt the DAV tree without these; confirmed by running
  `nextcloudcmd` against this server without them ("Error connecting to
  server").
- `handlers::uploads` implements Nextcloud's chunked upload protocol (NG
  chunking v2) at `/remote.php/dav/uploads/<user>/<transfer-id>/...`: MKCOL
  creates a transfer, PUT lands numbered chunks in a staging `Storage`
  distinct from the served tree (`AppState::uploads`), PROPFIND on the
  transfer lists what has landed (the resumability mechanism), and MOVE
  assembles the chunks in numeric order into the real destination, validating
  `OC-Total-Length` and applying `X-OC-Mtime` if given. Verified end to end
  against a real `nextcloudcmd` (mirall's non-GUI sibling, same libsync code):
  multi-chunk upload, an interrupted-and-resumed upload picking up from the
  right chunk, and a genuine local+remote conflict correctly producing a
  "conflicted copy" on the client rather than a silent overwrite.
- `mtime.rs` applies the client's `X-OC-Mtime` header and echoes back
  `X-OC-MTime: accepted` (mirall logs "Server does not support X-OC-MTime" and
  falls back to its own bookkeeping without that confirmation header, even
  though the mtime was in fact applied -- confirmed against the real client).
- `handlers::put` adds `If-Match`/`If-None-Match` conflict-detection
  preconditions (RFC 7232) ahead of a write: `If-Match` protects an update
  against a conflicting change the client has not seen yet, `If-None-Match: *`
  protects a create against a race with another client. Both 412 on
  violation.
- `auth` is a placeholder HTTP Basic auth gate (single user/pass, or none at
  all if unconfigured). CLOUD-11 replaces this with real multi-user accounts
  and per-user quotas.

Explicitly out of scope for this issue and later epic issues: WebDAV locking
(`LOCK`/`UNLOCK`, DAV class 2), and file ids that survive a rename (this
server derives `oc:fileid` from the path, since there is no persistent
file-metadata database yet -- see `fileid.rs`).

### breadcloud-server

Thin binary. Configuration is environment variables:

| Variable | Default | Meaning |
|---|---|---|
| `BREADCLOUD_ROOT` | `./data` | Directory to serve (created if missing) |
| `BREADCLOUD_UPLOADS_ROOT` | system temp dir + `breadcloud-uploads` | Staging area for in-progress chunked uploads (created if missing). Deliberately outside `BREADCLOUD_ROOT`, not just dot-hidden inside it, so it can never appear in a PROPFIND of the served tree |
| `BREADCLOUD_BIND` | `127.0.0.1:8080` | Address to listen on |
| `BREADCLOUD_USER` / `BREADCLOUD_PASS` | unset | HTTP Basic auth credentials. If either is unset, the server runs open (fine for `127.0.0.1`, never for anything reachable off localhost) |

## Running locally

```bash
mkdir -p /tmp/breadcloud-data
BREADCLOUD_ROOT=/tmp/breadcloud-data \
BREADCLOUD_BIND=127.0.0.1:8080 \
BREADCLOUD_USER=dev BREADCLOUD_PASS=dev \
cargo run -p breadcloud-server
```

Then, from another terminal:

```bash
# list the (empty) root
curl -u dev:dev -X PROPFIND -H "Depth: 1" http://127.0.0.1:8080/

# upload a file
curl -u dev:dev -X PUT --data-binary @somefile.txt http://127.0.0.1:8080/somefile.txt

# make a directory
curl -u dev:dev -X MKCOL http://127.0.0.1:8080/photos

# move it
curl -u dev:dev -X MOVE -H "Destination: /renamed.txt" http://127.0.0.1:8080/somefile.txt
```

Or point a real WebDAV client at `http://127.0.0.1:8080/` with those
credentials -- `davfs2`/`rclone serve webdav` on Linux, or the Files app on
iOS. Nextcloud's desktop client (mirall) specifically wants
`http://127.0.0.1:8080/` as the *server* URL (not the DAV path): it discovers
`status.php` and the DAV mount (`/remote.php/dav/files/<user>/...`) itself,
and the mount prefix is handled transparently either way (see
`split_mount_prefix` in `lib.rs`).

## Testing

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

`breadcloud-webdav`'s `tests/webdav_test.rs` drives the real `axum::Router`
end to end via `tower::ServiceExt::oneshot` (no socket needed), covering
PROPFIND depth 0/1, PUT create/overwrite/conflict, GET, MKCOL, MOVE, COPY,
Overwrite header handling, and Basic auth.

### Real-client verification (CLOUD-3)

Chunked upload, resumability, and conflict detection were verified against a
real Nextcloud desktop client on the machine this was built on
(`nextcloudcmd`, the non-GUI sibling of mirall built from the same libsync
code), pointed at a scratch source directory and a disposable local server
instance -- never at any real synced folder. Covered: a multi-chunk upload of
a large file assembling correctly (content hash matched before/after), an
upload interrupted mid-transfer and resumed via a second client invocation
picking up from the correct next chunk (verified by inspecting the chunk
staging directory before and after), and a genuine local-and-remote conflict
producing a client-side "conflicted copy" rather than a silent overwrite. Temporary
request-logging instrumentation used to discover the exact request sequence
was removed before finishing; none of it is in this codebase.

## What's next in the epic

See the CLOUD project on breadboard for the full epic. In rough order after
CLOUD-3: file versioning, trash/soft-delete, CalDAV, CardDAV, notes,
multi-user auth + per-user quotas (replacing the placeholder Basic auth here,
and giving `fileid.rs` a real persistent id to hand out instead of a
path-derived one), a minimal web UI, and bread-ecosystem event integration.
Each of those should be a new crate under `crates/` that depends on
`breadcloud-storage` and, where it needs HTTP, nests alongside or reuses
`breadcloud-webdav`'s router rather than duplicating it.
