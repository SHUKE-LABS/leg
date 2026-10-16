# Skills v1 fixture corpus

Run this self-check from the repository root:

```bash
bash tests/fixtures/skills-v1/check.sh
```

The check validates only that the fixture files and
`expected-outcomes.txt` manifest are present and internally marked for the
Skills v1 contract. It does not implement discovery, parse YAML, activate a
Skill, run a provider, or test runtime behavior.

The corpus contains:

- `valid/fixture-valid`: valid metadata, a reference, and a script whose
  executable bit is part of the fixture.
- `malformed/fixture-malformed`: deliberately invalid frontmatter.
- `duplicate/project/.../fixture-duplicate` and
  `duplicate/user/.../fixture-duplicate`: the same valid name in two scopes;
  project is the expected winner.

The normative discovery, catalog, activation, and resource outcomes are
recorded in `expected-outcomes.txt` and described in
[`docs/skills-v1.md`](../../../docs/skills-v1.md).
