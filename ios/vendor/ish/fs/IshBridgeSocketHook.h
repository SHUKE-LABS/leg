/*
 * The capability bridge endpoint of the LegHost guest.
 *
 * The guest reaches the host's capability listener at one fixed Unix socket
 * path, /run/leghost-bridge.sock. The listener is a real AF_UNIX socket of
 * the app, at a private path the guest never sees: iSH rewrites every other
 * Unix socket path to /tmp/ishsock<pid>.<id>, and this hook maps a connect()
 * to the exact fake inode it created at the endpoint onto the listener's
 * path instead.
 *
 * The host owns one generation at a time:
 *
 *   1. register the generation and the listener path, before the session;
 *   2. provision the fake inode, in the task context of the session, before
 *      the program is executed;
 *   3. clean the fake inode up, in a task context, after a failed start or
 *      after the guest stopped;
 *   4. unregister the generation.
 *
 * The header uses plain C types only, so the Swift host can import it.
 */

#ifndef ISH_BRIDGE_SOCKET_HOOK_H
#define ISH_BRIDGE_SOCKET_HOOK_H

#include <stddef.h>
#include <stdint.h>

/* The path the guest connects to. */
#define ISH_BRIDGE_SOCKET_GUEST_PATH "/run/leghost-bridge.sock"

/* The longest listener path: the sun_path of a Darwin sockaddr_un, minus the
 * terminating NUL. */
#define ISH_BRIDGE_SOCKET_HOST_PATH_MAX 103

enum {
    ISH_BRIDGE_SOCKET_OK = 0,
    /* Another generation is registered. */
    ISH_BRIDGE_SOCKET_BUSY = -1,
    /* The generation is not the registered one. */
    ISH_BRIDGE_SOCKET_STALE = -2,
    /* The listener path does not fit a sockaddr_un. */
    ISH_BRIDGE_SOCKET_PATH_TOO_LONG = -3,
    /* Something other than a socket sits at the guest endpoint. */
    ISH_BRIDGE_SOCKET_NOT_SOCKET = -4,
    /* The guest file system refused the endpoint. */
    ISH_BRIDGE_SOCKET_FS_ERROR = -5,
    /* The call needs a task context and has none. */
    ISH_BRIDGE_SOCKET_NO_TASK = -6,
};

/* What the hook holds, for the host's checks. */
struct ish_bridge_socket_state {
    uint64_t generation;
    int registered;
    int provisioned;
    uint32_t socket_id;
    /* Whether the hook still records an endpoint it made and has not
     * removed. */
    int endpoint_recorded;
};

/* The faults the checks of the endpoint cleanup inject. */
enum {
    /* The next provision fails after it made the endpoint. */
    ISH_BRIDGE_SOCKET_FAULT_HOLD = 1 << 0,
    /* A socket the hook did not make takes the endpoint just before the
     * next provision makes it, so that provision fails. */
    ISH_BRIDGE_SOCKET_FAULT_RACE = 1 << 1,
    /* The next cleanup fails its lookup of the endpoint. */
    ISH_BRIDGE_SOCKET_FAULT_CLEANUP_LOOKUP = 1 << 2,
    /* A socket the hook did not make replaces the endpoint right after the
     * next provision made it, and that provision fails. */
    ISH_BRIDGE_SOCKET_FAULT_REPLACE = 1 << 3,
    /* After the next cleanup failed and kept its record, a socket the hook
     * did not make replaces the endpoint, and the cleanup runs again. */
    ISH_BRIDGE_SOCKET_FAULT_CLEANUP_RETRY = 1 << 4,
};

/* Registers `generation` and the listener at `host_path`. */
int ish_bridge_socket_register(uint64_t generation, const char *host_path);

/* Forgets `generation` and its mapping, and drops its hold on the fake inode
 * unless the endpoint is still recorded. */
int ish_bridge_socket_unregister(uint64_t generation);

/* Creates the fake socket inode at the guest endpoint for the registered
 * generation. Needs the task context of the guest. A socket left at the
 * endpoint by an earlier generation is replaced; anything else there fails.
 * Returns ISH_BRIDGE_SOCKET_OK when no generation is registered. */
int ish_bridge_socket_provision(void);

/* Removes the fake inode, when the endpoint is still the inode the hook
 * created, and drops the hold on it. The inode goes also when the provision
 * that made it failed afterwards. A lookup or unlink that fails keeps the
 * record and the hold and returns ISH_BRIDGE_SOCKET_FS_ERROR. Needs a task
 * context. */
int ish_bridge_socket_cleanup(void);

/* For the checks of the endpoint cleanup: injects the
 * ISH_BRIDGE_SOCKET_FAULT_* bits in `faults`, each for its next use. */
void ish_bridge_socket_inject_faults(int faults);

/* Copies what the hook holds into `state`. */
void ish_bridge_socket_get_state(struct ish_bridge_socket_state *state);

/* The connect() rewrite, for fs/sock.c: when `socket_id` is the socket of the
 * provisioned endpoint, writes the listener path into `sun_path` and returns
 * its length; returns 0 for any other socket. */
size_t ish_bridge_socket_connect_path(uint32_t socket_id, char *sun_path, size_t sun_path_size);

#endif
