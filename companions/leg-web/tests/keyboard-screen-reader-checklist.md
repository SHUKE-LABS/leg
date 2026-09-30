# Leg Web manual accessibility checklist

Run the local host and open its launch URL in a browser with a screen reader
available (VoiceOver, NVDA, or Orca). Check both 1280×800 and 768×1024, then
repeat at 200% browser zoom.

- [ ] Use only the keyboard to start a conversation, edit a multiline draft,
  send, reach Stop while a turn runs, and return to the latest content.
- [ ] Focus is always visible. Tab order reaches the workspace input, New,
  Message, Send, the safe same-ID retry when shown, Stop, and the new-content
  control. Streaming never moves focus.
- [ ] The screen reader announces the workspace/tool warning, field labels,
  connection changes, turn status, and active tool. It does not announce every
  streamed text fragment or repeatedly interrupt reading.
- [ ] Idle, starting, running, stopping, succeeded, failed, interrupted,
  incomplete, capped, and reconnecting each have readable text. No state
  depends only on color.
- [ ] Enter adds a line; Ctrl+Enter or Cmd+Enter submits; IME composition does
  not submit. A draft remains editable during a turn and remains available
  after a failed turn or reconnect.
- [ ] While scrolled into history, streamed content leaves the reading
  position intact. The new-content control is reachable and returns to the
  latest message. The composer and Stop remain available at 200% zoom.
- [ ] Code and long tool output remain readable without horizontal page
  overflow. A large result is summarized in the transcript.
