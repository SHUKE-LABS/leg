/*
 * iOS implementation of the platform hooks the vendored iSH runtime calls.
 *
 * Upstream ships platform/darwin.c, which uses Mach host accounting APIs that
 * are macOS only. The host provides its own implementation so the runtime can
 * be linked into an iOS app without the vendored macOS-only source.
 *
 * The vendored declarations live in vendor/ish/platform/platform.h.
 */

#include <os/proc.h>
#include <fcntl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/resource.h>
#include <sys/sysctl.h>
#include <sys/time.h>
#include <stddef.h>
#include <unistd.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>

#include "platform/platform.h"
#include "IshPlatformShim.h"
#include "util/fchdir.h"

/* The vendored platform layer counts in 10ms ticks. */
#define TICKS_PER_SECOND 100

static char host_bridge_socket[ISH_HOST_BRIDGE_SOCKET_MAX + 1];
static int host_bridge_directory = -1;

int ish_set_host_bridge_socket(const char *directory, const char *name) {
    if (directory == NULL || directory[0] == '\0'
        || name == NULL || name[0] == '\0') {
        if (host_bridge_directory >= 0) {
            close(host_bridge_directory);
            host_bridge_directory = -1;
        }
        host_bridge_socket[0] = '\0';
        return 0;
    }
    size_t name_length = strlen(name);
    if (name[0] == '/' || name_length >= ISH_HOST_BRIDGE_SOCKET_MAX)
        return -EINVAL;

    int directory_fd = open(directory, O_RDONLY);
    if (directory_fd < 0)
        return -errno;
    if (host_bridge_directory >= 0)
        close(host_bridge_directory);
    host_bridge_directory = directory_fd;
    snprintf(host_bridge_socket, sizeof(host_bridge_socket), "%s", name);
    return 0;
}

const char *ish_host_bridge_socket(void) {
    return host_bridge_socket[0] == '\0' ? NULL : host_bridge_socket;
}

/* Binds in the app's temporary directory while presenting only the short
 * basename to sockaddr_un. Changing cwd is process-wide, so the vendored
 * fchdir lock excludes the other fakefs operations that use the same trick. */
int ish_bind_host_bridge_socket(int socket_fd, const char *directory, const char *name) {
    if (directory == NULL || name == NULL || name[0] == '\0' || name[0] == '/')
        return -EINVAL;
    size_t name_length = strlen(name);
    if (name_length >= sizeof(((struct sockaddr_un *) 0)->sun_path))
        return -ENAMETOOLONG;

    int directory_fd = open(directory, O_RDONLY);
    if (directory_fd < 0)
        return -errno;
    int previous_fd = open(".", O_RDONLY);
    if (previous_fd < 0) {
        int error = errno;
        close(directory_fd);
        return -error;
    }

    int result = 0;
    int error = 0;
    lock_fchdir(directory_fd);
    if (fchdir(directory_fd) < 0) {
        result = -1;
        error = errno;
    } else {
        struct sockaddr_un address = {0};
        address.sun_family = (sa_family_t) AF_UNIX;
        memcpy(address.sun_path, name, name_length + 1);
        socklen_t address_length = (socklen_t) (offsetof(struct sockaddr_un, sun_path)
                                                + name_length + 1);
#if defined(__APPLE__)
        address.sun_len = (uint8_t) address_length;
#endif
        result = bind(socket_fd, (struct sockaddr *) &address, address_length);
        error = errno;
    }
    if (fchdir(previous_fd) < 0 && result == 0) {
        result = -1;
        error = errno;
    }
    unlock_fchdir();
    close(previous_fd);
    close(directory_fd);

    if (result < 0)
        return -error;
    return 0;
}

/* Connects using the directory fd held by ish_set_host_bridge_socket. The
 * guest address contains only the short basename; the long app-container
 * path never enters sockaddr_un. */
int ish_connect_host_bridge_socket(int socket_fd, struct sockaddr *address,
                                   socklen_t address_length) {
    if (host_bridge_directory < 0)
        return connect(socket_fd, address, address_length);

    int previous_fd = open(".", O_RDONLY);
    if (previous_fd < 0)
        return -1;

    int result = 0;
    int error = 0;
    lock_fchdir(host_bridge_directory);
    if (fchdir(host_bridge_directory) < 0) {
        result = -1;
        error = errno;
    } else {
        result = connect(socket_fd, address, address_length);
        error = errno;
    }
    if (fchdir(previous_fd) < 0 && result == 0) {
        result = -1;
        error = errno;
    }
    unlock_fchdir();
    close(previous_fd);

    if (result < 0)
        errno = error;
    return result;
}

static uint64_t timeval_ticks(struct timeval tv) {
    return (uint64_t) tv.tv_sec * TICKS_PER_SECOND + (uint64_t) tv.tv_usec / 10000;
}

struct cpu_usage get_cpu_usage(void) {
    struct cpu_usage usage = {0};
    struct rusage self;

    /* iOS exposes no system-wide CPU accounting, so the host reports its own
     * usage; the idle and nice counters stay at zero. */
    if (getrusage(RUSAGE_SELF, &self) != 0) {
        return usage;
    }
    usage.user_ticks = timeval_ticks(self.ru_utime);
    usage.system_ticks = timeval_ticks(self.ru_stime);
    return usage;
}

struct mem_usage get_mem_usage(void) {
    struct mem_usage usage = {0};
    uint64_t total = 0;
    size_t size = sizeof(total);

    if (sysctlbyname("hw.memsize", &total, &size, NULL, 0) != 0) {
        return usage;
    }
    usage.total = total;
    usage.free = (uint64_t) os_proc_available_memory();
    return usage;
}

struct uptime_info get_uptime(void) {
    struct uptime_info uptime = {0};
    struct timeval boot = {0};
    struct timeval now;
    struct {
        uint32_t ldavg[3];
        long scale;
    } loadavg;
    size_t size = sizeof(boot);

    if (sysctlbyname("kern.boottime", &boot, &size, NULL, 0) == 0) {
        gettimeofday(&now, NULL);
        uptime.uptime_ticks = (uint64_t)(now.tv_sec - boot.tv_sec) * TICKS_PER_SECOND;
    }

    size = sizeof(loadavg);
    if (sysctlbyname("vm.loadavg", &loadavg, &size, NULL, 0) == 0) {
        uptime.load_1m = loadavg.ldavg[0];
        uptime.load_5m = loadavg.ldavg[1];
        uptime.load_15m = loadavg.ldavg[2];
    }
    return uptime;
}
