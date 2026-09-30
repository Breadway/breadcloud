# Android setup and acceptance (CLOUD-9)

Use DAVx5 for Calendar and Contacts, plus **Material Files through DAVx5's
WebDAV mount** for documents. DAVx5's [WebDAV mount documentation](https://manual.davx5.com/webdav_mounts.html)
lists Material Files as a compatible Storage Access Framework file manager.
This is the selected integration; physical-device acceptance remains pending.

## Calendar and Contacts

1. In DAVx5, add an account using URL and username, with base URL
   `https://breadcloud.breadway.dev/` and username `breadway`.
2. Enter the breadcloud account password privately. This is HTTP Basic over
   HTTPS, not Nextcloud Login Flow. A Nextcloud app password does not work here.
3. Grant Calendar and Contacts permissions. Select the discovered calendars
   and Contacts address book, then synchronize.
4. Save new test events/contacts to this account rather than a device-local
   account. Existing local items are not automatically moved.
5. Allow background synchronization according to Android's battery settings
   after confirming foreground sync succeeds.

The [DAVx5 account guide](https://manual.davx5.com/accounts_collections.html)
describes discovery through the root URL and well-known redirects. The
[first steps guide](https://manual.davx5.com/introduction.html) covers account
selection and permissions.

## Files

1. In DAVx5, open Tools / WebDAV mounts and add a mount named Breadcloud.
2. URL: `https://breadcloud.breadway.dev/remote.php/dav/files/breadway/`.
   Enter the same username/password privately. Keep certificate verification.
3. In Material Files, add external storage. In the system directory picker,
   choose the Breadcloud DAVx5 mount and grant access to that directory.
4. Open a small test document. Upload a new test file, refresh, download it,
   and compare its bytes. Verify a renamed file remains visible after refresh.

Some document editors request a read/write descriptor which DAVx5 mounts do
not support. Test the actual intended editor; download/edit/upload can be used
when direct editing is unavailable. This limitation is documented by DAVx5.

## Repeatable server preflight

From a checkout, run:

```sh
python3 tools/check_mobile_dav.py \
  --base-url https://breadcloud.breadway.dev --user breadway
```

The password prompt is private. By default this sends only GET, OPTIONS and
PROPFIND requests, validates successful DAV properties, both home sets,
collections and the file root. It refuses off-origin discovery URLs. Output
contains HTTP statuses and collection URLs, never passwords or personal object
bodies. Passing preflight checks server protocol behavior, not Android sync.

On an isolated disposable server/account, add `--isolated-write-test` to verify
create/read/update/delete plus content ETags for one event, contact and file.
Do not run this option on production: file deletion can leave trash/version
history, and cleanup after a connection failure may need manual inspection.

## Physical-device sign-off

Record device/Android/DAVx5/Material Files versions and the following results:

| Check | Required evidence |
| --- | --- |
| Discovery | Calendar and Contacts both appear in one DAVx5 account |
| Calendar | Create on phone, observe server/second client, update remotely, sync back, delete test item |
| Contacts | Create on phone, observe server/second client, update remotely, sync back, delete test item |
| Files | Upload, refresh, download with matching bytes, rename, remove test file |
| Background | Changes arrive after device sleeps and wakes |
| Network | Repeat discovery and one sync on Wi-Fi and mobile data |

Keep existing Nextcloud accounts intact during testing. Remove only explicitly
created test items. No real-device result is claimed by the protocol harness.

## Recorded protocol verification

On 2026-09-30, the read-only checker passed through the public HTTPS endpoint:
both well-known redirects, unauthenticated 401, authenticated principal and both
home sets, family/personal/school calendars, Contacts address book and file root.
On a separate loopback server built from main, create/read/update/delete and
content ETag checks passed for disposable event, contact and file fixtures.
The server and all fixture roots were removed after the check. Android device
setup and background synchronization are still awaiting physical-device review.
