# Nextcloud to breadcloud migration staging (CLOUD-14)

This document defines a staged migration and the evidence needed before a
cutover. It does not run an export, import user data, change either service,
route traffic, or delete anything. The live Nextcloud remains authoritative
until every gate below has passed and cutover is separately approved.

## Scope and known compatibility limits

The migration set is the live files, three calendars, the contacts address
book, and file history requested by CLOUD-14. Nextcloud's backup is the
rollback source of truth. A WebDAV export is an interchange copy, not a full
Nextcloud backup: it does not carry the database, sharing state, app state, or
all file metadata. Nextcloud documents a consistent backup as including its
configuration, data, and database, with maintenance mode used to avoid
inconsistent data while taking it ([backup guide](https://docs.nextcloud.com/server/stable/admin_manual/maintenance/backup.html)).

The issue's initial inventory was 3.6 GB of live files, 76 contacts, about
300 calendar events, and 94 GB in old trash. These are planning estimates,
not the export manifest. Re-measure them during Phase 1.

Calendar and address-book payloads use interoperable iCalendar and vCard
objects. Nextcloud supports exporting calendars as `.ics` and address books
from Contacts settings ([Calendar guide](https://docs.nextcloud.com/server/latest/user_manual/en/groupware/calendar.html),
[Contacts guide](https://docs.nextcloud.com/server/latest/user_manual/en/groupware/contacts.html)).
Files can be enumerated and downloaded with authenticated WebDAV. Nextcloud
exposes prior file versions through its own file-id based versions endpoint
([versions guide](https://docs.nextcloud.com/server/stable/developer_manual/client_apis/WebDAV/versions.html)).

Breadcloud accepts ordinary WebDAV writes for current files, calendar objects,
and contacts. Its version history is path based, created by overwriting an
existing file, limited to ten versions and 500 MiB per file, and has no import
operation for original version timestamps. Therefore a complete, timestamp
preserving import of Nextcloud file history is not currently supported. Keep
the original Nextcloud backup and exported history intact. Do not claim full
history migration or cut over until the required history behavior is agreed
and verified. If CLOUD-14 means only the current file contents, record that
scope explicitly before final acceptance.

## Staging boundary

Use an isolated breadcloud instance with disposable credentials and separate
file, upload, version, trash, calendar, and address-book roots. Do not point the
staging instance at the live breadcloud roots or public hostname. Keep the
export in a private, encrypted staging location with access limited to the
operator. Do not put credentials, contact details, event bodies, or file names
in logs, tickets, or a public repository.

Size the staging filesystem for the complete source backup plus the export,
the imported files, and temporary copies. The live file total alone is not a
sufficient capacity estimate because Nextcloud also retains historical
versions and a large trash area. Check the disposable target account's quota
before import. Never raise the production quota as part of this plan.

## Phase 1: inventory and source capture

1. Record the exact Nextcloud and Calendar/Contacts app versions, source
   account, calendar names, address-book name, and an export timestamp. Record
   counts and total bytes without publishing personal names or content.
2. Check free space and permissions at the private staging location. Create a
   new empty run directory with owner-only access. Refuse to reuse an old run
   directory.
3. Before any final source snapshot, schedule a maintenance window with the
   user. Quiesce writes and background jobs for a consistent snapshot. The
   documented maintenance-mode step blocks user sessions, so it must not be
   enabled outside that approved window.
4. Capture a restorable Nextcloud backup containing its database, data,
   configuration, and any custom apps or themes. Store secrets in the protected
   backup, not in a command transcript. Record the backup checksum and test a
   restore in an isolated Nextcloud instance before relying on it.
5. Export current files recursively from the source account's WebDAV files
   collection. Preserve relative paths. For each item, record its relative
   path, size, modification time, and SHA-256 digest in a private manifest.
   Reject path traversal, duplicate normalized paths, and symlinks whose
   targets escape the export root. Retry interrupted downloads and verify the
   completed file against its recorded size and digest.
6. Export each of the three calendars as a separate `.ics` file and the
   contacts address book as `.vcf`. Keep each collection separate. Record the
   collection name, object count, and a digest for each export. If a collection
   export bundles multiple resources, split it into individual CardDAV contact
   objects and CalDAV calendar objects for import. Keep all components that
   share an event UID together so recurring overrides are not separated.
   Validate that every event/contact has a stable UID and that recurring rules,
   time zones, attendees, contact groups, and embedded attachments are
   represented before accepting the export.
7. Enumerate retained file versions through Nextcloud's documented versions
   WebDAV endpoint. Save the returned metadata and each version payload under
   a private per-file history directory, keyed by source relative path and
   source timestamp. Resolve the source file ID from DAV properties because
   Nextcloud addresses history by file ID, not by path. Verify byte counts and
   SHA-256 digests. This is an archival export only until breadcloud's history
   import limitation is resolved.
8. Produce a private summary with file count/bytes, calendar event counts,
   contact count, version count/bytes, failed paths, and duplicate UIDs. Do not
   include object names or values. Any failed or changing source enumeration
   invalidates this run and requires a fresh capture.

## Phase 2: disposable breadcloud import rehearsal

1. Start a throwaway breadcloud server against empty temporary roots and a
   disposable account. Bind only to loopback or an isolated test network. Keep
   the public service and its account untouched.
2. Create the three target calendar collections and the contacts collection
   in the disposable instance. Import each calendar object and each vCard
   object without flattening or rewriting UIDs. Keep recurrence exception
   components with their matching master event. Stop on a conflict rather than
   overwriting a different object.
3. Import current files under their original relative paths. Create parent
   collections first. Use conditional create requests where supported, and
   record every response. Do not delete an existing target object to make an
   import pass.
4. Re-download each staged file and compare its SHA-256 digest and size with
   the source manifest. Compare per-calendar event UIDs and per-address-book
   contact UIDs. Investigate all missing, extra, changed, or duplicate items.
5. Test representative items from each collection: Unicode names, long paths,
   recurring events, time-zone transitions, multi-value contacts, contact
   groups, large files, and files with multiple source versions. Record
   unsupported properties or transformations as explicit exceptions.
6. Verify restore behavior by restoring the throwaway Nextcloud backup to an
   isolated instance and checking its database, file tree, calendars,
   contacts, versions, and trash. A successful export or import alone is not a
   rollback test. Remove the disposable restored Nextcloud instance only after
   its evidence has been recorded.
7. Retain the private source backup and export unchanged after the rehearsal.
   Destroy only the disposable breadcloud roots after recording test evidence.

## Phase 3: cutover gate, not part of this PR

Cutover may be planned only after the import rehearsal, exact inventory
comparison, version-history decision, and restore test all pass. A later
approved change must define a single-writer interval, client reconfiguration,
DNS/Caddy change, monitoring, and a rollback window. Keep Nextcloud and its
backup available and unchanged during that window. Do not decommission its
container, purge its trash, remove its volumes, or delete its data until the
new service has passed the agreed observation period and the user approves
that separate cleanup step.

## Acceptance record

Attach a private evidence reference, not the data itself, for each item:

| Gate | Evidence required | Current status |
| --- | --- | --- |
| Source inventory | Counts and total bytes for files, three calendars, contacts, and versions | Not run |
| Restorable source backup | Isolated restore passed, backup checksum recorded | Not run |
| Export integrity | All downloads match size and SHA-256 manifest | Not run |
| Disposable import | All supported objects imported without collisions | Not run |
| Data comparison | File digests and calendar/contact UIDs match | Not run |
| Version history | Required history semantics agreed and verified on breadcloud | Blocked by missing timestamp-preserving import support |
| Rollback rehearsal | Restored Nextcloud is usable in isolation | Not run |
| Production cutover | Separately approved and observed | Not authorized by this staging plan |
| Decommission and trash cleanup | Separately approved after observation window | Not authorized by this staging plan |

No live source export, live breadcloud import, cutover, or decommission has
been performed for this PR.
