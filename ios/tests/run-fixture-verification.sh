#!/usr/bin/env bash
#
# Verify the iOS provider fixture and scan the run for the credential.
#
# The script runs the scenarios of the loopback fixture in the simulator and
# then looks at the four artifacts of every run: the raw terminal
# transcript, the application log, the guest file system and the exchange
# trail. The run fails when the configured credential shows up in any of
# them, and when the turn did not reach the fixture.
#
# The script runs the provider error, the unreachable and the runtime failure
# scenario as well, because the failure of a turn is reported by the guest in
# its exchange trail and the host writes that report into the terminal: the
# transcript of those runs has to hold the marker of the failure. The four
# artifacts of those runs are scanned as well.
#
# The script runs the relaunch and the background run as well: the relaunch
# run starts the app twice and checks the durable ledger of the requests
# across the two processes, and the background run checks the trail the host
# writes out when it stops the session. The retry run stops the session and
# starts the host again, so it checks that the ledger, the trail and the
# catalog of the guest survive the retry. The stopped guest run stops the
# session and starts the host again as well, so it checks that the guest of
# the stopped session did not write to the trail of the guest after the
# interruption.
#
# With --skip-run the script runs no scenario and scans the artifacts of the
# run that is already in the simulator.
#
# Usage: ios/tests/run-fixture-verification.sh [--udid <simulator udid>] [--skip-run]

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUNDLE_ID="com.shukelabs.leg.ioshost"
SENTINEL="IOS_FIXTURE_CREDENTIAL_SENTINEL"
SUCCESS_TEST="LegHostUITests/LegHostFixtureUITests/testTheSuccessScenarioCompletesOneTurn"
PROVIDER_ERROR_TEST="LegHostUITests/LegHostFixtureUITests/testTheProviderErrorScenarioReportsAServerError"
UNREACHABLE_TEST="LegHostUITests/LegHostFixtureUITests/testTheUnreachableScenarioReportsATransportError"
RUNTIME_FAILURE_TEST="LegHostUITests/LegHostFixtureUITests/testTheRuntimeFailureScenarioReportsTheRuntimeDiagnostic"
RELAUNCH_TEST="LegHostUITests/LegHostFixtureUITests/testTheRelaunchKeepsTheWorkspaceTheTrailAndTheRequests"
BACKGROUND_TEST="LegHostUITests/LegHostFixtureUITests/testTheBackgroundInterruptsTheTurnAndKeepsTheTrail"
RETRY_TEST="LegHostUITests/LegHostFixtureUITests/testTheRetryAfterAnInterruptionKeepsTheStateOfTheTurn"
STOPPED_GUEST_TEST="LegHostUITests/LegHostFixtureUITests/testTheStoppedGuestCannotWriteAfterTheRetry"
PROVIDER_ERROR_MARKER="provider server error (500, api_error): IOS_FIXTURE_PROVIDER_ERROR"
UNREACHABLE_MARKER="transport error:"
RUNTIME_FAILURE_MARKER="IOS_GUEST_RUNTIME_ERROR"
DERIVED_DATA="${ROOT}/build/dd"

export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"

udid=""
skip_run=0
while [[ $# -gt 0 ]]; do
    case "$1" in
    --udid)
        udid="${2:-}"
        shift 2
        ;;
    --skip-run)
        skip_run=1
        shift
        ;;
    *)
        echo "error: unknown argument ${1}" >&2
        exit 2
        ;;
    esac
done

if [[ -z "${udid}" ]]; then
    udid="$(xcrun simctl list devices booted |
        sed -n 's/.*(\([0-9A-Fa-f-]\{36\}\)) (Booted).*/\1/p' | head -n 1)"
fi
if [[ -z "${udid}" ]]; then
    echo "error: no booted simulator. Boot one or pass --udid." >&2
    exit 1
fi
echo "simulator: ${udid}"

status=0

run_scenario() {
    local test_id="$1"
    # Every scenario gets a simulator of its own. The accessibility server of
    # a simulator wedges when a second run drives the app on the simulator of
    # the first one, and the run that follows the wedge fails with an
    # accessibility timeout instead of an assertion.
    xcrun simctl shutdown "${udid}" >/dev/null 2>&1 || true
    sleep 5
    xcrun simctl boot "${udid}" >/dev/null 2>&1
    xcrun simctl bootstatus "${udid}" -b >/dev/null 2>&1
    xcrun simctl terminate "${udid}" "${BUNDLE_ID}" >/dev/null 2>&1 || true
    sleep 2
    xcodebuild test \
        -project "${ROOT}/ios/LegHost.xcodeproj" \
        -scheme LegHost \
        -sdk iphonesimulator \
        -destination "platform=iOS Simulator,id=${udid}" \
        -derivedDataPath "${DERIVED_DATA}" \
        -only-testing:"${test_id}"
}

# The container of the app in the simulator.
#
# Every run installs the app again, and the install gives the app a new
# container, so the container is asked for after every run.
app_container() {
    xcrun simctl get_app_container "${udid}" "${BUNDLE_ID}" data
}

# The log of the app in the simulator.
capture_app_log() {
    local path="$1"
    xcrun simctl spawn "${udid}" log show --style compact --last 10m \
        --predicate 'process == "LegHost"' >"${path}" 2>/dev/null || true
}

scan_file() {
    local label="$1"
    local path="$2"
    if [[ ! -f "${path}" ]]; then
        echo "error: the ${label} is missing at ${path}" >&2
        status=1
        return
    fi
    if grep -F -q "${SENTINEL}" "${path}"; then
        echo "error: the ${label} holds the configured credential" >&2
        status=1
        return
    fi
    echo "ok: the ${label} holds no credential"
}

scan_tree() {
    local label="$1"
    local path="$2"
    if [[ ! -d "${path}" ]]; then
        echo "error: the ${label} is missing at ${path}" >&2
        status=1
        return
    fi
    if grep -R -F -q "${SENTINEL}" "${path}" 2>/dev/null; then
        echo "error: the ${label} holds the configured credential" >&2
        status=1
        return
    fi
    echo "ok: the ${label} holds no credential"
}

# The four artifacts of one run.
#
# The names of the artifacts are read from the container of the run, because
# every run installs the app again and the install gives the app a new
# container.
TRANSCRIPT=""
GUEST_TREE=""
TRAIL=""
APP_LOG=""
LEDGER=""
PERSISTED_TRAIL=""

resolve_artifacts() {
    local container="$1"
    local log_name="$2"
    TRANSCRIPT="${container}/Library/Application Support/LegHost/test-artifacts/terminal-transcript.log"
    GUEST_TREE="${container}/Library/Application Support/LegHost/roots/root.active/data"
    TRAIL="${GUEST_TREE}/tmp/leg-ios-fixture/exchange.jsonl"
    LEDGER="${container}/Library/Application Support/LegHost/test-artifacts/request-ledger.jsonl"
    PERSISTED_TRAIL="${container}/Library/Application Support/LegHost/test-artifacts/exchange-trail.jsonl"
    APP_LOG="${DERIVED_DATA}/fixture-verification-${log_name}.log"
    capture_app_log "${APP_LOG}"
}

# The credential scan of one run.
#
# The scan covers the four artifacts of the run: the raw terminal transcript,
# the application log, the guest file system and the exchange trail. The
# trail is only there when the guest program ran, so the mode of the scan
# says what the run owes:
#
#   required  the trail has to be there, the guest program ran
#   absent    the trail must not be there, the guest program never ran
#   optional  the trail is scanned when it is there
scan_artifacts() {
    local label="$1"
    local trail_mode="$2"
    scan_file "terminal transcript of ${label}" "${TRANSCRIPT}"
    scan_file "application log of ${label}" "${APP_LOG}"
    scan_tree "guest file system of ${label}" "${GUEST_TREE}"
    case "${trail_mode}" in
    required)
        scan_file "exchange trail of ${label}" "${TRAIL}"
        ;;
    absent)
        if [[ -f "${TRAIL}" ]]; then
            echo "error: the exchange trail of ${label} is there, the guest program never ran" >&2
            status=1
        else
            echo "ok: the exchange trail of ${label} is not there, the guest program never ran"
        fi
        ;;
    *)
        if [[ -f "${TRAIL}" ]]; then
            scan_file "exchange trail of ${label}" "${TRAIL}"
        else
            echo "ok: the exchange trail of ${label} is not there"
        fi
        ;;
    esac
}

# The markers of a turn that reached the fixture.
check_turn_markers() {
    local label="$1"
    for marker in IOS_FIXTURE_STREAM_TEXT IOS_FIXTURE_TOOL_RESULT IOS_FIXTURE_FINAL_RESPONSE; do
        if ! grep -F -q "${marker}" "${TRANSCRIPT}" 2>/dev/null; then
            echo "error: the terminal transcript of ${label} does not hold ${marker}" >&2
            status=1
        fi
    done
    if ! grep -F -q "IOS_FIXTURE_TURN" "${TRAIL}" 2>/dev/null; then
        echo "error: the exchange trail of ${label} does not hold the prompt of the turn" >&2
        status=1
    fi
    if ! grep -F -q "127.0.0.1" "${TRAIL}" 2>/dev/null; then
        echo "error: the exchange trail of ${label} does not hold the loopback base URL" >&2
        status=1
    fi
}

# The durable ledger of the requests the fixture accepted.
#
# The ledger holds one JSON object per line: the turn of the request, the
# ordinal of the request in that turn and the status it was answered with.
# The ledger of a run that was not reset holds the requests of the runs
# before it, so the number of lines is the number of accepted requests of
# every run. A repeated request shows up as a second line with the same
# ordinal of the same turn.
check_ledger() {
    local label="$1"
    local expected="$2"
    if [[ ! -f "${LEDGER}" ]]; then
        echo "error: the request ledger of ${label} is missing at ${LEDGER}" >&2
        status=1
        return
    fi
    if grep -F -q "${SENTINEL}" "${LEDGER}"; then
        echo "error: the request ledger of ${label} holds the configured credential" >&2
        status=1
    fi
    local lines
    lines="$(grep -c . "${LEDGER}" || true)"
    if [[ "${lines}" != "${expected}" ]]; then
        echo "error: the request ledger of ${label} holds ${lines} requests, expected ${expected}" >&2
        status=1
        return
    fi
    local turns
    turns="$(sed -n 's/.*"turn":"\([^"]*\)".*/\1/p' "${LEDGER}" | sort -u | wc -l | tr -d ' ')"
    if [[ "${turns}" != "1" ]]; then
        echo "error: the request ledger of ${label} holds ${turns} turns, a request was repeated" >&2
        status=1
        return
    fi
    local repeated
    repeated="$(sed -n 's/.*"request":\([0-9]*\).*/\1/p' "${LEDGER}" | sort | uniq -d | wc -l | tr -d ' ')"
    if [[ "${repeated}" != "0" ]]; then
        echo "error: the request ledger of ${label} holds a repeated ordinal of a turn" >&2
        status=1
        return
    fi
    echo "ok: the request ledger of ${label} holds ${lines} requests of one turn"
}

# The trail of the session that was stopped.
#
# The host writes the trail of the guest out when the session is stopped at
# the boundary of the host, so the trail of a turn that was interrupted is
# kept outside the guest root as well.
check_persisted_trail() {
    local label="$1"
    if [[ ! -f "${PERSISTED_TRAIL}" ]]; then
        echo "error: the persisted trail of ${label} is missing at ${PERSISTED_TRAIL}" >&2
        status=1
        return
    fi
    if grep -F -q "${SENTINEL}" "${PERSISTED_TRAIL}"; then
        echo "error: the persisted trail of ${label} holds the configured credential" >&2
        status=1
        return
    fi
    if [[ ! -s "${PERSISTED_TRAIL}" ]]; then
        echo "error: the persisted trail of ${label} is empty" >&2
        status=1
        return
    fi
    echo "ok: the persisted trail of ${label} holds no credential"
}

# The catalog of the guest.
#
# The catalog holds the sessions the guest knows about. A run that starts
# again has to leave the catalog of the stopped turn alone, so the guest
# finds its session again instead of asking for a workspace. Every session
# of the catalog holds the workspace it runs in.
check_catalog() {
    local label="$1"
    local catalog="${GUEST_TREE}/root/.local/state/leg-ui/catalog.json"
    if [[ ! -f "${catalog}" ]]; then
        echo "error: the catalog of ${label} is missing at ${catalog}" >&2
        status=1
        return
    fi
    if grep -F -q "${SENTINEL}" "${catalog}"; then
        echo "error: the catalog of ${label} holds the configured credential" >&2
        status=1
    fi
    local sessions
    sessions="$(grep -c '"cwd"' "${catalog}" || true)"
    if [[ "${sessions}" -lt 1 ]]; then
        echo "error: the catalog of ${label} holds no session" >&2
        status=1
        return
    fi
    echo "ok: the catalog of ${label} holds ${sessions} sessions"
}

# The trail of the guest after the session was stopped.
#
# The host writes the trail of the stopped session out when it stops the
# session. The trail of the guest itself may not hold more records then: the
# guest of the stopped session is stopped and waited for before the fixture
# is stopped, so it did not write to the trail of the guest anymore.
check_untouched_trail() {
    local label="$1"
    if [[ ! -f "${TRAIL}" ]]; then
        echo "error: the exchange trail of ${label} is missing at ${TRAIL}" >&2
        status=1
        return
    fi
    local live
    live="$(grep -c . "${TRAIL}" || true)"
    local persisted
    persisted="$(grep -c . "${PERSISTED_TRAIL}" || true)"
    if [[ "${live}" != "${persisted}" ]]; then
        echo "error: the exchange trail of ${label} holds ${live} records, the trail of the stopped session holds ${persisted}: the guest of the stopped session wrote to the trail" >&2
        status=1
        return
    fi
    echo "ok: the exchange trail of ${label} holds the records of the stopped session only"
}

# Runs one scenario and scans the artifacts of that run.
#
# The marker is the text the transcript of the run has to hold, so the
# terminal of the guest is the record of the failure of the scenario.
run_and_scan() {
    local test_id="$1"
    local label="$2"
    local log_name="$3"
    local marker="$4"
    local marker_label="$5"
    local trail_mode="${6:-required}"

    echo "=== the ${label}"
    run_scenario "${test_id}"

    # Every run installs the app again, and the install gives the app a new
    # container, so the container is asked for after the run.
    local container
    container="$(app_container)"
    if [[ -z "${container}" ]]; then
        echo "error: the app container of ${BUNDLE_ID} is not there." >&2
        exit 1
    fi
    echo "container: ${container}"

    resolve_artifacts "${container}" "${log_name}"
    scan_artifacts "${label}" "${trail_mode}"

    if [[ -n "${marker}" ]]; then
        if ! grep -F -q "${marker}" "${TRANSCRIPT}" 2>/dev/null; then
            echo "error: the terminal transcript of ${label} does not hold ${marker_label}" >&2
            status=1
        else
            echo "ok: the terminal transcript of ${label} holds ${marker_label}"
        fi
    fi
}

if [[ "${skip_run}" -eq 1 ]]; then
    # The scan of a run that already happened. The artifacts of the last run
    # are in the container of the app in the simulator.
    echo "=== the run that is already there"
    container="$(app_container)"
    if [[ -z "${container}" ]]; then
        echo "error: the app container of ${BUNDLE_ID} is not there." >&2
        exit 1
    fi
    echo "container: ${container}"

    resolve_artifacts "${container}" "existing"
    scan_artifacts "the run that is already there" optional
else
    run_and_scan "${SUCCESS_TEST}" "success run" "success" "" ""
    check_turn_markers "the success run"

    # The failure of a turn is reported by the guest in its exchange trail,
    # and the host writes that report into the terminal. The transcript of
    # the run has to hold the marker, so the terminal is the record of it.
    run_and_scan "${PROVIDER_ERROR_TEST}" "provider error run" "provider-error" \
        "${PROVIDER_ERROR_MARKER}" "${PROVIDER_ERROR_MARKER}"
    run_and_scan "${UNREACHABLE_TEST}" "unreachable run" "unreachable" \
        "${UNREACHABLE_MARKER}" "${UNREACHABLE_MARKER}"
    # The guest program never ran in the runtime failure run, so it left no
    # trail behind: the trail of that run is scanned when it is there.
    run_and_scan "${RUNTIME_FAILURE_TEST}" "runtime failure run" "runtime-failure" \
        "${RUNTIME_FAILURE_MARKER}" "${RUNTIME_FAILURE_MARKER}" optional

    # The relaunch run starts the app twice: the second launch keeps the
    # workspace and the trail of the first one and does not send the turn
    # again, so the durable ledger holds the two requests of the first turn
    # and nothing else.
    run_and_scan "${RELAUNCH_TEST}" "relaunch run" "relaunch" "" ""
    check_ledger "the relaunch run" 2
    check_catalog "the relaunch run"

    # The background run stops the session in the middle of the turn, so the
    # host writes the trail of the stopped turn out.
    run_and_scan "${BACKGROUND_TEST}" "background run" "background" "" ""
    check_persisted_trail "the background run"

    # The retry run stops the session in the middle of the turn and starts
    # the host again. The reset belongs to the launch, so the retry keeps the
    # ledger, the trail and the catalog of the stopped turn: the ledger holds
    # the one request of that turn and nothing else, and the guest finds its
    # session in the catalog again.
    run_and_scan "${RETRY_TEST}" "retry run" "retry" "" ""
    check_ledger "the retry run" 1
    check_persisted_trail "the retry run"
    check_catalog "the retry run"

    # The stopped guest run stops the session in the middle of the turn and
    # starts the host again. The guest of the stopped session is stopped and
    # waited for, so it does not write to the trail of the guest anymore:
    # the trail of the guest holds the records of the stopped turn only.
    run_and_scan "${STOPPED_GUEST_TEST}" "stopped guest run" "stopped-guest" "" ""
    check_ledger "the stopped guest run" 1
    check_persisted_trail "the stopped guest run"
    check_untouched_trail "the stopped guest run"
fi

if [[ "${status}" -ne 0 ]]; then
    echo "fixture verification failed" >&2
    exit 1
fi
echo "fixture verification passed"
