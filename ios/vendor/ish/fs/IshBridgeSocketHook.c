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

static lock_t hook_lock = LOCK_INITIALIZER;

static struct {
    uint64_t generation;
    bool registered;
    char host_path[ISH_BRIDGE_SOCKET_HOST_PATH_MAX + 1];
    /* The fake inode the hook created, held so its socket id stays the
     * same for as long as the endpoint is provisioned. */
    struct inode_data *inode;
    /* The endpoint the hook made, recorded before the node is made, so a
     * start that fails after that still removes it. The identity is known
     * once the node was looked up; until then the record covers a socket at
     * the endpoint, which is reserved to the host. */
    bool made;
    bool made_known;
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

/* Takes a hold on the inode at the endpoint. */
static struct inode_data *endpoint_inode(const struct statbuf *stat) {
    char path[MAX_PATH];
    if (path_normalize(AT_PWD, ISH_BRIDGE_SOCKET_GUEST_PATH, path, N_SYMLINK_NOFOLLOW) < 0)
        return NULL;
    struct mount *mount = find_mount_and_trim_path(path);
    struct inode_data *inode = inode_get(mount, stat->inode);
    mount_release(mount);
    return inode;
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
    bool ours = S_ISSOCK(stat.mode) &&
        (!hook.made_known || (stat.dev == hook.made_dev && stat.inode == hook.made_inode));
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
    hook.made_known = false;

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

    /* The endpoint was absent a moment ago, so a socket there from now on
     * is the one made here. */
    hook.made = true;
    if (generic_mknodat(AT_PWD, ISH_BRIDGE_SOCKET_GUEST_PATH, S_IFSOCK | 0666, 0) < 0)
        goto out;
    if (take_fault(ISH_BRIDGE_SOCKET_FAULT_LOOKUP) || endpoint_stat(&stat) < 0 || !S_ISSOCK(stat.mode))
        goto out;
    hook.made_known = true;
    hook.made_dev = stat.dev;
    hook.made_inode = stat.inode;
    if (take_fault(ISH_BRIDGE_SOCKET_FAULT_HOLD))
        goto out;
    hook.inode = endpoint_inode(&stat);
    if (hook.inode == NULL)
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
