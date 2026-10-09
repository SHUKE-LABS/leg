# Matched UI experiments

This protocol compares the TUI and Web companion interfaces on the same work.
It is a local trial tool, separate from `leg` and both shipped UIs. It adds no
Cargo dependency and makes no network request beyond loopback. Trial completion
and public release are separate: the umbrella is complete when its child issues
are delivered; shuke chooses whether to release after reviewing comparable
evidence.

## Deterministic local provider

Use Python 3; the runner uses only the standard library and serves the
Anthropic Messages streaming endpoint expected by `leg`. Start it with the
workspace copy assigned to that interface:

```sh
python3 companions/trials/fake_provider.py --scenario trial --workspace /absolute/path/to/disposable-workspace --port 8765
```

The runner binds to `127.0.0.1` on the selected port, prints Bash/zsh and PowerShell
environment blocks, and stays open until Ctrl-C. Copy the block for the shell
that launches the UI. It sets `ANTHROPIC_BASE_URL`, a fake-only
`ANTHROPIC_API_KEY`, `LEG_PROVIDER`, `LEG_MODEL`, disabled provider retries,
and the trial's tool-round, bash-timeout, and deny-hook settings. No real key is
needed. The `--workspace` directory must be the same directory selected in the
UI; the status endpoint checks expected files there.

The combined `trial` scenario sets `LEG_BASH_TIMEOUT_SECS=600`, twice the
task's 300-second deadline, so Bash cannot auto-stop before the manual Stop
window ends. The `browser` scenario is automated and `stalled-bash` is a
separate gate scenario; both keep their 120-second timeout because neither uses
that manual Stop window.

Read status while the runner is open at the printed `/__trial/status` URL, for
example:

```sh
curl http://127.0.0.1:PORT/__trial/status
```

The status contains request counts, fixed simulated provider waits, prompt
shape booleans, and workspace-effect checks. It never stores prompts, headers,
credentials, or transcripts. Ctrl-C prints the final status and removes the
temporary deny hook. Use a fresh matched pair of disposable workspace copies
for each participant. Run the two interfaces sequentially and reuse the same
free port so their emitted base URL matches. The default port `0` selects a
free port for a one-off fixture. Do not point a trial at a personal workspace.

The combined `trial` scenario recognizes the `TRIAL-*` task prompts below.
Individual behavior can also be run directly, for example:

```sh
python3 companions/trials/fake_provider.py --scenario text-tool-text --workspace /absolute/path/to/disposable-workspace
```

| Scenario | Deterministic behavior | Verification |
| --- | --- | --- |
| `first-answer` | Returns one local answer. | A complete answer arrives without a paid API call. |
| `chinese-multiline` | Checks the expected three lines, including Chinese text. | `input_checks.chinese_multiline_prompt` is true. |
| `text-tool-text` | Streams text, writes `fixture-write.txt`, then streams final text. | Tool result is returned and file equals `fixture-write-ok\n`. |
| `denied-tool` | The temporary hook denies a marked bash call. | Error result is returned and `denied-marker.txt` is absent. |
| `failed-tool` | Bash rejects a negative timeout before execution. | The provider receives an error tool result and `failed-marker.txt` stays absent. |
| `auth-error` | Returns a deterministic HTTP 401. | The UI reports provider authentication failure. |
| `capped-tool-loop` | Requests a harmless bash append on each round. | With the emitted two-round cap, the file contains exactly rounds 1 and 2. |
| `paused-live-text` | Sends text, pauses for exactly 1,200 ms, then finishes. | Text is visible during the pause; status records the wait. |
| `stalled-bash` | Starts a long child process and writes its PID. | After Stop, status checks the PID is gone and no finish marker exists. |
| `reopen-after-failure` | First request returns 401; the next succeeds. | Reopening and explicitly retrying returns an answer. |
| `reopen-after-interruption` | First response closes after partial live text; the next succeeds. | Reopening and repeating the prompt returns an answer. |

The Linux PTY smoke test opts into a fixture gate that holds
`paused-live-text` after its first chunk until the test releases it. The
fixture reports the active held request and exposes a release endpoint for
bounded cleanup. This is test synchronization, not a human measurement
scenario; without the opt-in `--hold-after-first-chunk` flag, the documented
1,200 ms pause and Web/trial timing remain unchanged.

For `trial`, task prompts also cover Chinese/multiline input, a four-second
visible bash run, denial, Stop, reopen, explicit retry, and a 180-line answer.
Run the denied-hook and bash scenarios on a Unix-like host with Python and bash
available. Both candidates in a pair must use the same OS, device, model/config,
fixture version, and matched workspace baseline. Record any platform limitation
in the results.

The fixture can prove deterministic behavior such as tool dispatch, error
reporting, and workspace effects. Those synthetic checks can establish the
deterministic eligibility gates below, but cannot replace human measurements.

## Shared setup and task script

Facilitator note: the TUI has a searchable action palette at F2 or Ctrl-P;
disabled entries explain why they are unavailable, and the footer reflects the
view's enabled actions. Keep task prompts location-neutral and do not direct the
participant to a particular key or menu; record whether they discover actions
through the interface.

Prepare two disposable copies of one baseline workspace, one for each UI. Before
each participant begins, seed both catalogs with the same prior session by
sending `TRIAL-SEED-SESSION: save this as the prior session seed.` The fixture
answers `Fixture seed: blue lantern.` Do not use personal transcripts. Start
the local provider for each copy and have the participant set up the assigned
UI using this document. Setup starts when the participant first opens the setup
instructions and stops when the first complete answer is visible. Documentation
time counts. The setup limit is 10 minutes; exceeding it or receiving any
facilitator help is a setup failure.

For every task, start timing when the participant receives its instruction and
stop at the stated observable outcome. The active-time limit is 5 minutes per
task. An exceeded limit or facilitator help is a failure. Keep the full wall
elapsed time in milliseconds and separately record the fixture's exact
`simulated_provider_wait_ms`; task active time is `max(1, elapsed_ms -
provider_wait_ms)`. If no fixture pause occurred, record provider wait as 0. If
the task has several provider requests, sum the wait entries for its marker. If
the wait was not measured, leave it `null`. Setup uses wall time and does not
subtract provider wait. The `paused-live-text` gate scenario has one fixed
1,200 ms wait; status identifies it by task marker and request number. No
scored task adds a pause. For the manual Stop task, the combined `trial`
scenario's 600-second bash timeout is longer than its 300-second task limit.
If the deadline passes and automatic timeout cleanup stops the command, record
a task failure; cleanup after the deadline is not a successful Stop measurement.

Give the participant these tasks, in this order, without suggesting where a
control should be or how it should look:

1. **Compose and send multiline text.** Send this as one prompt, preserving all
   lines:

   ```text
   TRIAL-CHINESE
   Line one: keep this first.
   第二行：保留中文。
   Line three: keep this third.
   ```

   Success: the fixture status says `chinese_multiline_prompt: true` and the
   complete response is visible.

2. **Continue.** In the same conversation, send
   `TRIAL-CONTINUE: continue the conversation and confirm the earlier answer is still present.`
   Success: the follow-up response is visible in the existing conversation.

3. **Find a prior session.** Find the seeded session and show its answer
   `Fixture seed: blue lantern.` Success: the participant opens the correct
   prior session and the answer is visible.

4. **Identify a running tool.** Send
   `TRIAL-RUNNING: run the short fixture command and tell me when it is done.`
   Success: while it runs, the participant identifies the active bash tool; the
   run then finishes and `trial-running-tool-finished.txt` exists.

5. **Inspect a denied tool.** Send
   `TRIAL-DENIED: try the marked fixture command and explain its result.`
   Success: the participant locates the denial reason, the fixture reports an
   error tool result, and `denied-marker.txt` does not exist. The separate
   `failed-tool` scenario checks negative-timeout rejection before execution;
   status confirms an error tool result and no `failed-marker.txt`.

6. **Stop.** Send `TRIAL-STOP: start the fixture's stalled command.` Once the
   tool is visibly running and `trial-stalled-child.pid` exists, use the UI's
   Stop action. Success: the turn is visibly stopped, `/__trial/status` reports
   the child process gone, and `trial-stall-finished.txt` is absent.

7. **Reopen after interruption.** Reopen that interrupted session and send
   `TRIAL-REOPEN-INTERRUPTION: repeat this prompt after reopening.` The fixture
   closes the first response after partial text; reopen the session and submit
   the same prompt again. Success: the incomplete attempt remains identifiable
   and the repeated prompt receives a complete answer.

8. **Explicitly retry.** Send
   `TRIAL-RETRY: return a fixture failure, then wait for my explicit retry.`
   After the 401, use the UI's explicit retry action. Success: a complete answer
   appears only after that action.

9. **Browse and copy a long answer.** Send
   `TRIAL-LONG: provide the long fixture answer.` Browse to the final line,
   copy `END OF FIXTURE ANSWER`, then return to an earlier place in the answer.
   Success: the copied text is exact and the earlier reading position is still
   available.

After each task, record success, elapsed milliseconds, fixture provider-wait
milliseconds, and a 1–7 ease rating (1 = very difficult, 7 = very easy). Ease
may be null if not recorded. A task failure or missing task result contributes
zero to completion, time, and ease; keep missing raw values null. Set
`full_script_attempted` true only after the participant tried all nine tasks in
that interface; a failed task still counts as attempted, but a skipped task
does not. Add one anonymous object per
participant under `participants`; include `id`, `leg_unfamiliar`,
`interface_order` (`TUI-Web` or `Web-TUI`),
`full_script_attempted: {"TUI": boolean, "Web": boolean}`, a `setup` object,
and a `tasks` object keyed by these IDs: `compose_send_multiline`, `continue`,
`find_prior_session`, `identify_running_tool`,
`inspect_failed_or_denied_tool`, `stop_running_tool`,
`reopen_after_failure_or_interruption`, `explicit_retry`, and
`browse_copy_long_answer`. Each setup measurement has `success` and
`elapsed_ms`; each task measurement has `success`, `elapsed_ms`,
`provider_wait_ms`, and `ease`. The setup object and each task entry map the
interface names `TUI` and `Web` to their measurements. Use null for any missing
raw value.

## Paired protocol and eligibility

Each participant uses both interfaces. Counterbalance interface order using
alternating assignments (TUI then Web, Web then TUI), keeping task order and
instructions fixed. Match the tasks, model/config, hardware, OS, and baseline
workspace. Record the exact core and UI revisions, device/OS, provider/model/
config, workspace fixture revision, session length, and interface order in the
result template. Use anonymous participant IDs; record leg familiarity only as
whether the participant has ever used leg hands-on.

Before recommending a scored winner, collect at least five participants who
attempt both complete scripts, including at least two who have never used leg
hands-on. With fewer, report scores as exploratory observations and leave the
winner undecided. Synthetic checks do not count as paired participants.

Each interface must pass every applicable deterministic gate:

- all common critical workflows pass;
- no accidental submit or replay;
- committed history is preserved;
- Stop leaves no tool process running;
- no provider credential is exposed; and
- the Web interface rejects mutations from an unauthenticated request.

Use `true` only when a gate passed, `false` when it failed, and `null` when it
was not measured. For TUI, set the Web-only authentication gate to `n/a`. An
unmeasured applicable gate does not pass. Human measurements are still required
even when synthetic checks establish all gates.

## Predeclared score

For each interface, calculate a score out of 100:

- **Completion (40 points):** `40 × successful tasks / (9 × participant count)`.
- **Time (25 points):** for each task, take the median across participants of
  its paired speed ratio; average the nine task medians, then multiply by 25.
  When both interfaces succeed, interface `I` has ratio
  `min(active_TUI_ms, active_Web_ms) / active_I_ms`. If only one succeeds, its
  ratio is 1 and the other's is 0. If neither succeeds, both are 0. Missing
  timing data contributes 0 to time; when a successful task has no required
  timing data, both paired ratios are 0. Keep every pair in that task's median.
  Task active time excludes the fixture's measured provider wait; setup time
  does not.
- **Ease (25 points):** `25 × mean((rating - 1) / 6)` across all nine tasks and
  participants. Failed tasks and missing ratings contribute 0.
- **Setup (10 points):** `10 × median(paired setup speed ratio)`, including
  documentation time and using wall time. Apply the same paired success rules;
  when both succeed, ratio for interface `I` is
  `min(setup_TUI_ms, setup_Web_ms) / setup_I_ms`.

Record durations as positive integer milliseconds. The calculator uses a 1 ms
floor for a duration after provider-wait subtraction. Keep absent raw values as
JSON `null`; do not fill them with estimates. The score denominator includes
all participant rows and all nine tasks, so failed/missing observations remain
zero rather than disappearing from an average. A recommendation also requires
every included participant row to show both full scripts attempted; a partial
participant may stay in exploratory raw data but prevents an overall
recommendation until the gate is satisfied.

Worked example: for one paired task, if TUI takes 4,000 active ms and Web takes
5,000, their ratios are 1.0 and 0.8. If TUI fails and Web succeeds on another
participant, that pair contributes 0 and 1. The per-task medians are computed
from both pairs, so they are 0.5 for TUI and 0.9 for Web. For a full illustrative
score, suppose TUI completes 80% of tasks, has mean task median ratio 0.80,
mean normalized ease 0.75, and setup ratio 0.90: its points are 32 + 20 +
18.75 + 9 = **79.75**. If Web's corresponding values are 90%, 0.70, 0.80, and
0.70, its points are 36 + 17.5 + 20 + 7 = **80.50**. Both candidates pass the
eligibility gates, so this 0.75-point gap leaves selection to shuke.

If both interfaces succeed on a task but either active time is missing, both
paired ratios are 0; this also applies when both timings are missing. Keep that
pair in the task median. Setup uses the same rule with wall time.

Run the executable calculator self-check and calculate a completed result file:

```sh
python3 companions/trials/score.py --self-check
python3 companions/trials/score.py trial-results.json
```

The self-check includes explicit-null validation, missing/failed measurements
in the denominator, provider-wait subtraction, the 1 ms floor, participant
gating, the exact-one-eligible rule, and the 5-point threshold. The calculator
withholds any overall recommendation until the paired participant gate passes.
Then it recommends the sole eligible candidate if exactly one passes; if both
pass, it recommends the higher score only when the gap is at least 5 points. If
neither passes or the gap is smaller, it leaves selection to shuke.

Run the focused fixture checks with:

```sh
python3 -m unittest companions/trials/test_fake_provider.py
```

## Results and reuse

Copy `companions/trials/results-template.json` for each trial report. It
separates revision/environment context, deterministic gates, raw anonymous
paired measurements, calculated scores, setup friction, startup time/idle
RSS/bundle size, and limitations. Do not add names, credentials, or personal
transcripts. Leave unavailable measurements as `null` or note them as
unmeasured. Record sanitized provider configuration without credential values,
and paste the calculator output into `calculated_scores`. Never create
participant records from fixture runs.

The final TUI and Web trial tickets share one paired result file. For each
participant, use separate fresh copies of the baseline workspace, but keep the
provider port fixed and run the fixture sequentially for each interface:

```sh
cp companions/trials/results-template.json paired-trial-results.json
python3 companions/trials/fake_provider.py --scenario trial --workspace "$TUI_WORKSPACE" --port 8765
# Stop the TUI fixture, then start the Web fixture.
python3 companions/trials/fake_provider.py --scenario trial --workspace "$WEB_WORKSPACE" --port 8765
# After recording both interfaces' paired measurements:
python3 companions/trials/score.py paired-trial-results.json
```

Fill raw paired observations only after human sessions. Each ticket can run its
fixture command; run the calculator after both interfaces' measurements are in
the shared file. The umbrella ends when both child trial tickets are delivered.
Evidence informs shuke's separate public-release decision.

## Resource measurements

Measure TUI and Web on the same reference machine, OS, workspace fixture,
session length, and browser build. Record the OS version, CPU model and
architecture, physical memory, browser/version, core/UI revisions, and the
exact bundle files. These resource measurements describe setup cost; they do
not determine a usability winner.

Startup time begins when the interface process starts and ends when its first
workspace-selection control is visible. For TUI, use the first rendered
workspace chooser. For Web, start the host, open its printed authenticated URL
in the selected browser, and stop when the workspace field is visible.

For idle RSS, leave the initial screen idle for 10 seconds, then sample once
per second for 10 samples with
[`companions/trials/sample_process_rss.py`](../companions/trials/sample_process_rss.py).
The script uses `ps -axo pid=,ppid=,rss=,command=` and sums RSS for each
specified process root and its descendants; report the median sampled sum.
Count the TUI process tree rooted at `leg-tui`. For Web, count the `leg-web`
host tree and the browser process tree rooted at the browser process (not the
Playwright driver). Also measure that same browser with `about:blank` and no
Web host. Record the combined Web RSS, browser-only baseline, and their
difference; the difference is Web's incremental idle RSS. Example sampling
commands, once each PID has been identified:

```sh
python3 companions/trials/sample_process_rss.py --root-pid "$TUI_PID"
python3 companions/trials/sample_process_rss.py --root-pid "$WEB_PID" --root-pid "$BROWSER_PID"
python3 companions/trials/sample_process_rss.py --root-pid "$BROWSER_PID"
```

Record bundle size as the exact byte length of the compressed trial archive.
Use Python's `os.path.getsize()` or an equivalent byte-counting command. Keep
both archives and all measurements from the same source revisions and host.
The Web report records the browser-only baseline separately so its incremental
host-and-page cost can be interpreted.

## TUI responsiveness harness (#182)

The test-only harness measures key injection to rendered terminal cells on a
native PTY. Windows uses pywinpty's ConPTY backend; Linux/macOS use the native
PTY. Its fixture creates 1,000
history turns with 4 KiB replies, then adds one 10,000-line answer and one
1 MiB tool result. It runs background stream pairs; each pair has two sessions
that each emit 6,000 32-byte chunks at 200 chunks/second for 30 seconds. The
harness starts another pair as needed, and every timed sample must have both its
input and visible-output timestamps inside that pair's overlap. Untimed session
switching may occur between pairs.

Use Python 3.12 and build the three binaries in release mode. On Windows, run
from Git Bash/MSYS2. The timing harness uses pywinpty 3.0.5; the behavior
harness uses a separate pywinpty 2.0.15 environment. Version 2.0.15 batches
ConPTY reads for about 100 ms, so it cannot measure the 100 ms input-latency
gate accurately. With 3.0.5, `PtyProcess.write()` returns 0 while a raw child
still receives the complete input, including non-BMP code points; however, the
TUI's bracketed-paste path drops the emoji under that transport. The same TUI
paste check passes under 2.0.15, so CI uses 3.0.5 for timing and 2.0.15 for
behavior checks. Reports identify the pywinpty version, Windows build, and the
direct ConPTY setup; the Windows job also checks that a 3.0.5 write returning
0 reaches a child as the exact Unicode code points. There is no Windows
Terminal frontend. The memory sampler uses psutil to sum the `leg-tui` process
tree and excludes the harness and fixture.

On Windows, install the test requirements in a task-specific environment:

```sh
LEG_TUI_RESPONSIVENESS_VENV="$(cygpath -u "${TEMP}")/leg-tui-responsiveness-venv"
LEG_TUI_WINDOWS_BEHAVIOR_VENV="$(cygpath -u "${TEMP}")/leg-tui-windows-behavior-venv"
python -m venv "${LEG_TUI_RESPONSIVENESS_VENV}"
python -m venv "${LEG_TUI_WINDOWS_BEHAVIOR_VENV}"
"${LEG_TUI_RESPONSIVENESS_VENV}/Scripts/python.exe" -m pip install \
  --requirement companions/leg-tui/tests/requirements.txt
"${LEG_TUI_WINDOWS_BEHAVIOR_VENV}/Scripts/python.exe" -m pip install \
  --requirement companions/leg-tui/tests/requirements.txt
"${LEG_TUI_WINDOWS_BEHAVIOR_VENV}/Scripts/python.exe" -m pip install \
  --force-reinstall pywinpty==2.0.15
cargo build --locked --release --bin leg
cargo build --locked --manifest-path companions/Cargo.toml --release \
  -p leg-ui-client --bin leg-ui-supervisor
cargo build --locked --manifest-path companions/Cargo.toml --release -p leg-tui
"${LEG_TUI_RESPONSIVENESS_VENV}/Scripts/python.exe" \
  companions/leg-tui/tests/responsiveness_harness.py \
  --tui-bin companions/target/release/leg-tui.exe \
  --leg-bin target/release/leg.exe \
  --supervisor-bin companions/target/release/leg-ui-supervisor.exe \
  --build-profile release \
  --output companions/leg-tui/tests/reports/responsiveness-windows.json

LEG_TUI_REPORT_DIR="$(cygpath -u "${TEMP}")/leg-tui-183"
TUI_WINDOWS_BEHAVIOR_PYTHON="$(cygpath -w "${LEG_TUI_WINDOWS_BEHAVIOR_VENV}/Scripts/python.exe")" \
"${LEG_TUI_RESPONSIVENESS_VENV}/Scripts/python.exe" \
  companions/leg-tui/tests/compare_responsiveness.py \
  --host windows \
  --baseline-revision b5444f9f409a6edbab7cd02ed776a7df36e6e412 \
  --final-revision "$(git rev-parse HEAD)" \
  --pull-request-head-revision "$(git rev-parse HEAD)" \
  --tui-bin companions/target/release/leg-tui.exe \
  --leg-bin target/release/leg.exe \
  --supervisor-bin companions/target/release/leg-ui-supervisor.exe \
  --output-dir "${LEG_TUI_REPORT_DIR}"

"${LEG_TUI_WINDOWS_BEHAVIOR_VENV}/Scripts/python.exe" \
  companions/leg-tui/tests/windows_behavior_harness.py \
  --tui-bin companions/target/release/leg-tui.exe \
  --leg-bin target/release/leg.exe \
  --supervisor-bin companions/target/release/leg-ui-supervisor.exe \
  --source-revision "$(git rev-parse HEAD)" \
  --output "${LEG_TUI_REPORT_DIR}/windows-behavior-standalone.json"
```

On Linux/macOS, install the same requirements and use the native PTY binaries:

```sh
LEG_TUI_RESPONSIVENESS_VENV="${TMPDIR:-/tmp}/leg-tui-responsiveness-venv"
python3 -m venv "${LEG_TUI_RESPONSIVENESS_VENV}"
"${LEG_TUI_RESPONSIVENESS_VENV}/bin/python" -m pip install \
  --requirement companions/leg-tui/tests/requirements.txt
cargo build --locked --release --bin leg
cargo build --locked --manifest-path companions/Cargo.toml --release \
  -p leg-ui-client --bin leg-ui-supervisor
cargo build --locked --manifest-path companions/Cargo.toml --release -p leg-tui
"${LEG_TUI_RESPONSIVENESS_VENV}/bin/python" \
  companions/leg-tui/tests/responsiveness_harness.py \
  --tui-bin companions/target/release/leg-tui \
  --leg-bin target/release/leg \
  --supervisor-bin companions/target/release/leg-ui-supervisor \
  --build-profile release \
  --output companions/leg-tui/tests/reports/responsiveness-unix.json
```

Each report contains at least 100 raw samples for every action/size pair:
draft editing, history scrolling, palette filtering, and inspection at 80x24
and 120x40, with p95/max per group.
Sample records include provider chunk emit timestamps before and after each
input; the provider's actual inter-chunk intervals are reported separately
from UI latency. Each sample identifies its stream pair, and the report records
each pair's stream IDs and overlap interval. The report also separates first
catalog load from cached history/inspector opening, and records startup,
idle/peak process-tree RSS, revision, OS/CPU, terminal transport/version,
dimensions, and build profile.
For pull request CI runs, `source_revision` is GitHub Actions' `github.sha`
(the merge commit tested by CI). The comparison report records both that
revision and `pull_request_head_revision`, the PR branch SHA.
Ordinary CI runs upload separate `leg-tui-responsiveness-linux-*` and
`leg-tui-responsiveness-windows-*` artifacts. A PR whose body contains
`Closes #183` or `Part of #183` runs the paired baseline/final gate on native
Linux and Windows instead. It builds baseline
`b5444f9f409a6edbab7cd02ed776a7df36e6e412` and the PR revision in release mode
on each same runner, then uploads each report and
its command logs as `leg-tui-responsiveness-comparison-<os>-*`.

Run 37853682089 recorded these baseline-to-final p95 input latencies (80x24 /
120x40): Linux draft editing 67.7/77.4 ms, history scrolling 5.9/7.8 ms, and
inspection 11.2/13.8 ms; Windows draft editing 227/285 ms, history scrolling
16.4/19.2 ms, and inspection 23.8/25.1 ms. The baseline has no command palette,
so palette filtering is `not_comparable`. Final p95 across the eight comparable
action/size groups was 32.6–39.7 ms on Linux and 47.4–54.3 ms on Windows. The
cached-history / inspector opening times were 8.165/13.216 ms on the Linux
baseline and 34.412/39.663 ms on the Linux final revision; Windows measured
11.205/24.704 ms on the baseline and 46.437/54.149 ms on the final revision.
Final idle CPU was 0.033% of one logical CPU on Linux and 0.104% on Windows.

The #183 final revision passes only when every action/size group has at least
100 samples and p95 input-to-visible-output latency is at most 100 ms, cached
history and inspector opening each take at most 200 ms, and idle TUI CPU stays
at or below 1% of one logical CPU during a fixed 30-second window. The CPU
report records requested and actual elapsed time plus raw user/system counters.
A Windows baseline that cannot run the workload may be recorded with its exact
failure; a Linux baseline failure fails the gate. A native core/controller
failure remains an unmet gate: link a separate issue in the PR and docs, and do
not relax a threshold to pass. The paired job also runs the Windows behavior
harness for multiline Chinese/emoji paste, resize recovery, `NO_COLOR`, OSC 52
with F7 save fallback, terminal restoration, Stop process-tree cleanup, session
switching, retry cancellation, and same-session busy rejection. Its terminal is
Windows ConPTY with no Windows Terminal frontend; reports identify that setup
and pywinpty version. The pywinpty 2.0.15 behavior run passes the multiline
Chinese/emoji paste check. Stop remains unmet: the active catalog reports
`Ownership unknown`, the TUI says `Active session changed state; nothing was
stopped`, and the owned Bash/PowerShell process tree remains alive after 10
seconds. This is tracked by [#187](https://github.com/SHUKE-LABS/leg/issues/187);
keep the #183 Stop gate unmet until that issue is resolved and the native check
passes.
