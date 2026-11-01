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
#include <sys/resource.h>
#include <sys/sysctl.h>
#include <sys/time.h>
#include <unistd.h>

#include "platform/platform.h"

/* The vendored platform layer counts in 10ms ticks. */
#define TICKS_PER_SECOND 100

static uint64_t timeval_ticks(struct timeval tv) {
    return (uint64_t) tv.tv_sec * TICKS_PER_SECOND + (uint64_t) tv.tv_usec / 10000;
}

struct cpu_usage get_cpu_usage(void) {
    struct cpu_usage usage = {0};
    struct rusage self;

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
