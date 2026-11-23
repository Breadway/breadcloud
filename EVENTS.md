# breadcloud: bread event integration

breadcloud is a standalone WebDAV/CalDAV/CardDAV server: it works exactly
the same with or without `breadd` running. When breadd *is* present,
breadcloud publishes events into the shared bread automation fabric. See the
parent `bread` repo's `Documentation.md` (specifically its "Namespaces" and
"Integrating a bread\* app" sections) for the general convention this
follows, and breadclip's own `EVENTS.md` for the worked reference
implementation.

App id: **`cloud`**. Transport: `bread-utils`'s `bread_client` module
(feature `bread-client`): breadcloud is a long-running server (unlike
breadclip's per-event CLI invocations), so every emit call is dispatched
onto Tokio's blocking thread pool rather than run inline on the request
handling task -- see `crates/breadcloud-events/src/lib.rs`'s module docs for
why.

## Events published (`bread.cloud.*`)

| Event | Data | When |
|-------|------|------|
| `bread.cloud.file.synced` | `{ "owner": <username or null>, "path": "<dav path>", "op": "put" \| "chunked_upload" \| "delete" \| "move", "bytes": <u64 or null> }` | A plain PUT, a chunked-upload (NG chunking v2) finalize, a DELETE (soft-deleted into trash), or a MOVE completes. `bytes` is the resulting resource's size where already known for free (PUT, chunked finalize); omitted where it isn't (a directory delete or move, which would need an extra recursive walk this path hasn't otherwise paid for). COPY is deliberately excluded: it produces an independent new resource, not a sync of existing content. |
| `bread.cloud.sync.conflict` | `{ "owner": <username or null>, "path": "<dav path>", "reason": "if-match" \| "if-none-match" }` | An `If-Match` or `If-None-Match` precondition (RFC 7232) rejects a PUT or chunked-upload finalize with 412 (CLOUD-3's conflict detection): exactly the case a real sync client (mirall/nextcloudcmd) surfaces to the user as a "conflicted copy" instead of silently overwriting or losing a change. |
| `bread.cloud.quota.warning` | `{ "owner": <username or null>, "used_bytes": <u64>, "limit_bytes": <u64>, "percent": 80 \| 90 \| 100 }` | A write pushes an account's usage across an 80/90/100 percent-of-quota threshold it had not already crossed (CLOUD-11's per-account quota). `percent` is the threshold crossed, not the exact usage percentage; a single large write that jumps past more than one threshold at once reports only the highest. `percent: 100` also covers the write that gets rejected for exceeding quota -- that is still a legitimate "hit 100%" crossing worth surfacing. |

`owner` is the account username where known: always populated on the real
multi-user combined server (CLOUD-11); `null` on a standalone/dev instance
with no accounts configured, never a placeholder value.

## Commands honored (`bread.command.cloud.*`)

None. breadcloud does not currently subscribe to any inbound commands; it
only publishes. If a real use case for a remote-triggered action (e.g.
"force a quota recheck") comes up, the matching `bread.command.cloud.<verb>`
verb and any confirmation event should be added at the same time, per the
`bread` repo's integration checklist -- not stubbed ahead of an actual need.

## Not implemented

- No `bread.command.cloud.*` verb exists. breadcloud is publish-only; a remote-triggered action would add its verb and confirmation event together.
- No event reports authentication, login, per-file transfer progress, or collection enumeration. Only completed sync operations, precondition conflicts, and quota-threshold crossings are published.

## Fail-safe behavior

- If breadd isn't installed or isn't running, every publish is a silent
  no-op (`BreadClient::emit` never blocks or errors the caller): none of
  breadcloud's actual WebDAV/CalDAV/CardDAV functionality is affected
  either way.
- Every publish is dispatched onto Tokio's blocking thread pool rather than
  run inline on the async task handling the request that triggered it, so
  even a slow or wedged breadd cannot add latency to that request.
- Nothing in `breadcloud-events`'s public API returns a `Result`: there is
  nothing meaningful for a caller to do with a failure to notify an
  optional automation bus.
