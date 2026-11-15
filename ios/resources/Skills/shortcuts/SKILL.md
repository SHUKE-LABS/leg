---
name: shortcuts
description: Run an existing user-registered iOS Shortcut through Leg's confirmed mobile bridge.
---

# Registered iOS Shortcuts

Use this Skill only when the user asks to run an existing iOS Shortcut.

1. Ask for the exact registered Shortcut name when the user has not supplied
   one. Ask for the missing text, file selection, or JSON object when the
   registered Shortcut's input kind requires it. Do not invent a name.
2. Call the existing `bash` tool with one `leg-bridge` request whose
   capability is exactly `shortcuts.invoke` and whose input contains only the
   exact `name` and the bounded `input` object. Never send a shell command,
   URL, host path, Shortcut source, or automation definition as input.
3. Follow the bridge approval flow. A response with status
   `approval_required` means that no Shortcut ran; continue only with the
   fresh request id, the same name and input, and the approval token returned
   by the bridge. Report the final `ok`, `denied`, `cancelled`, `unavailable`,
   or `error` status plainly and do not claim success without an `ok` result.
4. If the bridge returns `denied` with reason
   `shortcut_not_registered`, tell the user to open Leg's Shortcuts settings,
   enter the exact existing Shortcut name, choose its accepted input kind,
   save it, and retry. Do not fall back to a similarly named Shortcut.
5. Never claim that Leg can create, edit, enable, schedule, or silently run
   arbitrary time-, location-, or automation-triggered Shortcuts. Every real
   run is confirmed by the host and the host may need to foreground Apple's
   Shortcuts app.
