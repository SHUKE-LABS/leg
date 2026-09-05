# leg

`leg` binary skeleton.

## Usage

```
ANTHROPIC_API_KEY=sk-... leg ask [--model <model>] "prompt"
```

Prints the assistant reply on success. A provider or delivery failure
(bad credentials, unreachable base URL, etc.) prints a `baton.message/v1`
envelope with `"kind":"error"` instead of exiting non-zero — only a
configuration failure (missing/malformed env vars) exits non-zero.
Also accepts `ANTHROPIC_AUTH_TOKEN`/`CLAUDE_CODE_OAUTH_TOKEN`,
`ANTHROPIC_BASE_URL`, `LEG_MODEL`, `LEG_TIMEOUT_SECS`, `LEG_MAX_TOKENS`,
and `LEG_SYSTEM_PROMPT`.

## CI-supported targets

- x86_64-unknown-linux-gnu
- aarch64-unknown-linux-gnu
- x86_64-apple-darwin
- aarch64-apple-darwin
- x86_64-pc-windows-msvc
- armv7-unknown-linux-musleabihf (cross-compiled; build-only, no native runner)
