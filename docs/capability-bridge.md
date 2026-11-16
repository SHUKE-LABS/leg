# The capability bridge

`leg-bridge` is the guest-side endpoint of the mobile capability bridge. It
carries one capability request from a guest Skill to the host that owns the
native permission, and one answer back. The bridge is a *protocol* boundary,
not a capability implementation: it validates the request, binds the approval,
keeps the request id unique, bounds the time, and summarizes the exchange. The
host owns the transport, the permission prompt, the dispatch, the
cancellation, and the user confirmation. The iOS host implements the
`shortcuts.invoke` adapter described below; the desktop bridge and its Skills
registry remain unchanged.

This document defines the request and answer schemas, the capability names,
the approval semantics, the size and time limits, the audit summary, and the
rule a platform adapter has to keep.

## How a Skill calls the bridge

A mobile Skill calls the bridge through the existing `bash` tool — the
desktop agent core, the tool registry, and the Skills registry are unchanged
by this contract:

```
printf '%s\n' '{"version":1,"request_id":"r-1","capability":"photos.list","input":{"limit":5}}' \
  | leg-bridge
```

The request is **one line** of JSON on standard input; the answer is **one
line** of JSON on standard output. The bridge reads the first line and
ignores whatever follows it, so one invocation answers one request.

The answer always has the same shape, whatever happened:

```json
{"version":1,"request_id":"r-1","status":"ok","result":{},"error":null}
```

The exit status of the command is 0 whenever an answer was written; the
answer's `status` carries the outcome.

## The request

| Field | Required | Meaning |
| --- | --- | --- |
| `version` | yes | The protocol version. Only `1` is accepted. |
| `request_id` | yes | 1–128 characters of `A–Z`, `a–z`, `0–9`, `.`, `_`, `-`. |
| `capability` | yes | One of the capability names below. |
| `input` | yes | A JSON object with the fields the capability accepts. |
| `approval_token` | no | The token an earlier answer issued, replayed to redeem an approval. |
| `timeout_ms` | no | The deadline, between 100 and 30000 milliseconds. The default is 5000. |

A request carries **no other field**. A request never carries shell source,
native code, or a host filesystem path: the input is a JSON object whose
fields the capability table names, and the only path shape the bridge accepts
is a relative, normalized path.

The request is decoded strictly: bytes that are not UTF-8 are a malformed
request, not a request with replacement characters in it.

The bridge refuses a request — before it reaches the host — when it speaks an
unknown version, names an unknown capability, carries an unknown field, is not
valid JSON, is larger than 64 KiB, carries an input larger than 32 KiB, reuses
a request id, or carries a disallowed path.

### Capabilities

| Capability | Consent | Input fields |
| --- | --- | --- |
| `photos.list` | needed | `limit` (count, optional), `cursor` (text, optional) |
| `photos.read` | needed | `id` (text) |
| `files.list` | not needed | `path` (path, optional), `limit` (count, optional) |
| `files.read` | needed | `path` (path) |
| `shortcuts.invoke` | needed | `name` (text), `input` (object, optional) |
| `photos.search` | not needed | `filter`, `sort`, `limit` — see [Photos and Files](#photos-and-files) |
| `photos.export` | required | `asset_id`, `destination`, `filename` |
| `files.import` | not needed | `destination` |
| `files.export` | required | `copy_id`, `relative_path`, `destination`, `filename` |

The names are the contract the mobile slices implement. The iOS host implements
`shortcuts.invoke` and the Photos and Files rows in this slice; the other rows
remain protocol names, not desktop tools. The *consent* column names the
capabilities the host is expected to gate behind a user approval. The host owns
the consent decision: it may ask for an approval for any capability, and the
bridge honours that too. For `photos.export` and `files.export` the bridge
enforces the column as well: see [the preview rule](#the-preview-rule).

### iOS Shortcuts adapter

The iOS host has a Shortcuts settings form where the user registers the exact,
case-sensitive name of an existing Shortcut and declares one input kind:
bounded text or a bounded JSON object. An unregistered name is rejected before
any URL is opened with
`status=denied` and `reason=shortcut_not_registered`; the remediation tells the
user to register the exact name. Registration stores only the name and input
kind, never Shortcut source or an arbitrary URL.

The mobile `shortcuts` Skill is staged at
`/root/.agents/skills/shortcuts/SKILL.md` during iOS guest provisioning. It
calls only `shortcuts.invoke`, asks for missing names or input, follows the
approval token flow, and reports the final status. The host opens Apple's
public `shortcuts://x-callback-url/run-shortcut` endpoint with `x-success`,
`x-cancel`, and `x-error` callbacks. Text and JSON use `input=text` and the
`text` parameter. A run is never dispatched without an alert showing the
registered name and an input summary. The host maps a missing result to
`error/missing_result`, a cancelled
callback to `cancelled/cancelled`, a missing Shortcut to
`unavailable/shortcut_not_found`, and an unavailable Shortcuts app to
`unavailable/shortcuts_unavailable`.
Each callback URL carries a host-generated one-time state value, not the
guest's predictable request id; unknown or stale callback states are ignored.

The host limits text and JSON input to 32 KiB, callback results to 32 KiB, and
callback waits to 30 s. File input is out of scope for this slice because
Apple's public URL scheme defines no file parameter; the follow-up is tracked
in [issue #312](https://github.com/SHUKE-LABS/leg/issues/312).
The host may need to foreground Apple's Shortcuts app; backgrounding the iOS
host cancels the pending callback.

There is no private Shortcuts database entitlement and no automatic permission
grant. The host confirmation is required for every run, and individual
Shortcut actions may show Apple's own permission prompts. The guest/model can
see the registered name and bounded text/JSON shape it requested, the approval
review, and the structured result or error returned by the Shortcut. It cannot
see Shortcut source or unregistered automations.

A **path** is relative and normalized: no leading `/`, no `..` or `.` segment,
no empty segment, no backslash, no drive letter, no `~`, no URL scheme, no
control character, and at most 256 bytes. The host maps the relative path into
its own sandbox; the bridge never resolves it against the guest filesystem.

### Photos and Files

These four capabilities have exact schemas: an input and a result carry exactly
the fields below. The bridge refuses an input outside its schema before it
reaches the host (`unknown_field`, `missing_field`, `invalid_input`, or
`invalid_path` for a file name or a managed path). A host that answers `ok`
with a result outside its schema broke the protocol: the bridge answers
`error` with `host_protocol_violation` and withholds the result.

**`photos.search`** finds videos by metadata. It reads no asset.

| Input field | Required | Rule |
| --- | --- | --- |
| `filter.media_type` | yes | `"video"` |
| `filter.created_after` | no | RFC 3339 date-time, inclusive |
| `filter.created_before` | no | RFC 3339 date-time, inclusive, not before `created_after` |
| `sort` | yes | `"duration_desc"` or `"created_desc"` |
| `limit` | no | 1–100, 20 by default |

The result is `{"authorization":"full"|"limited","assets":[…],"truncated":…}`.
An asset is exactly `asset_id`, `media_type` (`"video"`), `duration_ms`,
`created_at`, `width`, and `height`. The page holds at most `limit` assets,
each inside the date filter, in the requested order; equal keys fall back to
`asset_id`, ascending. Only a full page may be `truncated`.

**`photos.export`** copies one video out of the library.

| Input field | Required | Rule |
| --- | --- | --- |
| `asset_id` | yes | an id `photos.search` returned |
| `destination` | yes | `"guest_workspace"` or `"files_picker"` |
| `filename` | no | a safe file name |

The result is exactly `authorization`, `copy_id`, `relative_path`, `name`,
`media_type` (`"video"`), `size_bytes`, and `sha256`. `relative_path` is
`copy_id/name` for `guest_workspace` and `null` for `files_picker`.

**`files.import`** copies one file the user picks into the guest workspace.
The input is exactly `{"destination":"guest_workspace"}`. The result is exactly
`copy_id`, `relative_path` (`copy_id/name`), `name`, `content_type`,
`size_bytes`, and `sha256`.

**`files.export`** hands one imported copy to the Files picker. The input is
`copy_id`, `relative_path` exactly as the import returned it,
`destination` (`"files_picker"`), and an optional `filename`. The result is
exactly `export_id`, `name`, `content_type`, `size_bytes`, and `sha256`.

| Value | Rule |
| --- | --- |
| `asset_id` | printable text, 1–128 bytes |
| `copy_id`, `export_id` | 1–64 bytes of `a–z`, `0–9`, `-`, starting with a letter or a digit |
| file name (`filename`, `name`) | 1–128 bytes; no `/`, `\`, `:`, or control character; no leading or trailing space or dot; not `.` or `..` |
| `relative_path` | `copy_id/name`, at most 256 bytes, under the managed root `~/.local/share/leg/bridge` |
| `content_type` | lowercase `type/subtype` without parameters, 3–128 bytes |
| `size_bytes` | at most 268435456 (256 MiB) |
| `sha256` | 64 lowercase hex digits |
| date-time | `YYYY-MM-DDTHH:MM:SS[.fraction](Z\|±HH:MM)`, at most 64 bytes |
| `duration_ms` | at most 604800000 (7 days) |
| `width`, `height` | 1–32768 |

A name that would need a change to be safe is refused, not changed. The host
also requires a file name in Unicode NFC; the guest bridge does not check NFC.
A `relative_path` is never a native host path.

## The answer

| Status | Meaning |
| --- | --- |
| `ok` | The capability ran; `result` carries its answer. |
| `approval_required` | The capability needs an approval; nothing was performed. |
| `denied` | The host refused the request. |
| `cancelled` | The request was cancelled before it finished. |
| `unavailable` | The host cannot serve the capability, or the permission is not granted. |
| `error` | The request or the host failed. |

`result` and `error` are always present; one of them is `null`.

A failure carries a stable reason, a message, and a remediation:

```json
{
  "version": 1,
  "request_id": "r-1",
  "status": "error",
  "result": null,
  "error": {
    "reason": "unsupported_version",
    "message": "the request speaks version 2, not version 1",
    "remediation": "send a request that speaks version 1"
  }
}
```

Every answer is bounded: the result, the failure reason, and the failure
message each have a fixed limit, and an answer that grew past the response
limit is replaced by a fixed one that says so (`response_too_large`). A host
that answers with a huge failure detail cannot make the bridge's answer grow.

The reasons are grouped by the status they answer with:

| Status | Reasons |
| --- | --- |
| `denied` | `user_refused`, `approval_unknown`, `approval_mismatch`, `approval_expired`, `approval_used`, `shortcut_not_registered`, `invalid_selection`, `duplicate_destination` |
| `cancelled` | `cancelled` |
| `unavailable` | `no_host_transport`, `host_unreachable`, `host_transport_unsupported`, `permission_denied`, `permission_restricted`, `permission_revoked`, `authorization_failed`, `capability_unsupported`, `picker_unavailable`, `shortcut_not_found`, `shortcuts_unavailable` |
| `error` | `invalid_request`, `malformed_request`, `request_too_large`, `unsupported_version`, `invalid_request_id`, `unknown_field`, `unknown_capability`, `invalid_input`, `input_too_large`, `missing_field`, `invalid_path`, `invalid_approval_token`, `invalid_timeout`, `duplicate_request_id`, `state_unavailable`, `timeout`, `host_crashed`, `host_protocol_violation`, `transport_error`, `result_too_large`, `missing_result`, `shortcut_failed`, `invalid_shortcut_url`, `review_too_large`, `response_too_large`, `host_error`, `io_failure`, `insufficient_space`, `size_limit`, `asset_not_found` |

The Photos and Files host answers with these reasons of its own:

| Reason | Meaning |
| --- | --- |
| `user_refused` | The user refused the approval or the Photos permission. |
| `permission_restricted` | The device restricts the Photos permission. |
| `permission_revoked` | A permission the host held is gone. |
| `authorization_failed` | Photos answered no usable authorization. |
| `invalid_selection` | The picked item cannot be copied: a bad name, not a regular file, or outside the provider's scope. |
| `duplicate_destination` | The destination already holds the name; nothing was replaced. |
| `insufficient_space` | The device has no space for the copy. |
| `io_failure` | A read, write, or commit failed. |
| `size_limit` | The file is larger than 256 MiB. |
| `capability_unsupported` | The host does not serve the capability. |
| `asset_not_found` | The asset id names no asset the library still holds. |
| `picker_unavailable` | The app has no window to show a picker in. |

## Approval semantics

A capability that needs consent never runs before the host approved it.

1. The Skill sends the request without a token. The host answers
   `approval_required` with a review summary and a one-time token. Nothing was
   performed. The summary is not optional: a host that asks for an approval
   without one, with one that is not reviewable, with one that is empty, or
   with one that is over the limit broke the protocol, and the bridge answers
   `host_protocol_violation` rather than asking a human to approve nothing.
   A summary is a string with content in it, or an object with at least one
   field that carries content; an empty string, an empty object, and an
   object whose fields are all empty are not summaries.
2. The bridge records the binding: the capability, the digest of the input,
   the digest of the token, and the expiry.
3. The Skill sends a **follow-up request**: a **fresh `request_id`**, the same
   capability, the same input, and the token in the top-level
   `approval_token` field. The bridge checks the binding and hands the token
   to the host, which verifies it before any side effect.

The binding is what makes the approval mean something:

- a token the bridge did not issue is refused (`approval_unknown`);
- an approval for another capability or for a different input is refused
  (`approval_mismatch`);
- an approval is fresh through its 600th second, inclusive; from the 601st
  it is refused (`approval_expired`);
- a token that was already redeemed is refused (`approval_used`).

A follow-up the host answers `ok` or `denied` redeems the token. A follow-up
that ends any other way — `cancelled`, `timeout`, or a failure — invalidates
it: the binding is removed, the next follow-up with that token is
`approval_unknown`, and the Skill starts again with a fresh preview. The ledger
keeps the newest 32 bindings.

The approval token is a host-side credential: keep it out of any channel the
model can read, or the gate means nothing.

### The preview rule

`photos.export` and `files.export` always need an approval. A request for one
of them without a token is a **preview**: the host answers it
`approval_required` and performs nothing. A host that answers a preview `ok`
broke the protocol: the bridge answers `error` with `host_protocol_violation`,
withholds the result, and records no approval.

### The input digest

The binding holds `input_sha256`: the lowercase hex SHA-256 of the input in
the RFC 8785 canonical form (JCS). The bridge computes it — a request that
carries `input_sha256` is refused with `unknown_field` — and the host computes
it again from the input it receives. Inputs that differ only in member order or
number spelling have one digest, so a follow-up may reorder its keys.

The canonical form:

- members sorted by their UTF-16 code units, with no whitespace;
- strings escape only `"`, `\`, and control characters (`\b`, `\f`,
  `\n`, `\r`, `\t`, else `\u00xx`);
- numbers in the shortest ECMAScript form: `1.0` is `1`, `-0` is `0`, `1e21`
  is `1e+21`, `1e-6` is `0.000001`.

The request line is parsed strictly, and the bridge answers
`malformed_request` for a line that carries:

- a member name twice in one object, also when an escape spells the repeat
  (`"a"` and `"\u0061"`);
- a number a binary64 cannot hold: one that overflows (`1e400`), or a non-zero
  literal that underflows to zero (`1e-400`);
- a lone surrogate escape;
- nesting deeper than 64 levels.

The guest bridge and the host share these vectors:

| Input | Canonical form | SHA-256 |
| --- | --- | --- |
| `{"sort":"duration_desc","filter":{"media_type":"video"},"limit":20}` | `{"filter":{"media_type":"video"},"limit":20,"sort":"duration_desc"}` | `a9a83ee45388e790690d5f5683782ecf738d5f703189c18b139a01f29c8bce4a` |
| `{"destination":"guest_workspace"}` | `{"destination":"guest_workspace"}` | `8045fd38b79820c6825bd8096a324dea1431cfe9e3e17870b1592de88295a750` |
| `{"\uE000":"bmp","\uD834\uDD1E":"supp"}` | the two members as raw UTF-8, U+1D11E first: its UTF-16 lead unit 0xD834 sorts before 0xE000 | `73e96d65d5acff2a00259e91342ce76055ab13ce7809e00e9920246333341f59` |
| `{"n":1.0,"small":1e-6,"tiny":1e-7,"huge":1e21,"negzero":-0}` | `{"huge":1e+21,"n":1,"negzero":0,"small":0.000001,"tiny":1e-7}` | `c25482e66991d36fc4ac2b9d6aa6ede849b011530dbed02a79fda81c5974dd24` |

## The host transport

The bridge talks to the host over **one fixed transport**: a Unix socket.
The host listens on the socket and the guest connects to it; the socket is
named by `LEG_BRIDGE_HOST_SOCKET`.

The endpoint comes from the environment, so a request can never choose one.
The bridge never loads a request-selected command, plugin, native library, or
endpoint, and it never runs a capability itself.

**The default is unavailable.** When the host configured no transport, the
bridge answers `unavailable` with the reason `no_host_transport` rather than
pretending the request failed:

| Situation | Status | Reason |
| --- | --- | --- |
| No socket configured | `unavailable` | `no_host_transport` |
| The socket is configured, but nothing listens on it | `unavailable` | `host_unreachable` |
| The platform has no Unix sockets | `unavailable` | `host_transport_unsupported` |

The guest is a Linux guest, so the Unix socket is the transport that matters;
the last row only names the platforms this slice cannot serve.

On iOS, `LEG_BRIDGE_HOST_SOCKET` is the reserved guest name
`/run/leghost-bridge.sock`. The vendored iSH hook maps that fake socket inode
to a private per-session app-temporary Unix socket file; the guest cannot
select the host endpoint. The host accepts one bounded request per connection
and waits at most 30 s. All iOS capabilities use this same listener and its
approval ledger.

### The envelope

One JSON line each way, bounded in size:

```json
{"version":1,"request_id":"r-1","nonce":"…","capability":"photos.list","input":{},"input_sha256":"…","timeout_ms":5000}
```

```json
{"version":1,"nonce":"…","request_id":"r-1","status":"ok","result":{},"error":null}
```

The bridge accepts an answer only when it carries the request's nonce and
request id, so a stale answer is never mistaken for a fresh one. An answer in
another version, an answer for another request, an answer that cannot be
parsed, an answer that is not valid UTF-8, and a connection the host closed
without answering are distinct failures (`host_protocol_violation` and
`host_crashed`).

### Cancellation and the deadline

Every request has a deadline: the bridge stops waiting at the deadline and
answers `error` with the reason `timeout`. The deadline covers the whole
exchange — the connect, the write, and the read — so a host that stops
answering cannot hold the bridge past it. A write that runs past the deadline
is a `timeout` as well, not a transport failure. The bridge also stops when the
guest's process is interrupted, and answers `cancelled`; a write that is
interrupted is a `cancelled` too. A write that is stuck does not wait for the
whole of the remaining time: the bridge gives the socket a short interval,
looks at the clock and at the interrupt flag, and writes again, so a cancelled
request ends within that interval rather than at the deadline. In both cases
it closes the connection, so the host sees the end of the exchange and can stop
the work it started.

### What a platform adapter must keep

- **The sandbox does not grow.** An adapter maps a request onto the host API
  the capability names, with the host's own permissions. It must not widen the
  guest's reach: no shell, no native code loading, no private API, and no
  path outside the host's own sandbox.
- **The host decides.** The permission prompt, the user confirmation, the
  dispatch, the cancellation, and the verification of an approval token
  belong to the host.
- **The bridge validates.** Every check in this document happens in the
  bridge, before the host sees the request. An adapter may add checks of its
  own; it may not skip the bridge's.
- **No second registry.** The bridge is not an agent tool. It adds no tool to
  the desktop registry and changes no Skills contract.

### The iOS host

The LegHost app serves `shortcuts.invoke`, `photos.search`, `photos.export`,
`files.import`, and `files.export` (`ios/LegHost/BridgeListener.swift`,
`ShortcutsAdapter.swift`, `PhotosAdapter.swift`, `FilesAdapter.swift`). All
five capabilities use the same listener and approval ledger.

**The socket.** For every guest generation the host binds an `AF_UNIX`
listener at a private path, `lb-<random>/s` (directory `0700`, socket `0600`)
in the app's temporary directory — the Mac's `/tmp` in a simulator, whose
container path is longer than a Darwin socket path (103 bytes). The guest
never sees that path. It gets `LEG_BRIDGE_HOST_SOCKET=/run/leghost-bridge.sock`,
and the vendored iSH maps a `connect()` to that exact fake socket inode onto
the listener (`ios/vendor/ish/fs/IshBridgeSocketHook.c`). Every other guest
Unix socket keeps the upstream mapping.

Start order: bind and listen, register the generation with the hook, install
the Skill, build the environment, start the session, provision the fake inode
in the guest's task context, execute the guest program. The hook makes the
fake inode, records its identity, and holds it in one step, so a start that
fails after that removes it and never a socket it did not make, and a cleanup
that cannot look the endpoint up keeps the record and the hold;
a failed start also closes the listener. Teardown order: cancel the adapters'
work, stop accepting and shut the open connections, stop the guest, remove
the fake inode in a task context, join the workers, unregister the
generation, then unlink the socket the listener bound (matched by device and
inode) and its empty directory and remove the generation's staging. The work
of an adapter is a worker until it returns, even after its request was
answered; when a worker outlives the 10 s join, the mapping, socket, and
staging stay and the teardown completes when the last worker ends.

**One exchange per connection.** The listener reads one request line of at
most 64 KiB within 5 s and writes at most one reply line. The request must be
a version 1 HostRequest with no unknown field, no duplicate name, only I-JSON
numbers, a `timeout_ms` of 100–30000, and an `input_sha256` equal to the
digest the host recomputes; anything else is `error`/`host_protocol_violation`
when the frame names a printable request id and nonce, and the connection is
closed without a reply otherwise (the bridge reports `host_crashed`). No
refusal reaches an adapter. Work stops at the request deadline
(`error`/`timeout`), when the guest closes the connection, and at teardown
(`cancelled`).

**The host ledger.** The listener keeps its own approvals, in memory, per
generation: the SHA-256 of the token, the capability, the input digest, the
issue and expiry seconds, and the state. A preview inserts an unused record;
a continuation marks it in flight; `ok` or `denied` redeems it; a cancel,
timeout, or failure removes it. The token is valid while
`now <= issued_at + 600`, expired records are pruned before every lookup,
insert, and transition, the ledger holds at most 32 records, and teardown
discards it. An unknown, expired, used, or mismatched token is `denied` with
`approval_unknown`, `approval_expired`, `approval_used`, or
`approval_mismatch`, before the adapter runs.

**Photos.** Both operations authorize first and carry the authorization in a
successful result:

| Authorization | Status | Reason / result |
| --- | --- | --- |
| authorized | `ok` | `authorization: "full"` |
| limited | `ok` | `authorization: "limited"` |
| denied after the request | `denied` | `user_refused` |
| restricted | `unavailable` | `permission_restricted` |
| granted, then gone at the recheck | `unavailable` | `permission_revoked` |
| still undetermined, or the API failed | `unavailable` | `authorization_failed` |

A refused row fetches no metadata and reads no asset. `photos.search` skips an
asset whose metadata is outside the schema bounds.

**Files.** A picked file is read only inside its security-scoped root. A
file opened in place grants the scope of that file alone, so the live root is
the picked item: it is opened with `O_NOFOLLOW_ANY` (no link anywhere on its
path), its descriptor must name the granted path (`F_GETPATH`), and it must be
a regular file. Below a root that is a granted directory, the file is reached
one component at a time: every directory with `openat(O_DIRECTORY|O_NOFOLLOW)`,
every component checked with `fstatat(AT_SYMLINK_NOFOLLOW)`, and the file
opened with `O_NOFOLLOW` and checked as a regular file by its descriptor. A
link on the way, a path outside the root, or a provider with no root is
`denied`/`invalid_selection` before a byte is read. `files.export` resolves only `copy_id/name` under the
managed root the same way; a native path is never accepted.

**Staging and commit.** A transfer is staged under
`Library/Caches/LegHost/BridgeStaging/<generation>/<operation>/`: every byte
is counted before it is written (over 256 MiB is `error`/`size_limit`), hashed
as it streams, synced, and checked for size and digest before the commit. The
commit never replaces: `renameatx_np(RENAME_EXCL)` into the managed root
(`~/.local/share/leg/bridge/<copy_id>/<name>`), or an exclusively created
sibling renamed with `RENAME_EXCL` under a coordinated write of a picked
folder. An existing destination is `denied`/`duplicate_destination` and stays
unchanged. The guest's fakefs hides a file that has no entry in its database,
so a managed commit then records the copy and its directories there (owned by
root, the mode of the file on disk); a copy that cannot be recorded is removed
and the request is `error`/`io_failure`. Any failure removes only the
operation's staging; no partial file is left where the guest or the user can
see it.

| Failure | Status | Reason |
| --- | --- | --- |
| picker dismissed, request cancelled | `cancelled` | `cancelled` |
| security scope lost | `unavailable` | `permission_revoked` |
| destination exists | `denied` | `duplicate_destination` |
| no space | `error` | `insufficient_space` |
| any other read or write failure | `error` | `io_failure` |
| over 256 MiB | `error` | `size_limit` |

**Redaction.** The host logs no asset bytes, file contents, credentials,
approval tokens, or native paths; its diagnostics name the step that failed.

## Duplicate request ids

A request id names one request, and it is used once.

The bridge keeps a bounded ledger in the state directory. Every request that
reached the host leaves a record of the request id, the digest of the input,
and the terminal state the request ended in. A request that arrives with an id
the ledger already holds is refused with `duplicate_request_id`, and it never
reaches the host.

- **Scope.** The ledger lives in the state directory, so the ids are refused
  across the invocations that share it — the invocations of one guest.
- **The ledger is locked.** The bridge takes an exclusive lock on the state
  directory before it reads the ledger and holds it until the request is
  answered: the request id is checked, reserved, dispatched, and recorded
  under that one lock. Two invocations that carry the same id therefore
  cannot dispatch it twice — one of them finds the id taken and is refused.
  A request that cannot take the lock before its own deadline answers
  `timeout`, and one that was cancelled while it waited answers `cancelled`.
  The operating system releases the lock when the holder ends, so a crash
  leaves no lock behind.
- **The ledger is bounded and it is the bridge's own.** A ledger that is
  larger than the limit, that speaks another schema, that carries fields the
  bridge does not know — in the ledger itself or in the records inside it — or
  that holds a record that is not the shape the bridge writes is refused: the
  bridge never reads a foreign ledger as if it were its own, and never resets
  one it does not understand. The bound holds on the way out as well: a ledger
  that would grow past the limit is not published.
- **Retention.** The ledger keeps the last 64 request ids. An id that falls
  out of the ledger may be used again.
- **A request that never reached the host keeps its id.** A guest whose host
  was not reachable may try again with the same id, because nothing happened.
  That covers a transport that never connected — a connect that ran past the
  deadline, or one that was cancelled — and a request that was refused before
  it was dispatched. A write that failed halfway is not proof that the host
  never saw the request, so that id is used.
- **A ledger that cannot be written is a failure of its own.** The bridge
  refuses the request before it calls the host (`state_unavailable`), and it
  answers a recorded failure when the ledger cannot be written after the host
  answered — it never claims a request id that was not remembered.

## Limits

| Limit | Value |
| --- | --- |
| Request line | 64 KiB |
| Request input | 32 KiB |
| Answer | 64 KiB |
| Answer result | 64 KiB |
| Review summary | 8 KiB |
| Host frame | 64 KiB |
| Failure reason | 64 characters |
| Failure message | 1 KiB |
| Text field | 4 KiB |
| Object field | 4 KiB |
| Item count | 100 |
| Path | 256 bytes |
| Request id | 128 characters |
| Deadline | 100 ms … 30 s, 5 s by default |
| Approval lifetime | 10 minutes |
| Ledger | 64 request ids, 32 approval bindings, 128 KiB on disk |

## The audit summary and the ledger

The bridge appends one JSON line per request to the audit log — `audit.log` in
the state directory, or `LEG_BRIDGE_AUDIT_FILE`. The summary carries the
request id, the capability, the status, the failure reason, the duration, the
names of the input fields, the digest of the input, and whether the request
redeemed an approval.

It never carries the *values*: no provider credential, no photo contents, no
file contents, and no shortcut input. The capability name and the field names
it carries are cut to a fixed length, so a caller cannot make the summary
grow. `LEG_BRIDGE_AUDIT=off` turns the log off.

The ledger holds no values either: it keeps request ids, digests, terminal
states, and the approval binding. The approval token itself is not written
down — the ledger keeps its digest, so a token that reaches the ledger file
cannot be replayed from it.

## Environment

| Variable | Meaning |
| --- | --- |
| `LEG_BRIDGE_HOST_SOCKET` | The Unix socket the host listens on. Without it the bridge answers `unavailable`. |
| `LEG_BRIDGE_STATE_DIR` | The directory the ledger and the audit log live in. Else `XDG_STATE_HOME/leg/bridge`, else `~/.local/state/leg/bridge`. |
| `LEG_BRIDGE_AUDIT_FILE` | The file the audit summary is appended to. |
| `LEG_BRIDGE_AUDIT` | `off` turns the audit summary off. |

The state directory holds the ledger that keeps a request id unique and the
approval bindings that wait to be redeemed. It lives beside the guest, not on
the host: the approval an answer issues is redeemed by a later request in the
same guest.
