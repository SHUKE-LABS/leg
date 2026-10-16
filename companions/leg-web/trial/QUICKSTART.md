# Leg Web trial bundle

This is an experimental, local-only interface. It is not the selected public
interface for leg. The host binds to loopback and opens a one-time authenticated
URL. Tools run as your operating-system user; the selected workspace is a
working directory, not a sandbox. Do not use a personal workspace or real
provider key for the trial.

## Prerequisites

- A supported Linux or macOS machine matching this bundle's architecture.
- A current browser with JavaScript enabled.
- Python 3 for the local deterministic fixture. No Python package is needed.

The Web host embeds its page, JavaScript, and CSS. Running it does not need
Cargo, Node, a CDN, or an Internet connection. A real provider can be selected
through the usual `leg` environment settings, but the shared trial fixture
needs no paid API or real key.

## Build locally

From the repository root, run:

```sh
bash scripts/build-web-trial.sh --output /tmp/leg-web-trial-build
```

The command builds the core binary with the root `Cargo.lock` and the Web host
and supervisor with `companions/Cargo.lock`, both in locked mode. Build
prerequisites are Rust 1.89 or newer, Node.js 22 for rendering the dependency
notices, Python 3, and `shasum` or `sha256sum`. The generated archive records
the source revision and target. These tools are not needed to run an unpacked
bundle.

## Start the deterministic trial

In the directory where you downloaded the CI artifact, unpack its Web bundle
archive and enter the new directory:

```sh
set -- leg-web-experimental-*.tar.gz
test -f "$1"
archive=$1
bundle=${archive%.tar.gz}
tar -xzf "$archive"
cd "$bundle"
pwd
```

Make a new disposable workspace outside your personal files. Start the local
fixture from the unpacked bundle directory:

```sh
mkdir -p /tmp/leg-web-trial-workspace
python3 companions/trials/fake_provider.py \
  --scenario trial \
  --workspace /tmp/leg-web-trial-workspace \
  --port 8765
```

The fixture prints environment settings for the local fake provider. In a
second terminal, change to the bundle directory shown by `pwd` above, save its
path for cleanup, apply the printed settings to that shell, then start the Web
host:

```sh
bundle_dir=$PWD
./start-web.sh --no-open --state-dir "$PWD/.trial-state"
```

Open the one-time URL printed by the host. Keep both terminals open while
working. `--no-open` leaves the browser launch to you and still prints the
authenticated local URL. The URL fragment contains a launch token; do not
share it. Stop the host with Ctrl-C, then stop the fixture with Ctrl-C.

## Shared tasks and report

The full task script and safety gates are in `docs/ui-experiments.md`. Fixture
scenarios and their expected workspace effects are documented there as well.
Use a fresh disposable workspace for each interface. Copy
`companions/trials/results-template.json` for human observations; leave unavailable
observations unmeasured and do not recommend a winner from fixture results.

The host does not send telemetry. The fixture reports request counts and
expected disposable-workspace effects; it does not save prompts, keys, or
transcripts. Bundle revision metadata is in `bundle-info.json`. The Linux CI
artifact also includes machine measurements and deterministic gate results;
locally built archives contain the bundle only.

## Remove the bundle

Stop the host and fixture. From the directory that contains the unpacked
bundle, remove it and the fixture workspace:

```sh
cd ..
rm -rf "$bundle_dir"
rm -rf /tmp/leg-web-trial-workspace
```

The selected state directory stays inside the unpacked bundle, separate from
the normal leg catalog. Removing the unpacked bundle removes the trial catalog;
keep any other leg data.
