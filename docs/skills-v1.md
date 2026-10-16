# Leg Skills v1 contract

Status: normative v1 contract for issue #229. This document freezes package,
catalog, activation, resource, refresh, and budget semantics implemented by
Leg's Skills discovery and provider integration.

`AGENTS.md` and Skills are deliberately different layers. `AGENTS.md` is
always-on project instruction discovered for every turn. A Skill is a named,
selectable package whose metadata may be catalogued without loading its body;
its instructions are available only after explicit activation.

## Reference audit

The matrix records observed behavior, not automatic commitments. The final
column records the leg v1 decision where compatibility requires a deliberate
choice.

| Reference (pinned source and license) | Package and metadata | Discovery and precedence | Activation, resources, refresh, and security | Leg v1 decision |
| --- | --- | --- | --- | --- |
| [Pi coding agent 1.0.4](https://github.com/earendil-works/pi/tree/v1.0.4) at commit [`7c10bd4337495ee613f2224843ecdf349b80d1df`](https://github.com/earendil-works/pi/tree/7c10bd4337495ee613f2224843ecdf349b80d1df) (local package metadata; MIT), `docs/skills.md`, `docs/configuration.md`, `docs/packages.md` | A directory contains `SKILL.md`; `name` and `description` are portable fields, with optional Agent Skills fields. Pi supports scripts, references, and assets. | User and project `.pi/skills`, plus `~/.agents/skills` and project `.agents/skills` through ancestors. Malformed skills are skipped with warnings; first discovered duplicate wins. | Metadata and description are advertised first; body is loaded on demand. `/skill:name` explicitly activates; `/reload` refreshes. Project resources are reviewed as untrusted before trust. | Keep the package shape, progressive loading, warnings, and explicit activation. Make precedence and duplicate resolution deterministic rather than depending on traversal order. |
| [Codex CLI 0.162.0](https://github.com/openai/codex/releases/tag/rust-v0.162.0), upstream tag `rust-v0.162.0` at [`c1382380de69521303b416720a52f42d51af6248`](https://github.com/openai/codex/tree/c1382380de69521303b416720a52f42d51af6248) (Apache-2.0; bundled Ratatui material is separately MIT; see [official Skills docs](https://developers.openai.com/codex/skills)) | `SKILL.md` has YAML frontmatter; `name` and `description` are required by the parser, with `agents/openai.yaml` for optional UI/dependency metadata. Skill roots may contain scripts and other resources. | Project `.agents/skills`, `$CODEX_HOME/skills`, `~/.agents/skills`, system roots, and plugin roots are merged by scope. The implementation keeps root snapshots and reports load errors. | Explicit `$skill` selection and host skill reads use path identities; implicit selection can be disabled. Root snapshots keep a turn stable, and path authorities prevent resource escape. | Use the portable subset and per-turn snapshots. Do not import provider-specific `agents/openai.yaml`, implicit invocation, plugins, or tool-dependency permissions into v1. |
| [Agent Skills specification](https://agentskills.io/specification), `agentskills/agentskills` main at [`69ef37e9424c0a7ea9dd2293b559e43ec8176379`](https://github.com/agentskills/agentskills/tree/69ef37e9424c0a7ea9dd2293b559e43ec8176379) (Apache-2.0) | Required `SKILL.md`, `name` (1–64 lowercase letters/numbers/hyphens), and `description` (1–1024); optional `license`, `compatibility`, `metadata`, and experimental `allowed-tools`. Conventional `scripts/`, `references/`, and `assets/` directories are optional. | The standard defines the portable package shape but leaves host discovery and precedence to implementations. | Progressive disclosure is metadata, then instructions, then on-demand resources. Relative references are recommended; `allowed-tools` support varies. | Adopt the portable format and limits. Leg treats unknown fields as harmless metadata, rejects malformed required/typed fields, and never grants permissions from `allowed-tools`. |
| [Anthropic skills reference](https://github.com/anthropics/skills/tree/dbd4588f9e1033efb41dad4bef2f7947c8993d44), main at [`dbd4588f9e1033efb41dad4bef2f7947c8993d44`](https://github.com/anthropics/skills/tree/dbd4588f9e1033efb41dad4bef2f7947c8993d44) (README says many examples are Apache-2.0; document skills are source-available; no blanket top-level license) | Self-contained folders with `SKILL.md` frontmatter and instructions, plus scripts and resources. | The repository demonstrates installable skill sets and plugin packaging, but does not define leg's root precedence. | Skills are dynamically loaded for specialized work; individual skill licensing must be checked. | Use only the folder/documentation pattern as a behavioral reference. No Anthropic code or skill body is copied; any future borrowed asset must carry its own compatible license and attribution. |

The leg contract is original documentation. The references above contribute
ideas and compatibility constraints, not copied implementation code. A future
implementation must preserve the applicable license and attribution notices
when it copies any code or asset; the v1 implementation is not permitted to
copy code merely because a format is compatible.

## Package format

A Skill package is a directory below a configured Skills root. It contains one
required file and may contain supporting material:

```text
skill-name/
├── SKILL.md                 # required metadata and instructions
├── scripts/                 # optional; never run by discovery or activation
├── references/              # optional documentation, read on demand
├── assets/                  # optional data/templates, read on demand
└── ...                      # optional non-executable supporting files
```

`SKILL.md` must be the immediate child of the package directory. A root-level
`SKILL.md` is not a package. A recursive scan treats a directory containing
`SKILL.md` as a package and does not discover nested packages beneath it. A
provider-specific file such as `agents/openai.yaml` may be present, but leg v1
treats it as opaque data and gives it no UI, activation, or permission
semantics.

The file starts with YAML frontmatter delimited by `---` lines and then
contains Markdown instructions. The frontmatter fields are:

| Field | Required | v1 rule |
| --- | --- | --- |
| `name` | Yes | A string of 1–64 ASCII lowercase letters, digits, and single hyphens; it must match `^[a-z0-9]+(?:-[a-z0-9]+)*$` and must equal the package directory name. |
| `description` | Yes | A non-empty string of at most 1024 characters that says what the Skill does and when it applies. |
| `license` | No | A short license name or a relative bundled license-file name. It is descriptive only. |
| `compatibility` | No | A string of 1–500 characters describing environment requirements. |
| `metadata` | No | A mapping whose keys and values are strings. It is exposed as catalog metadata, not policy. |

Unknown top-level fields, including the standard's experimental
`allowed-tools`, are accepted and ignored by leg v1. They do not appear as
authorization or tool grants. A missing delimiter, invalid YAML, missing or
wrongly typed required field, invalid optional field, name/directory mismatch,
or name/length violation makes the package invalid. Invalid packages are
skipped with a diagnostic; they do not prevent other packages in the same root
from being discovered.

## Discovery scopes and precedence

Leg v1 recognizes exactly these filesystem roots. Missing roots are skipped.
`$CODEX_HOME` defaults to `~/.codex` when unset.

| Priority | Scope | Roots |
| ---: | --- | --- |
| 1 | Project | For every directory from the repository root through the current working directory, discover `.agents/skills`, then `.codex/skills`, `.claude/skills`, and `.pi/skills`. Without a repository, use only the current directory. |
| 2 | User | `~/.agents/skills`. |
| 3 | User compatibility | `$CODEX_HOME/skills`, `~/.claude/skills`, and `~/.pi/agent/skills`, in that order. |

The effective ordering is nearest project directory first, then the root-kind
order shown in that row; all project roots outrank all user roots. Within one
root, packages are sorted by normalized relative path. The canonical path of a
package is used to collapse the same package reached through equivalent or
symlinked roots. System/admin roots, plugin manifests, network locations, and
arbitrary configured directories are outside v1.

For duplicate valid names, the first package in this ordering wins. If two
packages still have the same priority and normalized path order, the
lexicographically smaller canonical path wins. The catalog contains only the
winner and emits a duplicate warning naming both candidates. An invalid
package does not reserve its name, so a lower-priority valid package may win.

Discovery records warnings but remains usable when one package is unreadable,
malformed, or duplicated. A warning is not an activation entry.

## Catalog representation

Discovery creates one per-turn snapshot. The provider-facing catalog has this
shape (fields with no value are omitted):

```json
{
  "schema": "leg.skills.catalog/v1",
  "snapshot_id": "opaque-snapshot-id",
  "skills": [
    {
      "id": "opaque-entry-id",
      "name": "pdf-tools",
      "description": "Extract and inspect PDF documents.",
      "scope": "project",
      "source": "project:.agents/skills",
      "license": "Apache-2.0",
      "compatibility": "Requires Python 3",
      "metadata": {"owner": "example"}
    }
  ],
  "warnings": []
}
```

`schema`, `snapshot_id`, `skills`, and `warnings` are required catalog fields.
Each entry requires `id`, `name`, `description`, `scope`, and `source`; the
optional fields mirror valid frontmatter. `id` is opaque and valid only for
the snapshot. `source` is a stable scope/root label, not an absolute
filesystem path. Catalog entries never contain the Markdown body, absolute
paths, or a script list.

Entries are emitted in effective precedence order, then by name. If the
catalog budget cannot fit another complete entry, the entry is omitted and a
`catalog_budget_exceeded` warning is emitted; metadata is never partly
serialized.

## Explicit activation and progressive resources

The host exposes the following provider-neutral operation:

```text
activate_skill(snapshot_id, name)
```

The caller must explicitly select an exact catalog name from the current
snapshot. On success, the host binds that catalog entry to its private package
identity and loads exactly that package's complete `SKILL.md` body. The
provider tool spelling is `activate_skill(snapshot_id, name)`: both values
must match the current catalog, and the selected package must still match its
discovered identity. The tool result is a short acknowledgement; the body is
available only in the next composed provider request. It does not load another
Skill, scan a neighboring package, or run a script. Fuzzy matching,
prompt-triggered implicit activation, and path-based activation are not v1
behavior.

The host may then read a selected package resource with:

```text
read_skill_resource(snapshot_id, name, relative_path)
```

`relative_path` must be a normalized relative path below the selected package
root. Absolute paths, empty paths, `..` traversal, alternate separators that
escape the root, non-regular files, and symlinks whose target leaves the
package root are rejected. Resources are read only when requested by the
activated instructions. The provider tool spelling is
`read_skill_resource(snapshot_id, name, relative_path)` and requires that the
Skill be activated in the current turn. Its result is also an acknowledgement;
resource text is available only in the next composed provider request. A
script remains a file; activation never executes it and frontmatter never
grants a tool permission.

Activation fails without injecting partial instructions when the name is
missing, the snapshot is stale, the selected package changed after the
snapshot, the resource escapes its root, or the complete content exceeds the
remaining context budget. These failures are explicit diagnostics, not a
fallback to a different package.

## Refresh, context, and instruction precedence

- The host discovers `AGENTS.md` and Skills at the start of each turn. The
  catalog and selected-package identity are one immutable snapshot across all
  provider calls and tool rounds in that turn.
- The next turn creates a fresh snapshot, so added, removed, or changed Skills
  take effect then. Activations do not carry across turns or resumed sessions;
  the model must activate again against the fresh catalog snapshot. A refresh
  command may discard the current snapshot before a new turn, but cannot mutate
  an active tool continuation.
- The prompt catalog is capped at 16 KiB of serialized metadata. A complete
  `SKILL.md` is capped at 64 KiB, each resource at 64 KiB, and resource paths
  at 512 bytes. Combined activated instructions and resources are capped at
  256 KiB per turn. Larger content fails with a bounded tool diagnostic and is
  never partly injected.
- When a context window is configured, catalog metadata, the activated body,
  and on-demand resources consume the host's context budget. Leg estimates
  input tokens as the ceiling of serialized provider-request bytes divided by
  three, then applies its observed calibration factor. At activation or
  resource-read time it reserves the complete added content plus the tool call
  response tokens and acknowledgement against the remaining input allowance;
  reported response usage is used when available, otherwise the response is
  estimated from its serialized content. Overflow returns a bounded tool error
  before state changes; the pre-dispatch request guard remains the backstop for
  the complete composed prompt. Without a configured context window, the fixed
  byte limits above still apply.
- Injected Skill bodies, catalog entries, and resource contents are not copied
  into session history or trail records as hidden instructions. Explicit tool
  calls and their ordinary results remain subject to the existing trail rules.
- Instruction precedence is: system policy, developer policy, project
  `AGENTS.md`, the caller's current request, then activated Skill instructions
  and resources. A Skill cannot weaken or replace a higher layer, disable
  `AGENTS.md`, grant permissions, or turn unselected text into instructions.

Discovery and cataloguing are read-only. A Skill can describe commands or
scripts, but executing them still requires an ordinary, separately authorized
tool call under the host's existing policy. Untrusted package content is
instruction data, not a trust grant.

## Executable fixture corpus

The implementation fixtures live in
[`tests/fixtures/skills-v1/`](../tests/fixtures/skills-v1/). From the
repository root, run:

```bash
bash tests/fixtures/skills-v1/check.sh
```

This check intentionally validates only the fixture corpus and its
`expected-outcomes.txt` manifest: required files, frontmatter markers,
distinctive body/resource markers, executable bits, and duplicate-scope
markers. It does not parse Skills, discover directories, activate a package,
invoke a provider, or claim runtime behavior. A drifted fixture or manifest
causes a non-zero exit.

The contract-level expected outcomes are:

| Fixture | Discovery/catalog outcome | Activation/resource outcome |
| --- | --- | --- |
| `valid/fixture-valid` | One catalog entry named `fixture-valid`; the unknown frontmatter key is ignored. | Activation returns only its `SKILL.md` body. `references/guide.md` is readable on demand; `scripts/emit-fixture.sh` is not run automatically. |
| `malformed/fixture-malformed` | No catalog entry; one invalid-package warning. | Activation is unavailable because invalid packages are never catalogued. |
| `duplicate/project/.../fixture-duplicate` plus `duplicate/user/.../fixture-duplicate` | One `fixture-duplicate` entry; the project package wins and the user package is named in a duplicate warning. | Activation returns the project marker; the user body is never loaded. |

The fixture self-check remains deliberately weaker than the runtime tests.
Rust tests exercise discovery, activation, resource confinement, refresh, and
budget behavior against these fixtures and temporary package trees.

## Non-goals

Skills v1 does not add hooks, subagents, plugins, marketplace behavior,
automatic script execution, or changes to `AGENTS.md` semantics. Frontmatter
does not grant tools or permissions.
