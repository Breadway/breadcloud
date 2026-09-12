# breadcloud

A scoped Rust replacement for a self-hosted Nextcloud instance: file sync
(WebDAV), calendar (CalDAV), contacts (CardDAV), notes, multi-user auth, and a
minimal web UI, built as a series of breadboard issues (CLOUD-2 through
CLOUD-14). CLOUD-2 built the WebDAV core; CLOUD-3 made it actually work as a
real Nextcloud desktop client (mirall) sync backend; CLOUD-4 added capped
per-file version history; CLOUD-5 (this one) added soft-delete trash with an
explicit age and size retention cap.

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

#### Soft-delete trash (`trash` module, CLOUD-5)

`TrashStore` is `breadcloud-storage`'s third root-sandboxed store. Versioning
is per-file *write* history; trash is *deleted*-resource recovery -- a
deliberately separate concept and a separate store, not an extension of
`VersionStore`, though the two do interact (see below). DELETE no longer
removes a resource from disk: it moves the resource (renamed, not copied, so
trashing a large directory is one filesystem operation on the common case of
the trash root and served tree sharing a volume, with a recursive
copy-then-remove fallback when they do not -- e.g. `TrashStore` configured on
a different volume) into a new entry directory under the trash root, named by
a sortable id (the same zero-padded-nanosecond-timestamp scheme as
`VersionStore`'s version ids). Each entry holds the resource's content
(`payload`) alongside small sidecar files recording its original path, kind
(file or directory), and total size.

**Both an age cap and a total-size cap, not just one.** The issue this
implements exists because the real Nextcloud instance this server replaces
has *neither* cap and had accumulated 94 GB of trash against a 3.6 GB live
dataset. An age cap alone would not have caught that on its own (repeated
small deletions do not age out fast enough relative to how often they
happen); a size cap alone would not either (it only bites once the bound is
already hit, by which point genuinely old, forgotten deletions are exactly
what should already have been reclaimed). So `TrashStore::enforce_caps` does
both: purge anything older than `DEFAULT_TRASH_MAX_AGE` (30 days -- generous
for "I deleted something by mistake, when do I notice", but a hard, finite
bound where the real instance had none at all), then evict the oldest
remaining entries until the total is back under
`DEFAULT_TRASH_MAX_TOTAL_BYTES` (16 GiB -- sized relative to this
deployment's real live dataset of about 3.6 GB rather than picked
arbitrarily: generous enough that ordinary day-to-day deletions, plus the
occasional large folder, comfortably fit well before this bites, while still
being a small, bounded multiple of the live tree instead of the unbounded
growth that is the bug). Neither cap is user-configurable (no policy engine),
matching `VersionStore`'s own caps and the issue's ask for "generous but
bounded", not a configurable retention system nobody asked for. Enforcement
runs after every trash/restore/purge rather than needing an external cron job
that does not exist yet, since this is a single-process server with nothing
else to run one.

Unlike `VersionStore::prune`'s "never evict the last retained version" rule,
cap eviction here can empty the trash entirely: an empty trash is a
perfectly ordinary state, unlike zero retained versions of a file that is
still *live*, which would defeat that feature for that one file.

**Version history follows a soft-deleted file into trash.** Deleting a file
moves its version history (`VersionStore::move_out`, moving the whole
per-path version subtree into a `versions/` subdirectory the webdav layer
reserves inside the same trash entry) rather than purging it outright or
leaving it independently retained. Restoring moves it back the same way
(`VersionStore::move_in`). Both go through `move_path` (shared with
`TrashStore`'s own payload move above), the safe replace primitive described
in the next section, rather than a bare rename, since
`BREADCLOUD_VERSIONS_ROOT` is exactly as independently configurable as the
trash root and can equally land on a different volume.
This was a deliberate choice among the three options the issue calls out:
purging history immediately would mean an accidental deletion loses every
prior draft too, a worse outcome than trash exists to prevent; keeping it
independently retained in `VersionStore` while the live file is gone would
either leak forever (nothing there knows the path is trashed, so nothing
would ever purge it) or need a second cap/enforcement path duplicating
`TrashStore`'s own. Moving it into the same trash entry means one purge
(cap eviction or an explicit early purge) cleans up both, and one restore
brings both back -- nothing to leak or orphan in either store.

Restore moves the version history *before* the payload, and rolls that move
back if the payload move then fails, not the other way around. An earlier
version of this fix moved the payload first: if the version-history move
then failed, the fix correctly left the trash entry in place rather than
purging it, but the payload had already landed on the live tree by that
point, so the entry looked "occupied, retry with `Overwrite: T`" to a
client, and following that exact suggestion deleted the just-restored file
and then found no payload left in trash to put back, losing the file for
good. Moving the version history first instead means the payload -- the
only thing a client can observe land on the live tree -- is always the last
move `handlers::trashbin::restore` makes: every early return before that
move leaves the live tree completely untouched and the trash entry fully
intact, so a retry always starts clean. If the payload move itself then
fails after the version history already moved, that move is rolled back
into the trash entry (`VersionStore::move_out` again, in reverse) rather
than left stranded on the live side with nothing to point a later retry
back to. See `breadcloud-webdav`'s notes below for the DAV-layer half of
this (`crate::trash`, `handlers::trashbin`).

#### The `move_path` safe replace primitive

Three review rounds on this restore-ordering fix each found a genuine
data-loss bug, and each fix introduced a new one one level deeper, before
the actual structural problem got addressed:

1. `VersionStore::move_out`/`move_in` had no fallback for a cross-device
   move failing outright (`fs::rename` cannot cross filesystems), so a
   failure could leave version history behind on delete, or get a trash
   entry's directory purged out from under a move that never completed on
   restore.
2. A rename-then-copy fallback fixed that, but restore still moved the
   payload before the version history; a version-history failure after the
   payload had already landed live meant a retry's "occupied, use
   `Overwrite: T`" response deleted the just-restored file and found no
   payload left in trash to replace it with.
3. Reordering restore to move version history first, and rolling that move
   back on a payload failure, fixed *that* -- but the rollback was itself
   just another call to `VersionStore::move_in`, whose destination-clobber
   step (an unconditional `remove_dir_all` on whatever was at the
   destination) could not tell a genuine, complete version history apart
   from a partial leftover of a previous, half-failed attempt at the exact
   same move. If the rollback's own cross-device copy failed partway, the
   next retry would see "destination has content" and delete the real,
   complete history sitting on the live-mirror side, reporting success.
4. The `move_path` redesign below (round 4) fixed *that* by never discarding
   a destination's prior content until the replacement was confirmed
   whole -- but its own step 2/3 error recovery (undoing a rename it had
   just done) discarded the result of that undo via `let _ = fs::rename(...)`
   and returned the *primary* error regardless of whether the undo actually
   worked. A failure in the recovery itself was indistinguishable from the
   safe "nothing moved" case, while the content it was supposed to put back
   was actually stranded at a stage/backup path.

Each round patched one partial-failure path and the fix itself became the
next round's bug. Rounds 1 through 3 shared one root cause: "does the
destination already have content" cannot distinguish a genuine pre-existing
file from a partial leftover of a previous attempt at the same move. Round 4
fixed that, but introduced a narrower version of the same pattern one level
down: a filesystem mutation that "basically always succeeds" (the recovery
rename) had its result discarded, on the assumption that the only failure
worth handling was the primary one.

`move_path` (`breadcloud-storage/src/lib.rs`) sidesteps the distinction
instead of trying to make it. Nothing at the destination is ever replaced
until the source's content is fully staged next to it -- on the
destination's own filesystem, not the source's -- and verified complete;
nothing at the destination's *previous* content is discarded until the
staged content has been atomically swapped into place; and the source is
only removed once that swap has committed. Concretely:

1. The source is moved into a staging path beside the destination (same
   directory, so guaranteed the same filesystem): a plain rename first
   (free, when source and destination happen to already share a
   filesystem), falling back to a recursive copy that is verified complete
   (same file count and total bytes as the source) before anything
   downstream trusts it. A failure here leaves both the source and the
   destination untouched.
2. Whatever currently occupies the destination, if anything, is renamed
   aside rather than deleted, and is not touched again until step 3
   commits.
3. The staged content is renamed into the destination -- a single atomic,
   same-filesystem rename, since the staging path, the old-content backup,
   and the destination all share a parent directory.
4. Only now, with new content durably live at the destination, are the old
   destination content (from step 2) and the original source cleaned up,
   both best-effort: neither failing is a correctness problem, only litter.

A failure at step 2 or 3 unwinds back to the pre-call state (the source
restored if step 1 moved it by rename; the destination's original content
renamed back if it had been moved aside), so every caller can keep treating
an *ordinary* `Err` from `move_path` as "nothing moved". But the property
that actually closes the round-3 class of bug is stronger than that: because
nothing is ever destroyed before its replacement is confirmed safely in
place, the whole operation is idempotent. A caller that retries a failed
higher-level operation (a soft-delete, a restore, or a rollback of either)
from scratch never compounds a first failure into a second one, and needs
no bespoke per-call rollback logic layered on top of the move itself --
retrying just re-runs this same safe replace, which is a no-op if the
destination already holds the right content. This is why
`handlers::trashbin::restore` no longer pre-deletes an occupied restore
destination by hand, or cleans up partial content after a failed move by
hand, the way earlier rounds' fixes did: `move_path` already guarantees
there is nothing to clean up.

**Round 5: the unwind itself can fail.** "A failure at step 2 or 3 unwinds
back to the pre-call state" is only true when the unwind itself succeeds --
and it is just more filesystem mutation, exposed to the exact same class of
fault (a permission change, a disk remount, an I/O hiccup) that broke the
primary step a moment earlier. `move_path` used to run that unwind (a
rename-back in `restore_staged_src`, and a rename-back of the destination's
backup before it) and discard the result via `let _ = fs::rename(...)`,
returning the primary error either way. A failed unwind then looked
identical to the safe "nothing moved" case, while the content it was
supposed to restore was actually stranded at a `.breadcloud-stage-*` or
`.breadcloud-backup-*` path with nothing pointing back to it. This is not
limited to the cross-device case: it only requires the ordinary same-
filesystem rename path (`staged_via_rename == true`), i.e. the default
single-disk deployment.

`move_path` now checks the result of every compensating rename in its own
unwind. If either one fails, it returns `StorageError::RecoveryFailed`
instead of the primary error -- a distinct variant naming exactly which
path(s) (`stranded_src`, `stranded_dest`) hold content that was not lost but
also was not automatically restored. Callers must not treat this the way
they treat an ordinary `Err`: `handlers::trashbin::restore` and
`breadcloud_webdav::trash::move_to_trash` both check for it explicitly and
log it at `error!` with the exact stranded path(s), rather than folding it
into the same "safe to retry" message an ordinary failure gets. Every other
discarded filesystem-mutation result in `breadcloud-storage` was audited at
the same time (see the round 5 entry in "Real-client verification" below for
the full list): the ones left as `let _ = ...` are cleanup of content this
crate does not need for correctness (a temp file whose source bytes the
caller still has, a redundant `remove_dir_all` on an ordinary `MOVE` where
discarding the destination's old history is the intended behavior and any
partial failure surfaces via the subsequent rename's own error anyway), and
each one now has a comment explaining why.

`move_path` backs every cross-storage-root move in this codebase: both
directions of `TrashStore`'s payload move, and both directions of
`VersionStore`'s version-history move, including the rollback move a failed
restore uses to undo an already-succeeded version-history move. It is not
used by `VersionStore::move_all` (the version-history carry on an ordinary
DAV `MOVE`), which intentionally discards a destination's own prior history
outright -- correct there, since that destination's old history genuinely
belongs to content the move is removing, not to a partial leftover of
anything -- nor by `Storage::relocate` (`COPY`/`MOVE` within the served
tree), which never crosses a storage root and has no separate source to
protect a failure's data in. The plain snapshot-on-write path in
`breadcloud-webdav::versioning` was checked too and needs none of this: it
only ever creates a new, uniquely-named version file, never replaces or
discards existing content, so it has no analogous risk to begin with.

`breadcloud-storage`'s own tests exercise `move_path` directly (a fresh
destination, an occupied destination replaced without merging rather than
clobbered, a forced permission failure before the swap that leaves both
sides untouched), and `VersionStore`'s tests include a scenario that drives
the exact round-3 sequence end to end: a forward restore move forced to
fail and retried, then a rollback of that same move forced to fail and
retried, asserting the real, multi-version history is never destroyed at
either forced failure and the final state is correct either way -- two
independent forced failures within one retry arc, not just the single
forced failure each earlier round's tests added.

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
  - **DELETE** moves the deleted resource's version history into trash
    alongside its content (see below) rather than purging it, since CLOUD-5
    added somewhere meaningful to keep it.
  - **MOVE** carries the source's version history over to the new path
    (`VersionStore::move_all`). If the destination already had its own
    version history (a file there is being replaced or removed by the same
    move), that history is discarded first: it belonged to content the move
    is removing.
  - **COPY** does not duplicate version history: a copy is a new,
    independent resource with no overwrite events of its own yet.
- `trash` coordinates soft-delete across `Storage`, `VersionStore`, and
  `TrashStore` (CLOUD-5), the same way `versioning` coordinates `Storage` and
  `VersionStore` for snapshot-on-overwrite. `handlers::delete` now calls
  `trash::move_to_trash` instead of `Storage::delete`: it computes the
  resource's size (recursively, for a directory), moves its filesystem
  content into a new `TrashStore` entry, moves its version history into that
  same entry, then enforces the retention caps. **MOVE never goes through
  this**: `handlers::copy_move` never called into version or trash logic in
  the first place (a rename is not a deletion), so there was nothing to
  change there -- confirmed by an end-to-end test.
- `handlers::trashbin` is a minimal, Nextcloud-trashbin-inspired DAV surface
  at `/remote.php/dav/trashbin/<user>/...`: `PROPFIND` lists every trashed
  item (with `nc:trashbin-original-location` / `nc:trashbin-deletion-time`
  properties named after real Nextcloud's own, mirroring the existing
  client-facing convention), `MOVE` restores one (`Destination` optional --
  omit it to restore to the original path, or give one when that path is
  occupied; unlike the main MOVE handler, `Overwrite` defaults to *false*
  here, since silently replacing whatever now occupies the restore path with
  old, trashed content is a worse default than requiring an explicit
  `Overwrite: T`), and `DELETE` permanently purges an entry early. Real
  Nextcloud addresses a trashed item by fileid plus a deletion timestamp;
  this server has no fileid (see `fileid.rs`), so entries are addressed by
  `TrashStore`'s own opaque id instead. Scoped down the same way
  `handlers::versions` is: no browsing inside a trashed directory's own
  subtree (it is restored or purged as the one unit it was deleted as), and
  no GET to fetch trashed content directly -- the issue asks for
  list/restore/purge, not a download endpoint.

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
| `BREADCLOUD_TRASH_ROOT` | `./breadcloud-trash` | Soft-delete trash root (created if missing), outside `BREADCLOUD_ROOT` for the same reason as uploads staging and version history, and (like version history) needs to persist across restarts, so it also defaults to a sibling directory rather than the system temp dir |
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

# delete it -- this moves it (and its version history) into trash, it does
# not actually remove it from disk yet
curl -u dev:dev -X DELETE http://127.0.0.1:8080/renamed.txt

# list what's in trash (id, original path, and deletion time per entry)
curl -u dev:dev -X PROPFIND -H "Depth: 1" http://127.0.0.1:8080/remote.php/dav/trashbin/dev/

# restore it to its original path (id from the PROPFIND above)
curl -u dev:dev -X MOVE http://127.0.0.1:8080/remote.php/dav/trashbin/dev/<trash-id>

# ...or purge it permanently instead of restoring it
curl -u dev:dev -X DELETE http://127.0.0.1:8080/remote.php/dav/trashbin/dev/<trash-id>
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
default count cap evicts old versions, MOVE carries history to the new path,
and a chunked-upload finalize overwrite versions the same as a plain PUT.
`breadcloud-storage`'s own unit tests cover `VersionStore` directly: eviction
under both the count and size caps, the "never evict the last version" rule,
and delete/move behavior including a directory delete cascading to every
descendant's history.

It also covers soft-delete trash (CLOUD-5) end to end: DELETE moves content
out of the served tree without erasing it from disk, the trashbin surface
lists a deleted item with its original location and deletion time, restoring
with no `Destination` returns it to that original path (and brings its
version history back with it), restoring onto an occupied original path is
412 unless `Overwrite: T` is given or a different `Destination` is named,
`DELETE` on a trashbin entry purges it early (and 404s on a second purge or a
restore attempt afterward), deleting a directory trashes and restores its
whole subtree in one piece, a MOVE of a live file never creates a trash
entry, and a tiny size cap evicts the older of two deletions once exceeded.
Two further tests force a genuine filesystem-level failure (permission
denied, injected by `chmod`ing the live versions root or the live served
root read-only mid-test) at each of restore's two internal moves in turn,
and assert the invariant this ordering exists for: after either forced
failure the live tree is untouched and the trash entry is still fully
restorable, and a naive retry with `Overwrite: T` (what the pre-fix 412
response used to recommend) fully succeeds and loses nothing, rather than
deleting a live file that was never actually restored.
`breadcloud-storage`'s own unit tests cover `TrashStore` directly: trash/list/
restore/purge roundtrips, a whole-directory trash and restore, the age cap
emptying the trash outright, and the size cap evicting oldest-first with no
"keep at least one" exception (unlike `VersionStore`'s cap).

Manually smoke-tested the running binary with curl against a disposable
`/tmp` data, versions, and trash root: a PUT then DELETE left the file
findable on disk under the trash root (not erased) and gone from the served
tree; the trashbin PROPFIND listed it with the right original path, size, and
a deletion time; MOVE-restoring it with no `Destination` returned it to
`/a.txt` with the original content and emptied the trashbin listing;
deleting it again and issuing `DELETE` on its new trashbin entry purged it
permanently, after which both a second purge and a restore attempt correctly
404'd; and a plain MOVE of an unrelated live file left the trashbin listing
at zero entries throughout.

Also re-verified the round-3 restore-ordering fix live, against three
genuinely distinct filesystems (`stat` confirmed distinct `st_dev`: served
root on btrfs under `/home`, versions root on tmpfs `/dev/shm`, trash root on
tmpfs `/tmp`), the same technique used to find the bug in the first place.
Forced a version-history-move failure by `chmod`ing the live versions root
read-only mid-restore: got a 500, the live tree stayed empty, and the trash
entry kept both its payload and its version history; a follow-up retry with
`Overwrite: T` then fully restored the file and its history and emptied the
trash entry. Separately, forced a payload-move failure (after a successful
version-history move) by `chmod`ing the live served root read-only: got a
500, the file never appeared on the live tree, and the version history that
had briefly moved out was confirmed rolled back into the trash entry; a
retry with `Overwrite: T` then fully restored the file and its history with
no data loss. All scratch roots and the test server were removed afterward.

**Round 4 (this fix): the `move_path` primitive redesign.** The round-3
bug (the rollback's own `move_in` call unconditionally clobbering a
destination that could genuinely hold the real, complete version history)
is fixed by the `move_path` redesign described above, not by another
narrow patch. Verified with `cargo test --workspace` (126 tests, up from
121: `breadcloud-storage`'s unit tests gained direct `move_path` coverage
and a `VersionStore` test that forces the forward move and then the
rollback to each fail in turn within one retry arc, `breadcloud-webdav`'s
existing restore-failure tests are unchanged and still pass against the
new primitive), `cargo clippy --workspace --all-targets -- -D warnings`,
and `cargo fmt --all -- --check`, all clean.

Live re-verification used the same three-distinct-filesystem rig as
before (served root on btrfs under `/home`, versions root on tmpfs
`/tmp`, trash root on tmpfs `/dev/shm`, `stat`-confirmed distinct
`st_dev`), against the real compiled binary, disposable directories only:

- A plain PUT/overwrite/overwrite/DELETE/restore roundtrip across all
  three filesystems, confirming payload and version history both still
  move correctly cross-device end to end.
- The round-1/2-style forced failure (versions root read-only, forward
  move fails): 500, live tree untouched, trash entry fully intact with
  both retained versions; retry after fixing permissions fully recovers.
- The round-2/3-style forced failure (served root read-only, payload move
  fails after the version-history move already succeeded, triggering the
  rollback): 500, live tree untouched, and critically the version history
  is confirmed rolled back into the trash entry rather than stranded under
  the live versions root; retry after fixing permissions fully recovers.

The scenario that actually reproduces the round-3 bug -- the rollback's
own move also failing, requiring a chmod injected between the forward
move succeeding and the rollback being attempted, both inside a single
restore call -- cannot be driven from outside the process through curl
alone (there is no pause point to inject the second permission change at
mid-request). That precise interleaving is exactly what the new
`VersionStore` test above exercises directly and repeatably (forward move
forced to fail and retried, then the rollback forced to fail and retried,
asserting the real multi-version history survives both), which is why
that scenario is covered there rather than in this manual pass. All
scratch roots and the test server were removed afterward.

**Round 5 (this fix): `move_path`'s own recovery, audited and made to fail
loudly.** The round-4 primitive closed the round 1-3 class of bug, but its
own step 2/3 error recovery had the same shape of problem one level down:
`restore_staged_src`'s rename-back (undoing step 1) and the step 3
backup-restore (undoing step 2) each discarded their own `fs::rename`
result via `let _ = ...` and returned the primary error regardless of
whether the undo worked, so a failed recovery was indistinguishable from
the safe "nothing moved" case while the content it was supposed to restore
was actually stranded. Per the review, every discarded filesystem-mutation
result in `breadcloud-storage` used for error recovery or cleanup was
audited, not just the two the reviewer pointed at directly:

- `restore_staged_src`'s rename-back (`lib.rs`, undoing step 1) and the step
  3 backup-restore (`lib.rs`, undoing step 2): both were load-bearing and
  both now propagate their result. A new helper, `unwind_after_failure`,
  runs both compensating actions and returns `StorageError::RecoveryFailed`
  (naming the exact `stranded_src`/`stranded_dest` path) instead of the
  primary error if either one fails, rather than folding a failed recovery
  into the same "safe to retry" `Err` a caller already knows how to handle.
- `VersionStore::move_all`'s `let _ = fs::remove_dir_all(&to_dir)` (an
  ordinary DAV `MOVE`'s destination-discard) was checked and left as a
  discarded result deliberately: this call site *wants* unconditional
  discard (the destination's old history genuinely belongs to content the
  move is removing), and a partial failure there surfaces on its own via
  the very next line's `fs::rename(&from_dir, &to_dir)` failing with a real
  I/O error (a non-empty leftover directory cannot be renamed over) rather
  than being silently swallowed -- now documented as such at the call site.
- The `let _ = fs::remove_file(...)` sites in `write_bytes`,
  `prepare_assemble`, and `commit_assemble` (`lib.rs`), and
  `PreparedAssembly::discard`, were all checked and are genuinely benign:
  each cleans up a temp file whose only content is data the caller already
  has in hand elsewhere (the `data` slice, or the chunk files), so a failed
  cleanup is disk litter, never data loss. Each now has a comment saying so.
- The best-effort cleanup in `handlers::uploads::finalize` (removing the
  chunked-upload staging directory after the assembled file is already live)
  was checked too: it runs strictly after the commit that matters has
  already succeeded, so it was already correctly documented and needed no
  change.
- `remove_any`'s own call sites inside `move_path` (post-success cleanup in
  step 4, and cleaning up a failed copy's partial output before step 1 even
  reaches step 2) were re-checked and are correctly best-effort: in every
  case the one real copy of the content in question is confirmed elsewhere
  before `remove_any` runs.

Verified with `cargo test --workspace` (129 tests, up from 126: three new
`breadcloud-storage` tests -- the reviewer's exact repro reproduced directly
against `restore_staged_src`, and two direct tests of `unwind_after_failure`
forcing the src-restore and the backup-restore to fail independently and
asserting `RecoveryFailed` is returned with the correct stranded path and
the content is still findable there -- plus the existing round-4
doubly-forced-failure `VersionStore` test, unchanged and still passing
against the stricter error handling), `cargo clippy --workspace --all-targets
-- -D warnings`, and `cargo fmt --all -- --check`, all clean.

As with round 4's own doubly-forced scenario, the precise interleaving this
bug lives in -- the primary move failing partway, *and* the compensating
rename that would undo it also failing, within one call -- cannot be driven
from outside the process through curl: `staging`, the destination's backup,
and the destination itself all share one parent directory, so any static,
pre-request permission change that blocks the primary step blocks the
recovery step identically (there is no window to inject a second, different
fault mid-call without instrumenting the code itself). This is exactly why
the reviewer's own reproduction was built as a direct call to
`restore_staged_src`, not a live HTTP repro, and why the three new tests
above are at that same level.

What live re-verification against the real compiled binary *can* and did
confirm, on disposable directories only:

- A single-disk deployment (served root, versions root, and trash root all
  on the same filesystem, `stat`-confirmed identical `st_dev` under `/tmp`)
  -- the deployment shape this round's bug specifically does not need
  cross-device conditions to hit -- put/put/delete/restore roundtrips
  correctly through the `staged_via_rename == true` path end to end, with no
  `.breadcloud-stage-*`/`.breadcloud-backup-*` litter left behind.
- The same forced-failure regression checks as round 4 (versions root
  read-only forcing the forward move to fail, served root read-only forcing
  the payload move to fail after the version-history move succeeded), rerun
  against both a three-distinct-filesystem rig and the single-disk rig
  above: each still returns a 500, leaves the live tree and trash entry in
  the same safe, fully-retryable state as before, and a retry after fixing
  permissions fully recovers with no data loss, confirming the round 1-4
  fixes are unchanged by this round's stricter error handling.

All scratch roots and the test server were removed afterward.

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
CLOUD-5: CalDAV, CardDAV, notes, multi-user auth + per-user quotas (replacing
the placeholder Basic auth here, and giving `fileid.rs` a real persistent id
to hand out instead of a path-derived one -- which would also let the
versions and trashbin DAV surfaces move to a real `<fileid>`-keyed layout
instead of the path-/opaque-id-keyed ones they use today), a minimal web UI,
and bread-ecosystem event integration. Each of those should be a new crate
under `crates/` that depends on `breadcloud-storage` and, where it needs
HTTP, nests alongside or reuses `breadcloud-webdav`'s router rather than
duplicating it.
