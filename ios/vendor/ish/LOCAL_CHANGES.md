# Local changes to the vendored iSH tree

The tree under `vendor/ish` is the upstream iSH tree at the revision in
`ISH_REVISION`, plus the changes listed here. Everything else is upstream
material.

## `emu`: `movmskps` (single precision)

Upstream implements `movmskpd` (`0f 50 /r` with a memory operand) in the
single precision decode branch of `emu/decode.h`, but it does not implement
`movmskps` at all. The pinned guest bundle is built for
`i686-unknown-linux-musl`, and rustc enables `sse` and `sse2` for that
target, so the guest programs contain `movmskps` instructions. Without the
instruction the guest dies with `SIGILL` before it can draw anything.

The change adds the missing instruction:

- `emu/vec.c`: `vec_fmovmask_s128` moves the sign bits of the four single
  precision lanes of an XMM register into the low four bits of the
  destination.
- `emu/vec.h`: the declaration of that helper.
- `emu/decode.h`: the `0x50` opcode in the single precision branch now
  dispatches on the form of the operand. A register operand is `movmskps`
  and takes the four bit mask; a memory operand is `movmskpd` and keeps
  the two bit mask that upstream implemented.

`movmskpd` with a register operand is not a valid encoding, so the two
forms cannot be confused.

This is the only instruction the pinned guest bundle needs that the
vendored emulator did not implement.
