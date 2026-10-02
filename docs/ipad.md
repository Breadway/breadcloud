# iPad setup and acceptance (CLOUD-10)

Calendar and Contacts can use native CalDAV/CardDAV accounts. Document access
through Apple's Files app requires a WebDAV file provider, such as FileBrowser.
The earlier suggestion to enter an HTTPS WebDAV URL in Files / Connect to
Server was incorrect. FileBrowser's [WebDAV guide](https://www.stratospherix.com/articles/how-to-connect-to-webdav-from-your-iphone-ipad.php)
documents this limitation and the provider setup. No SMB service is added by
breadcloud.

## Native Calendar and Contacts

Add separate CalDAV and CardDAV accounts in Settings using the account settings
for Calendar and Contacts. Depending on iPadOS version these may be under Apps.
Choose Other, server `breadcloud.breadway.dev`, username `breadway`, and the
breadcloud account password. Use SSL on port 443. For an explicit account URL,
use `https://breadcloud.breadway.dev/remote.php/dav/principals/breadway/`.
Keep existing Nextcloud accounts until validation finishes.

An optional profile generator makes these two account payloads without
embedding any password:

```sh
python3 tools/make_ipad_profile.py --output /tmp/breadcloud.mobileconfig
```

It creates a new file and refuses to overwrite an existing one. Review the
XML before transferring it to the iPad. It contains only Calendar and Contacts
accounts, HTTPS, username and principal URL, with no certificates, VPN,
restrictions, remote management or passwords. The profile is unsigned and
removable. Nothing installs automatically. Enter credentials privately on the
device if prompted. Alternatively add the accounts manually; importing a
profile is optional and has not been tested on a physical iPad.

The payload fields follow Apple's [CalDAV schema](https://github.com/apple/device-management/blob/release/mdm/profiles/com.apple.caldav.account.yaml)
and [CardDAV schema](https://github.com/apple/device-management/blob/release/mdm/profiles/com.apple.carddav.account.yaml).
Omitting password fields follows Apple's requirement to include passwords only
in encrypted profiles. Removing the profile removes these account settings,
not the server's original objects.

## Documents in Files

1. Configure a WebDAV location in FileBrowser or its trial variant with URL
   `https://breadcloud.breadway.dev/remote.php/dav/files/breadway/`.
   Enter the username and password in the app, keep certificate verification.
2. Confirm that location lists and opens a test document inside FileBrowser.
3. Enable its provider in Files / Browse / Edit, then open the Breadcloud
   location. The provider makes the WebDAV location available to Files.
4. Verify opening a document from another app's Files picker, then upload,
   refresh and download a temporary file to compare bytes.

See the provider's [Files integration guide](https://stratospherix.com/products/filebrowserprofessional/help/files-app-integration.php).
FileBrowser availability and behavior can differ by variant/version. Review
its current App Store terms before installing; no purchase or app installation
has been made as part of this issue.

## Acceptance record

| Check | Required physical-device evidence |
| --- | --- |
| Calendar | Select a breadcloud calendar, create test event, see it remotely, update remotely, refresh iPad |
| Contacts | Create test contact in the breadcloud address book, see it remotely, update remotely, refresh iPad |
| Files | Provider appears in Files, list/open/upload/download succeed with matching bytes |
| Persistence | Accounts/provider continue working after closing and reopening their apps |
| Cleanup | Remove only created test items and verify their deletion remotely |

Record iPad model/iPadOS/provider versions and dates. Do not mark CLOUD-10
complete until these checks pass. Public HTTPS server discovery was verified
on 2026-09-30 for both DAV home sets, three calendars, Contacts and files, but
this does not establish native iPad client behavior. Profile tests check XML
structure, payload types, SSL, unique UUIDs, password absence and input validation.
