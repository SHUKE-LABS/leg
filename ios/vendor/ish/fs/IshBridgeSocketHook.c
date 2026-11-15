/*
 * The capability bridge endpoint of the LegHost guest. See
 * IshBridgeSocketHook.h.
 */

#include <string.h>
#include <sys/stat.h>

#include "kernel/errno.h"
#include "kernel/fs.h"
#include "kernel/task.h"
#include "fs/inode.h"
#include "fs/path.h"
#include "fs/IshBridgeSocketHook.h"
#include "util/sync.h"
#define ISH_INTERNAL
#include "fs/fake.h"

static lock_t hook_lock = LOCK_INITIALIZER;

static struct {
    uint64_t generation;
    bool registered;
    char host_path[ISH_BRIDGE_SOCKET_HOST_PATH_MAX + 1];
    /* The fake inode the hook created, held from the moment it exists: its
     * socket id stays the same for as long as the endpoint is provisioned,
     * and fakefs cannot give its number to another node while the hook
     * records it. */
    struct inode_data *inode;
    /* The identity of the endpoint the hook made. The node, its identity
     * and the hold come together, so a start that fails after that still
     * removes it, and the record never covers a node the hook did not
     * make. */
    bool made;
    qword_t made_dev;
    qword_t made_inode;
    /* The faults the checks of the cleanup inject, ISH_BRIDGE_SOCKET_FAULT_*
     * bits, each used once. */
    int faults;
} hook;

/* Takes the fault `fault` when it was injected. Called with hook_lock held. */
static bool take_fault(int fault) {
    if (!(hook.faults & fault))
        return false;
    hook.faults &= ~fault;
    return true;
}

/* Drops the hold on the fake inode. Called with hook_lock held. */
static void release_inode(void) {
    if (hook.inode != NULL) {
        inode_release(hook.inode);
        hook.inode = NULL;
    }
}

int ish_bridge_socket_register(uint64_t generation, const char *host_path) {
    if (host_path == NULL)
        return ISH_BRIDGE_SOCKET_PATH_TOO_LONG;
    size_t length = strlen(host_path);
    if (length == 0 || length > ISH_BRIDGE_SOCKET_HOST_PATH_MAX)
        return ISH_BRIDGE_SOCKET_PATH_TOO_LONG;
    lock(&hook_lock);
    if (hook.registered && hook.generation != generation) {
        unlock(&hook_lock);
        return ISH_BRIDGE_SOCKET_BUSY;
    }
    hook.generation = generation;
    hook.registered = true;
    memcpy(hook.host_path, host_path, length + 1);
    unlock(&hook_lock);
    return ISH_BRIDGE_SOCKET_OK;
}

int ish_bridge_socket_unregister(uint64_t generation) {
    lock(&hook_lock);
    if (!hook.registered || hook.generation != generation) {
        unlock(&hook_lock);
        return ISH_BRIDGE_SOCKET_STALE;
    }
    release_inode();
    hook.registered = false;
    hook.generation = 0;
    memset(hook.host_path, 0, sizeof(hook.host_path));
    unlock(&hook_lock);
    return ISH_BRIDGE_SOCKET_OK;
}

/* Looks the endpoint up without following a link at its last component. */
static int endpoint_stat(struct statbuf *stat) {
    return generic_statat(AT_PWD, ISH_BRIDGE_SOCKET_GUEST_PATH, stat, false);
}

/* Makes the endpoint socket, looks it up in the same fakefs transaction, and
 * takes a hold on its inode. As in generic_open, inodes_lock is held from
 * before the node exists until the hold does, so no unlink can orphan the
 * inode and let fakefs give its number to another node in between. */
static int make_endpoint(struct statbuf *stat, struct inode_data **inode) {
    char path[MAX_PATH];
    int err = path_normalize(AT_PWD, ISH_BRIDGE_SOCKET_GUEST_PATH, path, N_SYMLINK_NOFOLLOW | N_PARENT_DIR_WRITE);
    if (err < 0)
        return err;
    struct mount *mount = find_mount_and_trim_path(path);
    err = _EPERM;
    lock(&inodes_lock);
    if (mount->fs == &fakefs)
        err = fakefs_mknod_stat(mount, path, S_IFSOCK | 0666, 0, stat);
    if (err == 0)
        *inode = inode_get_unlocked(mount, stat->inode);
    unlock(&inodes_lock);
    mount_release(mount);
    return err;
}

/* For the checks of the cleanup: a socket the hook did not make takes the
 * endpoint. */
static void put_foreign_endpoint(void) {
    generic_unlinkat(AT_PWD, ISH_BRIDGE_SOCKET_GUEST_PATH);
    generic_mknodat(AT_PWD, ISH_BRIDGE_SOCKET_GUEST_PATH, S_IFSOCK | 0666, 0);
}

void ish_bridge_socket_inject_faults(int faults) {
    lock(&hook_lock);
    hook.faults = faults;
    unlock(&hook_lock);
}

/* Removes the endpoint the hook made, when it is still that socket: a file
 * the guest put there in its place is the guest's. The record goes only
 * once the endpoint is known to be gone or not the hook's; a lookup or an
 * unlink that fails keeps it, so a later cleanup can retry. Called with
 * hook_lock held. */
static int remove_made_endpoint(void) {
    if (!hook.made)
        return ISH_BRIDGE_SOCKET_OK;
    struct statbuf stat;
    int err = take_fault(ISH_BRIDGE_SOCKET_FAULT_CLEANUP_LOOKUP) ? _EIO : endpoint_stat(&stat);
    if (err == _ENOENT) {
        hook.made = false;
        return ISH_BRIDGE_SOCKET_OK;
    }
    if (err < 0)
        return ISH_BRIDGE_SOCKET_FS_ERROR;
    bool ours = S_ISSOCK(stat.mode) && stat.dev == hook.made_dev && stat.inode == hook.made_inode;
    if (ours && generic_unlinkat(AT_PWD, ISH_BRIDGE_SOCKET_GUEST_PATH) < 0)
        return ISH_BRIDGE_SOCKET_FS_ERROR;
    hook.made = false;
    return ISH_BRIDGE_SOCKET_OK;
}

int ish_bridge_socket_provision(void) {
    if (current == NULL)
        return ISH_BRIDGE_SOCKET_NO_TASK;
    lock(&hook_lock);
    if (!hook.registered) {
        unlock(&hook_lock);
        return ISH_BRIDGE_SOCKET_OK;
    }
    /* A retry starts from nothing: the hold of an earlier start goes, and a
     * socket it left at the endpoint is replaced below. */
    release_inode();
    hook.made = false;

    int result = ISH_BRIDGE_SOCKET_FS_ERROR;
    int err = generic_mkdirat(AT_PWD, "/run", 0755);
    if (err < 0 && err != _EEXIST)
        goto out;

    struct statbuf stat;
    err = endpoint_stat(&stat);
    if (err == 0) {
        /* The endpoint is reserved, so a socket there is one an earlier
         * generation left behind. Anything else is not the host's to
         * remove. */
        if (!S_ISSOCK(stat.mode)) {
            result = ISH_BRIDGE_SOCKET_NOT_SOCKET;
            goto out;
        }
        if (generic_unlinkat(AT_PWD, ISH_BRIDGE_SOCKET_GUEST_PATH) < 0)
            goto out;
    } else if (err != _ENOENT) {
        goto out;
    }

    if (take_fault(ISH_BRIDGE_SOCKET_FAULT_RACE))
        put_foreign_endpoint();
    if (make_endpoint(&stat, &hook.inode) < 0)
        goto out;
    hook.made = true;
    hook.made_dev = stat.dev;
    hook.made_inode = stat.inode;
    if (take_fault(ISH_BRIDGE_SOCKET_FAULT_REPLACE)) {
        put_foreign_endpoint();
        goto out;
    }
    if (take_fault(ISH_BRIDGE_SOCKET_FAULT_HOLD))
        goto out;
    result = ISH_BRIDGE_SOCKET_OK;

out:
    unlock(&hook_lock);
    return result;
}

int ish_bridge_socket_cleanup(void) {
    if (current == NULL)
        return ISH_BRIDGE_SOCKET_NO_TASK;
    lock(&hook_lock);
    int result = remove_made_endpoint();
    release_inode();
    unlock(&hook_lock);
    return result;
}

void ish_bridge_socket_get_state(struct ish_bridge_socket_state *state) {
    lock(&hook_lock);
    state->generation = hook.generation;
    state->registered = hook.registered;
    state->provisioned = hook.inode != NULL;
    state->endpoint_recorded = hook.made;
    state->socket_id = 0;
    if (hook.inode != NULL) {
        lock(&hook.inode->lock);
        state->socket_id = hook.inode->socket_id;
        unlock(&hook.inode->lock);
    }
    unlock(&hook_lock);
}

size_t ish_bridge_socket_connect_path(uint32_t socket_id, char *sun_path, size_t sun_path_size) {
    if (socket_id == 0)
        return 0;
    size_t length = 0;
    lock(&hook_lock);
    if (hook.registered && hook.inode != NULL) {
        lock(&hook.inode->lock);
        bool ours = hook.inode->socket_id == socket_id;
        unlock(&hook.inode->lock);
        size_t path_length = strlen(hook.host_path);
        if (ours && path_length < sun_path_size) {
            memcpy(sun_path, hook.host_path, path_length + 1);
            length = path_length;
        }
    }
    unlock(&hook_lock);
    return length;
}
