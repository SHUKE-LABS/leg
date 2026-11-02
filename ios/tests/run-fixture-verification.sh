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

resolve_artifacts() {
    local container="$1"
    local log_name="$2"
    TRANSCRIPT="${container}/Library/Application Support/LegHost/test-artifacts/terminal-transcript.log"
    GUEST_TREE="${container}/Library/Application Support/LegHost/roots/root.active/data"
    TRAIL="${GUEST_TREE}/tmp/leg-ios-fixture/exchange.jsonl"
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
fi

if [[ "${status}" -ne 0 ]]; then
    echo "fixture verification failed" >&2
    exit 1
fi
echo "fixture verification passed"
