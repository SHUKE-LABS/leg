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

## `fs/proc`: the `exe` link of a task without an address space

`fs/proc/pid.c` answers the other `/proc/<pid>` entries with a check for a
task that has no address space: a task that is on its way out has already
released its memory, and `task->mm` is `NULL` for it. The `exe` link did not
have that check and dereferenced `task->mm` directly, so a `readlink` of
`/proc/<pid>/exe` for such a task took the whole emulator down with a
segmentation fault inside the host.

The change adds the check the other handlers already have: a task without an
address space, or without an executable in it, has no `exe` link to point at,
so the read fails with `ESRCH` instead of crashing the emulator.

## `fs/sock`: the socket timeout options

A guest that set a read or a write timeout on a socket had its `setsockopt`
refused with `EINVAL`, so every program that bounded its socket reads, a HTTP
client among them, failed before it sent anything.

Two things were wrong:

- The numbers the emulator knew the timeout options by, 66 and 67, are not the
  numbers a Linux guest uses. Linux has `SO_RCVTIMEO` at 20 and `SO_SNDTIMEO`
  at 21, and a guest asks for those. The option was unknown, so the emulator
  refused it.
- The value of a timeout is a `struct timeval`, whose fields are 32-bit wide
  in the guest and 64-bit wide in a 64-bit host. The kernel checks the length
  of the value it is given, so the guest's eight bytes were refused as well.

The change gives the two options the numbers a Linux guest uses and
translates the timeout in both directions: the guest's timeval goes out as the
host's, and the host's comes back as the guest's.

## `kernel`: the interrupt of a task that waits

The kernel hands a signal to a task only when the signal is not pending yet,
and the signal of a kill stays pending until the task runs the next
instruction of the guest. A task that waits in a system call of the host
therefore gets the interrupt of the kernel only once: the system call of the
host does not return, the task does not run another instruction of the guest,
and the signal stays pending.

A host that stops a guest needs the interrupt again and again, because a task
that was running when the interrupt arrived waits again right away. The
kernel has an API for that now:

- `kernel/signal.c`: `task_interrupt` ends the system call a task waits in
  (the thread of the task gets the signal `SIGUSR1`) and notifies the
  condition the task waits on. The wake of the condition is the code that
  `deliver_signal_unlocked` had, moved into `wake_waiting_cond` and used by
  both.
- `kernel/signal.h`: the declaration of `task_interrupt`.

The API sends no signal of the guest: the guest may block a signal or ignore
it, and a host that stops a guest does not want to change what the guest
sees. The caller holds `pids_lock`, so the thread of the task is alive while
the interrupt is sent.
