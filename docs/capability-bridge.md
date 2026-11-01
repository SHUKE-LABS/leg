# The capability bridge

`leg-bridge` is the guest-side endpoint of the mobile capability bridge. It
carries one capability request from a guest Skill to the host that owns the
native permission, and one answer back. The bridge is a *protocol* boundary,
not a capability implementation: it validates the request, binds the approval,
keeps the request id unique, bounds the time, and summarizes the exchange. The
host owns the transport, the permission prompt, the dispatch, the
cancellation, and the user confirmation.

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

The names are the contract the mobile slices implement; this slice runs none
of them. The *consent* column names the capabilities the host is expected to
gate behind a user approval. The host owns the consent decision: it may ask
for an approval for any capability, and the bridge honours that too.

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

Every answer is bounded: the result, the failure reason, and the failure
message each have a fixed limit, and an answer that grew past the response
limit is replaced by a fixed one that says so (`response_too_large`). A host
that answers with a huge failure detail cannot make the bridge's answer grow.

The reasons are grouped by the status they answer with:

| Status | Reasons |
| --- | --- |
| `denied` | `user_refused`, `approval_unknown`, `approval_mismatch`, `approval_expired`, `approval_used` |
| `cancelled` | `cancelled` |
| `unavailable` | `no_host_transport`, `host_unreachable`, `host_transport_unsupported`, `permission_denied` |
| `error` | `malformed_request`, `request_too_large`, `unsupported_version`, `invalid_request_id`, `unknown_capability`, `unknown_field`, `invalid_input`, `input_too_large`, `missing_field`, `invalid_path`, `invalid_approval_token`, `invalid_timeout`, `duplicate_request_id`, `state_unavailable`, `timeout`, `host_crashed`, `host_protocol_violation`, `transport_error`, `result_too_large` |

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
- an approval older than ten minutes is refused (`approval_expired`);
- a token that was already redeemed is refused (`approval_used`).

The approval token is a host-side credential: keep it out of any channel the
model can read, or the gate means nothing.

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
interrupted is a `cancelled` too. In both cases it closes the connection, so
the host sees the end of the exchange and can stop the work it started.

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
