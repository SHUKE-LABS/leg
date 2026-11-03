/*
 * The guest runtime for the LegHost iOS host.
 *
 * The runtime drives the vendored iSH kernel: it mounts the provisioned
 * fakefs root, opens a pseudo terminal and runs one guest program on it. The
 * host reads the guest output from a ring buffer and writes guest input into
 * the terminal, so the terminal stays responsive even when the guest is
 * slower than the user.
 */

#ifndef LEGHOST_GUEST_RUNTIME_H
#define LEGHOST_GUEST_RUNTIME_H

#include <stddef.h>

/* Called after guest output was queued, from the guest thread. */
typedef void (*guest_output_notify_callback)(void *context);

/* Called when the guest program ended, from the guest thread. The code is
 * the exit status of the program, or the signal that killed it. */
typedef void (*guest_exit_notify_callback)(int code, void *context);

/*
 * Mounts the provisioned root and prepares the kernel. `root_data_dir` is the
 * `data` directory of a provisioned root: the fakefs keeps its database in the
 * parent directory and mirrors the guest files below `data`.
 */
int guest_runtime_boot(const char *root_data_dir);

/*
 * Runs `program` on a pseudo terminal.
 *
 * `arguments` holds the program name followed by its arguments as one buffer
 * of NUL separated strings, and `argument_count` counts those strings.
 * `environment` holds the "KEY=VALUE" entries the same way and may be NULL.
 */
int guest_runtime_start_session(const char *program, const char *arguments,
                                size_t argument_count, const char *environment);

/* Registers the callback that is invoked after guest output was queued. */
void guest_runtime_set_output_notify(guest_output_notify_callback callback, void *context);

/* Registers the callback that is invoked when the guest program ended. */
void guest_runtime_set_exit_notify(guest_exit_notify_callback callback, void *context);

/* Reads queued guest output. Returns the number of bytes copied. */
size_t guest_runtime_read_output(void *buffer, size_t max_length);

/* Throws the output of the session that stopped away.
 *
 * The output belongs to the session that produced it, so it is cleared once
 * that session is quiet: the pump of the session that comes next does not
 * read the text of the session that was stopped. */
void guest_runtime_clear_output(void);

/* Writes guest input into the terminal. Returns the number of bytes taken. */
int guest_runtime_write_input(const void *data, size_t length);

/* Reports the new terminal size to the guest. */
int guest_runtime_resize(int columns, int rows);

/* Ends the session and releases the terminal. */
void guest_runtime_shutdown(void);

/*
 * Stops the guest of the session and waits for it to end.
 *
 * Every task of the guest is killed and the call returns when none of them
 * runs anymore: a guest that waits for the provider is interrupted in the
 * syscall it waits in, and the kernel delivers the kill before the guest
 * runs another instruction, so the guest cannot write to the trail of the
 * guest anymore and cannot run next to the session that comes next.
 *
 * Returns 0 when the guest stopped, and -ETIMEDOUT when a task of the guest
 * still runs after the wait.
 */
int guest_runtime_stop_session(void);

/*
 * The number of tasks of the guest that run.
 *
 * The init process of the kernel is not counted: it is the parent of every
 * session and it does not run.
 */
int guest_runtime_running_tasks(void);

#endif /* LEGHOST_GUEST_RUNTIME_H */
