---
name: fixture-valid
description: Valid fixture for Skills v1 discovery and activation.
license: MIT
compatibility: POSIX shell
metadata:
  fixture: valid
  version: "1"
x-fixture-unknown: accepted-and-ignored
---

# Valid fixture

This body is loaded only after explicit activation of `fixture-valid`.

Read `references/guide.md` on demand. The script in `scripts/emit-fixture.sh`
is a fixture resource and must not run merely because this package is found or
activated.
