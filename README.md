# breadcloud

A scoped Rust replacement for a self-hosted Nextcloud instance: file sync
(WebDAV), calendar (CalDAV), contacts (CardDAV), notes, multi-user auth, and a
minimal web UI, built as a series of breadboard issues (CLOUD-2 through
CLOUD-14). CLOUD-2 built the WebDAV core; CLOUD-3 made it actually work as a
real Nextcloud desktop client (mirall) sync backend; CLOUD-4 added capped
per-file version history; CLOUD-5 added soft-delete trash with an explicit
age and size retention cap; CLOUD-6 added a CalDAV server (calendar
collections, multi-calendar per user, sync-collection/ctag) that runs on the
same HTTP server as WebDAV rather than as a second process; CLOUD-7 added a
CardDAV server (a single addressbook, contact groups as vCard `KIND:group`
resources, sync-collection/ctag) on that same shared server; CLOUD-11
replaced the single placeholder account every prior issue ran against with a
real, small multi-user account model: independently authenticated,
independently quota'd, independently storage-isolated accounts, see "Multi-
user accounts (CLOUD-11)" under `breadcloud-server` below; CLOUD-12 added a
minimal server-rendered web UI (file browser + read-only calendar month
view) on that same combined server and the same account model, see "Web UI
(CLOUD-12)" under `breadcloud-server` below; CLOUD-8 added the Nextcloud
Notes OCS API (`breadcloud-notes`) as a JSON view over the same `Notes/`
markdown files WebDAV already served, for the official Notes mobile app;
CLOUD-28 added Login Flow v2 (`breadcloud-login`), the poll-based,
browser-driven account setup the official Nextcloud Desktop and Android
apps default to -- see "Login Flow v2 (CLOUD-28)" below.

Android setup, protocol preflight and the physical-device acceptance checklist
are documented in [docs/android.md](docs/android.md).

iPad account setup, optional password-free profiles and the document-provider
acceptance checklist are documented in [docs/ipad.md](docs/ipad.md).

## Workspace layout

A Cargo workspace, not a single binary, so later issues (the web UI) can add
crates that reuse the storage and HTTP layers without a rewrite.

```
crates/
  breadcloud-storage/   filesystem-backed storage primitives, no HTTP knowledge
  breadcloud-webdav/    WebDAV (RFC 4918) HTTP layer, built on axum
  breadcloud-caldav/    CalDAV (RFC 4791) HTTP layer, built on axum
  breadcloud-carddav/   CardDAV (RFC 6352) HTTP layer, built on axum
  breadcloud-webui/     minimal server-rendered web UI (file browser + calendar view), built on axum
  breadcloud-notes/     Nextcloud Notes OCS API (JSON view over the Notes/ file tree), built on axum
  breadcloud-login/     Login Flow v2 (browser-based account setup), built on axum
  breadcloud-server/    binary: wires storage + webdav + caldav + carddav + webui + notes + login into one server
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
restore uses to undo an already-succeeded version-history move. It also backs
`Storage::relocate` after COPY staging. `VersionStore::move_all` carries source
history after MOVE, while the replaced destination history has already been
retained with the displaced payload in trash. The plain snapshot-on-write path in
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

#### Calendar storage (`calendar` module, CLOUD-6)

`CalendarStore` is `breadcloud-storage`'s fourth root-sandboxed store,
alongside `Storage`/`VersionStore`/`TrashStore` -- but a genuinely different
shape than the other three, because a calendar event is not addressed by a
served-tree path the way a file is. Layout: one directory per `(user,
calendar id)` pair, holding an `events/` subdirectory (itself an ordinary
`Storage` root: one `.ics` file per event, named `<uid>.ics`) plus small
sidecar bookkeeping files (`display-name`, `color`, `ctag`, `changelog`) kept
*outside* `events/`, the same "reserved sibling, not a dotfile inside the
served tree" pattern the uploads/versions/trash roots already use, so a
listing of a calendar's events never sees its own bookkeeping.

Reusing `Storage` for `events/` rather than writing a second
content-addressed file store from scratch gets calendar objects `Storage`'s
existing atomic (temp-file-then-rename) writes and cached, content-derived
blake3 etags for free -- exactly what CalDAV's `getetag` needs.

`getctag` (the property every real CalDAV client polls to cheaply ask "did
anything in this calendar change") is a plain decimal counter file, bumped on
every event create/update/delete: checking it is an O(1) read, never a scan.
`sync-collection` (RFC 6578) replays an append-only `changelog` (one line per
change: `<seq>\t<uid>\t<C|D>`) since the sync-token a client presents, deduped
to each uid's *latest* entry in that range -- an event created then deleted
within one sync window collapses to "deleted", several updates collapse to
one "changed", which is exactly the "tell me only the current diff" contract
the REPORT needs.

A single `tokio::sync::Mutex` guards every mutating operation across the
*whole* store, not per-calendar. At this deployment's real scale (three
calendars, on the order of a few hundred events total, one interactive user)
that is not a contended bottleneck, and it sidesteps an entire class of
sequence-number races a per-calendar lock map would need careful design to
rule out (two events landing in the same calendar at once must never be
assigned the same ctag bump).

#### Addressbook storage (`addressbook` module, CLOUD-7)

`AddressbookStore` is `breadcloud-storage`'s fifth root-sandboxed store, and a
close structural mirror of `CalendarStore` (see that module's own doc
comment): one directory per `(user, addressbook id)` pair, a `contacts/`
subdirectory that is itself an ordinary `Storage` root (one `.vcf` file per
contact, named `<uid>.vcf`), and the same sidecar bookkeeping
(`display-name`, `ctag`, `changelog`) outside `contacts/`. `getctag` and
`sync-collection` work identically to the calendar case -- same O(1) ctag
read, same append-only changelog deduped to each uid's latest entry -- since
"a collection of UID-addressed opaque text objects with change tracking" is
exactly the same shape for a vCard as for an iCalendar object. The one
storage-layer difference from `CalendarInfo` is that `AddressbookInfo` has no
`color` field -- vCard/CardDAV has no equivalent of `calendar-color`.

**One addressbook is what this deployment actually needs.** The issue's real
data is 76 contacts split across groups (Friends/ICE/starred/Family team),
not across several independent addressbooks the way the calendar issue's
personal/family/school data genuinely was. `breadcloud-server` bootstraps a
single `contacts` addressbook per user. The store itself still supports more
than one per user (`create_addressbook`/`list_addressbooks` are not
hardcoded to one), the same way `CalendarStore` does, since that costs
almost nothing on top of the structural mirror this module already is, and
`MKCOL` (see `breadcloud-carddav`'s section below) needs somewhere to create
a second addressbook if a client or a human ever asks for one.

**Contact groups are not a separate storage concept.** The real data's four
groups map onto ordinary vCard resources with `KIND:group` and
`MEMBER:urn:uuid:<uid>` properties -- the standard CardDAV representation
both DAVx5 and iOS Contacts understand -- stored as just another `.vcf` file
in the same `contacts/` directory as the people it lists. This store has no
separate path, API, table, or index for "this one is a group"; see
`breadcloud-carddav::vcard`'s doc comment for the client-facing side of this
decision.

### breadcloud-webdav

Turns a `Storage` into an `axum::Router` implementing RFC 4918: `PROPFIND`,
`GET`/`HEAD`, `PUT`, `DELETE`, `MKCOL`, `MOVE`, `COPY`, `OPTIONS`. All non-GET
methods route through a single `fallback` handler (`dispatch` in `lib.rs`)
rather than axum's per-method routing, because `PROPFIND`/`MKCOL`/`MOVE`/`COPY`
aren't in axum's built-in `MethodFilter`.

Notable pieces:

- `PROPFIND` supports `Depth: 0` and `Depth: 1`. An explicit or implicit
  `Depth: infinity` is rejected with 403 and `DAV:propfind-finite-depth`.

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
  It also serves graceful stubs (CLOUD-29) for OCS endpoints clients probe
  even when the feature is unused, so they degrade cleanly instead of
  erroring on a 404: empty lists for `apps/notifications/api/v{1,2}/notifications`
  and `apps/activity/api/v2/activity`, an empty editor/creator list for
  `apps/files/api/v1/directEditing`, and `cloud/users/<id>` (own id: same as
  `cloud/user`, including quota; any other id: 403). The capabilities
  response says "off" explicitly for `files_sharing`, `notifications` and
  `activity`, and carries a minimal `theming` entry (product name only, no
  logo URLs). There is no real notification, activity, sharing or theming
  system behind any of this. The OCS envelope's `statuscode` is 100 on `v1.php`
  and 200 on `v2.php`, as in Nextcloud.
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
- `auth` is a single-credential HTTP Basic auth gate (or none at all if
  unconfigured), still used by this crate's own standalone router/tests.
  The real server's multi-user accounts and per-user quotas (CLOUD-11) are
  a `breadcloud-server`-level concern layered on top -- see that crate's
  section below and "Multi-user accounts (CLOUD-11)".
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
  - **MOVE** carries source history to the new path. An overwritten
    destination and its previous history are retained together in trash.
  - **COPY** does not duplicate source history. File-over-file COPY snapshots
    the previous destination content like PUT. Replaced collections and type
    changes retain the displaced resource and history in trash.
- MOVE/COPY validates the operation and fully stages COPY content before
  retaining or replacing the destination. Replacement uses `move_path`, so
  copy failure preserves the previous content. A failed replacement restores
  a destination that was retained in trash; failed recovery preserves the
  retained payload and reports an error. Retention caps run after replacement.
- `trash` coordinates DELETE across `Storage`, `VersionStore`, and `TrashStore`:
  the deleted resource and its history remain together until restored or purged.
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

### breadcloud-caldav (CLOUD-6)

Turns a `breadcloud_storage::CalendarStore` into an `axum::Router`
implementing the CalDAV surface real clients (DAVx5 on Android, iOS's native
Calendar account type) actually drive: principal / calendar-home-set
discovery, `PROPFIND` on calendar collections, `MKCALENDAR`,
`PUT`/`GET`/`DELETE` on individual `.ics` event resources, and `REPORT` with
`calendar-query`, `calendar-multiget`, and `sync-collection` (RFC 6578).

**What's reused from `breadcloud-webdav`, and what isn't.** CalDAV is
WebDAV-derived (RFC 4791 extends RFC 4918: still `PROPFIND`, still a
`multistatus` response, still HTTP Basic auth over the same kind of
collection/resource hierarchy), so this crate depends on `breadcloud-webdav`
and reuses:
- `breadcloud_webdav::auth::check_basic_auth` for the actual credential
  check (the base64/header-parsing logic itself, not the middleware wrapper
  around it -- each crate's own wrapper is generic over that crate's own
  `axum::State` type, so the wrapper can't literally be shared, only the
  check it calls);
- `breadcloud_webdav::xml::request::parse_propfind_body` for parsing a
  PROPFIND body's requested-property list -- namespace-agnostic local-name
  matching is exactly as applicable to a calendar property as a file one;
- `breadcloud_webdav::error::storage_error_response` for mapping a
  `StorageError` to an HTTP status, since both crates' storage layers mean
  the same thing by `NotFound`/`Conflict`/etc.

What it does *not* reuse: `ResourceEntry`/`build_multistatus` (built around a
file's `Metadata` -- kind, length, mtime -- and a fixed WebDAV/Nextcloud
property set; a principal, a calendar-home-set collection, a calendar
collection, and a calendar object each have their own small, genuinely
different property set instead). There is also no `REPORT` support in
`breadcloud-webdav` to share at all -- WebDAV as implemented there never
needed it, CalDAV cannot do without it. Calendar objects (`.ics` bytes) are
stored and served as opaque content; `ical.rs` reads just enough of that
content (a `UID`, a time range) to answer CalDAV queries, it is not a general
iCalendar library.

**One HTTP server, not two.** Per the issue's ask, this does not stand up a
second server process. `breadcloud-server`'s `combined.rs` wires both
protocols into one axum app on one port: a single top-level fallback asks
`breadcloud_caldav::owns_path` which protocol a request belongs to
(`/remote.php/dav/{principals,calendars}`, `/.well-known/caldav`, and the
bare DAV root are CalDAV's; everything else is WebDAV's -- the two never
overlap) and calls that crate's plain `dispatch(state, req)` function
directly, under one shared `middleware::from_fn` auth layer checking one
shared account store (CLOUD-11: `breadcloud_webdav::AccountStore`, see
"Multi-user accounts (CLOUD-11)" below -- originally a single
`BREADCLOUD_USER`/`BREADCLOUD_PASS` credential pair, before that issue).
Both crates still
expose their own standalone `router()` (used by each crate's own test
suite, and available to a caller that only wants one protocol), each with
its own independent auth layer -- `combined.rs` deliberately does not call
either, since composing them that way would check credentials twice per
request. Composing two routers that both only use a top-level `.fallback()`
does not work cleanly with axum's routing (`nest()`/`fallback_service()`
compose only when at least one side has concrete, non-fallback routes;
overriding one fallback-only router's fallback with the other just replaces
it), which is why the combination happens at the plain-function level in
`breadcloud-server` instead of by composing `Router`s.

**Calendar storage.** Built on `breadcloud_storage::CalendarStore`
(documented in that crate's own README section above) rather than a second
storage concept in this crate: calendars-as-collections-of-events is a
storage-layer decision, not an HTTP-layer one.

**Bare-root discovery.** `owns_path` deliberately does not claim the bare
server root (`/`, as opposed to `/remote.php/dav[/]`) -- that path is a
WebDAV resource (the file tree's own root collection), and WebDAV needs to
keep answering it normally for WebDAV-only clients. But a real CalDAV
client configured with nothing more than a server address (DAVx5's and
iOS's "Calendars" account type, the exact two clients this issue names)
PROPFINDs that bare root first, looking for `current-user-principal`, and
falls back to `.well-known/caldav` only when that request *errors* -- not
when it succeeds without the property. Since WebDAV's own PROPFIND happily
answers `/` with a valid (if `current-user-principal`-less) `207`, that
fallback never used to fire, and discovery dead-ended right there (found
live against `vdirsyncer` pointed at the bare root -- see "Real-client
verification" below). Fixed additively, entirely in
`breadcloud-server::combined`, without touching `breadcloud-webdav` at all:
after `combined::dispatch` falls a bare-root `PROPFIND` through to
`breadcloud_webdav::dispatch` as before, it splices one extra
`<d:current-user-principal>` property, pointing at this server's CalDAV
principal, into that same response before returning it. A WebDAV-only
client's `/` response is otherwise byte-for-byte what
`breadcloud-webdav`'s own (separately tested) PROPFIND handler produced --
this only adds a property, never removes or changes one, and
`breadcloud-webdav`'s own test suite is untouched by this fix.

**Scope decisions in `ical.rs`, both load-bearing for correctness, not just
style:**
- **Timezones are compared as wall-clock digit strings, not resolved
  against a real tz database.** `DTSTART;TZID=Australia/Sydney:20260115T090000`
  and a UTC `time-range` bound are compared after stripping the `TZID`
  parameter, not by actually resolving the zone and adjusting for
  offset/DST. Accurate enough for a personal deployment where every real
  calendar's events and every real client's query windows are in the same
  one or two timezones, and avoids a full tz-database dependency for a
  pre-filter whose job is "bound roughly this window" -- real clients
  (DAVx5, iOS) always re-filter and re-render server results against their
  own tz-aware logic anyway.
- **A recurring event (`RRULE` present) always passes a time-range filter**,
  rather than this module expanding the recurrence rule to find actual
  occurrence instances. Full RFC 5545 recurrence expansion
  (`FREQ`/`INTERVAL`/`BYDAY`/`COUNT`/`UNTIL`/exceptions) is substantially
  more machinery than this issue asks for, and getting it subtly wrong would
  silently drop a recurring event from a client's view of a queried window --
  a real correctness bug a user would not notice until a meeting they
  expected to see was missing. Always including it instead means a
  time-range query on a recurring event is a superset filter (may return
  more than the exact window), never a subset that silently loses data; RFC
  4791 9.9 explicitly allows a server to return a superset, and both DAVx5
  and iOS already do their own recurrence expansion client-side for display
  regardless of what the server pre-filtered.
- `xml/request.rs`'s REPORT body parser is a flat scan (first non-`VCALENDAR`
  `comp-filter`, first `time-range`, anywhere in the body), not a
  structurally-validating nested parser -- real clients always send the
  canonical `VCALENDAR > VEVENT|VTODO [> time-range]` shape RFC 4791's own
  examples use, so a flat scan finds the same element a structural parser
  would, with much less code. Similarly, this server does not implement RFC
  4791 9.7's full general filter grammar (property text-match filters,
  nested `AND`/`OR` combinations of multiple component filters) -- DAVx5 and
  iOS's Calendar app do not send it in practice.
- `xml/response.rs` always answers the full property set this server knows
  for a given resource kind, regardless of exactly which properties a
  PROPFIND's `<d:prop>` named (still honoring a bare `<d:propname/>` request
  -- names only, no values). Unlike `breadcloud-webdav`'s PROPFIND (which
  precisely honors an exact requested-property list, because a real
  Nextcloud desktop client's second sync pass depends on getting an exact
  404 for a handful of specific unsupported properties), neither DAVx5 nor
  iOS's Calendar app depends on selective per-property 404s here.

**Multi-calendar and discovery.** At least personal/family/school are
bootstrapped on server startup (`breadcloud-server`'s `ensure_calendar`
calls, idempotent across restarts); more can be created at any time via
`MKCALENDAR`. Discovery follows the real chain both DAVx5 and iOS's Calendar
account type walk: `.well-known/caldav` redirects to the DAV root, `PROPFIND`
there answers `current-user-principal`, `PROPFIND` on the principal resource
answers `calendar-home-set`, and `PROPFIND` (Depth 0 or 1) on the
calendar-home-set collection lists every calendar under it with `getctag`,
`calendar-color`, and `supported-calendar-component-set`.
`breadcloud-webdav`'s existing `handlers::discovery` (status.php, OCS
capabilities/user) is WebDAV/Nextcloud-client-specific and unrelated to this
chain -- CalDAV discovery needed its own properties (`current-user-principal`,
`calendar-home-set`) on its own resources (a principal, a calendar
collection), not an extension of the file-tree discovery endpoints. A client
that starts this chain at the bare server root rather than the `/remote.php/dav/`
CalDAV root gets `current-user-principal` there too -- see
`breadcloud-server`'s "Bare-root discovery" section below for why that needed
its own fix.

**A real client surfaced a real gap: PROPFIND Depth 1 on a calendar
collection must list its event resources, not just its own properties.**
The initial implementation only ever answered a calendar collection's own
properties on PROPFIND, on the assumption clients would always use `REPORT`
to enumerate events. Live-testing against `vdirsyncer` (see "Real-client
verification" below) found this wrong: vdirsyncer's CalDAV storage backend
uses a plain PROPFIND Depth 1 on the calendar collection itself -- the same
"list this collection's immediate members" idiom an ordinary WebDAV PROPFIND
Depth 1 uses -- to enumerate a calendar's objects, and saw every calendar as
permanently empty without it (`"Storage ... was completely emptied"`). Fixed
by having Depth 1 also render each child event resource (href + `getetag`,
`crate::xml::response::calendar_collection_multistatus`'s `children`
parameter) -- this server now supports enumeration via either Depth 1
PROPFIND or `REPORT`, since real clients are not consistent about which they
prefer.

### breadcloud-carddav (CLOUD-7)

Turns a `breadcloud_storage::AddressbookStore` into an `axum::Router`
implementing the CardDAV surface real clients (DAVx5 on Android, iOS's
native Contacts account type) actually drive: `PROPFIND` on addressbook
collections and addressbook-home-set discovery, `MKCOL` (with an addressbook
`resourcetype` -- CardDAV has no `MKCALENDAR`-equivalent verb of its own),
`PUT`/`GET`/`DELETE` on individual `.vcf` contact resources, and `REPORT`
with `addressbook-query`, `addressbook-multiget`, and `sync-collection` (RFC
6578).

**Structural mirror of `breadcloud-caldav`, deliberately, not a fresh
design.** CardDAV (RFC 6352) and CalDAV (RFC 4791) are both WebDAV
extensions for exactly the same shape -- a collection of typed, UID-addressed
text objects with sync-collection/ctag change tracking and a
PROPFIND/REPORT-based discovery and query surface -- so this crate's storage
pattern (`AddressbookStore`, mirroring `CalendarStore`), discovery pattern,
and REPORT handling are all directly carried over from `breadcloud-caldav`'s
CLOUD-6 implementation rather than redesigned from first principles. It
reuses concretely from `breadcloud-webdav` the same three things
`breadcloud-caldav` does (`check_basic_auth`, `parse_propfind_body`,
`storage_error_response`), plus `breadcloud_storage::calendar::ChangeEntry`
directly from `breadcloud-caldav`'s own storage module for its sync diff --
that type is just "a uid, and whether it was deleted", with nothing
calendar-specific about its shape despite living in the `calendar` module.
Everything else (path parsing, XML property sets, REPORT bodies, the
storage layout) is its own copy, structurally parallel but independently
owned, deliberately not factored into a shared generic
"collection-of-typed-objects" layer: iCalendar's time-range filtering and
`RRULE` handling have no vCard analog, and vCard's `KIND:group`/`MEMBER`
group model has no iCalendar analog, so a shared abstraction built from only
two data points would either save nothing or grow speculative hooks neither
format needs yet.

**Contact groups are ordinary vCards, not a protocol-level concept.** The
real data this issue targets (76 contacts split across "Friends"/"ICE"/
"starred"/"Family team" groups) is represented the standard CardDAV way, per
RFC 6350 sections 6.1.4 and 6.6.5: a group is a vCard like any other, with
`KIND:group` and one `MEMBER:urn:uuid:<uid>` line per member. Both DAVx5 and
iOS Contacts read and write groups in exactly this shape, so `PUT`/`GET`/
`DELETE`/`REPORT` treat a group vCard exactly like a person vCard -- the
addressbook storage layer has no notion of "kind" at all. `vcard.rs`'s
`extract_kind`/`has_kind` exist only so `addressbook-query`'s optional
`CARD:group` component filter can distinguish the two if a client ever sends
one.

**Does not own the principal resource or the bare DAV root.** Unlike
`breadcloud-caldav`, this crate has no `Root`/`Principal` path variant: there
is exactly one principal per user regardless of how many DAV extensions it
has home-sets for, and `breadcloud-caldav` already owns
`/remote.php/dav/principals`. `breadcloud-server::combined` splices
`card:addressbook-home-set` onto `breadcloud-caldav`'s existing principal
PROPFIND response, the same additive-splice technique already used to put
`current-user-principal` onto a bare-root WebDAV PROPFIND (see
`breadcloud-server`'s "Bare-root discovery" section above) -- discovering the
addressbook the same way DAVx5/iOS already discover a calendar, not
bypassing that hop.

**One HTTP server, not three.** Same reasoning and same mechanism as
`breadcloud-caldav`'s own section above: `breadcloud-server::combined` calls
this crate's plain `dispatch` alongside `breadcloud_webdav::dispatch` and
`breadcloud_caldav::dispatch`, from one shared top-level fallback under one
shared auth layer, rather than standing up a third HTTP server.

**One addressbook is what this deployment actually needs.** See
`breadcloud-storage`'s "Addressbook storage" section above -- a single
`contacts` addressbook is bootstrapped per user, with groups living inside
it rather than as separate addressbooks, since that is what the real 76-
contact/four-group data actually looks like. The store and `MKCOL` both
still support creating more than one addressbook if ever needed.

**MKCOL, not MKCALENDAR-equivalent.** RFC 6352 does not define its own
collection-creation verb the way RFC 4791 defines `MKCALENDAR`: an
addressbook is created with a plain `MKCOL`, optionally carrying an RFC 5689
extended-MKCOL body that also sets `resourcetype` (to include
`<card:addressbook/>`) and `displayname`. This server does not require or
validate the extended body -- every `MKCOL` under
`/remote.php/dav/addressbooks/<user>/` unambiguously means "create an
addressbook" regardless of body shape, since nothing else lives under that
path -- it just reads `displayname` out of the body if present, falling back
to the addressbook id if the body is absent or has none.

### breadcloud-webui (CLOUD-12)

A minimal server-rendered web UI: a file browser and a read-only calendar
month view, covering the "open Firefox and look at my files/calendar"
access pattern the issue names -- explicitly not a Nextcloud-style
dashboard, activity feed, or notification center.

**No new dependency-heavy toolchain.** Pages are plain `format!`-built HTML
strings (see `template.rs`), not a templating crate (askama or similar) or a
JS frontend framework/build step -- the whole UI is a handful of small
pages, genuinely minimal per the issue's own framing, and this workspace has
no `serde`, no JS build step, and no date/time crate anywhere else either
(see `breadcloud-webui::datetime`'s own doc comment for the small
dependency-free civil-calendar math that keeps it that way). A sprinkle of
vanilla JS (`confirm()` on delete) is the only client-side script.

**Same auth gate, not a second login system.** There is no session/cookie
login here: a browser hitting `/ui/*` unauthenticated gets the same 401 with
`WWW-Authenticate: Basic` every DAV client already gets, and the browser's
native Basic Auth prompt handles the rest -- consistent with how every DAV
client already authenticates against this server, and correctly scoped for
a single-family home-server deployment. `breadcloud-server::combined`
resolves the authenticated account once, centrally, exactly as it already
does for WebDAV/CalDAV/CardDAV (CLOUD-11); `breadcloud-webui::dispatch`
receives that identity directly rather than checking credentials a second
time. There is deliberately no `<user>` segment anywhere in the `/ui/*` URL
space (unlike `/remote.php/dav/files/<user>/...`) for a request to name a
different account than the one that authenticated -- every page always
operates against the caller's own account, so CLOUD-11's `url_scoped_user`
check has nothing to validate in this URL space at all.

**File browser** (`/ui/files/`, and `/ui/files/<path>` to navigate into a
subdirectory): list, navigate, download (`/ui/download/<path>`, which calls
straight into `breadcloud_webdav::handlers::get::handle` -- the same
etag/content-type/streaming logic real DAV downloads already use), upload
(`/ui/upload`, a multipart POST; goes through the same
`versioning::snapshot_before_overwrite` + `Storage::quota_gate` +
`Storage::write_bytes` sequence the plain WebDAV PUT handler uses), create a
folder (`/ui/mkdir`, calling `breadcloud_webdav::handlers::mkcol::handle`
directly), and delete (`/ui/delete`, calling
`breadcloud_webdav::handlers::delete::handle` directly -- so a UI delete
soft-deletes into trash exactly like a DAV client's DELETE already does,
CLOUD-5's trash surface is not a UI-only concept). **Trash/version browsing
through the UI is deliberately not included**: a delete already lands in
trash (nothing is destructively lost -- recoverable via a DAV client or
`handlers::trashbin` directly), and the issue frames this as "just a file
browser," so a second browsing surface for CLOUD-4/CLOUD-5's existing DAV
surfaces would be scope beyond what "minimal" asks for.

**Calendar view** (`/ui/calendar?year=&month=`, defaulting to the current
month): a month grid across every one of the account's calendars
(personal/family/school), reusing `breadcloud_caldav::ical::summarize` (a
small CLOUD-12 addition to that module) for the same "wall-clock digits, not
tz-aware" `DTSTART` reading the CalDAV `time-range` filter already relies
on, rather than a second iCalendar parser. **Read-only**: creating/editing
events is left to a real CalDAV client (DAVx5, iOS Calendar, any desktop
client already pointed at this server per CLOUD-6), which already has to
exist for this deployment to have events at all -- a write path here would
mean either a second, form-shaped entry point into
`breadcloud-caldav`'s PUT logic or a second event-authoring implementation,
more surface than a "minimal" calendar *view* needs.

**Styling and a root redirect (CLOUD-24).** Direct user feedback after
using the live deployment: the bare server root showed nothing useful (no
WebDAV directory listing is expected there, but a person navigating in a
browser reasonably expects the web UI to be the front door), and the UI
itself was unstyled browser-default form controls. Two independent fixes:

- **Root redirect.** `breadcloud-server::combined::dispatch` now redirects
  a plain browser GET at `/` (302, `Location: /ui/files/`) to the file
  browser, ahead of the existing PROPFIND-at-root splice. "Plain browser
  GET" means exactly: method GET, and an `Accept` header naming
  `text/html` -- the same shape every real browser navigation sends and
  no WebDAV/CalDAV/CardDAV client this codebase has been tested against
  (rclone, DAVx5, vdirsyncer, Cyberduck) sends at `/`. PROPFIND, OPTIONS,
  PUT, and every other verb WebDAV already answers at the bare root are
  untouched (different method, so the new check never even reaches its
  `Accept` check), and a non-browser GET (curl's default `Accept: */*`,
  or no `Accept` at all) keeps getting `breadcloud_webdav`'s own bare
  directory-listing fallback exactly as before. See
  `combined::is_browser_root_navigation` and its test coverage.
- **Real CSS.** One shared, dark-themed stylesheet (`breadcloud-webui/src/
  style.css`, embedded via `include_str!`) served at `/ui/style.css`
  (`template::style_response`) and linked from every page with a plain
  `<link rel="stylesheet">`, replacing the old per-response inline
  `<style>` block -- still no build step, no CSS/JS framework, no
  light/dark toggle (this is a personal home-server tool for a family,
  not a product). Covers real typography and spacing, styled buttons and
  inputs (including the native file input's `::file-selector-button`,
  which is the only way to style an upload control's Browse button
  without JavaScript), a colored quota bar that picks up a `warn`/
  `critical` class at the same 80/90/100 percent-of-quota bands
  `breadcloud-events`'s `crossed_threshold` (CLOUD-13) already warns on
  (presentation only -- no event is emitted by rendering a page), small
  inline-SVG folder/file icons in the file listing
  (`template::FOLDER_ICON`/`FILE_ICON`), a styled nav bar and
  breadcrumbs, and a bordered calendar month grid that highlights today's
  cell. The XSS-escaping discipline the CLOUD-12 review enforced
  (`html_escape` on every user-controlled value interpolated into an
  `href` or text node) is unchanged and still covered by its own
  regression tests -- this issue only added static, trusted markup (SVG
  icons, CSS classes), never a new site for user-controlled data to reach
  the page unescaped.

### breadcloud-notes (CLOUD-8)

The Nextcloud Notes OCS API: `/index.php/apps/notes/api/v1/{notes,settings,attachment}`,
for the official Nextcloud Notes mobile app (confirmed in use for this
deployment; see the CLOUD-8 card's investigation comment) rather than the
plain WebDAV `Notes/` folder access every desktop client already gets for
free with zero notes-specific code. The endpoint contract (routes, JSON
field names, ETag/`If-Match` semantics, the `.attachments.<id>/` folder
convention) is read directly off `nextcloud/notes`'s own source
(`appinfo/routes.php`, `NotesApiController.php`, `Helper.php`, `Note.php`,
`NoteUtil.php`, `Capabilities.php`), not guessed.

**A JSON view over the same files, not a second store.** A note *is* a
markdown file under an account's `Notes/` tree, addressed by a
deterministically-derived id (`breadcloud_webdav::fileid::for_path`, the
same scheme CLOUD-3 already uses for `OC-FileId`) instead of a path. Every
write goes through the same `Storage::write_bytes` /
`versioning::snapshot_before_overwrite` / `Storage::quota_gate` /
`breadcloud_events::file_synced` sequence a plain WebDAV `PUT` runs (the
same pattern `breadcloud-webui`'s own `files::upload` already established
for a different wire format), and delete goes through
`breadcloud_webdav::handlers::delete::handle` directly -- so a note
created or edited through this API gets version history (CLOUD-4), goes
through trash on delete (CLOUD-5), is quota-checked (CLOUD-11), and is
immediately visible and editable through the file browser (CLOUD-12) or
any WebDAV/desktop-sync client, with no divergent internal
representation. Renaming a note (a `title`/`category` change) moves the
underlying file and carries its version history along, the same two-step
sequence `MOVE` runs; the one known gap this carries over from
`breadcloud_webdav::fileid`'s own documented limitation is that a rename
also changes the note's derived id, where real Nextcloud's database-row
id would not.

**Known, deliberate gaps**, called out here rather than left silent:
`favorite`/`readonly` are accepted on create/update (matching the real
request shape) but never persisted -- real Nextcloud stores `favorite` as
a tag external to the file and `readonly` from filesystem permissions,
neither of which this server has a per-file equivalent of, and adding a
sidecar-metadata mechanism for one cosmetic flag was judged more scope
than the issue asks for. `internalPath`/`shareTypes`/`isShared` (real
Nextcloud's sharing metadata) are omitted entirely -- breadcloud has no
sharing subsystem. Chunked/paginated listing (`chunkSize`/`chunkCursor`)
and `pruneBefore` are not implemented; `category` filtering, `exclude`
field-dropping, and the collection-level `If-None-Match` "has anything
changed" shortcut are.

**Wired in the same way as CalDAV/CardDAV/the web UI**: a plain
`owns_path`/`dispatch` pair `breadcloud-server::combined` routes to from
its one shared fallback, against the authenticated account's own already-
resolved `breadcloud_webdav::AppState` (CLOUD-11) -- there is no `<user>`
segment in this crate's URL space for a request to spoof. The OCS
capabilities probe a real Notes client makes before calling its API at
all (`GET /ocs/v1.php/cloud/capabilities`) now advertises a `notes` key
(`breadcloud-webdav`'s `discovery.rs`), matching real Nextcloud's own
`Capabilities.php` shape (`api_version`, `version`, `notes_path`).

### breadcloud-login (CLOUD-28)

Nextcloud Login Flow v2: the poll-based, browser-driven account setup the
official Nextcloud Desktop and Android apps default to for initial account
setup. Without it, those clients cannot complete setup at all (CLOUD-9
flagged exactly this for the official Android app). The real protocol
(routes, JSON field names, status codes, the 1200-second/20-minute token
lifetime, single-use-via-deletion) was read directly off `nextcloud/server`'s
own source (`core/Controller/ClientFlowLoginV2Controller.php`,
`core/Service/LoginFlowV2Service.php`, `core/Db/LoginFlowV2Mapper.php`), not
reconstructed from memory, the same discipline CLOUD-6/7/8 each applied to
their own protocols -- see `breadcloud-login`'s own crate-level doc comment
for the exact routes and source references.

**Four routes**, matching real Nextcloud's own path shapes so the official
clients work unmodified: `POST /index.php/login/v2` (start a flow, no auth),
`POST /login/v2/poll` (client polling, 404 while pending), `GET`/`POST
/login/v2/flow/{token}` (the browser-facing login form and its submission).
Wired into `breadcloud-server::combined` the same `owns_path`/`dispatch`
pattern every other protocol crate uses, except this one runs *ahead of*
CLOUD-11's shared Basic-auth gate rather than behind it -- `combined::auth_gate`
carries an explicit exemption for these four paths, the same "first request,
before the client has anything to send" reason `status.php` and
`.well-known/*` are already exempted for.

**Integrates with CLOUD-11's account model.** The browser-facing form's
submission is checked via `breadcloud_webdav::AccountStore::authenticate` --
the same username/password check `authenticate_request` (Basic auth) now
also funnels through -- rather than a parallel credential path.

**Security tradeoffs, documented rather than hidden:**

- **The `appPassword` this flow hands back is the account's real password.**
  Per the issue's own scoping note, breadcloud does not implement a separate,
  per-device app-password mechanism (a second random credential,
  independently revocable, that never equals the login password). This means
  anyone who intercepts a poll response, or compromises the client that
  requested it, gets full, standing access to the real account -- the same
  blast radius as if the user had typed their password directly into that
  client, not the narrower, independently-revocable blast radius a real app
  password would give. Accepted deliberately, matching breadcloud's existing
  "keep it simple" stance elsewhere (no roles, no sharing, no per-device
  session tracking).
- **Tokens** (the login token embedded in the browser URL, the poll token,
  and the CSRF token) are each 256 bits of `OsRng` output, stored hashed
  (BLAKE3) rather than in the clear, and expire after the same 1200-second
  window real Nextcloud uses, checked lazily on every access.
- **Single-use, enforced by deletion**: a granted flow is removed from the
  store the instant a `poll` call successfully returns it, so a second poll
  with the same token cannot succeed even if attempted concurrently with the
  first.
- **Resource-exhaustion bound**: `POST /index.php/login/v2` takes no
  credentials, so `breadcloud-login::flow::FlowStore` refuses to create a new
  flow once 100 (`MAX_PENDING_FLOWS`) are already outstanding, after reaping
  expired ones -- a bound on outstanding server-side state, not on request
  rate, but a bound all the same.
- **CSRF**: the browser form carries a fixed, per-flow token that must match
  on submission (a classic synchronizer-token check), and every page this
  crate renders sends `X-Frame-Options: DENY` /
  `Content-Security-Policy: frame-ancestors 'none'` against clickjacking.
  This does not and cannot defend against a user being phished into opening
  an *attacker-initiated* flow's legitimate login URL and typing their real
  password into it -- that risk is inherent to login-flow-v2 itself
  (identical in real Nextcloud) and is not a server-side-fixable gap.
- **Standalone page styling**: the login form does not link
  `breadcloud-webui`'s `/ui/style.css` (CLOUD-24), since that route sits
  behind the same CLOUD-11 Basic-auth gate this page exists to get a client
  past in the first place -- an unauthenticated browser fetching it would hit
  a spurious auth prompt before ever seeing the login form. It carries a
  small inline stylesheet that mirrors the same dark palette instead.
- **Login flow v2 requires at least one configured account**: in open/dev
  mode (no `BREADCLOUD_ACCOUNT_<n>_*` set at all), `POST /index.php/login/v2`
  returns `501 Not Implemented` -- there is no real account for the flow to
  grant access to.

### breadcloud-server

Thin binary. Configuration is environment variables:

| Variable | Default | Meaning |
|---|---|---|
| `BREADCLOUD_ROOT` | `./data` | Directory to serve (created if missing). |
| `BREADCLOUD_ASSEMBLY_ROOT` | `./breadcloud-assembly` | Reserved assembly staging outside the served root. Must share its filesystem for atomic commits; startup validates access and rejects overlapping roots. Multi-account mode uses per-account subdirectories. |
| `BREADCLOUD_UPLOADS_ROOT` | `./breadcloud-uploads` | Staging area for in-progress chunked uploads (created if missing). Deliberately outside `BREADCLOUD_ROOT`, not just dot-hidden inside it, so it can never appear in a PROPFIND of the served tree. The sibling-directory default avoids staging multi-GB uploads in a system temp directory that may be a tmpfs or subject to periodic cleanup |
| `BREADCLOUD_VERSIONS_ROOT` | `./breadcloud-versions` | Per-file version history root (created if missing), outside `BREADCLOUD_ROOT` for the same reason as uploads staging. Unlike uploads staging this needs to persist across restarts, so it defaults to a sibling directory rather than the system temp dir |
| `BREADCLOUD_TRASH_ROOT` | `./breadcloud-trash` | Soft-delete trash root (created if missing), outside `BREADCLOUD_ROOT` for the same reason as uploads staging and version history, and (like version history) needs to persist across restarts, so it also defaults to a sibling directory rather than the system temp dir |
| `BREADCLOUD_CALENDARS_ROOT` | `./breadcloud-calendars` | Calendar storage root (created if missing), outside `BREADCLOUD_ROOT` for the same reason as uploads/versions/trash, and (like versions/trash) needs to persist across restarts, so it also defaults to a sibling directory rather than the system temp dir |
| `BREADCLOUD_ADDRESSBOOKS_ROOT` | `./breadcloud-addressbooks` | Addressbook storage root (created if missing), outside `BREADCLOUD_ROOT` for the same reason as uploads/versions/trash/calendars, and (like them) needs to persist across restarts, so it also defaults to a sibling directory rather than the system temp dir |
| `BREADCLOUD_BIND` | `127.0.0.1:8080` | Address to listen on |
| `BREADCLOUD_ACCOUNT_<n>_USER` / `BREADCLOUD_ACCOUNT_<n>_PASS` / `BREADCLOUD_ACCOUNT_<n>_QUOTA_GB` | unset | Multi-user accounts (CLOUD-11), `<n>` = `1`, `2`, ... See "Multi-user accounts (CLOUD-11)" below |
| `BREADCLOUD_USER` / `BREADCLOUD_PASS` | unset | Legacy single-account fallback, used only when no `BREADCLOUD_ACCOUNT_1_USER` is set. See "Multi-user accounts (CLOUD-11)" below |

### Multi-user accounts (CLOUD-11)

Before CLOUD-11 this server had exactly one, placeholder account: a single
`BREADCLOUD_USER`/`BREADCLOUD_PASS` pair checked by a shared HTTP Basic auth
gate, with every store (`Storage`, `VersionStore`, `TrashStore`) rooted once
for the whole server. The URL already carried a `<user>` path segment
everywhere a real Nextcloud client puts one
(`/remote.php/dav/files/<user>/...`, `.../calendars/<user>/...`, and so on --
CLOUD-3's real-client-compatibility work), but nothing checked it against
anything: it was accepted as a label, not an identity, which was harmless
only because there was ever one real account to begin with.

CLOUD-11 replaces that with a real, intentionally small account model: a
fixed set of independently authenticated, independently quota'd,
independently storage-isolated accounts, configured once at startup and never
mutated while running. Per the issue's own ask, this stays simple on
purpose -- no roles or permissions, no sharing between accounts, no
self-service account creation, no admin UI.

**Configuration.** `breadcloud-server` tries three shapes, in order (see
`main.rs`'s `load_accounts`):

1. **Multi-account** -- `BREADCLOUD_ACCOUNT_<n>_USER` /
   `BREADCLOUD_ACCOUNT_<n>_PASS` / `BREADCLOUD_ACCOUNT_<n>_QUOTA_GB`, scanned
   from `n = 1` until the first missing `_USER`. This is the real shape: e.g.
   five accounts (matching the issue's "5 existing accounts and their quotas
   (100/100/50/10/100GB)", using generic `user1`..`user5` identifiers since
   the issue names no real usernames and none turned up elsewhere in this
   codebase's real-data context) would be `BREADCLOUD_ACCOUNT_1_USER=user1`
   / `..._PASS=...` / `..._QUOTA_GB=100`, `BREADCLOUD_ACCOUNT_2_...` for the
   second 100GB account, `BREADCLOUD_ACCOUNT_3_QUOTA_GB=50`,
   `BREADCLOUD_ACCOUNT_4_QUOTA_GB=10`, `BREADCLOUD_ACCOUNT_5_QUOTA_GB=100`.
   Quota is in whole GiB (binary gibibytes, matching this codebase's other
   size constants like `DEFAULT_TRASH_MAX_TOTAL_BYTES`), not decimal GB --
   close enough to the issue's "100/100/50/10/100GB" figures that the
   difference does not matter at this scale, and consistent with everything
   else in this codebase. Each account's file/upload/version/trash storage
   gets its own subdirectory of the configured base roots
   (`BREADCLOUD_ROOT/<username>`, etc.).
2. **Legacy single-account** -- the pre-CLOUD-11 `BREADCLOUD_USER`/
   `BREADCLOUD_PASS` pair, used only when no `BREADCLOUD_ACCOUNT_1_USER` is
   set. Keeps this README's own "Running locally" quickstart, and any
   existing single-user deployment's on-disk layout (data directly under
   `BREADCLOUD_ROOT`, not a per-user subdirectory of it), working with zero
   migration -- exactly the "only one account is actually active today"
   deployment the issue describes. Gets a default 100 GiB quota (the most
   common tier among the five-account mapping above).
3. **Open** -- neither of the above set at all: the server runs
   unauthenticated, exactly as it always could, as a single implicit `"dev"`
   account with no quota limit.

**Per-user storage isolation.** `Storage`, `VersionStore`, and `TrashStore`
are each root-sandboxed via `DavPath` (no `..`, cannot escape the root by
construction -- see `breadcloud-storage`'s own section above). CLOUD-11
gives each account its own instance of each, rooted at its own subdirectory,
rather than one shared instance namespaced by a URL segment: this means one
account's requests are *structurally* incapable of reaching another's files,
version history, or trash, regardless of what any request claims, the same
rigor CLOUD-2/4/5 already had to get right for path traversal within one
account's tree. `CalendarStore`/`AddressbookStore` (CLOUD-6/CLOUD-7) do not
need this treatment: they already namespace every operation by a `user`
argument internally (`<root>/<user>/<calendar-or-addressbook-id>/`,
validated the same way a file path is), so a single shared instance per
store, fed the *validated* authenticated username, is already real
per-account isolation.

**Authenticated identity, not a URL label.** `breadcloud-server::combined`'s
`auth_gate` checks `Authorization` against the account store exactly once
per request and resolves an authenticated username; `dispatch` then rejects
outright (403) any request whose URL names a *different* account than the
one that actually authenticated (`url_scoped_user`, covering every
`<user>`-carrying prefix across all three protocols), instead of silently
substituting one for the other. This closes the exact gap CLOUD-11 set out
to close: before it, `/remote.php/dav/files/<user>/...` accepted `<user>` as
a matching label, not a real per-request identity check.

**Quota.** Enforced on write (`PUT`, and chunked-upload finalize) via
`Storage::quota_gate`, which wraps the read-only `Storage::check_quota`
check in this account's own async mutex, held from the check through to
the write it gates (`write_bytes`/`commit_assemble`). A rejected write
(507 Insufficient Storage, the RFC 4918-correct status for "well-formed
request, no room") never touches disk -- no partial write, no corrupted
existing content -- and the mutex closes the check-then-write TOCTOU
window a bare `check_quota` call would otherwise leave open: two
concurrent writes against the same account can never both observe "room
available" for overlapping capacity. Different accounts never contend on
this, each gets its own `Storage` instance and its own mutex. Scoped to
the live served tree only, deliberately not also counting each account's
version history or trash: those already have their own independent,
already-bounded retention caps (CLOUD-4/CLOUD-5), so folding them into
quota too would be double-accounting against a number that is not what a
user thinks of as "my files" anyway.
`quota-used-bytes`/`quota-available-bytes` (already present per CLOUD-2)
report this same per-account figure on PROPFIND now, not a filesystem-wide
one.

A chunked upload's finalize assembles its chunks into a temp file before
`quota_gate` can know the exact size to check (`Storage::prepare_assemble`).
That temp file is staged in a reserved directory that sits beside the
account's storage root rather than inside it, specifically so the
account's own quota-usage tally (a recursive walk of the served tree)
never counts an in-flight assembly as already-on-disk usage on top of
counting it again as the incoming write -- a real bug in an earlier round
of this feature that silently rejected large chunked uploads at roughly
half their real allowed size. See `Storage::prepare_assemble`'s doc
comment in `breadcloud-storage` for the full reasoning, including why
that staging directory is not simply the existing chunked-upload staging
area (`BREADCLOUD_UPLOADS_ROOT`).

`BREADCLOUD_USER` (or `dev` in open mode) also names the owner the baseline
personal/family/school calendars and the default `contacts` addressbook are
bootstrapped under on startup in single-account/open mode; in multi-account
mode, every configured account gets its own personal/family/school
calendars and its own `contacts` addressbook -- see `breadcloud-caldav`'s
and `breadcloud-carddav`'s sections above.

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

CalDAV (CLOUD-6) runs on the same server and port. The startup log lists the
calendars root; `personal`/`family`/`school` are bootstrapped automatically:

```bash
# discover the current-user-principal
curl -u dev:dev -X PROPFIND -H "Depth: 0" http://127.0.0.1:8080/remote.php/dav/

# discover calendar-home-set from the principal
curl -u dev:dev -X PROPFIND -H "Depth: 0" http://127.0.0.1:8080/remote.php/dav/principals/dev/

# list every calendar (Depth 1) with getctag/color/supported components
curl -u dev:dev -X PROPFIND -H "Depth: 1" http://127.0.0.1:8080/remote.php/dav/calendars/dev/

# create an event
curl -u dev:dev -X PUT --data-binary @- \
  http://127.0.0.1:8080/remote.php/dav/calendars/dev/personal/example-1.ics <<'EOF'
BEGIN:VCALENDAR
VERSION:2.0
BEGIN:VEVENT
UID:example-1
DTSTART:20260401T090000Z
DTEND:20260401T100000Z
SUMMARY:Example event
END:VEVENT
END:VCALENDAR
EOF

# list a calendar's events directly (Depth 1 PROPFIND on the calendar itself)
curl -u dev:dev -X PROPFIND -H "Depth: 1" \
  http://127.0.0.1:8080/remote.php/dav/calendars/dev/personal/

# ...or fetch matching events via REPORT calendar-query (a time-range filter)
curl -u dev:dev -X REPORT --data '<?xml version="1.0"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT">
    <c:time-range start="20260301T000000Z" end="20260501T000000Z"/>
  </c:comp-filter></c:comp-filter></c:filter>
</c:calendar-query>' http://127.0.0.1:8080/remote.php/dav/calendars/dev/personal/

# incremental sync (RFC 6578) -- an empty <d:sync-token/> means "initial sync,
# give me everything and a fresh token"; the response's own <d:sync-token>
# is what a client presents next time to get only what changed since
curl -u dev:dev -X REPORT --data '<?xml version="1.0"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token/><d:prop><d:getetag/></d:prop>
</d:sync-collection>' http://127.0.0.1:8080/remote.php/dav/calendars/dev/personal/

# delete it
curl -u dev:dev -X DELETE \
  http://127.0.0.1:8080/remote.php/dav/calendars/dev/personal/example-1.ics
```

Or point a real CalDAV client at the server: DAVx5 (Android) and iOS's native
"Calendars" account type both take just the bare server address
(`http://127.0.0.1:8080/`, the way both are normally configured -- neither
client makes you type a DAV path) plus the same Basic-auth credentials and
discover the calendars automatically, via `current-user-principal` answered
right at that bare root (see "Bare-root discovery" above for why this needed
its own fix beyond the basic discovery chain). The full `/remote.php/dav/`
URL works too, and both are covered in "Real-client verification" below.

CardDAV (CLOUD-7) runs on the same server and port too. The startup log
lists the addressbooks root; a single `contacts` addressbook is bootstrapped
automatically:

```bash
# discover addressbook-home-set from the principal (spliced onto the same
# response calendar-home-set already answers -- see breadcloud-carddav's
# section above)
curl -u dev:dev -X PROPFIND -H "Depth: 0" http://127.0.0.1:8080/remote.php/dav/principals/dev/

# list the addressbook (Depth 1) with getctag
curl -u dev:dev -X PROPFIND -H "Depth: 1" http://127.0.0.1:8080/remote.php/dav/addressbooks/dev/

# create a contact
curl -u dev:dev -X PUT --data-binary @- \
  http://127.0.0.1:8080/remote.php/dav/addressbooks/dev/contacts/example-1.vcf <<'EOF'
BEGIN:VCARD
VERSION:4.0
UID:example-1
FN:Ada Lovelace
EMAIL:ada@example.com
END:VCARD
EOF

# create a group that references it (KIND:group + MEMBER, the standard
# CardDAV representation DAVx5 and iOS Contacts both understand)
curl -u dev:dev -X PUT --data-binary @- \
  http://127.0.0.1:8080/remote.php/dav/addressbooks/dev/contacts/friends.vcf <<'EOF'
BEGIN:VCARD
VERSION:4.0
UID:friends
FN:Friends
KIND:group
MEMBER:urn:uuid:example-1
END:VCARD
EOF

# list an addressbook's contacts directly (Depth 1 PROPFIND on the addressbook itself)
curl -u dev:dev -X PROPFIND -H "Depth: 1" \
  http://127.0.0.1:8080/remote.php/dav/addressbooks/dev/contacts/

# ...or fetch matching contacts via REPORT addressbook-query (a property text-match)
curl -u dev:dev -X REPORT --data '<?xml version="1.0"?>
<c:addressbook-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:prop><d:getetag/><c:address-data/></d:prop>
  <c:filter><c:prop-filter name="FN"><c:text-match>Ada</c:text-match></c:prop-filter></c:filter>
</c:addressbook-query>' http://127.0.0.1:8080/remote.php/dav/addressbooks/dev/contacts/

# incremental sync (RFC 6578), identical shape to CalDAV's above
curl -u dev:dev -X REPORT --data '<?xml version="1.0"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token/><d:prop><d:getetag/></d:prop>
</d:sync-collection>' http://127.0.0.1:8080/remote.php/dav/addressbooks/dev/contacts/

# delete it
curl -u dev:dev -X DELETE \
  http://127.0.0.1:8080/remote.php/dav/addressbooks/dev/contacts/example-1.vcf
```

Or point a real CardDAV client at the server: DAVx5 (Android) and iOS's
native Contacts account type both take just the bare server address plus the
same Basic-auth credentials and discover the addressbook automatically, via
the same `current-user-principal` -> principal -> `addressbook-home-set`
chain CalDAV discovery uses. Covered in "Real-client verification" below.

```bash
# minimal web UI (CLOUD-12): open in a browser, or exercise it directly --
# the browser's native Basic Auth prompt is the login, same credentials as
# every DAV client above.
open http://127.0.0.1:8080/ui/          # file browser landing page
open http://127.0.0.1:8080/ui/calendar  # this month's calendar view

# or drive it with curl, same as any other endpoint on this server
curl -u dev:dev http://127.0.0.1:8080/ui/files/
curl -u dev:dev -F dir=/ -F file=@example.txt http://127.0.0.1:8080/ui/upload
curl -u dev:dev "http://127.0.0.1:8080/ui/calendar?year=2026&month=3"
```

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

### CalDAV coverage (CLOUD-6)

`breadcloud-caldav`'s own unit tests (`ical.rs`, `path.rs`, `xml/request.rs`,
`xml/response.rs`) cover: UID extraction and line-unfolding, component-type
detection, time-range overlap including the half-open-interval boundary
cases, all-day (`VALUE=DATE`) events with no explicit `DTEND`, `DURATION`-based
end times, and recurring (`RRULE`) events always matching; CalDAV path
parsing for every resource shape and its rejections; REPORT body parsing for
all three shapes (`calendar-multiget`, `calendar-query` with and without a
time-range, `sync-collection` with and without a token) plus an unrecognized
REPORT type; MKCALENDAR body parsing; and multistatus response building for
every resource kind, including `PropfindRequest::PropName` (names only).

`tests/caldav_test.rs` drives the real `axum::Router` end to end via
`tower::ServiceExt::oneshot`, simulating the request sequence a real client
actually issues: `current-user-principal` discovery, principal
`calendar-home-set` discovery, calendar-home-set listing at Depth 0 and 1,
MKCALENDAR (including rejecting a duplicate id with 405), PUT/GET/DELETE on
events (including a missing-UID body rejected with 400 and a PUT into a
nonexistent calendar rejected with 409), `getctag` bumping on every event
create/update/delete, `REPORT calendar-query` filtering by time-range,
`REPORT calendar-multiget` fetching only the explicit hrefs given, `REPORT
sync-collection` doing a correct initial sync (live resources only, no
tombstones) followed by a correct incremental sync (only what changed,
deletions reported as 404, a new sync-token), Basic auth rejecting missing
and wrong credentials, `.well-known/caldav`'s redirect, `OPTIONS`
advertising the `calendar-access` DAV class, a Depth 1 PROPFIND on a
calendar collection listing its event children (the real-client-discovered
regression described in `breadcloud-caldav`'s own README section above), and
a `full_discovery_to_sync_sequence` test that chains discovery -> MKCALENDAR
-> multiple PUTs -> `calendar-query` -> `sync-collection` in one sequence,
mirroring a real client's actual flow end to end.

`breadcloud-storage`'s own unit tests cover `CalendarStore` directly:
create/list/get roundtrips, duplicate-id rejection, idempotent bootstrap
(`ensure_calendar`), calendar-id traversal rejection, `events_storage`
requiring an existing calendar, ctag bumping on every recorded change, and
`changes_since` both deduping to each uid's latest state and correctly
filtering to only entries newer than a given token.

### Real-client verification (CLOUD-6)

DAVx5 and iOS were not available on this machine, so this was verified
against `vdirsyncer` (already installed on this machine; a real, widely-used
CalDAV/CardDAV sync client, not a purpose-built test harness), pointed at a
disposable local server instance with disposable local storage -- never at
any real calendar data. Covered end to end, with `vdirsyncer` pointed at the
**bare server root** (`http://127.0.0.1:8099/`, not `/remote.php/dav/` --
the address DAVx5's and iOS's "just enter a server address" setup actually
uses, and the exact scenario a change-request review round found broken,
see "Bare-root discovery" above): `vdirsyncer discover` finding all three
calendars (`personal`/`family`/`school`) purely through this server's
`current-user-principal` -> `calendar-home-set` -> calendar-listing
discovery chain starting from that bare address, with no manual DAV path
needed; a local `.ics` file uploaded to the server via `vdirsyncer sync` and
confirmed present via a server-side `PROPFIND` Depth 1; an event created
directly on the server via `curl PUT` and correctly downloaded to the local
side on the next sync; `vdirsyncer` correctly recognizing identical content
on both sides as "nothing to do" (a false-conflict check) rather than
re-uploading; and a local deletion propagating to the server as a real
`DELETE` that a subsequent `GET` then correctly 404s.

This live pass is what caught the Depth 1 PROPFIND gap documented in
`breadcloud-caldav`'s own README section: the first implementation passed
every test in this crate's own suite (which, being written by the same
implementation, shared its blind spot) but failed immediately against
`vdirsyncer`, which showed every calendar as permanently empty. Fixed, then
re-verified against `vdirsyncer` again through a full
discover/sync/upload/download/delete cycle. A later change-request review
round caught the separate bare-root gap described above (discovery worked
against `/remote.php/dav/` but not the bare address DAVx5/iOS actually use);
fixed, then the entire discover/upload/no-op/download/delete cycle above was
re-run pointed at the bare root specifically, which is the version reflected
above.

Separately smoke-tested at moderate scale (50 events PUT into one calendar
via a loop of plain `curl` requests, matching the order of magnitude the
issue's real data implies -- 171/106/22 events across three calendars): a
Depth 1 PROPFIND listing all 50 plus the collection itself completed in
single-digit milliseconds, and `getctag` correctly read back as `50` after
50 recorded changes. This did not use real personal calendar data --
synthetic events only, on disposable scratch directories removed afterward
-- since importing the real 171/106/22-event calendars into a running
instance was not something this task was asked to do and touching real
personal calendar content without being asked would be scope creep beyond
implementing and verifying the protocol.

**Not live-verified**: DAVx5 (Android) and iOS's native Calendar account
type specifically -- neither device/emulator was available on this machine.
Both are implemented against the documented RFC 4791/6578 protocol and the
specific request shapes those two clients are known to send (the
`comp-filter`/`time-range` shape, `sync-collection` with/without a token,
Depth 1 PROPFIND-based enumeration alongside REPORT-based enumeration), and
`vdirsyncer`'s own CalDAV storage backend exercises a materially similar
protocol surface, but a real DAVx5/iOS pass has not happened. Flagged here
explicitly per the issue's own ask to be clear about what was and wasn't
live-verified.

### CardDAV coverage (CLOUD-7)

`breadcloud-carddav`'s own unit tests (`vcard.rs`, `path.rs`,
`xml/request.rs`, `xml/response.rs`) cover: UID extraction and
line-unfolding, `KIND:group` detection, property text-match (case-insensitive
substring, including the "property absent" and "empty needle always
matches" edge cases), CardDAV path parsing for every resource shape and its
rejections, REPORT body parsing for all three shapes
(`addressbook-multiget`, `addressbook-query` with and without a
`prop-filter`/`text-match`, `sync-collection` with and without a token) plus
an unrecognized REPORT type, extended-MKCOL body parsing for `displayname`,
and multistatus response building for every resource kind.

`tests/carddav_test.rs` drives the real `axum::Router` end to end via
`tower::ServiceExt::oneshot`, simulating the request sequence a real client
actually issues: addressbook-home-set listing at Depth 0 and 1, MKCOL
(including rejecting a duplicate id with 405), PUT/GET/DELETE on contacts
(including a missing-UID body rejected with 400 and a PUT into a
nonexistent addressbook rejected with 409), a group vCard (`KIND:group` +
`MEMBER`) round-tripping through PUT/GET exactly like a person vCard,
`getctag` bumping on every contact create/update/delete, `REPORT
addressbook-query` filtering by property text-match, `REPORT
addressbook-multiget` fetching only the explicit hrefs given, `REPORT
sync-collection` doing a correct initial sync (live resources only, no
tombstones) followed by a correct incremental sync (only what changed,
deletions reported as 404, a new sync-token), Basic auth rejecting missing
and wrong credentials, `.well-known/carddav`'s redirect, `OPTIONS`
advertising the `addressbook` DAV class, a Depth 1 PROPFIND on an
addressbook collection listing its contact children, and a
`full_discovery_to_sync_sequence` test chaining
MKCOL -> multiple PUTs (including a group) -> `addressbook-query` ->
`sync-collection` in one sequence, mirroring a real client's actual flow end
to end.

`breadcloud-storage`'s own unit tests cover `AddressbookStore` directly:
create/list/get roundtrips, duplicate-id rejection, idempotent bootstrap
(`ensure_addressbook`), addressbook-id traversal rejection,
`contacts_storage` requiring an existing addressbook, ctag bumping on every
recorded change, and `changes_since` both deduping to each uid's latest
state and correctly filtering to only entries newer than a given token.

`breadcloud-server`'s `combined.rs` test suite additionally covers the
principal-splice mechanism itself: `card:addressbook-home-set` appearing
alongside (not replacing) `cal:calendar-home-set` on the principal PROPFIND
response, that splice not leaking onto a non-principal CalDAV PROPFIND,
CardDAV requests dispatching correctly under the shared auth gate, and
`.well-known/carddav`'s auth exemption.

### Real-client verification (CLOUD-7)

DAVx5 and iOS were not available on this machine, so as with CLOUD-6 this
was verified against `vdirsyncer`, pointed at a disposable local server
instance with disposable local storage -- never at any real contact data.
Covered end to end, with `vdirsyncer` pointed at the **bare server root**
(`http://127.0.0.1:8099/`, the same address DAVx5's/iOS's "just enter a
server address" setup actually uses): `vdirsyncer discover` finding the
`contacts` addressbook purely through this server's
`current-user-principal` -> principal (`addressbook-home-set`) ->
addressbook-listing discovery chain starting from that bare address, with no
manual DAV path needed and no code changes required to make it work (unlike
CLOUD-6, which needed two live-verification-driven fixes -- the
addressbook-home-set splice and the Depth 1 PROPFIND enumeration were both
already in place going into this pass, having been built as direct mirrors
of the CLOUD-6 code those fixes landed in); two synthetic vCards (one
person, one `KIND:group` group referencing it) created directly on the
server via `curl PUT` and correctly downloaded to the local side on
`vdirsyncer sync`; a new synthetic contact created locally and correctly
uploaded to the server on the next sync, confirmed present via a
server-side `GET`; `vdirsyncer` correctly recognizing identical content on
both sides as "nothing to do" (a false-conflict check) rather than
re-uploading or re-downloading; and a local deletion propagating to the
server as a real `DELETE` that a subsequent `GET` then correctly 404s.

Also manually verified via `curl` against that same disposable instance,
independent of vdirsyncer's own sync logic: the full discovery chain
(bare-root `current-user-principal`, principal PROPFIND carrying both
`cal:calendar-home-set` and `card:addressbook-home-set` side by side,
addressbook-home-set Depth 1 listing the bootstrapped `contacts`
addressbook with `getctag`); PUT/GET round-tripping both a person vCard and
a `KIND:group`/`MEMBER` group vCard; Depth 1 PROPFIND on the addressbook
correctly listing both as children with `getctag` bumped to `2`; and
`REPORT addressbook-query` with an `FN` `text-match` filter correctly
returning only the matching contact's `address-data`.

This did not use real contact data -- synthetic vCards only, on disposable
scratch directories removed afterward -- since importing the real 76-contact
address book with its Friends/ICE/starred/Family team groups into a running
instance was not something this task was asked to do, and touching real
personal contact content without being asked would be scope creep beyond
implementing and verifying the protocol.

### Real-client verification (CLOUD-12)

No browser automation was available, so this was verified with a real
running `breadcloud-server` instance (a disposable local instance, two
synthetic multi-account users, never real data) driven with `curl`, exactly
as a browser's requests would look on the wire:

- Unauthenticated `GET /ui/` returns `401` with the same `WWW-Authenticate:
  Basic` challenge every DAV endpoint already returns.
- A multipart upload (`POST /ui/upload`) lands real content on the
  account's real `Storage` root: fetchable afterward both through the
  plain WebDAV `GET` path and through `/ui/download/<path>`, byte for
  byte.
- `POST /ui/mkdir` creates a real collection (`MKCOL` under the hood);
  the new folder appears in the `/ui/files/` listing and can be navigated
  into (`/ui/files/<folder>`), which then correctly lists the file
  uploaded into it.
- `POST /ui/delete` soft-deletes (confirmed via the real
  `/remote.php/dav/trashbin/<user>/` PROPFIND surface showing the deleted
  item, not destructively removed) and the item disappears from
  `/ui/files/`.
- Two independent accounts (`alice`, `bob`, `BREADCLOUD_ACCOUNT_<n>_*`):
  bob's `/ui/files/` never shows alice's uploads, and bob cannot browse
  alice's folder by guessing its path (`404`, since there is no `<user>`
  segment in this URL space to spoof in the first place -- see
  `breadcloud-webui`'s own section above).
- A real event PUT via CalDAV (`/remote.php/dav/calendars/alice/personal/
  <uid>.ics`) appears in `/ui/calendar?year=&month=` for its actual month,
  correctly does not appear in the adjacent month, the default (no query
  params) view renders the current month without error, the prev/next nav
  links point at the correct adjacent year/month, and bob's calendar view
  never shows alice's event.

Real DAVx5/iOS browser verification of the rendered pages themselves was not
available in this environment; the `curl`-driven checks above confirm the
served HTML/redirect/status-code behavior for every real scenario the issue
names (list files, navigate a subdirectory, view a calendar month with real
events in it) against the real data model (real per-user storage roots,
real calendars, real multi-account auth), which is what this issue asks to
be verified against. The full `breadcloud-server` integration test suite
(`combined.rs`'s `ui_*` tests) covers the same surface automatically,
including the quota-rejection and cross-account isolation paths.

**Not live-verified**: DAVx5 (Android) and iOS's native Contacts account
type specifically -- neither device/emulator was available on this machine.
Both are implemented against the documented RFC 6352/6578 protocol and the
specific request shapes those two clients are known to send (the same
`prop-filter`/`text-match` and `sync-collection` shapes CLOUD-6 already
verified those clients use for CalDAV, plus the standard `KIND:group`/
`MEMBER` group representation both clients are documented to read and
write), and `vdirsyncer`'s own CardDAV storage backend exercises a
materially similar protocol surface, but a real DAVx5/iOS pass has not
happened. Flagged here explicitly per the issue's own ask to be clear about
what was and wasn't live-verified.

## bread ecosystem integration

`crates/breadcloud-events` (CLOUD-13) publishes `bread.cloud.*` events into
the bread automation fabric, following the `AdapterSource::App` registry /
command-as-event convention documented in the `bread` repo's
`Documentation.md` ("Namespaces" and "Integrating a bread\* app") and worked
out as breadclip's reference implementation (see its own `EVENTS.md`).
breadcloud registers app id `cloud`; see this repo's own `EVENTS.md` for the
full event catalogue.

This is explicitly optional: breadcloud behaves identically whether or not
`breadd` is installed or running. Every publish goes through `BreadClient`'s
already fail-silent `emit` (no socket, no reply, no error, just dropped) and
is additionally dispatched onto a blocking-thread-pool task rather than run
inline on the request-handling async task, so a slow or wedged `breadd` can
never add latency to the WebDAV request that triggered the event, and a
failure to notify it never fails that request.

Three events, each tied to already-implemented server behavior: a completed
write or removal (`bread.cloud.file.synced`, covering plain PUT,
chunked-upload finalize, DELETE, and MOVE -- not COPY, which produces an
independent resource with no prior state to "sync"), a rejected
`If-Match`/`If-None-Match` precondition (`bread.cloud.sync.conflict`, the
exact case a real sync client turns into a "conflicted copy" instead of
silently overwriting or losing a change, CLOUD-3's conflict detection), and
usage crossing an 80/90/100 percent-of-quota threshold on a write
(`bread.cloud.quota.warning`, CLOUD-11's per-account quota).

**Live-verified**: the wire protocol against the real `breadd` running on
this machine (`bread ping`, and a raw socket write reproducing exactly what
`BreadClient::emit` sends) -- confirmed the request/response shape matches,
and confirmed `breadd`'s registered-app-id gate (`bread_shared::apps::is_known_app`)
rejects an unregistered `cloud` source with a plain error string that
`BreadClient::emit` never even reads, i.e. a rejection at that layer is
already invisible to breadcloud today, before `cloud` is even registered.
**Not live-verified**: `cloud` accepted end to end and observed via
`bread events`/`bread.on("bread.cloud.**", ...)` -- `cloud` is not yet in
`bread`'s `KNOWN_APPS`; registering it is a companion one-line change to the
`bread` repo (see `Documentation.md`'s integration checklist, step 1), opened
as `Breadway/bread` PR #5 (`feature/register-breadcloud-app`), not yet
merged. Once that merges and a `breadd` build that includes it is installed,
these events flow through unchanged; nothing on breadcloud's side is gated
on it landing.

## What's next in the epic

See the CLOUD project on breadboard for the full epic. CLOUD-11 (multi-user
auth + per-user quotas), CLOUD-12 (the minimal web UI), CLOUD-13
(bread-ecosystem event integration, above), and CLOUD-8 (the Notes OCS API,
above) are all done, per their own sections above. An open question CLOUD-8
leaves for a future issue: giving `fileid.rs` a real persistent id to hand
out instead of a path-derived one (which would also let the versions,
trashbin, and notes surfaces move to a real `<fileid>`-keyed layout instead
of the path-/opaque-id-keyed ones they use today, and would let a note's id
survive a rename the way real Nextcloud's does). The rest of the epic is
intentionally held back for the user to pick up.
