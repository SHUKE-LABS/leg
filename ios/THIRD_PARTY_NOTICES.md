# Third party notices for the Leg iOS host

Everything under `ios/` is licensed under the GNU General Public License,
version 3 (see `LICENSE`), because it embeds and adapts iSH. This directory is
separate from the proprietary desktop code in the rest of the repository: no
proprietary leg source is compiled or linked into the iOS host.

## iSH

- Upstream: https://github.com/ish-app/ish
- Vendored revision: `de5ab2c4b1039551203c1106e75f715cd917544b`
- Vendored path: `vendor/ish` (the upstream tree at that commit, unmodified
  except for the added `ISH_REVISION` marker)
- License: GPLv3, plus the additional terms in `LICENSE.IOS`. Contributions
  made upstream after commit `0e3a4144f93135c4fd618c8397d2cfd87194f69f` are
  additionally licensed under GPLv2; see `vendor/ish/LICENSE.md` for the full
  statement and the list of authors who agreed to the relicensing.

The vendored tree is copied from upstream at the pinned revision, so the
commit above identifies the exact material in this directory.

### Omitted upstream material

The upstream repository references three git submodules that are not vendored
here, because the iOS host does not build or link them:

- `deps/libapps` (iSH's file provider app extensions)
- `deps/libarchive` (used by those extensions)
- `deps/linux` (the Linux kernel build used by iSH's alternative
  user-mode-linux runtime)

The iOS host builds the same runtime as upstream's `kernel=ish` meson
configuration, which uses none of them.

## Runtime build

`scripts/build-ish-runtime.sh` builds the vendored runtime with the stock
Xcode clang. The vDSO (`vendor/ish/vdso`) is an i386 ELF shared object and
needs an ELF-capable linker (`ld.lld`, or the `rust-lld` shipped with a rustup
toolchain); that is a build-host prerequisite, not vendored material.
