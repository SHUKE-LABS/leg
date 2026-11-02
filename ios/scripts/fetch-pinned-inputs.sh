#!/usr/bin/env bash
#
# Fetch the pinned guest inputs into the app bundle.
#
# The two archives the guest needs (the leg guest bundle for i386 and the
# Alpine i386 root) are pinned by URL and SHA-256 in LegHost/PinnedInputs.plist.
# This phase downloads exactly those files, verifies their digests and fails
# the build when a download or a digest does not match. The app only ever
# reads the embedded copies; it never follows a moving release at runtime.
#
# The download cache lives in build/pinned-inputs so repeated builds do not
# hit the network. Set LEGHOST_PINNED_INPUTS_DIR to a pre-populated directory
# to build without network access.

set -euo pipefail

MANIFEST="${SRCROOT:?SRCROOT is not set}/LegHost/PinnedInputs.plist"
CACHE_DIR="${LEGHOST_PINNED_INPUTS_DIR:-${SRCROOT:?SRCROOT is not set}/build/pinned-inputs}"
OUTPUT_DIR="${SRCROOT:?SRCROOT is not set}/resources/PinnedInputs"

if [[ ! -f "${MANIFEST}" ]]; then
    echo "error: the pinned input manifest is missing at ${MANIFEST}" >&2
    exit 1
fi

if ! command -v plutil >/dev/null 2>&1; then
    echo "error: plutil is required to read the pinned input manifest" >&2
    exit 1
fi

mkdir -p "${CACHE_DIR}" "${OUTPUT_DIR}"

# The staged folder is copied into the app bundle, so it may only hold the
# pinned inputs.
find "${OUTPUT_DIR}" -mindepth 1 -maxdepth 1 -type f ! -name '.gitkeep' -delete

# Every pinned input, one tab separated line per input.
read_pins() {
    local index=0
    while true; do
        local name
        name="$(plutil -extract "inputs.${index}.name" raw -o - "${MANIFEST}" 2>/dev/null)" || break
        local url filename sha256
        url="$(plutil -extract "inputs.${index}.url" raw -o - "${MANIFEST}")"
        filename="$(plutil -extract "inputs.${index}.filename" raw -o - "${MANIFEST}")"
        sha256="$(plutil -extract "inputs.${index}.sha256" raw -o - "${MANIFEST}")"
        printf '%s\t%s\t%s\t%s\n' "${name}" "${url}" "${filename}" "${sha256}"
        index=$((index + 1))
    done
}

pins="$(read_pins)"
if [[ -z "${pins}" ]]; then
    echo "error: the pinned input manifest lists no inputs" >&2
    exit 1
fi

while IFS=$'\t' read -r name url filename expected; do
    [[ -n "${name}" ]] || continue

    case "${url}" in
        https://*) ;;
        *)
            echo "error: the pinned input ${name} does not use https" >&2
            exit 1
            ;;
    esac

    cached="${CACHE_DIR}/${filename}"
    staged="${OUTPUT_DIR}/${filename}"

    if [[ -f "${cached}" ]]; then
        actual="$(shasum -a 256 "${cached}" | awk '{print $1}')"
        if [[ "${actual}" != "${expected}" ]]; then
            echo "error: the cached ${name} has the digest ${actual}, expected ${expected}" >&2
            exit 1
        fi
    else
        echo "fetching ${name} from ${url}"
        temporary="${CACHE_DIR}/.${filename}.download"
        rm -f "${temporary}"
        if ! curl --fail --location --silent --show-error --retry 3 --retry-delay 2 \
                --proto '=https' --tlsv1.2 --output "${temporary}" "${url}"; then
            rm -f "${temporary}"
            echo "error: cannot download ${name} from ${url}" >&2
            exit 1
        fi
        actual="$(shasum -a 256 "${temporary}" | awk '{print $1}')"
        if [[ "${actual}" != "${expected}" ]]; then
            rm -f "${temporary}"
            echo "error: the downloaded ${name} has the digest ${actual}, expected ${expected}" >&2
            exit 1
        fi
        mv "${temporary}" "${cached}"
    fi

    cp "${cached}" "${staged}"
    echo "staged ${filename} for ${name}"
done <<<"${pins}"
