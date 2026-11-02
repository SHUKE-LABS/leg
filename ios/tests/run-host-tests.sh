#!/usr/bin/env bash
#
# Run the host side fixtures for the LegHost guest provisioner.
#
# The fixtures build their own pinned inputs, so this needs no network and no
# simulator: the provisioner, the vendored iSH importer and the vendored
# libarchive are compiled for the build host and exercised directly.
#
# Usage: ios/tests/run-host-tests.sh [work directory]

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ISH_SRC="${ROOT}/ios/vendor/ish"
LIBARCHIVE_SRC="${ISH_SRC}/deps/libarchive"
BUILD_DIR="${ROOT}/build/host-tests"
WORK_DIR="${1:-${BUILD_DIR}/work}"

CC="${CC:-clang}"
CFLAGS=(
    -std=gnu11
    -g
    -O1
    -Wall
    -Wextra
    -Wno-unused-parameter
    -Wno-unused-function
    -DLOG_HANDLER_DPRINTF=1
    -I"${ISH_SRC}"
    -I"${LIBARCHIVE_SRC}"
    -I"${ISH_SRC}/deps"
    -I"${ROOT}/ios/LegHost"
)
# HOST_TEST_CFLAGS is handy when a fixture misbehaves: "-fsanitize=address -g"
# turns a crash into a report with a stack trace.
# shellcheck disable=SC2206
CFLAGS+=(${HOST_TEST_CFLAGS:-})
LINK_FLAGS=(-lsqlite3 -lz -lbz2 -liconv)
if [[ -n "${HOST_TEST_LDFLAGS:-}" ]]; then
    LINK_FLAGS+=(${HOST_TEST_LDFLAGS})
fi

if ! command -v "${CC}" >/dev/null 2>&1; then
    echo "error: ${CC} is required to build the host fixtures" >&2
    exit 1
fi

rm -rf "${BUILD_DIR}"
mkdir -p "${BUILD_DIR}"

objects=()

compile() {
    local source="$1"
    local object="${BUILD_DIR}/$(echo "${source#${ROOT}/}" | tr '/' '_').o"
    "${CC}" "${CFLAGS[@]}" -c "${source}" -o "${object}"
    objects+=("${object}")
}

# The vendored libarchive: the importer reads the pinned archives with it.
while IFS= read -r source; do
    object="${BUILD_DIR}/libarchive_$(basename "${source}" .c).o"
    "${CC}" "${CFLAGS[@]}" -DHAVE_CONFIG_H -c "${source}" -o "${object}"
    objects+=("${object}")
done < <(find "${LIBARCHIVE_SRC}" -maxdepth 1 -name '*.c' | LC_ALL=C sort)

# The vendored iSH importer and the database it writes.
compile "${ISH_SRC}/tools/fakefs.c"
compile "${ISH_SRC}/util/fchdir.c"
compile "${ISH_SRC}/fs/fake-db.c"
compile "${ISH_SRC}/fs/fake-migrate.c"
compile "${ISH_SRC}/fs/fake-rebuild.c"

# The host under test.
compile "${ROOT}/ios/LegHost/Sha256.c"
compile "${ROOT}/ios/LegHost/GuestProvision.c"
compile "${ROOT}/ios/tests/guest_provision_test.c"

"${CC}" -o "${BUILD_DIR}/guest_provision_test" "${objects[@]}" "${LINK_FLAGS[@]}"

if [[ "${HOST_TEST_BUILD_ONLY:-0}" == "1" ]]; then
    echo "${BUILD_DIR}/guest_provision_test"
    exit 0
fi

echo "running the guest provisioner fixtures"
"${BUILD_DIR}/guest_provision_test" "${WORK_DIR}"
