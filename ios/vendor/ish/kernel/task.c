#define _GNU_SOURCE
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>
#include "debug.h"
#include "kernel/calls.h"
#include "kernel/task.h"
#include "kernel/memory.h"
#include "emu/tlb.h"

__thread struct task *current;

static struct pid pids[MAX_PID + 1] = {};
lock_t pids_lock = LOCK_INITIALIZER;

static bool pid_empty(struct pid *pid) {
    return pid->task == NULL && list_empty(&pid->session) && list_empty(&pid->pgroup);
}

struct pid *pid_get(dword_t id) {
    if (id > sizeof(pids)/sizeof(pids[0]))
        return NULL;
    struct pid *pid = &pids[id];
    if (pid_empty(pid))
        return NULL;
    return pid;
}

struct task *pid_get_task_zombie(dword_t id) {
    struct pid *pid = pid_get(id);
    if (pid == NULL)
        return NULL;
    struct task *task = pid->task;
    return task;
}

struct task *pid_get_task(dword_t id) {
    struct task *task = pid_get_task_zombie(id);
    if (task != NULL && task->zombie)
        return NULL;
    return task;
}

struct task *task_create_(struct task *parent) {
    lock(&pids_lock);
    static int cur_pid = 0;
    do {
        cur_pid++;
        if (cur_pid > MAX_PID) cur_pid = 1;
    } while (!pid_empty(&pids[cur_pid]));
    struct pid *pid = &pids[cur_pid];
    pid->id = cur_pid;
    list_init(&pid->session);
    list_init(&pid->pgroup);

    struct task *task = malloc(sizeof(struct task));
    if (task == NULL)
        return NULL;
    *task = (struct task) {};
    if (parent != NULL)
        *task = *parent;
    task->pid = pid->id;
    pid->task = task;

    list_init(&task->children);
    list_init(&task->siblings);
    if (parent != NULL) {
        task->parent = parent;
        list_add(&parent->children, &task->siblings);
    }
    unlock(&pids_lock);

    task->pending = 0;
    list_init(&task->queue);
    task->clear_tid = 0;
    task->robust_list = 0;
    task->did_exec = false;
    lock_init(&task->general_lock);

    task->sockrestart = (struct task_sockrestart) {};
    list_init(&task->sockrestart.listen);

    task->waiting_cond = NULL;
    task->waiting_lock = NULL;
    lock_init(&task->waiting_cond_lock);
    cond_init(&task->pause);

    lock_init(&task->ptrace.lock);
    cond_init(&task->ptrace.cond);
    return task;
}

void task_destroy(struct task *task) {
    list_remove(&task->siblings);
    pid_get(task->pid)->task = NULL;
    free(task);
}

void task_run_current(void) {
    struct cpu_state *cpu = &current->cpu;
    struct tlb tlb = {};
    tlb_refresh(&tlb, &current->mem->mmu);
    while (true) {
        read_wrlock(&current->mem->lock);
        int interrupt = cpu_run_to_interrupt(cpu, &tlb);
        read_wrunlock(&current->mem->lock);
        handle_interrupt(interrupt);
    }
}

static void *task_thread(void *task) {
    current = task;
    update_thread_name();
    task_run_current();
    die("task_thread returned"); // above function call should never return
}

/* Ends the waits of the tasks of the guest that wait for data.
 *
 * A task of the guest that waits for data sleeps in a read of the host, and
 * the read of the host is not interrupted by a signal: the signal of the kill
 * does not end that wait. The wait ends when the data comes: a byte is
 * written into every pipe of the guest, and every socket of the guest is shut
 * down. The task that waits wakes up, and the syscall that ends its wait
 * runs. The kill that waits for the task lands before the task runs another
 * instruction of the guest.
 *
 * The files of a task belong to the task, and the task frees them when it
 * exits. The walk holds pids_lock, and every path that lets go of the files
 * of a task holds that lock: a task that exits marks itself as exiting under
 * the lock before it lets go of its files, and a task that could not be made
 * lets go of its files under the lock as well. A table of files is therefore
 * not freed while the walk runs. The table is kept alive while it is walked
 * as well, so a task that exits in the meantime leaves its table behind: the
 * files of the task are closed when the last one that holds the table lets it
 * go.
 *
 * The caller does not hold pids_lock. */
void tasks_end_data_waits(void) {
    lock(&pids_lock);
    for (int pid = 2; pid <= MAX_PID; pid++) {
        struct task *task = pid_get_task((dword_t) pid);
        if (task == NULL)
            continue;
        /* A task that is on its way out is left alone: it does not hold its
         * files any more. A task that never started holds no files. */
        if (task->exiting || task->thread == 0)
            continue;
        struct fdtable *table = task->files;
        if (table == NULL)
            continue;
        /* The table stays alive while it is walked. */
        fdtable_retain(table);
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
                /* The write does not block: a pipe that is full holds data,
                 * and the task that waits for data is not waiting then. */
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
        fdtable_release(table);
    }
    unlock(&pids_lock);
}

/* The task is on its way out: it is marked as exiting and its files are
 * detached under pids_lock. The table that was detached is returned, and the
 * caller lets it go with fdtable_release after the lock is let go.
 *
 * The mark and the detaching are one step under the lock: a walker of the
 * tasks that holds pids_lock sees either a task that holds its files, or a
 * task that is marked and holds none. The two cannot both be true, so a
 * walker cannot see the files of a task that is let go, and the table of a
 * task that is let go cannot be freed while such a walker holds it.
 *
 * The caller does not hold pids_lock. */
struct fdtable *task_detach_files(struct task *task) {
    lock(&pids_lock);
    task->exiting = true;
    struct fdtable *files = task->files;
    task->files = NULL;
    unlock(&pids_lock);
    return files;
}

/* The task is let go: it is marked as exiting and its files are detached
 * under pids_lock, and the table that was detached is let go after the lock
 * is let go.
 *
 * A walker of the tasks that holds pids_lock sees either a task that holds
 * its files or a task that is marked, and a table that is detached is not
 * walked any more: the walk cannot see the files of a task that is let go.
 *
 * The caller does not hold pids_lock. */
void task_abandon(struct task *task) {
    struct fdtable *files = task_detach_files(task);
    if (files != NULL)
        fdtable_release(files);
}

static pthread_attr_t task_thread_attr;
__attribute__((constructor)) static void create_attr(void) {
    pthread_attr_init(&task_thread_attr);
    pthread_attr_setdetachstate(&task_thread_attr, PTHREAD_CREATE_DETACHED);
}

void task_start(struct task *task) {
    if (pthread_create(&task->thread, &task_thread_attr, task_thread, task) < 0)
        die("could not create thread");
}

int_t sys_sched_yield(void) {
    STRACE("sched_yield()");
    sched_yield();
    return 0;
}

void update_thread_name(void) {
    char name[16]; // As long as Linux will let us make this
    snprintf(name, sizeof(name), "-%d", current->pid);
    size_t pid_width = strlen(name);
    size_t name_width = snprintf(name, sizeof(name), "%s", current->comm);
    sprintf(name + (name_width < sizeof(name) - 1 - pid_width ? name_width : sizeof(name) - 1 - pid_width), "-%d", current->pid);
#if __APPLE__
    pthread_setname_np(name);
#else
    pthread_setname_np(pthread_self(), name);
#endif
}
