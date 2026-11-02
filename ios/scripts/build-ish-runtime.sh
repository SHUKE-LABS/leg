#!/usr/bin/env bash
#
# Build the vendored iSH runtime (pinned revision under vendor/ish) into a
# static library that the LegHost app links.
#
# Upstream iSH builds the same runtime with meson, which needs meson, ninja
# and an LLVM toolchain that can target i386 Linux (for the vDSO). A stock
# Xcode install has none of those, so this script drives clang directly:
#
#   * the vDSO is an i386 ELF shared object, linked with any LLD that can
#     emit ELF (ld.lld, or the rust-lld shipped with a rustup toolchain);
#   * cpu-offsets.h is regenerated the same way upstream's
#     tools/staticdefine.sh does, from the assembled asbestos/offsets.c;
#   * every runtime source is compiled for the iOS target and archived;
#   * the vendored libarchive is compiled the same way, because the guest
#     root importer (tools/fakefs.c) needs it to read the pinned archives.
#
# The source list mirrors the `kernel=ish` build in upstream's meson.build.

set -euo pipefail

ISH_SRC="${SRCROOT:?SRCROOT is not set}/vendor/ish"
LIBARCHIVE_SRC="${ISH_SRC}/deps/libarchive"
OUT_ROOT="${BUILT_PRODUCTS_DIR:?BUILT_PRODUCTS_DIR is not set}"
WORK_DIR="${OUT_ROOT}/ish-runtime"
OBJ_DIR="${WORK_DIR}/obj"
LIB="${OUT_ROOT}/libish_runtime.a"
LIBARCHIVE="${OUT_ROOT}/libarchive.a"

if [[ ! -d "${ISH_SRC}" ]]; then
    echo "error: vendored iSH runtime not found at ${ISH_SRC}" >&2
    exit 1
fi

# The object directory is kept between builds: the vendored runtime is
# recompiled only when it actually changes.
mkdir -p "${OBJ_DIR}"

DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-16.0}"
case "${PLATFORM_NAME:-iphonesimulator}" in
    *simulator) SDK_SUFFIX="-simulator" ;;
    *) SDK_SUFFIX="" ;;
esac

if [[ -n "${DEVELOPER_DIR:-}" && -x "${DEVELOPER_DIR}/Toolchains/XcodeDefault.xctoolchain/usr/bin/clang" ]]; then
    CLANG="${DEVELOPER_DIR}/Toolchains/XcodeDefault.xctoolchain/usr/bin/clang"
    AR="${DEVELOPER_DIR}/Toolchains/XcodeDefault.xctoolchain/usr/bin/ar"
    LIPO="${DEVELOPER_DIR}/Toolchains/XcodeDefault.xctoolchain/usr/bin/lipo"
else
    CLANG="$(xcrun --sdk "${PLATFORM_NAME:-iphonesimulator}" clang)"
    AR="$(xcrun --sdk "${PLATFORM_NAME:-iphonesimulator}" ar)"
    LIPO="$(xcrun --sdk "${PLATFORM_NAME:-iphonesimulator}" lipo)"
fi

# --- the linker for the i386 vDSO (an ELF object, so ld64 cannot build it)

elf_linker=""
if command -v ld.lld >/dev/null 2>&1; then
    elf_linker="$(command -v ld.lld)"
else
    for candidate in "${HOME}"/.rustup/toolchains/*/lib/rustlib/*/bin/rust-lld; do
        if [[ -x "${candidate}" ]]; then
            elf_linker="${candidate}"
            break
        fi
    done
fi
if [[ -z "${elf_linker}" ]]; then
    echo "error: no ELF linker found for the i386 vDSO." >&2
    echo "       Install lld (Homebrew) or a rustup toolchain that ships rust-lld." >&2
    exit 1
fi
linker_dir="$(mktemp -d)"
trap 'rm -rf "${linker_dir}"' EXIT
ln -sf "${elf_linker}" "${linker_dir}/ld.lld"

# --- runtime sources (mirrors the `kernel=ish` list in upstream meson.build)

sources_for_arch() {
    local gadget_dir
    case "$1" in
        x86_64) gadget_dir="asbestos/gadgets-x86_64" ;;
        arm64) gadget_dir="asbestos/gadgets-aarch64" ;;
        *)
            echo "error: no iSH gadget set for arch $1" >&2
            exit 1
            ;;
    esac
    cat <<EOF
emu/tlb.c emu/fpu.c emu/vec.c emu/mmx.c emu/float80.c
asbestos/asbestos.c asbestos/gen.c asbestos/helpers.c
${gadget_dir}/entry.S ${gadget_dir}/memory.S ${gadget_dir}/control.S
${gadget_dir}/math.S ${gadget_dir}/bits.S ${gadget_dir}/string.S
${gadget_dir}/misc.S
kernel/init.c kernel/errno.c kernel/calls.c kernel/memory.c kernel/user.c
kernel/vdso.c kernel/task.c kernel/group.c kernel/log.c kernel/fork.c
kernel/exec.c kernel/exit.c kernel/time.c kernel/mmap.c kernel/uname.c
kernel/tls.c kernel/futex.c kernel/getset.c kernel/signal.c
kernel/resource.c kernel/random.c kernel/misc.c kernel/eventfd.c
kernel/ipc.c kernel/ptrace.c kernel/fs.c kernel/fs_info.c kernel/poll.c
kernel/epoll.c
fs/mount.c fs/fd.c fs/inode.c fs/stat.c fs/dir.c fs/generic.c fs/path.c
fs/real.c fs/fake.c fs/proc.c fs/proc/entry.c fs/proc/ish.c fs/proc/root.c
fs/proc/pid.c fs/dyndev.c fs/adhoc.c fs/sock.c fs/pipe.c fs/sockrestart.c
fs/lock.c fs/dev.c fs/mem.c fs/tty.c fs/tty-real.c fs/pty.c fs/tmp.c
fs/poll.c fs/fake-db.c fs/fake-migrate.c fs/fake-rebuild.c
util/timer.c util/sync.c util/fifo.c util/fchdir.c
tools/fakefs.c
EOF
}

# The vendored libarchive: the guest root importer reads the pinned archives
# with it, and the iOS SDK has no libarchive of its own.
libarchive_sources() {
    find "${LIBARCHIVE_SRC}" -maxdepth 1 -name '*.c' -print | LC_ALL=C sort
}

archives=()
libarchive_archives=()
for arch in ${ARCHS}; do
    arch_dir="${OBJ_DIR}/${arch}"
    gen_dir="${arch_dir}/generated"
    mkdir -p "${gen_dir}/vdso"

    cflags=(
        -target "${arch}-apple-ios${DEPLOYMENT_TARGET}${SDK_SUFFIX}"
        -isysroot "${SDKROOT:?SDKROOT is not set}"
        -std=gnu11
        -DLOG_HANDLER_OS_LOG=1
        -DENGINE_ASBESTOS=1
        -I"${ISH_SRC}"
        -I"${LIBARCHIVE_SRC}"
        -I"${gen_dir}"
    )

    # The vendored runtime hardly ever changes, and recompiling it for every
    # change of the app is the slowest part of the build. The objects are kept
    # between builds and are only rebuilt when the compiler flags change or
    # when a file of the vendored tree is newer than them.
    flags_hash="$(printf '%s' "${cflags[*]}" | shasum -a 256 | cut -d' ' -f1)"
    config_file="${arch_dir}/.config"
    if [[ ! -f "${config_file}" ]] || [[ "$(cat "${config_file}")" != "${flags_hash}" ]]; then
        rm -f "${arch_dir}"/*.o
        rm -rf "${gen_dir}"
        mkdir -p "${gen_dir}/vdso"
        printf '%s' "${flags_hash}" >"${config_file}"
    fi

    newest_vendored="$(find "${ISH_SRC}" "${LIBARCHIVE_SRC}" -type f -exec stat -f '%m' {} + |
        LC_ALL=C sort -n | tail -1)"
    if [[ -z "${newest_vendored}" ]]; then
        newest_vendored=0
    fi

    up_to_date() {
        # The file is up to date when it exists and is not older than the
        # newest file of the vendored tree.
        [[ -f "$1" ]] && [[ "$(stat -f '%m' "$1")" -ge "${newest_vendored}" ]]
    }

    # vDSO: an i386 ELF shared object embedded into the runtime by kernel/vdso.c
    if ! up_to_date "${gen_dir}/vdso/libvdso.so.elf"; then
        PATH="${linker_dir}:${PATH}" "${CLANG}" \
            -target i386-unknown-linux-gnu \
            -fuse-ld=lld \
            -nostdlib \
            -fno-stack-protector \
            -Wl,-T,"${ISH_SRC}/vdso/vdso.lds" \
            -Wl,--hash-style,sysv \
            -shared \
            -fPIC \
            "${ISH_SRC}/vdso/vdso.S" \
            "${ISH_SRC}/vdso/vdso.c" \
            -o "${gen_dir}/vdso/libvdso.so.elf"
    fi

    # cpu-offsets.h: offsets of the emulated CPU state, used by the gadgets
    if ! up_to_date "${gen_dir}/cpu-offsets.h"; then
        "${CLANG}" "${cflags[@]}" -include "${ISH_SRC}/tools/staticdefine.h" -S -o - \
            "${ISH_SRC}/asbestos/offsets.c" |
            sed -ne 's:^[[:space:]]*\.ascii[[:space:]]*"\(.*\)".*:\1:;
                     /^->/{s:->#\(.*\):/* \1 */:;
                     s:^->\([^ ]*\) [\$$#]*\([^ ]*\) \(.*\):#define \1 \2 /* \3 */:;
                     s:->::; p;}' >"${gen_dir}/cpu-offsets.h"
    fi

    if [[ ! -s "${gen_dir}/cpu-offsets.h" ]]; then
        echo "error: cpu-offsets.h generation produced no output" >&2
        exit 1
    fi

    objects=()
    for source in $(sources_for_arch "${arch}"); do
        object="${arch_dir}/${source//\//_}.o"
        if ! up_to_date "${object}"; then
            "${CLANG}" "${cflags[@]}" -c "${ISH_SRC}/${source}" -o "${object}"
        fi
        objects+=("${object}")
    done

    archive="${OBJ_DIR}/libish_runtime-${arch}.a"
    rm -f "${archive}"
    "${AR}" rcs "${archive}" "${objects[@]}"
    archives+=("${archive}")
    echo "built ${archive} from ${#objects[@]} vendored iSH objects"

    libarchive_objects=()
    for source in $(libarchive_sources); do
        object="${arch_dir}/libarchive_$(basename "${source}" .c).o"
        if ! up_to_date "${object}"; then
            "${CLANG}" \
                -target "${arch}-apple-ios${DEPLOYMENT_TARGET}${SDK_SUFFIX}" \
                -isysroot "${SDKROOT}" \
                -std=gnu11 \
                -DHAVE_CONFIG_H \
                -I"${LIBARCHIVE_SRC}" \
                -I"${ISH_SRC}/deps" \
                -c "${source}" -o "${object}"
        fi
        libarchive_objects+=("${object}")
    done

    libarchive_archive="${OBJ_DIR}/libarchive-${arch}.a"
    rm -f "${libarchive_archive}"
    "${AR}" rcs "${libarchive_archive}" "${libarchive_objects[@]}"
    libarchive_archives+=("${libarchive_archive}")
    echo "built ${libarchive_archive} from ${#libarchive_objects[@]} vendored libarchive objects"
done

rm -f "${LIB}"
"${LIPO}" -create "${archives[@]}" -output "${LIB}"
echo "built ${LIB} for: ${ARCHS}"

rm -f "${LIBARCHIVE}"
"${LIPO}" -create "${libarchive_archives[@]}" -output "${LIBARCHIVE}"
echo "built ${LIBARCHIVE} for: ${ARCHS}"
