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
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <os/log.h>

#include "fs/devices.h"
#include "fs/fd.h"
#include "fs/path.h"
#include "fs/proc.h"
#include "fs/real.h"
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
    if (err < 0) {
        /* The session could not start, so the task of the session never ran.
         * The kernel lets the task go: the task is marked as gone and its
         * files are detached under the lock of the tasks, so a walk of the
         * files of the tasks leaves the task alone. */
        task_abandon(current);
        current = NULL;
        session_pid = 0;
        return err;
    }

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
        /* A task that never started does not run: the kernel leaves the task
         * of a session behind when the session could not start. */
        if (task != NULL && !task->exiting && task->thread != 0)
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

/* Ends the wait of the tasks of the guest that read the terminal.
 *
 * The terminal belongs to the host, so the host may hang it up: a task that
 * reads the terminal wakes up, and the kill that waits for the task lands
 * before the task runs another instruction of the guest.
 *
 * The wait of a task that reads the connection of the provider ends when the
 * host closes that connection: the host stops the fixture of the run after
 * the kill, and the fixture closes the connections it holds.
 *
 * The host does not touch the files of a task: the files of a task belong to
 * the task, and the task frees them when it exits. A host that reads the
 * files of a task while the task exits reads memory that is gone. */
static void guest_hangup_terminal(void) {
    struct tty *tty = session_tty;
    if (tty == NULL)
        return;
    lock(&tty->lock);
    tty_hangup(tty);
    unlock(&tty->lock);
}

/* Kills every task of the guest.
 *
 * The signal does not go through the syscall of the kernel: that path reads
 * the current task of the calling thread, and the host has none. The kernel
 * delivers the kill before the guest runs another instruction, so the guest
 * cannot write to the trail of the guest anymore.
 *
 * The kill is given again while the guest is still there: a task that the
 * guest forked after the first kill is killed as well. */
static void guest_kill_tasks(void) {
    lock(&pids_lock);
    for (int pid = 2; pid <= MAX_PID; pid++) {
        struct task *task = pid_get_task((dword_t) pid);
        if (task == NULL)
            continue;
        /* A task that is on its way out is left alone: its thread is gone
         * and it cannot write to the trail of the guest anymore. A task that
         * never started has no thread to signal. A task that holds no
         * signals cannot be signalled at all: the host makes such a task
         * for the check of the cleanup of a session, and the kill leaves
         * the task alone. */
        if (task->exiting || task->thread == 0 || task->sighand == NULL)
            continue;
        send_signal(task, SIGKILL_, SIGINFO_NIL);
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
    guest_hangup_terminal();
    /* The waits of the tasks that wait for data end here: the kernel knows
     * the files of the tasks and ends the waits that block in the host. The
     * host does not touch the files of a task itself: the files of a task
     * belong to the task, and the task frees them when it exits. */
    tasks_end_data_waits();
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
         * that the guest forked after the first kill is killed as well. The
         * waits are ended again as well: a task that waits again after a
         * wake waits for a long time, and the round that follows ends that
         * wait as well. */
        guest_kill_tasks();
        tasks_end_data_waits();
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

#if DEBUG
/*
 * The check of the cleanup of a session that failed and the walk of the files
 * of the tasks.
 *
 * The two run at the same time in the host: the walk of the files of the
 * tasks must not see the files of a task that is let go. The check makes
 * tasks with tables of files, lets one of them go while the walk runs, and
 * repeats it. A walk that sees the files of a task that is let go keeps a
 * table that is gone: it reads the table of a memory that is gone, and it
 * lets the table go a second time. The run ends in a crash then.
 *
 * The tasks of the check are made by the host: they hold files and no
 * signals, and they are left alone by the walk of the kill. The check runs
 * where the guest does not run, so the walk of the tasks walks the tasks of
 * the check only.
 *
 * The result is the number of rounds that ran. */
struct cleanup_walk_check {
    struct task **tasks;
    int task_count;
    int rounds;
    atomic_bool walking;
};

/* The number of files a task of the check holds. The files make the release
 * of the table of the task take a while, so the walk of the tasks has a
 * chance to see the table of the task while it is let go. */
#define CHECK_TASK_FILES 16

static struct task *walk_check_make_task(void) {
    struct task *task = task_create_(NULL);
    if (task == NULL)
        return NULL;
    task->files = fdtable_new(CHECK_TASK_FILES + 1);
    if (IS_ERR(task->files)) {
        task->files = NULL;
        lock(&pids_lock);
        task_destroy(task);
        unlock(&pids_lock);
        return NULL;
    }
    for (int index = 0; index < CHECK_TASK_FILES; index++) {
        /* The file is made the way the kernel makes a file of the host: it
         * has a mount, so the walk of the /proc can read its path. */
        struct fd *file = adhoc_fd_create(&realfs_fdops);
        if (file == NULL)
            continue;
        file->real_fd = open("/dev/null", O_RDONLY);
        if (file->real_fd < 0) {
            fd_close(file);
            continue;
        }
        task->files->files[index] = file;
    }
    /* The task runs, as far as the walk of the tasks knows: the walk leaves
     * a task that never started alone. */
    task->thread = (pthread_t) (uintptr_t) 1;
    return task;
}

static void *cleanup_walk_check_walk(void *context) {
    struct cleanup_walk_check *check = context;
    while (atomic_load(&check->walking))
        tasks_end_data_waits();
    return NULL;
}

static void *cleanup_walk_check_release(void *context) {
    struct cleanup_walk_check *check = context;
    for (int round = 0; round < check->rounds; round++) {
        int index = round % check->task_count;
        struct task *task = check->tasks[index];
        /* The task is let go the way the host lets a session that failed go:
         * the task is marked as gone and its files are detached under the
         * lock of the tasks. */
        task_abandon(task);
        lock(&pids_lock);
        check->tasks[index] = NULL;
        task_destroy(task);
        unlock(&pids_lock);
        struct task *fresh = walk_check_make_task();
        if (fresh == NULL)
            break;
        lock(&pids_lock);
        check->tasks[index] = fresh;
        unlock(&pids_lock);
    }
    atomic_store(&check->walking, false);
    return NULL;
}

int guest_runtime_check_cleanup_walk_race(int rounds, int tasks) {
    if (rounds < 1 || tasks < 1)
        return 0;
    struct cleanup_walk_check check = {0};
    check.task_count = tasks;
    check.rounds = rounds;
    check.tasks = calloc(tasks, sizeof(struct task *));
    if (check.tasks == NULL)
        return 0;
    for (int index = 0; index < tasks; index++) {
        check.tasks[index] = walk_check_make_task();
        if (check.tasks[index] == NULL) {
            for (int other = 0; other < index; other++) {
                task_abandon(check.tasks[other]);
                lock(&pids_lock);
                task_destroy(check.tasks[other]);
                unlock(&pids_lock);
            }
            free(check.tasks);
            return 0;
        }
    }
    atomic_store(&check.walking, true);
    pthread_t walker;
    pthread_t releaser;
    if (pthread_create(&walker, NULL, cleanup_walk_check_walk, &check) != 0) {
        free(check.tasks);
        return 0;
    }
    if (pthread_create(&releaser, NULL, cleanup_walk_check_release, &check) != 0) {
        atomic_store(&check.walking, false);
        pthread_join(walker, NULL);
        free(check.tasks);
        return 0;
    }
    pthread_join(releaser, NULL);
    pthread_join(walker, NULL);
    for (int index = 0; index < tasks; index++) {
        struct task *task = check.tasks[index];
        if (task == NULL)
            continue;
        task_abandon(task);
        lock(&pids_lock);
        task_destroy(task);
        unlock(&pids_lock);
    }
    free(check.tasks);
    return rounds;
}

/*
 * The check of the exit of a task and the walk of its files by the /proc.
 *
 * A task that exits lets its files go, and the /proc walks the files of a
 * task: the two run at the same time, and the walk must not see the files of
 * a task that is let go. The check lets a task go the way a task that exits
 * lets its files go, while the /proc walks the files of the task, and it
 * repeats it. A walk that sees the files of a task that is let go reads a
 * table that is gone, and the run ends in a crash then.
 *
 * The result is the number of rounds that ran. */
struct exit_walk_check {
    struct task **tasks;
    int task_count;
    int rounds;
    atomic_bool walking;
};

static void *exit_walk_check_walk(void *context) {
    struct exit_walk_check *check = context;
    while (atomic_load(&check->walking)) {
        for (int index = 0; index < check->task_count; index++) {
            /* The task is read under the lock of the tasks: the walk of the
             * /proc takes the lock itself, and the task must not be gone
             * while it is read. */
            pid_t_ pid = 0;
            lock(&pids_lock);
            struct task *task = check->tasks[index];
            if (task != NULL)
                pid = task->pid;
            unlock(&pids_lock);
            if (pid != 0)
                proc_pid_fd_walk(pid);
        }
    }
    return NULL;
}

static void *exit_walk_check_release(void *context) {
    struct exit_walk_check *check = context;
    for (int round = 0; round < check->rounds; round++) {
        int index = round % check->task_count;
        struct task *task = check->tasks[index];
        /* The task exits the way a task of the guest exits: the task is
         * marked as exiting and its files are detached under the lock of the
         * tasks, and the table that was detached is let go after the lock is
         * let go. The pause between the two widens the window in which a
         * walk of the /proc can come. */
        struct fdtable *files = task_detach_files(task);
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000};
        nanosleep(&pause, NULL);
        if (files != NULL)
            fdtable_release(files);
        lock(&pids_lock);
        check->tasks[index] = NULL;
        task_destroy(task);
        unlock(&pids_lock);
        struct task *fresh = walk_check_make_task();
        if (fresh == NULL)
            break;
        lock(&pids_lock);
        check->tasks[index] = fresh;
        unlock(&pids_lock);
    }
    atomic_store(&check.walking, false);
    return NULL;
}

int guest_runtime_check_exit_walk_race(int rounds, int tasks) {
    if (rounds < 1 || tasks < 1)
        return 0;
    struct exit_walk_check check = {0};
    check.task_count = tasks;
    check.rounds = rounds;
    check.tasks = calloc(tasks, sizeof(struct task *));
    if (check.tasks == NULL)
        return 0;
    for (int index = 0; index < tasks; index++) {
        check.tasks[index] = walk_check_make_task();
        if (check.tasks[index] == NULL) {
            for (int other = 0; other < index; other++) {
                struct fdtable *files = task_detach_files(check.tasks[other]);
                if (files != NULL)
                    fdtable_release(files);
                lock(&pids_lock);
                task_destroy(check.tasks[other]);
                unlock(&pids_lock);
            }
            free(check.tasks);
            return 0;
        }
    }
    atomic_store(&check.walking, true);
    pthread_t walker;
    pthread_t releaser;
    if (pthread_create(&walker, NULL, exit_walk_check_walk, &check) != 0) {
        free(check.tasks);
        return 0;
    }
    if (pthread_create(&releaser, NULL, exit_walk_check_release, &check) != 0) {
        atomic_store(&check.walking, false);
        pthread_join(walker, NULL);
        free(check.tasks);
        return 0;
    }
    pthread_join(releaser, NULL);
    pthread_join(walker, NULL);
    for (int index = 0; index < tasks; index++) {
        struct task *task = check.tasks[index];
        if (task == NULL)
            continue;
        struct fdtable *files = task_detach_files(task);
        if (files != NULL)
            fdtable_release(files);
        lock(&pids_lock);
        task_destroy(task);
        unlock(&pids_lock);
    }
    free(check.tasks);
    return rounds;
}
#endif
