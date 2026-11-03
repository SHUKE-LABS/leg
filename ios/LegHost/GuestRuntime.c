/*
 * The guest runtime for the LegHost iOS host. See GuestRuntime.h.
 *
 * The boot sequence follows the one the vendored iSH app uses: mount the
 * fakefs root, become the first process, create the device nodes, mount
 * /proc and /dev/pts and attach a console. The session then runs one program
 * on a pseudo terminal, exactly like iSH does for its shell.
 */

#include "GuestRuntime.h"

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include <os/log.h>

#include "fs/devices.h"
#include "fs/fd.h"
#include "fs/path.h"
#include "fs/tty.h"
#include "kernel/calls.h"
#include "kernel/fs.h"
#include "kernel/init.h"
#include "kernel/task.h"
#include "util/sync.h"

#define GUEST_OUTPUT_CAPACITY (256 * 1024)
#define GUEST_CONSOLE_TTYS 64

static pthread_mutex_t output_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t output_space = PTHREAD_COND_INITIALIZER;
static uint8_t output_buffer[GUEST_OUTPUT_CAPACITY];
static size_t output_head = 0;
static size_t output_length = 0;
static guest_output_notify_callback output_notify = NULL;
static void *output_notify_context = NULL;
static guest_exit_notify_callback exit_notify = NULL;
static void *exit_notify_context = NULL;

/* The callbacks of the runtime are registered by the host and called by the
 * guest thread. The lock keeps a registration from racing a call: the
 * registration waits for a call that is running, so the host can release the
 * context of a callback only after the callback is done with it. */
static pthread_mutex_t notify_mutex = PTHREAD_MUTEX_INITIALIZER;

/* The runtime is booted once per process. A session that was stopped and
 * started again runs on the kernel and on the root that are already there. */
static bool runtime_booted = false;

static struct tty *session_tty = NULL;

/* The pid of the process the session runs. Only its exit ends the session: a
 * helper process the guest program spawns exits on its own, and the session
 * has to keep running then. */
static int session_pid = 0;

/* The console driver owns its terminals, the pseudo terminal driver gets the
 * slave terminals from pty_open_fake. */
static struct tty *guest_console_ttys[GUEST_CONSOLE_TTYS];
static struct tty_driver guest_console_driver = {
    .ops = NULL,
    .major = TTY_CONSOLE_MAJOR,
    .ttys = guest_console_ttys,
    .limit = GUEST_CONSOLE_TTYS,
};
static struct tty_driver guest_pty_driver = {
    .ops = NULL,
    .major = 0,
    .ttys = NULL,
    .limit = 0,
};

/* ------------------------------------------------------------ output ring */

/*
 * Hands terminal output to the host. The buffer is a ring so the guest never
 * has to wait for the app; if the app stops draining, the oldest output is
 * dropped rather than stalling the guest.
 */
static void deliver_output(const void *data, size_t length) {
    if (length == 0)
        return;

    pthread_mutex_lock(&output_mutex);

    if (length > GUEST_OUTPUT_CAPACITY)
        length = GUEST_OUTPUT_CAPACITY;

    if (output_length + length > GUEST_OUTPUT_CAPACITY) {
        struct timespec deadline;
        clock_gettime(CLOCK_REALTIME, &deadline);
        deadline.tv_nsec += 100 * 1000 * 1000;
        if (deadline.tv_nsec >= 1000000000) {
            deadline.tv_sec += 1;
            deadline.tv_nsec -= 1000000000;
        }
        while (output_length + length > GUEST_OUTPUT_CAPACITY) {
            if (pthread_cond_timedwait(&output_space, &output_mutex, &deadline) != 0)
                break;
        }
        if (output_length + length > GUEST_OUTPUT_CAPACITY) {
            /* Still full: drop the oldest half so the guest keeps running. */
            size_t drop = (output_length + length) - GUEST_OUTPUT_CAPACITY;
            if (drop > output_length / 2)
                drop = output_length / 2;
            output_head = (output_head + drop) % GUEST_OUTPUT_CAPACITY;
            output_length -= drop;
        }
    }

    size_t tail = (output_head + output_length) % GUEST_OUTPUT_CAPACITY;
    size_t first = GUEST_OUTPUT_CAPACITY - tail;
    if (first > length)
        first = length;
    memcpy(output_buffer + tail, data, first);
    if (length > first)
        memcpy(output_buffer, (const uint8_t *) data + first, length - first);
    output_length += length;

    pthread_mutex_unlock(&output_mutex);

    /* The callback runs under the lock of the callbacks: a registration that
     * comes while it runs waits for it, so the host releases the context of
     * the callback only after the callback is done with it. */
    pthread_mutex_lock(&notify_mutex);
    if (output_notify != NULL)
        output_notify(output_notify_context);
    pthread_mutex_unlock(&notify_mutex);
}

size_t guest_runtime_read_output(void *buffer, size_t max_length) {
    pthread_mutex_lock(&output_mutex);

    size_t length = output_length < max_length ? output_length : max_length;
    if (length > 0) {
        size_t first = GUEST_OUTPUT_CAPACITY - output_head;
        if (first > length)
            first = length;
        memcpy(buffer, output_buffer + output_head, first);
        if (length > first)
            memcpy((uint8_t *) buffer + first, output_buffer, length - first);
        output_head = (output_head + length) % GUEST_OUTPUT_CAPACITY;
        output_length -= length;
        pthread_cond_broadcast(&output_space);
    }

    pthread_mutex_unlock(&output_mutex);
    return length;
}

void guest_runtime_clear_output(void) {
    pthread_mutex_lock(&output_mutex);
    output_head = 0;
    output_length = 0;
    pthread_cond_broadcast(&output_space);
    pthread_mutex_unlock(&output_mutex);
}

void guest_runtime_set_output_notify(guest_output_notify_callback callback, void *context) {
    pthread_mutex_lock(&notify_mutex);
    output_notify = callback;
    output_notify_context = context;
    pthread_mutex_unlock(&notify_mutex);
}

void guest_runtime_set_exit_notify(guest_exit_notify_callback callback, void *context) {
    pthread_mutex_lock(&notify_mutex);
    exit_notify = callback;
    exit_notify_context = context;
    pthread_mutex_unlock(&notify_mutex);
}

/* ------------------------------------------------------------ tty drivers */

static int guest_tty_init(struct tty *tty) {
    (void) tty;
    return 0;
}

static int guest_tty_write(struct tty *tty, const void *buf, size_t len, bool blocking) {
    (void) tty;
    (void) blocking;
    deliver_output(buf, len);
    return (int) len;
}

static void guest_tty_cleanup(struct tty *tty) {
    (void) tty;
}

static const struct tty_driver_ops guest_tty_ops = {
    .init = guest_tty_init,
    .write = guest_tty_write,
    .cleanup = guest_tty_cleanup,
};

/* ------------------------------------------------------------------- boot */

/* Reports how the guest program ended. The kernel calls this when a task
 * exits; the code is the exit status, or the signal that killed it.
 *
 * The hook is called for every task that exits, so it fires for the helper
 * processes the guest program spawns as well. Only the exit of the session
 * itself ends the session, so the group leader of the exiting task is
 * compared with the pid of the session: a helper has its own group and its
 * own leader. */
static void guest_exit_hook(struct task *task, int code) {
    struct task *leader = task->group->leader;
    os_log_with_type(OS_LOG_DEFAULT, OS_LOG_TYPE_INFO,
                     "[LegHost] the guest task %d (%s) exited with %d", leader->pid, leader->comm, code);
    if (leader->pid != session_pid)
        return;

    /* The callback runs under the lock of the callbacks: a registration that
     * comes while it runs waits for it, so the host releases the context of
     * the callback only after the callback is done with it. */
    pthread_mutex_lock(&notify_mutex);
    if (exit_notify != NULL)
        exit_notify(code, exit_notify_context);
    pthread_mutex_unlock(&notify_mutex);
}

int guest_runtime_boot(const char *root_data_dir) {
    /* The kernel and the root of the guest are set up once per process. A
     * session that starts again after it was stopped runs on them. */
    if (runtime_booted)
        return 0;

    exit_hook = guest_exit_hook;

    guest_console_driver.ops = &guest_tty_ops;
    guest_pty_driver.ops = &guest_tty_ops;

    int err = mount_root(&fakefs, root_data_dir);
    if (err < 0)
        return err;

    /* The first process has to exist before the device nodes are created. */
    err = become_first_process();
    if (err < 0)
        return err;

    create_some_device_nodes();

    /* The root of the fakefs comes out of the import with unusable
     * permissions, so fix them the way iSH does. */
    generic_setattrat(AT_PWD, "/", (struct attr) {.type = attr_mode, .mode = 0755}, false);

    do_mount(&procfs, "proc", "/proc", "", 0);
    do_mount(&devptsfs, "devpts", "/dev/pts", "", 0);

    tty_drivers[TTY_CONSOLE_MAJOR] = &guest_console_driver;
    set_console_device(TTY_CONSOLE_MAJOR, 1);

    err = create_stdio("/dev/console", TTY_CONSOLE_MAJOR, 1);
    if (err < 0)
        return err;

    runtime_booted = true;
    return 0;
}

/* ---------------------------------------------------------------- session */

int guest_runtime_start_session(const char *program, const char *arguments,
                                size_t argument_count, const char *environment) {
    if (program == NULL || arguments == NULL || argument_count == 0)
        return -EINVAL;

    /* The session runs as a child of the init process. The kernel keeps the
     * current task per thread, so the session needs a task of its own: the
     * terminal then has a foreground process group and the interrupt signals
     * reach the guest. */
    int err = become_new_init_child();
    if (err < 0)
        return err;

    /* The session runs in its own process group. Its pid tells the exit hook
     * which exit ends the session. */
    session_pid = current->pid;

    struct tty *tty = pty_open_fake(&guest_pty_driver);
    if (IS_ERR(tty))
        return PTR_ERR(tty);

    char stdio_file[64];
    snprintf(stdio_file, sizeof(stdio_file), "/dev/pts/%d", tty->num);
    err = create_stdio(stdio_file, TTY_PSEUDO_SLAVE_MAJOR, tty->num);
    if (err < 0) {
        lock(&ttys_lock);
        tty_release(tty);
        unlock(&ttys_lock);
        return err;
    }

    /* The runtime keeps a reference of its own: the guest closes its end of
     * the terminal when it exits, and the terminal may not be freed before
     * that. */
    lock(&tty->lock);
    tty->refcount++;
    unlock(&tty->lock);

    /* The reference that pty_open_fake took is not needed anymore. */
    lock(&ttys_lock);
    tty_release(tty);
    unlock(&ttys_lock);

    session_tty = tty;

    /* The guest program asks for the terminal size as soon as it starts, so
     * the terminal gets a usable size before the program runs. */
    lock(&tty->lock);
    tty_set_winsize(tty, (struct winsize_) {.col = 80, .row = 24});
    unlock(&tty->lock);

    /* The kernel copies the argument vector and the environment into the new
     * process, so both can be released right afterwards. */
    err = do_execve(program, argument_count, arguments, environment);
    os_log_with_type(OS_LOG_DEFAULT, OS_LOG_TYPE_INFO, "[LegHost] exec %{public}s returned %d", program, err);
    if (err < 0)
        return err;

    task_start(current);
    int pid = current->pid;
    /* The guest runs on a thread of its own now, and that thread knows which
     * task it runs. The thread that started the session is a thread of the
     * host, so it does not point at a task of the guest anymore: a task
     * that is gone must not be reached through it. */
    current = NULL;
    os_log_with_type(OS_LOG_DEFAULT, OS_LOG_TYPE_INFO, "[LegHost] guest task %d started", pid);
    return 0;
}

int guest_runtime_write_input(const void *data, size_t length) {
    struct tty *tty = session_tty;
    if (tty == NULL)
        return -ENOTTY;
    ssize_t written = tty_input(tty, data, length, false);
    if (written < 0)
        return (int) written;
    return (int) written;
}

int guest_runtime_resize(int columns, int rows) {
    struct tty *tty = session_tty;
    if (tty == NULL)
        return -ENOTTY;
    if (columns <= 0 || rows <= 0)
        return -EINVAL;

    lock(&tty->lock);
    tty_set_winsize(tty, (struct winsize_) {.col = columns, .row = rows});
    unlock(&tty->lock);
    return 0;
}

void guest_runtime_shutdown(void) {
    struct tty *tty = session_tty;
    if (tty == NULL)
        return;
    session_tty = NULL;

    lock(&tty->lock);
    tty_hangup(tty);
    unlock(&tty->lock);

    lock(&ttys_lock);
    tty_release(tty);
    unlock(&ttys_lock);
}

/* ---------------------------------------------------------- stopping a guest */

/* The time the runtime waits for the guest of a session to stop.
 *
 * The guest is killed before the wait starts, so the wait is short. It is
 * bounded anyway, so a guest that does not stop cannot hold the host. */
#define GUEST_STOP_TIMEOUT_MS 5000
/* The step between two checks of the tasks of the guest. */
#define GUEST_STOP_STEP_MS 20

/* The number of tasks of the guest that run.
 *
 * The init process of the kernel is not counted: it is the parent of every
 * session and it does not run. A task that is on its way out is not counted
 * either: it released its files already, so it cannot write to the trail of
 * the guest anymore. */
int guest_runtime_running_tasks(void) {
    int running = 0;
    lock(&pids_lock);
    for (int pid = 2; pid <= MAX_PID; pid++) {
        struct task *task = pid_get_task((dword_t) pid);
        if (task != NULL && !task->exiting)
            running++;
    }
    unlock(&pids_lock);
    return running;
}

/* Writes the tasks of the guest that run to the log.
 *
 * The tasks are the evidence of a guest that does not stop: the log says
 * which task of the guest is still there and what it waits for. */
static void guest_log_tasks(void) {
    lock(&pids_lock);
    for (int pid = 2; pid <= MAX_PID; pid++) {
        struct task *task = pid_get_task((dword_t) pid);
        if (task == NULL)
            continue;
        os_log_with_type(OS_LOG_DEFAULT, OS_LOG_TYPE_ERROR,
                         "[LegHost] the task %d (%s) of the guest runs: exiting %d, "
                         "pending 0x%llx, blocked 0x%llx, waits %d",
                         pid, task->comm, task->exiting,
                         (unsigned long long) task->pending,
                         (unsigned long long) task->blocked,
                         task->waiting_cond != NULL);
    }
    unlock(&pids_lock);
}

/* Wakes a task of the guest that waits.
 *
 * The kernel hands a signal to a task only when the signal is not pending
 * yet: a task that the first signal did not wake is not woken by the next
 * one either. The stop of the guest cannot lean on that, because a task
 * that sleeps in a wait that no signal breaks would keep the guest alive.
 *
 * The wake breaks the wait of the task in the two ways a task of the guest
 * can sleep. A signal interrupts the system call of the host the task waits
 * in, and the condition the task waits on is notified. */
static void guest_wake_task(struct task *task) {
    pthread_kill(task->thread, SIGUSR1);

    /* The condition is notified without the lock of the wait: the task that
     * holds the lock of its own wait is not woken by this round, and the
     * next round wakes it. The lock of the kernel is not taken here, so a
     * task of the guest cannot be held up by a lock the host waits for. */
    lock(&task->waiting_cond_lock);
    cond_t *cond = task->waiting_cond;
    if (cond != NULL)
        notify(cond);
    unlock(&task->waiting_cond_lock);
}

/* Ends the waits of the tasks of the guest that wait for data.
 *
 * A task of the guest that waits for data sleeps in a read of the host, and
 * the read of the host is not interrupted by a signal: the signal of the
 * kill does not end that wait. The wait ends when the data comes.
 *
 * A byte is written into every pipe of the guest, and every socket of the
 * guest is shut down. The task that waits wakes up, and the syscall that
 * ends its wait runs. The kill that waits for the task lands before the
 * task runs another instruction of the guest, so the guest does not write to
 * the trail of the guest anymore.
 *
 * The write does not block: a pipe that is full holds data, and the task
 * that waits for data is not waiting then. */
static void guest_unblock_tasks(void) {
    lock(&pids_lock);
    for (int pid = 2; pid <= MAX_PID; pid++) {
        struct task *task = pid_get_task((dword_t) pid);
        if (task == NULL)
            continue;
        /* A task that is on its way out is left alone: it released its
         * files already and it cannot write to the trail anymore. */
        if (task->exiting)
            continue;
        struct fdtable *table = task->files;
        if (table == NULL)
            continue;
        lock(&table->lock);
        for (unsigned index = 0; index < table->size; index++) {
            struct fd *fd = table->files[index];
            if (fd == NULL || fd->real_fd < 0)
                continue;
            struct stat status;
            if (fstat(fd->real_fd, &status) != 0)
                continue;
            if (S_ISSOCK(status.st_mode)) {
                shutdown(fd->real_fd, SHUT_RDWR);
            } else if (S_ISFIFO(status.st_mode)) {
                int flags = fcntl(fd->real_fd, F_GETFL);
                if (flags < 0 || (flags & O_ACCMODE) == O_RDONLY)
                    continue;
                fcntl(fd->real_fd, F_SETFL, flags | O_NONBLOCK);
                ssize_t written = write(fd->real_fd, "", 1);
                fcntl(fd->real_fd, F_SETFL, flags);
                (void) written;
            }
        }
        unlock(&table->lock);
    }
    unlock(&pids_lock);
}

/* Kills every task of the guest and wakes the tasks that wait.
 *
 * The signal does not go through the syscall of the kernel: that path reads
 * the current task of the calling thread, and the host has none. The kernel
 * interrupts the syscall the guest waits in and delivers the kill before the
 * guest runs another instruction, so the guest cannot write to the trail of
 * the guest anymore.
 *
 * The kill is given again while the guest is still there: a task that the
 * guest forked after the first kill is killed as well, and a task that the
 * first kill did not wake is woken again. */
static void guest_kill_tasks(void) {
    lock(&pids_lock);
    for (int pid = 2; pid <= MAX_PID; pid++) {
        struct task *task = pid_get_task((dword_t) pid);
        if (task == NULL)
            continue;
        /* A task that is on its way out is left alone: its thread is gone
         * and it cannot write to the trail of the guest anymore. */
        if (task->exiting)
            continue;
        send_signal(task, SIGKILL_, SIGINFO_NIL);
        guest_wake_task(task);
    }
    unlock(&pids_lock);
}

void guest_runtime_kill_guest(void) {
    /* The kernel keeps the current task per thread, and a thread of the
     * host is not a task of the guest. The signal of the kill has to reach
     * the tasks that run, so the current task of this thread is cleared for
     * the time of the stop. */
    current = NULL;

    guest_kill_tasks();
    guest_unblock_tasks();
}

int guest_runtime_stop_session(void) {
    guest_runtime_kill_guest();

    struct timespec deadline;
    clock_gettime(CLOCK_MONOTONIC, &deadline);
    deadline.tv_sec += GUEST_STOP_TIMEOUT_MS / 1000;
    deadline.tv_nsec += (long) (GUEST_STOP_TIMEOUT_MS % 1000) * 1000000L;
    if (deadline.tv_nsec >= 1000000000L) {
        deadline.tv_sec += 1;
        deadline.tv_nsec -= 1000000000L;
    }

    bool stopped = false;
    while (true) {
        if (guest_runtime_running_tasks() == 0) {
            stopped = true;
            break;
        }
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        if (now.tv_sec > deadline.tv_sec
            || (now.tv_sec == deadline.tv_sec && now.tv_nsec >= deadline.tv_nsec))
            break;
        struct timespec step = { .tv_sec = 0, .tv_nsec = GUEST_STOP_STEP_MS * 1000000L };
        nanosleep(&step, NULL);
        /* The kill is given again while the guest is still there: a task
         * that the guest forked after the first kill is killed as well, and
         * a task that waits for data is woken again. */
        guest_kill_tasks();
        guest_unblock_tasks();
    }

    if (!stopped) {
        guest_log_tasks();
        os_log_with_type(OS_LOG_DEFAULT, OS_LOG_TYPE_ERROR,
                         "[LegHost] the guest of the session did not stop");
        return -ETIMEDOUT;
    }

    /* The session is gone, so its pid is not the pid of a session any more:
     * the exit of a task of the guest does not end a session that is not
     * there. */
    session_pid = 0;
    return 0;
}
