#!/usr/bin/env bash
#
# Run the guest provisioner against the real pinned inputs.
#
# The fixtures in run-host-tests.sh build their own inputs, so they never
# touch the network. This script is the other half: it downloads exactly the
# pinned archives, checks their digests against the pin manifest and runs the
# provisioner against them, including the reuse of the verified root.
#
# Usage: ios/tests/run-pinned-inputs.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MANIFEST="${ROOT}/ios/LegHost/PinnedInputs.plist"
CACHE_DIR="${LEGHOST_PINNED_INPUTS_DIR:-${ROOT}/build/pinned-inputs}"
WORK_DIR="${ROOT}/build/pinned-inputs-test"

if ! command -v plutil >/dev/null 2>&1; then
    echo "error: plutil is required to read the pinned input manifest" >&2
    exit 1
fi

mkdir -p "${CACHE_DIR}" "${WORK_DIR}"

binary="$(HOST_TEST_BUILD_ONLY=1 bash "${ROOT}/ios/tests/run-host-tests.sh" | tail -n 1)"

input_count=0
paths=()
digests=()
index=0
while true; do
    name="$(plutil -extract "inputs.${index}.name" raw -o - "${MANIFEST}" 2>/dev/null)" || break
    url="$(plutil -extract "inputs.${index}.url" raw -o - "${MANIFEST}")"
    filename="$(plutil -extract "inputs.${index}.filename" raw -o - "${MANIFEST}")"
    expected="$(plutil -extract "inputs.${index}.sha256" raw -o - "${MANIFEST}")"
    index=$((index + 1))

    cached="${CACHE_DIR}/${filename}"
    if [[ ! -f "${cached}" ]]; then
        echo "fetching ${name} from ${url}"
        if ! curl --fail --location --silent --show-error --retry 3 --retry-delay 2 \
                --proto '=https' --tlsv1.2 --output "${cached}" "${url}"; then
            rm -f "${cached}"
            echo "error: cannot download ${name} from ${url}" >&2
            exit 1
        fi
    fi

    actual="$(shasum -a 256 "${cached}" | awk '{print $1}')"
    if [[ "${actual}" != "${expected}" ]]; then
        echo "error: ${name} has the digest ${actual}, expected ${expected}" >&2
        exit 1
    fi
    echo "verified ${name} (${actual})"

    paths+=("${cached}")
    digests+=("${actual}")
    input_count=$((input_count + 1))
done

if [[ "${input_count}" -lt 2 ]]; then
    echo "error: the pinned input manifest does not list both inputs" >&2
    exit 1
fi

pins="${WORK_DIR}/pins.txt"
{
    echo "bundle_kind=$(plutil -extract compatibility.bundle_kind raw -o - "${MANIFEST}")"
    echo "leg_version=$(plutil -extract compatibility.leg_version raw -o - "${MANIFEST}")"
    echo "target=$(plutil -extract compatibility.target raw -o - "${MANIFEST}")"
    echo "architecture=$(plutil -extract compatibility.architecture raw -o - "${MANIFEST}")"
    echo "os=$(plutil -extract compatibility.os raw -o - "${MANIFEST}")"
    echo "libc=$(plutil -extract compatibility.libc raw -o - "${MANIFEST}")"
    echo "alpine_release=$(plutil -extract compatibility.alpine_release raw -o - "${MANIFEST}")"
    echo "docker_platform=$(plutil -extract compatibility.docker_platform raw -o - "${MANIFEST}")"
    echo "bundle_sha256=${digests[0]}"
    echo "alpine_sha256=${digests[1]}"
} >"${pins}"

rm -rf "${WORK_DIR}/roots"

echo "running the provisioner against the pinned inputs"
"${binary}" --pinned "${pins}" "${paths[0]}" "${paths[1]}" "${WORK_DIR}/roots"
