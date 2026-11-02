/*
 * SHA-256 for the LegHost guest provisioner.
 *
 * The provisioner verifies pinned downloads and staged files, so it needs a
 * digest that behaves identically on the device and on the macOS build host.
 * CommonCrypto would work on both, but it is deprecated on modern SDKs and
 * pulls a platform dependency into the provisioning path; this small
 * implementation keeps the check self-contained and testable.
 */

#ifndef LEGHOST_SHA256_H
#define LEGHOST_SHA256_H

#include <stddef.h>
#include <stdint.h>

#define SHA256_DIGEST_LEN 32
#define SHA256_HEX_LEN (SHA256_DIGEST_LEN * 2)

struct sha256_ctx {
    uint32_t state[8];
    uint64_t bitlen;
    uint8_t buffer[64];
    size_t buffered;
};

void sha256_init(struct sha256_ctx *ctx);
void sha256_update(struct sha256_ctx *ctx, const void *data, size_t len);
void sha256_final(struct sha256_ctx *ctx, uint8_t out[SHA256_DIGEST_LEN]);

/* Writes the lowercase hex digest of `data` into `out`, which must hold
 * SHA256_HEX_LEN + 1 bytes. */
void sha256_hex(const void *data, size_t len, char out[SHA256_HEX_LEN + 1]);

#endif
