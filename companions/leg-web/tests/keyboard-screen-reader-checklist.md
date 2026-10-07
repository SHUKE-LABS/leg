# Leg Web manual accessibility checklist

Run the local host and open its launch URL in a browser with a screen reader
available (VoiceOver, NVDA, or Orca). Check both 1280×800 and 768×1024, then
repeat at 200% browser zoom.

- [ ] Use only the keyboard to start a conversation, edit a multiline draft,
  send, reach Stop while a turn runs, and return to the latest content.
- [ ] Focus is always visible. Tab order reaches the Sessions landmark, each
  open and rename action, New, the workspace input, Message, Send, the safe
  same-ID retry when shown, Stop, and the new-content control. Streaming never
  moves focus; refreshing session status keeps focus on the same action.
- [ ] The selected session is announced. Session entries expose title,
  workspace, recency, and status. Missing-workspace, recovered, read-only, and
  busy guidance says what action is available; setting a recovered workspace
  and renaming a session remain keyboard accessible.
- [ ] The screen reader announces the workspace/tool warning, field labels,
  connection changes, turn status, and active tool. It does not announce every
  streamed text fragment or repeatedly interrupt reading.
- [ ] Idle, starting, running, stopping, succeeded, failed, interrupted,
  incomplete, capped, and reconnecting each have readable text. No state
  depends only on color.
- [ ] Tool rows and finished-turn group rows are reachable with the keyboard
  and expose the correct `aria-expanded` state. Running calls stay individual;
  consecutive finished calls open from their group row. Collapsed rows omit
  call IDs, non-zero bash exits announce the exit code, and transcript search
  opens the matching group and tool row.
- [ ] Expanded bash details show the command as code, preserve stdout line
  breaks, show stderr after stdout, and report omitted bytes. Copy controls are
  reachable inside the expanded details.
- [ ] Enter adds a line; Ctrl+Enter or Cmd+Enter submits; IME composition does
  not submit. A draft remains editable during a turn and remains available
  after a failed turn or reconnect.
- [ ] While scrolled into history, streamed content leaves the reading
  position intact. The new-content control is reachable and returns to the
  latest message. The composer and Stop remain available at 200% zoom.
- [ ] Switching between sessions restores each draft and transcript anchor.
  A running session remains marked in the rail; reopening it reconnects without
  duplicate text or an extra submission.
- [ ] When multiple themes are registered, the theme selector has an accessible
  name, is keyboard reachable from every theme, and announces the selected
  theme. Switching themes restores the selected session, draft, reading
  position, and pending-send state without submitting, retrying, or stopping.
  Stop and an eligible explicit same-send retry remain usable afterward.
- [ ] Repeat the session/workspace, transcript, composer, Stop, recovery, and
  outcome checks in each theme. If detailed tool inspection is absent, failed,
  denied, missing, interrupted, and capped results remain readable in the
  transcript; omitted search/copy/download controls have no hidden controls or
  shortcuts.
- [ ] Code and long tool output remain readable without horizontal page
  overflow. A large result is summarized in the transcript.
