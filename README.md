# breadcloud

A scoped Rust replacement for a self-hosted Nextcloud instance: file sync
(WebDAV), calendar (CalDAV), contacts (CardDAV), notes, multi-user auth, and a
minimal web UI, built as a series of breadboard issues (CLOUD-2 through
CLOUD-14). This is the first issue: the WebDAV core.

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
`quota`. Each returns enough information (`WriteOutcome::Created` vs.
`Overwritten`, `StorageError::Conflict` vs. `NotFound` vs. `AlreadyExists`) for
the HTTP layer to pick the RFC-correct status code without re-deriving it from
raw filesystem errors.

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
- `xml::response` builds the `multistatus` XML. Properties outside its
  supported set (Nextcloud's `oc:`/`nc:` extension properties, WebDAV locking
  properties) correctly come back as 404 inside their own `propstat` --
  that's how PROPFIND is supposed to work, not a bug.
- `auth` is a placeholder HTTP Basic auth gate (single user/pass, or none at
  all if unconfigured). CLOUD-11 replaces this with real multi-user accounts
  and per-user quotas.

Explicitly out of scope for this issue (see the issue body and later epic
issues): WebDAV locking (`LOCK`/`UNLOCK`, DAV class 2) and
streamed/chunked upload for very large files -- `PUT` currently buffers the
whole request body (capped at 20 GiB) before writing it out, which CLOUD-3
(desktop sync client compatibility) will revisit alongside etag/mtime conflict
detection.

### breadcloud-server

Thin binary. Configuration is environment variables:

| Variable | Default | Meaning |
|---|---|---|
| `BREADCLOUD_ROOT` | `./data` | Directory to serve (created if missing) |
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
credentials -- Nextcloud's desktop client (mirall), the Files app on iOS, or
`davfs2`/`rclone serve webdav` on Linux.

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

## What's next in the epic

See the CLOUD project on breadboard for the full epic. In rough order after
this issue: CLOUD-3 (desktop sync client compatibility: chunked upload,
etag/mtime conflict detection), file versioning, trash/soft-delete, CalDAV,
CardDAV, notes, multi-user auth + per-user quotas (replacing the placeholder
Basic auth here), a minimal web UI, and bread-ecosystem event integration.
Each of those should be a new crate under `crates/` that depends on
`breadcloud-storage` and, where it needs HTTP, nests alongside or reuses
`breadcloud-webdav`'s router rather than duplicating it.
