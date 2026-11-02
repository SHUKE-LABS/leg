# Third party notices for the Leg iOS host

Everything under `ios/` is licensed under the GNU General Public License,
version 3 (see `LICENSE`), because it embeds and adapts iSH. This directory is
separate from the proprietary desktop code in the rest of the repository: no
proprietary leg source is compiled or linked into the iOS host.

## iSH

- Upstream: https://github.com/ish-app/ish
- Vendored revision: `de5ab2c4b1039551203c1106e75f715cd917544b`
- Vendored path: `vendor/ish` (the upstream tree at that commit, plus the
  added `ISH_REVISION` marker and the local changes listed in
  `vendor/ish/LOCAL_CHANGES.md`)
- License: GPLv3, plus the additional terms in `LICENSE.IOS`. Contributions
  made upstream after commit `0e3a4144f93135c4fd618c8397d2cfd87194f69f` are
  additionally licensed under GPLv2; see `vendor/ish/LICENSE.md` for the full
  statement and the list of authors who agreed to the relicensing.

The vendored tree is copied from upstream at the pinned revision, so the
commit above identifies the exact material in this directory.

### Local changes

`vendor/ish/LOCAL_CHANGES.md` lists the changes to the upstream tree. They
are small: the `ISH_REVISION` marker, and the `movmskps` instruction in the
emulator, which the pinned guest bundle needs and upstream does not
implement.

### Omitted upstream material

The upstream repository references three git submodules that are not vendored
here, because the iOS host does not build or link them:

- `deps/libapps` (iSH's file provider app extensions)
- `deps/linux` (the Linux kernel build used by iSH's alternative
  user-mode-linux runtime)

The iOS host builds the same runtime as upstream's `kernel=ish` meson
configuration, which uses none of them.

### Vendored libarchive

- Upstream: https://github.com/libarchive/libarchive
- Vendored revision: `fc6563f5130d8a7ee1fc27c0e55baef35119f26c` (libarchive
  3.4.3)
- Vendored path: `vendor/ish/deps/libarchive`
- License: BSD 2-Clause (`vendor/ish/deps/libarchive/LICENSE`)

The iOS SDK has no libarchive, and the guest root importer
(`vendor/ish/tools/fakefs.c`) needs one to read the pinned archives, so the
same submodule upstream vendors is vendored here. The upstream tree is copied
without its test suite and CMake build files. `vendor/ish/deps/libarchive/config.h`
is added by this host: it includes iSH's generated libarchive configuration and
turns the xar formats off, because xar needs libxml2, which iOS does not
have. The guest root importer only reads and writes tar archives.

### hterm

- Upstream: https://github.com/ish-app/libapps (the `hterm` submodule)
- Vendored build: `hterm/bin/mkdist` output (`hterm_all.js`) from
  `ish-app/libapps@b8cacae35e5b11d64bb736a053921c16ca7faf9e`
- Vendored path: `vendor/ish/app/terminal`
- License: MIT (`vendor/ish/app/terminal/hterm_all.js` carries the notice)

The terminal page (`term.html`, `term.js`, `term.css`) comes from the vendored
iSH app tree. `hterm_all.js` is a build artifact that upstream does not commit,
so it is built from the pinned libapps revision and vendored here. The host
writes the guest output into hterm and reads the guest input from it; no iSH
app code is compiled or linked.

## Runtime build

`scripts/build-ish-runtime.sh` builds the vendored runtime with the stock
Xcode clang. The vDSO (`vendor/ish/vdso`) is an i386 ELF shared object and
needs an ELF-capable linker (`ld.lld`, or the `rust-lld` shipped with a rustup
toolchain); that is a build-host prerequisite, not vendored material. The same
script builds the vendored libarchive into `libarchive.a`, which the app links
next to the runtime.
