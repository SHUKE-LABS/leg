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

## `kernel/exit.c`: the mark of a task that is on its way out

`do_exit` released the files of the task before it marked the task as
exiting, and the mark was made in the part of the exit that holds
`pids_lock`. A walker of the tasks that held `pids_lock` could therefore see
a task that was not marked, and the files of that task could be gone already:
the table of files of the task was freed while the walker held a pointer to
it.

The change moves the mark to the front of the exit: the task is marked as
exiting under `pids_lock` before it lets go of its files, and the mark that
the exit made later is gone. A walker that holds `pids_lock` sees a task that
holds its files, or a task that is marked. The two cannot both be true.

## `fs/fd.c`: `fdtable_retain` keeps a table of files alive

`fdtable_release` frees the table of files when its reference count falls to
zero, and the count is not protected by the lock of the table on the path
that raises it.

The change adds `fdtable_retain`: it takes the lock of the table, raises the
reference count, and lets the lock go. A caller that holds a table this way
keeps the table and the files in it alive until it lets the table go with
`fdtable_release`. The files of a table that a task owns stay open until then,
even when the task that owns the table lets it go.

## `kernel/task.c`: `tasks_end_data_waits` ends the waits that wait for data

The signal of a kill stays pending until the task runs the next instruction
of the guest, and a task that waits in a system call of the host does not run
that instruction until the system call returns. A task that waits for data
therefore stays in that wait after the kill, and a host that stops a guest
needs to end that wait.

The kernel has an API for that now:

- `kernel/task.c`: `tasks_end_data_waits` walks the tables of files of the
  tasks and ends the wait of every file that waits for data: the socket of
  the guest is shut down, and a byte is written into the pipe of the guest.
  The task that waits wakes up, and the kill that is pending lands before the
  guest runs another instruction.
- `kernel/task.h`: the declaration of `tasks_end_data_waits`.

The walk is done in the kernel and not in the host, because the files of a
task belong to the task: the task frees them when it exits, and a host that
reads them while the task exits reads memory that is gone. The walk holds
`pids_lock` the whole time, and a task marks itself as exiting under that
lock before it lets go of its files, so a task that is not marked still holds
its files while the walk runs. The walk keeps every table alive with
`fdtable_retain` while it walks it as well, so a task that exits in the
meantime leaves its table behind: the files of the task are closed when the
last one that holds the table lets it go. A task that is on its way out is
left alone, because the mark of the exit comes before the release of the
files.

The API sends no signal of the guest: the guest may block a signal or ignore
it, and a host that stops a guest does not want to change what the guest
sees.
