#!/usr/bin/env bash
#
# Verify the iOS provider fixture and scan the run for the credential.
#
# The script runs the success scenario of the loopback fixture in the
# simulator and then looks at the four artifacts of that run: the raw
# terminal transcript, the application log, the guest file system and the
# exchange trail. The run fails when the configured credential shows up in
# any of them, and when the turn did not reach the fixture.
#
# The script runs the provider error scenario as well, because the failure
# of a turn is reported by the guest in its exchange trail and the host
# writes that report into the terminal: the transcript of that run has to
# hold the marker of the failure.
#
# Usage: ios/tests/run-fixture-verification.sh [--udid <simulator udid>] [--skip-run]

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUNDLE_ID="com.shukelabs.leg.ioshost"
SENTINEL="IOS_FIXTURE_CREDENTIAL_SENTINEL"
SUCCESS_TEST="LegHostUITests/LegHostFixtureUITests/testTheSuccessScenarioCompletesOneTurn"
PROVIDER_ERROR_TEST="LegHostUITests/LegHostFixtureUITests/testTheProviderErrorScenarioReportsAServerError"
PROVIDER_ERROR_MARKER="provider server error (500, api_error): IOS_FIXTURE_PROVIDER_ERROR"
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

status=0

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

if [[ "${skip_run}" -eq 0 ]]; then
    echo "=== the success scenario"
    run_scenario "${SUCCESS_TEST}"

    container="$(app_container)"
    if [[ -z "${container}" ]]; then
        echo "error: the app container of ${BUNDLE_ID} is not there." >&2
        exit 1
    fi
    echo "container: ${container}"

    transcript="${container}/Library/Application Support/LegHost/test-artifacts/terminal-transcript.log"
    guest_tree="${container}/Library/Application Support/LegHost/roots/root.active/data"
    trail="${guest_tree}/tmp/leg-ios-fixture/exchange.jsonl"
    app_log="${DERIVED_DATA}/fixture-verification-app.log"

    capture_app_log "${app_log}"

    scan_file "terminal transcript" "${transcript}"
    scan_file "application log" "${app_log}"
    scan_tree "guest file system" "${guest_tree}"
    scan_file "exchange trail" "${trail}"

    for marker in IOS_FIXTURE_STREAM_TEXT IOS_FIXTURE_TOOL_RESULT IOS_FIXTURE_FINAL_RESPONSE; do
        if ! grep -F -q "${marker}" "${transcript}" 2>/dev/null; then
            echo "error: the terminal transcript does not hold ${marker}" >&2
            status=1
        fi
    done
    if ! grep -F -q "IOS_FIXTURE_TURN" "${trail}" 2>/dev/null; then
        echo "error: the exchange trail does not hold the prompt of the turn" >&2
        status=1
    fi
    if ! grep -F -q "127.0.0.1" "${trail}" 2>/dev/null; then
        echo "error: the exchange trail does not hold the loopback base URL" >&2
        status=1
    fi

    # The failure of a turn is reported by the guest in its exchange trail,
    # and the host writes that report into the terminal. The transcript of
    # the run has to hold the marker, so the terminal is the record of it.
    echo "=== the provider error scenario"
    run_scenario "${PROVIDER_ERROR_TEST}"

    # Every run installs the app again, and the install gives the app a new
    # container, so the container is asked for again after the run.
    container="$(app_container)"
    transcript="${container}/Library/Application Support/LegHost/test-artifacts/terminal-transcript.log"
    app_log="${DERIVED_DATA}/fixture-verification-provider-error-app.log"

    capture_app_log "${app_log}"

    if ! grep -F -q "${PROVIDER_ERROR_MARKER}" "${transcript}" 2>/dev/null; then
        echo "error: the terminal transcript does not hold the marker of the provider error" >&2
        status=1
    else
        echo "ok: the terminal transcript holds the marker of the provider error"
    fi
    scan_file "terminal transcript of the provider error run" "${transcript}"
    scan_file "application log of the provider error run" "${app_log}"
fi

if [[ "${status}" -ne 0 ]]; then
    echo "fixture verification failed" >&2
    exit 1
fi
echo "fixture verification passed"
