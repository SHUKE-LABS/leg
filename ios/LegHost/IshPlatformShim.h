/* The narrow host-socket hook used by the embedded iSH runtime. */

#ifndef LEGHOST_ISH_PLATFORM_SHIM_H
#define LEGHOST_ISH_PLATFORM_SHIM_H

#include <stdbool.h>
#include <sys/socket.h>

/* The guest bridge uses one fixed name; the host endpoint is a private
 * app-temporary Unix-socket directory plus a short basename. The basename
 * stays within sockaddr_un.sun_path even when the Simulator's container
 * path is long. */
#define ISH_HOST_BRIDGE_SOCKET_MAX 104

int ish_set_host_bridge_socket(const char *directory, const char *name);
const char *ish_host_bridge_socket(void);
int ish_bind_host_bridge_socket(int socket_fd, const char *directory, const char *name);
int ish_connect_host_bridge_socket(int socket_fd, struct sockaddr *address,
                                   socklen_t address_length);

#endif /* LEGHOST_ISH_PLATFORM_SHIM_H */
