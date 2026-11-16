---
name: mobile-media-files
description: Find videos in the iOS photo library, copy a photo or file into the guest workspace, and export a workspace copy to the iOS Files app through leg-bridge. Use when the user asks to work with their phone's videos or files.
compatibility: Requires the LegHost iOS app, which serves the bridge; elsewhere every request answers unavailable.
metadata:
  owner: leg-ios-bundled
---

# Mobile media and files

This Skill uses the iOS host's four media and file capabilities through
`leg-bridge`. Each call is one
JSON line on standard input and one JSON line on standard output:

```sh
printf '%s\n' '{"version":1,"request_id":"r-search-1","capability":"photos.search","input":{"filter":{"media_type":"video"},"sort":"duration_desc","limit":1}}' | leg-bridge
```

The answer is `{"version":1,"request_id":…,"status":…,"result":…,"error":…}`.
Act on `status`: `ok` carries `result`; `denied`, `cancelled`, `unavailable`,
and `error` carry `error.reason` and `result` is `null`. Report a failure to
the user by its reason; do not retry a `denied` or `cancelled` answer unless
the user asks.

## Rules

- Use a fresh `request_id` for every call: 1–128 characters of `A–Z a–z 0–9 . _ -`,
  for example `r-$(date +%s)-$$`. A reused id is refused.
- Send only the fields listed below. Unknown fields, at any depth, are refused.
- Never pass a host path. Workspace copies live in
  `~/.local/share/leg/bridge/<copy_id>/<name>`; refer to them by `copy_id` and
  `relative_path` (`<copy_id>/<name>`).
- A `filename` is a plain file name: 1–128 bytes, no `/`, `\`, or `:`, no
  control character, no leading or trailing space or dot, not `.` or `..`.
  A name that breaks these rules is refused, not corrected.
- Files are at most 256 MiB; a larger file fails with `size_limit`.
- A call ends at its deadline (`timeout_ms`, 5000 ms by default, at most
  30000 ms), also while a picker is open. Set `"timeout_ms":30000` on calls
  that open a picker.

## photos.search (no approval)

Input: `{"filter":{"media_type":"video","created_after"?:RFC3339,"created_before"?:RFC3339},"sort":"duration_desc"|"created_desc","limit"?:1..100}`
(`limit` defaults to 20; `created_after` must not be later than `created_before`).

Result: `{"authorization":"full"|"limited","assets":[{"asset_id","media_type":"video","duration_ms","created_at","width","height"}],"truncated":bool}`.
`truncated` is true when more matches exist beyond `limit`. With `limited`
authorization the user shared only some videos; say so when it matters.
For "my longest video", use `"sort":"duration_desc","limit":1`.

## photos.export (approval required)

Input: `{"asset_id":…,"destination":"guest_workspace"|"files_picker","filename"?:…}`.
Result: `{"authorization","copy_id","relative_path","name","media_type":"video","size_bytes","sha256"}`;
`relative_path` is `null` for `files_picker`.

## files.import (no approval)

Input: `{"destination":"guest_workspace"}`. The user picks a file in the iOS
picker. Result: `{"copy_id","relative_path","name","content_type","size_bytes","sha256"}`.

## files.export (approval required)

Input: `{"copy_id":…,"relative_path":"<copy_id>/<name>","destination":"files_picker","filename"?:…}`.
The user picks a folder; an existing file there is never replaced
(`duplicate_destination`). Result: `{"export_id","name","content_type","size_bytes","sha256"}`.

## Approval

`photos.export` and `files.export` first answer `approval_required` with
`result.review` and `result.approval_token`. Nothing was done yet.

1. Keep the answer out of the transcript; show only the review:

   ```sh
   answer=$(mktemp)
   printf '%s\n' "$request" | leg-bridge > "$answer"
   sed 's/"approval_token":"[^"]*"/"approval_token":"<hidden>"/' "$answer"
   ```

2. Show the user what the review says will happen and ask for explicit
   confirmation. Without a clear yes, stop.
3. On yes, send the **same capability and the same input** with a **fresh
   `request_id`** and the token in the top-level `approval_token` field, read
   from the saved answer without printing it:

   ```sh
   token=$(sed -n 's/.*"approval_token":"\([^"]*\)".*/\1/p' "$answer")
   printf '{"version":1,"request_id":"%s","capability":"photos.export","input":%s,"approval_token":"%s","timeout_ms":30000}\n' \
     "r-$(date +%s)-$$" "$input" "$token" | leg-bridge
   rm -f "$answer"
   ```

Never print, log, or repeat an approval token. A token is good for one call
within ten minutes; an expired, used, or mismatched token is refused, so start
again from step 1.
