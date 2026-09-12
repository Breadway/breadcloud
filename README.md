# breadcloud

A scoped Rust replacement for a self-hosted Nextcloud instance: file sync
(WebDAV), calendar (CalDAV), contacts (CardDAV), notes, multi-user auth, and a
minimal web UI, built as a series of breadboard issues (CLOUD-2 through
CLOUD-14). CLOUD-2 built the WebDAV core; CLOUD-3 made it actually work as a
real Nextcloud desktop client (mirall) sync backend; CLOUD-4 (this one) added
capped per-file version history.

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

`read_bytes` reads a file's full content into memory; used by version
snapshotting (see below) to capture a file's content immediately before an
overwrite. GET does not use it (it streams instead, see `handlers::get`).

#### Per-file version history (`versions` module, CLOUD-4)

`VersionStore` is `breadcloud-storage`'s other root-sandboxed store, alongside
`Storage` itself: a small, separate filesystem tree that mirrors the shape of
the live tree, but holds retained *previous* content instead of live content.
A live file at `/a/b/c.txt` has its retained versions as sibling files under
`<versions root>/a/b/c.txt/`, one file per version, named by a zero-padded
nanosecond timestamp (so a plain directory listing already sorts oldest to
newest). This is the same "separate root, out of band from the served tree"
pattern `AppState::uploads` uses for in-progress chunked uploads, applied to
version history instead of upload staging.

Retention is capped two ways on every snapshot, neither configurable (no
policy engine, per the issue -- see the module doc comment in
`versions.rs` for the full reasoning):

- a flat count cap, `DEFAULT_MAX_VERSIONS_PER_FILE = 10`: generous next to
  this deployment's real usage (~615 MB total) without being unbounded, and
  matches the ballpark of Nextcloud's own default without replicating its
  tapering "keep N recent, fewer older at wider intervals" scheme, which is
  more machinery than a flat "last N" needed to justify here;
- a total-size cap per file, `DEFAULT_MAX_VERSION_SET_BYTES = 500 MiB`,
  independent of the count cap, so a handful of versions of one huge file
  (a video, a disk image) cannot balloon storage the count cap alone would
  allow. This is the cap that actually bites for a huge file, pruning its
  history down toward a single retained version rather than ten. It never
  evicts the single newest retained version even if that version alone
  exceeds the cap -- a huge file still keeps at least one prior version
  rather than none.

This crate is also the one that will host trash / soft-delete (a separate,
later issue -- CLOUD-5): both are storage-layer concerns, not WebDAV-layer
ones, but they are deliberately kept as separate stores and separate concepts
-- versioning is per-file *write* history, trash is *deleted*-file recovery.
See `breadcloud-webdav`'s notes below on what happens to a file's version
history on DELETE and MOVE.

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
- `versioning` snapshots a file's current content into version history
  (`AppState::versions`, `breadcloud_storage::VersionStore`) immediately
  before an overwrite. Shared between `handlers::put` and
  `handlers::uploads::finalize` the same way `preconditions` is: both write
  to a destination in the main tree, and since most files in a real synced
  dataset take the chunked path (see CLOUD-3), skipping this in the finalize
  path would leave versioning working only for small files. First creation
  of a file is not a "previous version", so this is a no-op when the target
  path does not already exist.
- `handlers::versions` is a minimal, Nextcloud-versions-inspired DAV surface
  at `/remote.php/dav/versions/<user>/<file-path...>`: `PROPFIND` lists a
  file's retained versions, `GET` fetches one. Real Nextcloud addresses
  versions by a persistent numeric fileid; this server's `oc:fileid` is a
  one-way hash of the resource's *path* (see `fileid.rs`), not a reversible
  database key, so there is no id to route on. This surface is keyed by the
  live file's own path instead -- enough for a client (or a human with curl)
  that already knows which file it wants history for to browse and restore
  prior versions, but not a drop-in implementation of Nextcloud's actual
  versions protocol: nothing here makes a stock Nextcloud desktop client
  auto-discover or drive it the way it auto-discovers the files tree and the
  uploads staging area. There is no PUT/DELETE on this surface: restoring a
  version is "GET the old content, PUT it back to the live path", which the
  normal PUT handler already snapshots as any other overwrite.
- DELETE and MOVE interact with version history too, both by design and both
  documented rather than left to chance:
  - **DELETE** purges the deleted resource's version history along with the
    live content (`VersionStore::delete_all`; for a deleted collection this
    recursively purges every descendant's history in one call, since version
    directories mirror the live tree's shape). There is no trash yet
    (CLOUD-5) to make keeping a deleted file's history meaningful, and
    leaving it behind indefinitely would be a storage leak, so this server
    does not.
  - **MOVE** carries the source's version history over to the new path
    (`VersionStore::move_all`). If the destination already had its own
    version history (a file there is being replaced or removed by the same
    move), that history is discarded first: it belonged to content the move
    is removing.
  - **COPY** does not duplicate version history: a copy is a new,
    independent resource with no overwrite events of its own yet.

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
| `BREADCLOUD_VERSIONS_ROOT` | `./breadcloud-versions` | Per-file version history root (created if missing), outside `BREADCLOUD_ROOT` for the same reason as uploads staging. Unlike uploads staging this needs to persist across restarts, so it defaults to a sibling directory rather than the system temp dir |
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

# overwrite it (creates a version of what was just there)
curl -u dev:dev -X PUT --data-binary @newer-version.txt http://127.0.0.1:8080/renamed.txt

# browse its version history
curl -u dev:dev -X PROPFIND http://127.0.0.1:8080/remote.php/dav/versions/dev/renamed.txt

# fetch one specific version (id from the PROPFIND above)
curl -u dev:dev http://127.0.0.1:8080/remote.php/dav/versions/dev/renamed.txt/<version-id>
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
Overwrite header handling, Basic auth, and version history: first-creation
does not version, an overwrite creates a browsable and fetchable version, the
default count cap evicts old versions, DELETE purges history, MOVE carries
history to the new path, and a chunked-upload finalize overwrite versions the
same as a plain PUT. `breadcloud-storage`'s own unit tests cover
`VersionStore` directly: eviction under both the count and size caps, the
"never evict the last version" rule, and delete/move behavior including a
directory delete cascading to every descendant's history.

Manually smoke-tested the running binary with curl against a disposable
`/tmp` data root and versions root: three successive overwrites of one file
retained exactly the two prior versions with correct content on GET, MOVE
carried that history to the new path while the old path's version collection
came back empty, and DELETE on the moved file emptied both the served tree
and the version history root on disk (no leftover files).

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
CLOUD-4: trash/soft-delete (CLOUD-5 -- note this is deliberately a separate
concept and a separate store from version history, not an extension of it),
CalDAV, CardDAV, notes, multi-user auth + per-user quotas (replacing the
placeholder Basic auth here, and giving `fileid.rs` a real persistent id to
hand out instead of a path-derived one -- which would also let the versions
DAV surface move to the real `<fileid>`-keyed layout instead of the
path-keyed one it uses today), a minimal web UI, and bread-ecosystem event
integration. Each of those should be a new crate under `crates/` that depends
on `breadcloud-storage` and, where it needs HTTP, nests alongside or reuses
`breadcloud-webdav`'s router rather than duplicating it.
