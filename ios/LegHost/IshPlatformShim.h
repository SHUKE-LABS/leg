/* The narrow host-socket hook used by the embedded iSH runtime. */

#ifndef LEGHOST_ISH_PLATFORM_SHIM_H
#define LEGHOST_ISH_PLATFORM_SHIM_H

#include <stdbool.h>

/* The guest bridge uses one fixed name; the host endpoint is a private
 * app-temporary Unix-socket path. */
#define ISH_HOST_BRIDGE_SOCKET_MAX 104

int ish_set_host_bridge_socket(const char *name);
const char *ish_host_bridge_socket(void);

#endif /* LEGHOST_ISH_PLATFORM_SHIM_H */
