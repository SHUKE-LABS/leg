#!/usr/bin/env bash
#
# Run the host side fixtures for the provider configuration of the LegHost
# host.
#
# The fixtures compile the configuration source of the host for the build
# host and exercise it directly: the query of the read of the item of the
# Keychain, the JSON of the item and the diagnostics. The call into the
# Security framework is handed in, so the fixtures need neither a Keychain
# nor a simulator.
#
# Usage: ios/tests/run-provider-tests.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUILD_DIR="${ROOT}/build/host-tests"
SWIFTC="${SWIFTC:-swiftc}"

if ! command -v "${SWIFTC}" >/dev/null 2>&1; then
    echo "error: ${SWIFTC} is required to build the provider fixtures" >&2
    exit 1
fi

mkdir -p "${BUILD_DIR}"

"${SWIFTC}" -O \
    -o "${BUILD_DIR}/provider_configuration_test" \
    "${ROOT}/ios/LegHost/ProviderConfiguration.swift" \
    "${ROOT}/ios/tests/provider_configuration_test.swift"

echo "running the provider configuration fixtures"
"${BUILD_DIR}/provider_configuration_test"
