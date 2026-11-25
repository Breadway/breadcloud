# breadcloud

A small, self-hosted Rust server that covers the parts of a Nextcloud
instance this deployment actually uses: file sync (WebDAV), calendars
(CalDAV), contacts (CardDAV), notes (the Nextcloud Notes OCS API), Login
Flow v2 account setup, and a minimal web UI for browsing files and a
calendar month. One process, one port, one HTTP Basic auth gate, and plain
files on disk: no database, no app store, no sharing subsystem.

It is sized for a family deployment, not as a general Nextcloud
replacement. See [Scope and deliberate gaps](#scope-and-deliberate-gaps)
for what is intentionally missing and [docs/DESIGN.md](docs/DESIGN.md) for
the per-crate rationale and verification history.

## What it does

| Capability | Surface | Notes |
|---|---|---|
| Files | WebDAV (RFC 4918) | PROPFIND (Depth 0/1), GET/HEAD, PUT, DELETE, MKCOL, MOVE, COPY, ETags, If-Match/If-None-Match preconditions, Nextcloud NG chunked uploads (resumable) |
| File versions | DAV `/remote.php/dav/versions/<user>/<path>` | Capped per-file history, snapshot on overwrite |
| Trash | DAV `/remote.php/dav/trashbin/<user>/` | Soft delete with age and total-size caps, restore, early purge; MOVE/COPY replacements retain displaced content |
| Calendars | CalDAV (RFC 4791) | Discovery, `MKCALENDAR`, `calendar-query`, `calendar-multiget`, `sync-collection` (RFC 6578), ctag |
| Contacts | CardDAV (RFC 6352) | One `contacts` addressbook per user by default, `KIND:group` group vCards, `addressbook-query`, `sync-collection` |
| Notes | Nextcloud Notes OCS API | JSON view over markdown files in `Notes/` |
| Account setup | Nextcloud Login Flow v2 | Browser-driven setup for the official desktop and Android apps |
| Web UI | `/ui/files/`, `/ui/calendar` | Server-rendered file browser and read-only calendar month, same Basic auth, cross-site POST checks |
| Accounts | Startup config | Independent credentials (argon2id hashes), quotas, and storage roots per account |
| Events | `bread.cloud.*` over `breadd` | Optional integration with the bread automation fabric, see [EVENTS.md](EVENTS.md) |

The server also answers the OCS endpoints real clients probe even when the
feature is unused (notifications, activity, direct editing, per-user
lookup, capabilities with sharing/notifications/activity explicitly off),
so clients get clean empty answers instead of 404s.

Clients exercised against a real server: `nextcloudcmd` (the desktop
client's libsync code), `vdirsyncer` for CalDAV and CardDAV, and `curl`
for every surface. DAVx5, the official Android app, and iOS are
implemented against their documented request shapes and checklist-tested
as far as this hardware allowed; see [docs/android.md](docs/android.md),
[docs/ipad.md](docs/ipad.md), and
[docs/DESIGN.md](docs/DESIGN.md#verification-and-testing) for exactly what
was and was not live-verified.

## Workspace layout

A Cargo workspace so the storage and HTTP layers can be reused by more than
one binary.

| Crate | Responsibility |
|---|---|
| `breadcloud-storage` | Filesystem-backed stores (`Storage`, `VersionStore`, `TrashStore`, `CalendarStore`, `AddressbookStore`), path sandboxing, streaming writes, content ETags |
| `breadcloud-webdav` | WebDAV HTTP layer: dispatch, PROPFIND, PUT/GET, chunked uploads, versioning, trash, discovery and OCS stubs |
| `breadcloud-caldav` | CalDAV HTTP layer: principal discovery, calendars, REPORT handling, minimal iCalendar parsing |
| `breadcloud-carddav` | CardDAV HTTP layer: addressbook discovery, contacts, groups, REPORT handling |
| `breadcloud-notes` | Nextcloud Notes OCS API as a JSON view over the `Notes/` tree |
| `breadcloud-login` | Login Flow v2: flow state, browser form, polling endpoint |
| `breadcloud-webui` | Server-rendered file browser and calendar month, shared stylesheet |
| `breadcloud-events` | Optional `bread.cloud.*` event publishing |
| `breadcloud-server` | Binary: loads config and accounts, wires every layer into one axum app on one port |

## Requirements and build

- Linux
- A current stable Rust toolchain (built with `cargo`)

```bash
cargo build --release -p breadcloud-server
./target/release/breadcloud-server
```

There is no database and no migration step. Every root directory below is
created on startup if missing.

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

CalDAV runs on the same server and port. The startup log lists the
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

# incremental sync (RFC 6578): an empty <d:sync-token/> means "initial sync,
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

CardDAV runs on the same server and port too. A single `contacts`
addressbook is bootstrapped automatically:

```bash
# discover addressbook-home-set from the principal (spliced onto the same
# response calendar-home-set already answers)
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

The web UI uses the browser's native Basic Auth prompt, so the same
credentials work everywhere:

```bash
open http://127.0.0.1:8080/ui/          # file browser landing page
open http://127.0.0.1:8080/ui/calendar  # this month's calendar view

# or drive it with curl
curl -u dev:dev http://127.0.0.1:8080/ui/files/
curl -u dev:dev -F dir=/ -F file=@example.txt http://127.0.0.1:8080/ui/upload
curl -u dev:dev "http://127.0.0.1:8080/ui/calendar?year=2026&month=3"
```

## Account setup (Login Flow v2)

The official Nextcloud desktop and Android apps default to Login Flow v2
instead of asking for an app password. With at least one configured
account, `POST /index.php/login/v2` starts a flow, the client opens the
returned browser URL, the browser form checks the same credentials as
Basic auth, and the client polls `POST /login/v2/poll` until it receives
the account password. The flow is single-use and expires after 20 minutes.

Two consequences worth knowing before relying on it:

- The credential handed back is the account's real password, not a
  separate, revocable per-device app password. Anyone who captures the
  poll response gets standing account access.
- In open/dev mode (no accounts configured) the endpoint returns 501,
  because there is no account for it to grant.

## Pointing a client at it

| Client | Server URL | Notes |
|---|---|---|
| Nextcloud desktop client, official Android app | `http://host:8080/` | Enter the server root; complete Login Flow v2 in the browser when prompted |
| `nextcloudcmd` | `http://host:8080/` | Server root, not the DAV path; it discovers `status.php` and `/remote.php/dav/files/<user>/` itself |
| `rclone`, `davfs2`, Cyberduck | `http://host:8080/` | Plain WebDAV root |
| DAVx5, iOS Calendar | `http://host:8080/` | Discovers `/remote.php/dav/` via `.well-known` and `current-user-principal` |
| DAVx5, iOS Contacts | `http://host:8080/` | Same discovery chain, addressbook-home-set on the principal |
| Nextcloud Notes app | `http://host:8080/` | OCS API under `/index.php/apps/notes/api/v1/`; the capabilities probe advertises `notes` |
| Browser | `http://host:8080/ui/files/` | Same Basic auth; `/` redirects here for browser GETs |

The server speaks plain HTTP. Terminate TLS in front of it (a reverse
proxy) if it is reachable beyond the LAN. Platform-specific walkthroughs:
[Android](docs/android.md), [iPad](docs/ipad.md), and the
[Nextcloud migration staging plan](docs/nextcloud-migration-staging.md).

## Configuration

All configuration is environment variables. Accounts are loaded in this
order:

1. **Multi-account**: `BREADCLOUD_ACCOUNT_<n>_USER`, `_PASS`, and
   `_QUOTA_GB` for `n = 1, 2, ...` until the first missing `_USER`. Every
   configured account needs all three. Passwords are hashed with argon2id
   at startup. Each account gets its own subdirectory under every root
   below.
2. **Legacy single account**: `BREADCLOUD_USER` and `BREADCLOUD_PASS`,
   used only when no `BREADCLOUD_ACCOUNT_1_USER` is set. Storage sits
   directly under the roots, with a 100 GiB quota.
3. **Open**: nothing set. Runs unauthenticated as an implicit `dev`
   account with no quota limit, and refuses to start unless
   `BREADCLOUD_BIND` is a loopback address. Use this only for local
   experiments.

| Variable | Default | Meaning |
|---|---|---|
| `BREADCLOUD_ROOT` | `./data` | Served file tree (created if missing) |
| `BREADCLOUD_ASSEMBLY_ROOT` | `./breadcloud-assembly` | Staging for assembled uploads; must be outside `BREADCLOUD_ROOT` and on the same filesystem, validated at startup |
| `BREADCLOUD_UPLOADS_ROOT` | `./breadcloud-uploads` | Staging for in-progress chunked uploads; never visible in PROPFIND |
| `BREADCLOUD_VERSIONS_ROOT` | `./breadcloud-versions` | Per-file version history, persisted across restarts |
| `BREADCLOUD_TRASH_ROOT` | `./breadcloud-trash` | Soft-delete trash, persisted across restarts |
| `BREADCLOUD_CALENDARS_ROOT` | `./breadcloud-calendars` | Calendar storage, persisted across restarts |
| `BREADCLOUD_ADDRESSBOOKS_ROOT` | `./breadcloud-addressbooks` | Addressbook storage, persisted across restarts |
| `BREADCLOUD_BIND` | `127.0.0.1:8080` | Listen address |
| `BREADCLOUD_ACCOUNT_<n>_USER` / `_PASS` / `_QUOTA_GB` | unset | Multi-account configuration, quota in whole GiB |
| `BREADCLOUD_USER` / `BREADCLOUD_PASS` | unset | Legacy single account |

Limits and retention are fixed by design (there is no policy engine):

- A plain WebDAV PUT and a web UI upload are capped at 20 GiB. Bodies
  stream to a staging file, so the cap bounds disk use, not process
  memory.
- A Notes attachment is capped at 200 MiB.
- Ten retained versions per file and 500 MiB per file's version set.
- Trash keeps entries for 30 days, bounded by 16 GiB total.

## Testing

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

The suite drives the real axum routers end to end with
`tower::ServiceExt::oneshot` (no sockets) and includes forced
filesystem-failure cases for version, trash, restore, and MOVE/COPY
replacement paths. See
[docs/DESIGN.md](docs/DESIGN.md#verification-and-testing) for the detailed
coverage and the live-client verification log.

## Scope and deliberate gaps

breadcloud is not trying to be all of Nextcloud. It deliberately has:

- no sharing subsystem, activity feed, notifications, app store, or admin
  UI. The OCS endpoints for some of these return empty stubs, and the
  capabilities response says they are off.
- no per-device app passwords: Login Flow v2 returns the account password
- no WebDAV locking (`LOCK`/`UNLOCK`, DAV class 2)
- no persistent file ids: `oc:fileid` is derived from the path, so it
  changes on rename. The versions, trashbin, and notes surfaces are keyed
  by path or an opaque id as a result.
- no self-service account creation, roles, or permissions
- no built-in TLS

## Design notes and project links

- [docs/DESIGN.md](docs/DESIGN.md): per-crate rationale, scope decisions,
  and verification history
- [EVENTS.md](EVENTS.md): `bread.cloud.*` event catalogue and the
  `breadd` integration
- [docs/android.md](docs/android.md), [docs/ipad.md](docs/ipad.md):
  platform setup and acceptance checklists
- [docs/nextcloud-migration-staging.md](docs/nextcloud-migration-staging.md):
  staged cutover from the real Nextcloud
- The CLOUD project on breadboard for the epic and open follow-ups
