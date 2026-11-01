# The capability bridge

`leg-bridge` is the guest-side endpoint of the mobile capability bridge. It
carries one capability request from a guest Skill to the host that owns the
native permission, and one answer back. The bridge is a *protocol* boundary,
not a capability implementation: it validates the request, binds the approval,
keeps the request unique, bounds the time, and summarizes the exchange. The
host owns the transport, the permission prompt, the dispatch, the
cancellation, and the user confirmation.

This document defines the request and response schemas, the capability names,
the approval semantics, the size and time limits, the audit summary, and the
rule a platform adapter has to keep.

## How a Skill calls the bridge

The bridge ships in the iSH bundle next to `leg`. A mobile Skill calls it
through the existing `bash` tool — the desktop agent core, the tool registry,
and the Skills registry are unchanged by this contract:

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

The bridge refuses a request — before it reaches the host — when it speaks an
unknown version, names an unknown capability, carries an unknown field, is not
valid JSON, is larger than 64 KiB, carries an input larger than 32 KiB, reuses
a request id, or carries a disallowed path.

### Capabilities

| Capability | Approval | Input fields |
| --- | --- | --- |
| `photos.list` | required | `limit` (count, optional), `cursor` (text, optional) |
| `photos.read` | required | `id` (text) |
| `files.list` | not required | `path` (path, optional), `limit` (count, optional) |
| `files.read` | required | `path` (path) |
| `shortcuts.invoke` | required | `name` (text), `input` (object, optional) |

The names are the contract the mobile slices implement; this slice runs none
of them. The *approval* column is the bridge's own policy: a capability marked
"required" is previewed first and only runs once the host's approval token
has been replayed. The host may ask for an approval for any capability, and
the bridge honours that too.

A **path** is relative and normalized: no leading `/`, no `..` or `.` segment,
no empty segment, no backslash, no drive letter, no `~`, no URL scheme, no
control character, and at most 256 bytes. The host maps the relative path into
its own sandbox; the bridge never resolves it against the guest filesystem.

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

The reasons that distinguish the outcomes a host can produce are
`user_refused` (denied), `cancelled`, `no_host_transport` and
`host_unreachable` and `permission_denied` (unavailable), `timeout` (the host
did not answer in time), and `host_crashed` (the host's answer could not be
read).

## Approval semantics

A capability that needs consent never runs before the host approved it.

1. The Skill sends the request without a token. The bridge previews it: the
   host answers `approval_required` with a review summary and a one-time
   token. The bridge performs **no** side effect.
2. The bridge records the token against the request id, the capability, and
   the digest of the input.
3. The Skill replays the **same** request id with the **same** input and the
   token. The bridge checks all three, hands the token to the host, and the
   host performs the action and answers `ok`.

The binding is what makes the approval mean something:

- a token the bridge did not issue is refused (`approval_unknown`);
- a token that belongs to another request is refused (`approval_invalid`);
- an approval for another capability or for a different input is refused
  (`approval_mismatch`);
- an approval older than ten minutes is refused (`approval_expired`);
- a request id is used once; a second request with the same id and no token
  is refused (`duplicate_request_id`).

The approval token is a host-side credential: keep it out of any channel the
model can read, or the gate means nothing.

## The host adapter

The host reaches the bridge through the `HostAdapter` seam
(`src/bridge/adapter.rs`). This slice ships the file-backed transport and the
seam; a later mobile slice implements the seam for its platform.

The file-backed transport needs one directory the bridge and the host both
see — `LEG_BRIDGE_HOST_DIR`. The bridge writes its request to
`bridge-request-<request id>.json`, the host writes its answer to
`bridge-response-<request id>.json`, and the bridge reads the answer only when
it carries the request's nonce, so a stale answer is never mistaken for a
fresh one. When the bridge stops waiting — a deadline or a signal — it writes
`bridge-cancel-<request id>.json`, so the host can stop the work it started.
The file names carry the request id, so two requests never share a file.

The host's answer is one JSON object:

```json
{
  "version": 1,
  "nonce": "…",
  "request_id": "r-1",
  "status": "approval_required",
  "result": {"approval_token": "…", "review": {"summary": "…"}},
  "error": null
}
```

### What a platform adapter must keep

- **The sandbox does not grow.** An adapter maps a request onto the host API
  the capability names, with the host's own permissions. It must not widen the
  guest's reach: no shell, no native code loading, no private API, and no
  path outside the host's own sandbox.
- **The host decides.** The permission prompt, the user confirmation, the
  dispatch, and the cancellation belong to the host. A preview must not
  perform the action; the bridge reports a host that breaks that rule as
  `host_protocol_violation`.
- **The bridge validates.** Every check in this document happens in the
  bridge, before the host sees the request. An adapter may add checks of its
  own; it may not skip the bridge's.
- **No second registry.** The bridge is not an agent tool. It adds no tool to
  the desktop registry and changes no Skills contract.

## Limits

| Limit | Value |
| --- | --- |
| Request line | 64 KiB |
| Request input | 32 KiB |
| Answer result | 64 KiB |
| Text field | 4 KiB |
| Object field | 4 KiB |
| Item count | 100 |
| Path | 256 bytes |
| Request id | 128 characters |
| Deadline | 100 ms … 30 s, 5 s by default |

Every request has a deadline and a cancellation path: the bridge stops
waiting at the deadline, and it stops when the host stops it or when the
guest's process is interrupted.

## The audit summary

The bridge appends one JSON line per request to the audit log — `audit.log` in
the state directory, or `LEG_BRIDGE_AUDIT_FILE`. The summary carries the
request id, the capability, the status, the failure reason, the duration, the
names of the input fields, the digest of the input, and whether the request
redeemed an approval.

It never carries the *values*: no provider credential, no photo contents, no
file contents, and no shortcut input. `LEG_BRIDGE_AUDIT=off` turns the log
off.

## Environment

| Variable | Meaning |
| --- | --- |
| `LEG_BRIDGE_HOST_DIR` | The directory the bridge and the host share. Without it the bridge answers `unavailable`. |
| `LEG_BRIDGE_STATE_DIR` | The directory the request records and the audit log live in. Else `XDG_STATE_HOME/leg/bridge`, else `~/.local/state/leg/bridge`. |
| `LEG_BRIDGE_AUDIT_FILE` | The file the audit summary is appended to. |
| `LEG_BRIDGE_AUDIT` | `off` turns the audit summary off. |

The state directory holds the request records that keep a request id unique
and the approval tokens that wait to be redeemed. It lives beside the guest,
not on the host: the approval an answer issues is redeemed by a later request
in the same guest.
