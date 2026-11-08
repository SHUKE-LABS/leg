/*
 * Host side fixtures for the LegHost guest provisioner.
 *
 * The fixtures build their own pinned inputs, so they are hermetic and do not
 * need the network. They cover the paths the plan calls out: valid inputs,
 * one byte corruption of each input, a bundle for the wrong target, a
 * checksum failure, a failed import, stale staging cleanup, a reload with no
 * partial state, and a verified reuse of an existing root. The ios-mobile
 * contract cases reject every bundle that differs from the six-file mobile
 * bundle the pins describe, including the generic iSH bundle.
 */

#include <dirent.h>
#include <errno.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include <archive.h>
#include <archive_entry.h>

#include "GuestProvision.h"
#include "Sha256.h"

static int failures = 0;
static int checks = 0;

#define CHECK(condition, ...)                                                                      \
    do {                                                                                           \
        checks++;                                                                                  \
        if (!(condition)) {                                                                        \
            failures++;                                                                            \
            fprintf(stderr, "FAIL %s:%d: ", __FILE__, __LINE__);                                   \
            fprintf(stderr, __VA_ARGS__);                                                          \
            fprintf(stderr, "\n");                                                                 \
        }                                                                                          \
    } while (0)

/* The iSH runtime prints and aborts through these; the fixtures only need
 * them to link. */
void ish_printk(const char *msg, ...) {
    (void) msg;
}
void ish_vprintk(const char *msg, va_list args) {
    (void) msg;
    (void) args;
}
_Noreturn void die(const char *msg, ...) {
    (void) msg;
    abort();
}

/* ------------------------------------------------------------- fixtures */

#define FIXTURE_PROGRAM_SIZE 512

struct fixture_input {
    char path[1024];
    char sha256[SHA256_HEX_LEN + 1];
};

static void write_elf_header(uint8_t *buffer, bool elf64) {
    memset(buffer, 0, FIXTURE_PROGRAM_SIZE);
    buffer[0] = 0x7f;
    buffer[1] = 'E';
    buffer[2] = 'L';
    buffer[3] = 'F';
    buffer[4] = elf64 ? 2 : 1; /* ELFCLASS64 or ELFCLASS32 */
    buffer[5] = 1;            /* ELFDATA2LSB */
    buffer[6] = 1;            /* EV_CURRENT */
    buffer[16] = elf64 ? 2 : 2;
    buffer[17] = 0;
    buffer[18] = elf64 ? 0x3e : 3; /* EM_X86_64 or EM_386 */
    buffer[19] = 0;
    for (int i = 20; i < FIXTURE_PROGRAM_SIZE; i++)
        buffer[i] = (uint8_t) (i * 7);
}

static bool write_archive_entry(struct archive *archive, const char *path, const void *data,
                                size_t length, unsigned int filetype, unsigned int permissions,
                                const char *symlink_target) {
    struct archive_entry *entry = archive_entry_new();
    archive_entry_set_pathname(entry, path);
    archive_entry_set_filetype(entry, filetype);
    archive_entry_set_mode(entry, (mode_t) (filetype | (permissions & 07777)));
    archive_entry_set_size(entry, (la_int64_t) (filetype == AE_IFREG ? length : 0));
    archive_entry_set_mtime(entry, 1700000000, 0);
    if (symlink_target != NULL)
        archive_entry_set_symlink(entry, symlink_target);

    bool ok = archive_write_header(archive, entry) == ARCHIVE_OK;
    archive_entry_free(entry);
    if (!ok)
        return false;
    if (filetype == AE_IFREG && length > 0)
        return archive_write_data(archive, data, length) >= 0;
    return true;
}

/* A small Alpine-like root: a directory, a file, a symlink and a release file. */
static bool write_fixture_alpine(const char *path) {
    struct archive *archive = archive_write_new();
    archive_write_set_format_pax(archive);
    archive_write_add_filter_gzip(archive);
    if (archive_write_open_filename(archive, path) != ARCHIVE_OK) {
        archive_write_free(archive);
        return false;
    }

    static const char shell[] = "#!/bin/sh\necho fixture\n";
    static const char release[] = "3.20.3\n";

    bool ok = write_archive_entry(archive, "./", NULL, 0, AE_IFDIR, 0755, NULL) &&
              write_archive_entry(archive, "./bin/", NULL, 0, AE_IFDIR, 0755, NULL) &&
              write_archive_entry(archive, "./bin/sh", shell, sizeof(shell) - 1, AE_IFREG, 0755,
                                  NULL) &&
              write_archive_entry(archive, "./bin/ash", NULL, 0, AE_IFLNK, 0777, "sh") &&
              write_archive_entry(archive, "./etc/", NULL, 0, AE_IFDIR, 0755, NULL) &&
              write_archive_entry(archive, "./etc/alpine-release", release, sizeof(release) - 1,
                                  AE_IFREG, 0644, NULL);

    if (archive_write_close(archive) != ARCHIVE_OK)
        ok = false;
    archive_write_free(archive);
    return ok;
}

#define FIXTURE_BUNDLE_KIND "ios-mobile"
#define FIXTURE_LEG_VERSION "0.44.0"
#define FIXTURE_MANIFEST "ish-compatibility-v1.json"

/* The programs of the generic iSH bundle, which LegHost must not import. */
static const char *const generic_program_names[GUEST_PROGRAM_COUNT] = {
    "leg", "leg-tui", "leg-web", "leg-ui-supervisor",
};

/*
 * What a fixture bundle looks like. The zero value plus a target is the valid
 * ios-mobile bundle; every other field builds one rejected variant.
 */
struct fixture_bundle {
    const char *target;
    bool elf64;
    bool generic;                   /* the generic program set, with no bundle kind */
    bool omit_bundle_kind;
    const char *bundle_kind;        /* NULL means FIXTURE_BUNDLE_KIND */
    const char *leg_version;        /* NULL means FIXTURE_LEG_VERSION */
    const char *wrong_version_command; /* the program whose version command is wrong */
    const char *omit_file;          /* the archive entry that is left out */
    const char *extra_file;         /* an archive entry that is not in the contract */
    const char *omit_sum;           /* the SHA256SUMS line that is left out */
    const char *duplicate_sum;      /* the SHA256SUMS line that is listed twice */
    const char *unknown_sum;        /* a SHA256SUMS line for a file the bundle lacks */
    const char *sums_suffix;        /* raw text appended to SHA256SUMS */
};

/*
 * A guest bundle with the four programs, the compatibility manifest and the
 * inner SHA256SUMS, shaped by `fixture`.
 */
static bool write_fixture_bundle(const char *path, const struct fixture_bundle *fixture,
                                 const char program_sha256[GUEST_PROGRAM_COUNT][SHA256_HEX_LEN + 1],
                                 const uint64_t program_size[GUEST_PROGRAM_COUNT]) {
    const char *const *names = fixture->generic ? generic_program_names : guest_program_names;
    uint8_t program[FIXTURE_PROGRAM_SIZE];
    char manifest[4096];
    size_t manifest_length = 0;

    manifest_length += (size_t) snprintf(manifest + manifest_length, sizeof(manifest) - manifest_length,
                                         "{\n  \"schema_version\": 1,\n");
    if (!fixture->generic && !fixture->omit_bundle_kind)
        manifest_length += (size_t) snprintf(
            manifest + manifest_length, sizeof(manifest) - manifest_length, "  \"bundle_kind\": \"%s\",\n",
            fixture->bundle_kind != NULL ? fixture->bundle_kind : FIXTURE_BUNDLE_KIND);
    manifest_length += (size_t) snprintf(manifest + manifest_length, sizeof(manifest) - manifest_length,
                                         "  \"leg_version\": \"%s\",\n"
                                         "  \"target\": \"%s\",\n  \"architecture\": \"%s\",\n"
                                         "  \"os\": \"linux\",\n  \"libc\": \"musl\",\n  \"host\": \"ish\",\n"
                                         "  \"alpine_release\": \"3.20.3\",\n"
                                         "  \"docker_platform\": \"linux/386\",\n  \"binaries\": [\n",
                                         fixture->leg_version != NULL ? fixture->leg_version
                                                                      : FIXTURE_LEG_VERSION,
                                         fixture->target, fixture->elf64 ? "x86_64" : "i386");
    for (int i = 0; i < GUEST_PROGRAM_COUNT; i++) {
        bool wrong_command = fixture->wrong_version_command != NULL &&
                             strcmp(fixture->wrong_version_command, names[i]) == 0;
        manifest_length += (size_t) snprintf(
            manifest + manifest_length, sizeof(manifest) - manifest_length,
            "    {\"name\": \"%s\", \"version_command\": \"%s %s\", \"size\": %llu, "
            "\"sha256\": \"%s\"}%s\n",
            names[i], names[i], wrong_command ? "--help" : "--version",
            (unsigned long long) program_size[i], program_sha256[i],
            i + 1 < GUEST_PROGRAM_COUNT ? "," : "");
    }
    manifest_length += (size_t) snprintf(manifest + manifest_length,
                                         sizeof(manifest) - manifest_length, "  ]\n}\n");

    char manifest_sha[SHA256_HEX_LEN + 1];
    sha256_hex(manifest, manifest_length, manifest_sha);

    char sums[2048];
    size_t sums_length = 0;
    for (int i = 0; i <= GUEST_PROGRAM_COUNT; i++) {
        const char *name = i < GUEST_PROGRAM_COUNT ? names[i] : FIXTURE_MANIFEST;
        const char *sha = i < GUEST_PROGRAM_COUNT ? program_sha256[i] : manifest_sha;
        if (fixture->omit_sum != NULL && strcmp(fixture->omit_sum, name) == 0)
            continue;
        int copies = fixture->duplicate_sum != NULL && strcmp(fixture->duplicate_sum, name) == 0 ? 2 : 1;
        for (int copy = 0; copy < copies; copy++)
            sums_length += (size_t) snprintf(sums + sums_length, sizeof(sums) - sums_length, "%s  %s\n",
                                             sha, name);
    }
    if (fixture->unknown_sum != NULL)
        sums_length += (size_t) snprintf(sums + sums_length, sizeof(sums) - sums_length, "%s  %s\n",
                                         program_sha256[0], fixture->unknown_sum);
    if (fixture->sums_suffix != NULL)
        sums_length += (size_t) snprintf(sums + sums_length, sizeof(sums) - sums_length, "%s",
                                         fixture->sums_suffix);

    struct archive *archive = archive_write_new();
    archive_write_set_format_pax(archive);
    archive_write_add_filter_gzip(archive);
    if (archive_write_open_filename(archive, path) != ARCHIVE_OK) {
        archive_write_free(archive);
        return false;
    }

    bool ok = write_archive_entry(archive, "SHA256SUMS", sums, sums_length, AE_IFREG, 0644, NULL);
    if (ok && (fixture->omit_file == NULL || strcmp(fixture->omit_file, FIXTURE_MANIFEST) != 0))
        ok = write_archive_entry(archive, FIXTURE_MANIFEST, manifest, manifest_length, AE_IFREG, 0644,
                                 NULL);
    for (int i = 0; ok && i < GUEST_PROGRAM_COUNT; i++) {
        if (fixture->omit_file != NULL && strcmp(fixture->omit_file, names[i]) == 0)
            continue;
        write_elf_header(program, fixture->elf64);
        ok = write_archive_entry(archive, names[i], program, sizeof(program), AE_IFREG, 0755, NULL);
    }
    if (ok && fixture->extra_file != NULL) {
        write_elf_header(program, fixture->elf64);
        ok = write_archive_entry(archive, fixture->extra_file, program, sizeof(program), AE_IFREG, 0755,
                                 NULL);
    }

    if (archive_write_close(archive) != ARCHIVE_OK)
        ok = false;
    archive_write_free(archive);
    return ok;
}

static void digest_of(const char *path, char out[SHA256_HEX_LEN + 1]) {
    FILE *file = fopen(path, "rb");
    if (file == NULL) {
        fprintf(stderr, "cannot open %s: %s\n", path, strerror(errno));
        exit(1);
    }
    struct sha256_ctx ctx;
    sha256_init(&ctx);
    uint8_t buffer[8192];
    size_t got;
    while ((got = fread(buffer, 1, sizeof(buffer), file)) > 0)
        sha256_update(&ctx, buffer, got);
    fclose(file);
    uint8_t digest[SHA256_DIGEST_LEN];
    sha256_final(&ctx, digest);
    static const char hex[] = "0123456789abcdef";
    for (int i = 0; i < SHA256_DIGEST_LEN; i++) {
        out[i * 2] = hex[digest[i] >> 4];
        out[i * 2 + 1] = hex[digest[i] & 0xf];
    }
    out[SHA256_HEX_LEN] = '\0';
}

static void flip_one_byte(const char *path) {
    FILE *file = fopen(path, "r+b");
    if (file == NULL) {
        fprintf(stderr, "cannot open %s: %s\n", path, strerror(errno));
        exit(1);
    }
    fseek(file, 0, SEEK_END);
    long size = ftell(file);
    fseek(file, size / 2, SEEK_SET);
    int byte = fgetc(file);
    fseek(file, size / 2, SEEK_SET);
    fputc(byte ^ 0x01, file);
    fclose(file);
}

static bool path_exists(const char *path) {
    struct stat info;
    return stat(path, &info) == 0;
}

static bool path_under(const char *base, const char *suffix) {
    char path[2048];
    snprintf(path, sizeof(path), "%s/%s", base, suffix);
    return path_exists(path);
}

static bool copy_file(const char *source, const char *destination) {
    FILE *in = fopen(source, "rb");
    if (in == NULL)
        return false;
    FILE *out = fopen(destination, "wb");
    if (out == NULL) {
        fclose(in);
        return false;
    }
    char buffer[8192];
    size_t got;
    while ((got = fread(buffer, 1, sizeof(buffer), in)) > 0)
        fwrite(buffer, 1, got, out);
    fclose(in);
    fclose(out);
    return true;
}

static bool directory_is_empty(const char *path) {
    /* The roots directory must not keep any staging state behind. */
    DIR *dir = opendir(path);
    if (dir == NULL)
        return true;
    bool empty = true;
    struct dirent *entry;
    while ((entry = readdir(dir)) != NULL) {
        if (strcmp(entry->d_name, ".") == 0 || strcmp(entry->d_name, "..") == 0)
            continue;
        empty = false;
        break;
    }
    closedir(dir);
    return empty;
}

static void remove_directory(const char *path) {
    char command[2048];
    snprintf(command, sizeof(command), "rm -rf '%s'", path);
    if (system(command) != 0)
        fprintf(stderr, "warning: cannot remove %s\n", path);
}

/* --------------------------------------------------------------- helpers */

static void fill_pins(struct guest_pins *pins, const struct guest_pinned_input *inputs,
                      const struct guest_compatibility *compatibility) {
    pins->inputs = inputs;
    pins->compatibility = compatibility;
}

static const struct guest_compatibility fixture_compatibility = {
    .bundle_kind = FIXTURE_BUNDLE_KIND,
    .leg_version = FIXTURE_LEG_VERSION,
    .target = "i686-unknown-linux-musl",
    .architecture = "i386",
    .os = "linux",
    .libc = "musl",
    .alpine_release = "3.20.3",
    .docker_platform = "linux/386",
};

static void write_file(const char *path, const char *contents) {
    FILE *file = fopen(path, "wb");
    if (file == NULL) {
        fprintf(stderr, "cannot write %s: %s\n", path, strerror(errno));
        exit(1);
    }
    fputs(contents, file);
    fclose(file);
}

static char *read_file(const char *path) {
    FILE *file = fopen(path, "rb");
    if (file == NULL)
        return NULL;
    fseek(file, 0, SEEK_END);
    long size = ftell(file);
    fseek(file, 0, SEEK_SET);
    char *contents = malloc((size_t) size + 1);
    size_t got = fread(contents, 1, (size_t) size, file);
    contents[got] = '\0';
    fclose(file);
    return contents;
}

/* Replaces the first `from` in the file with `to`, which has the same length. */
static bool rewrite_in_file(const char *path, const char *from, const char *to) {
    char *contents = read_file(path);
    if (contents == NULL)
        return false;
    char *found = strstr(contents, from);
    bool ok = found != NULL && strlen(from) == strlen(to);
    if (ok) {
        memcpy(found, to, strlen(to));
        write_file(path, contents);
    }
    free(contents);
    return ok;
}

/*
 * Builds the bundle `fixture` describes, pins it as it is, and expects the
 * provisioner to reject it with `expected` before it publishes anything.
 */
static void expect_bundle_rejected(const char *work, const char *label, const struct fixture_bundle *fixture,
                                   const struct guest_compatibility *compatibility,
                                   const struct guest_pinned_input *inputs, const char *alpine_path,
                                   const char program_sha256[GUEST_PROGRAM_COUNT][SHA256_HEX_LEN + 1],
                                   const uint64_t program_size[GUEST_PROGRAM_COUNT], int expected,
                                   const char *message) {
    char bundle[1024];
    snprintf(bundle, sizeof(bundle), "%s/%s.tar.gz", work, label);
    if (!write_fixture_bundle(bundle, fixture, program_sha256, program_size)) {
        CHECK(false, "%s: cannot build the fixture bundle", label);
        return;
    }

    struct guest_pinned_input pinned[GUEST_INPUT_COUNT];
    memcpy(pinned, inputs, sizeof(pinned));
    char bundle_sha[SHA256_HEX_LEN + 1];
    digest_of(bundle, bundle_sha);
    pinned[GUEST_INPUT_LEG_BUNDLE].sha256 = bundle_sha;
    struct guest_pins pins;
    fill_pins(&pins, pinned, compatibility);

    char roots[1024];
    snprintf(roots, sizeof(roots), "%s/roots-%s", work, label);
    char error[1024] = {0};
    struct guest_provision_result result;
    int status = guest_provision(&pins, bundle, alpine_path, roots, NULL, &result, error, sizeof(error));
    CHECK(status == expected, "%s: expected status %d, got %d (%s)", label, expected, status, error);
    CHECK(strstr(error, message) != NULL, "%s: expected '%s' in '%s'", label, message, error);
    CHECK(!path_exists(roots) || directory_is_empty(roots), "%s: a rejected bundle must leave no state",
          label);
}

/* ------------------------------------------------------------------ main */

/*
 * The pinned input mode: run the provisioner against the real pinned
 * archives. The pins come from a small "key=value" file that the shell
 * script writes from the pin manifest, so there is one source of truth.
 */
struct pinned_values {
    char bundle_kind[128];
    char leg_version[128];
    char target[128];
    char architecture[128];
    char os[128];
    char libc[128];
    char alpine_release[128];
    char docker_platform[128];
    char bundle_sha256[SHA256_HEX_LEN + 1];
    char alpine_sha256[SHA256_HEX_LEN + 1];
};

static bool read_pinned_values(const char *path, struct pinned_values *values) {
    FILE *file = fopen(path, "rb");
    if (file == NULL) {
        fprintf(stderr, "cannot open %s: %s\n", path, strerror(errno));
        return false;
    }

    char line[1024];
    bool complete = true;
    while (fgets(line, sizeof(line), file) != NULL) {
        char *separator = strchr(line, '=');
        if (separator == NULL)
            continue;
        *separator = '\0';
        char *value = separator + 1;
        size_t length = strlen(value);
        while (length > 0 && (value[length - 1] == '\n' || value[length - 1] == '\r'))
            value[--length] = '\0';

        char *target = NULL;
        size_t size = 0;
        if (strcmp(line, "bundle_kind") == 0)
            target = values->bundle_kind, size = sizeof(values->bundle_kind);
        else if (strcmp(line, "leg_version") == 0)
            target = values->leg_version, size = sizeof(values->leg_version);
        else if (strcmp(line, "target") == 0)
            target = values->target, size = sizeof(values->target);
        else if (strcmp(line, "architecture") == 0)
            target = values->architecture, size = sizeof(values->architecture);
        else if (strcmp(line, "os") == 0)
            target = values->os, size = sizeof(values->os);
        else if (strcmp(line, "libc") == 0)
            target = values->libc, size = sizeof(values->libc);
        else if (strcmp(line, "alpine_release") == 0)
            target = values->alpine_release, size = sizeof(values->alpine_release);
        else if (strcmp(line, "docker_platform") == 0)
            target = values->docker_platform, size = sizeof(values->docker_platform);
        else if (strcmp(line, "bundle_sha256") == 0)
            target = values->bundle_sha256, size = sizeof(values->bundle_sha256);
        else if (strcmp(line, "alpine_sha256") == 0)
            target = values->alpine_sha256, size = sizeof(values->alpine_sha256);
        if (target == NULL)
            continue;
        if (length + 1 > size) {
            complete = false;
            continue;
        }
        memcpy(target, value, length + 1);
    }
    fclose(file);

    return complete && values->bundle_kind[0] != '\0' && values->leg_version[0] != '\0' &&
           values->target[0] != '\0' && values->bundle_sha256[0] != '\0' &&
           values->alpine_sha256[0] != '\0';
}

static int run_pinned_mode(int argc, char **argv) {
    if (argc < 6) {
        fprintf(stderr, "usage: %s --pinned <pins file> <bundle> <alpine root> <roots directory>\n",
                argv[0]);
        return 2;
    }

    struct pinned_values values;
    memset(&values, 0, sizeof(values));
    if (!read_pinned_values(argv[2], &values)) {
        fprintf(stderr, "the pins file is incomplete\n");
        return 2;
    }

    struct guest_pinned_input inputs[GUEST_INPUT_COUNT];
    inputs[GUEST_INPUT_LEG_BUNDLE].name = "leg_guest_bundle";
    inputs[GUEST_INPUT_LEG_BUNDLE].url = "https://example.invalid/leg-guest-bundle.tar.gz";
    inputs[GUEST_INPUT_LEG_BUNDLE].filename = "leg-i686-unknown-linux-musl-mobile.tar.gz";
    inputs[GUEST_INPUT_LEG_BUNDLE].sha256 = values.bundle_sha256;
    inputs[GUEST_INPUT_ALPINE_ROOT].name = "alpine_root";
    inputs[GUEST_INPUT_ALPINE_ROOT].url = "https://example.invalid/alpine.tar.gz";
    inputs[GUEST_INPUT_ALPINE_ROOT].filename = "alpine-minirootfs-3.20.3-x86.tar.gz";
    inputs[GUEST_INPUT_ALPINE_ROOT].sha256 = values.alpine_sha256;

    struct guest_compatibility compatibility = {
        .bundle_kind = values.bundle_kind,
        .leg_version = values.leg_version,
        .target = values.target,
        .architecture = values.architecture,
        .os = values.os,
        .libc = values.libc,
        .alpine_release = values.alpine_release,
        .docker_platform = values.docker_platform,
    };

    struct guest_pins pins;
    fill_pins(&pins, inputs, &compatibility);

    char error[1024];
    struct guest_provision_result result;
    int status = guest_provision(&pins, argv[3], argv[4], argv[5], NULL, &result, error,
                                 sizeof(error));
    CHECK(status == GUEST_PROVISION_OK, "the pinned inputs failed: %s", error);
    if (status != GUEST_PROVISION_OK) {
        printf("FAILED: %d of %d checks failed\n", failures, checks);
        return 1;
    }

    CHECK(path_under(argv[5], "root.active/meta.db"), "the fakefs database is missing");
    for (int i = 0; i < GUEST_PROGRAM_COUNT; i++) {
        char path[1024];
        snprintf(path, sizeof(path), "%s/root.active/data%s/%s", argv[5], GUEST_PROGRAMS_DIR,
                 guest_program_names[i]);
        CHECK(path_exists(path), "%s is missing from the pinned root", guest_program_names[i]);
    }
    {
        char path[1024];
        snprintf(path, sizeof(path), "%s/root.active/data%s/leg-web", argv[5], GUEST_PROGRAMS_DIR);
        CHECK(!path_exists(path), "the pinned root must not stage leg-web");
    }
    CHECK(path_under(argv[5], "root.active/data/etc/leg/guest-bundle.json"),
          "the guest bundle marker is missing");

    /* The second run has to reuse the verified root instead of importing it
     * again. */
    struct guest_provision_result second;
    status = guest_provision(&pins, argv[3], argv[4], argv[5], NULL, &second, error, sizeof(error));
    CHECK(status == GUEST_PROVISION_OK, "the second run with the pinned inputs failed: %s", error);
    CHECK(second.reused, "the second run must reuse the verified pinned root");
    CHECK(strcmp(second.marker_sha256, result.marker_sha256) == 0,
          "the reused root has a different guest bundle marker");

    if (failures == 0) {
        printf("ok: %d checks passed\n", checks);
        return 0;
    }
    printf("FAILED: %d of %d checks failed\n", failures, checks);
    return 1;
}

int main(int argc, char **argv) {
    if (argc >= 2 && strcmp(argv[1], "--pinned") == 0)
        return run_pinned_mode(argc, argv);

    if (argc < 2) {
        fprintf(stderr, "usage: %s <work directory>\n", argv[0]);
        return 2;
    }
    const char *work = argv[1];
    remove_directory(work);
    if (mkdir(work, 0755) != 0) {
        fprintf(stderr, "cannot create %s: %s\n", work, strerror(errno));
        return 2;
    }
    /* The checks below use paths relative to the work directory. */
    if (chdir(work) != 0) {
        fprintf(stderr, "cannot enter %s: %s\n", work, strerror(errno));
        return 2;
    }

    char alpine_path[1024];
    char bundle_path[1024];
    snprintf(alpine_path, sizeof(alpine_path), "%s/alpine.tar.gz", work);
    snprintf(bundle_path, sizeof(bundle_path), "%s/bundle.tar.gz", work);

    if (!write_fixture_alpine(alpine_path)) {
        fprintf(stderr, "cannot build the Alpine fixture\n");
        return 2;
    }

    /* The program digests have to be known before the manifest is written. */
    uint8_t program[GUEST_PROGRAM_COUNT][FIXTURE_PROGRAM_SIZE];
    char program_sha256[GUEST_PROGRAM_COUNT][SHA256_HEX_LEN + 1];
    uint64_t program_size[GUEST_PROGRAM_COUNT];
    for (int i = 0; i < GUEST_PROGRAM_COUNT; i++) {
        write_elf_header(program[i], false);
        sha256_hex(program[i], sizeof(program[i]), program_sha256[i]);
        program_size[i] = sizeof(program[i]);
    }
    const struct fixture_bundle valid = {.target = "i686-unknown-linux-musl"};
    if (!write_fixture_bundle(bundle_path, &valid, program_sha256, program_size)) {
        fprintf(stderr, "cannot build the guest bundle fixture\n");
        return 2;
    }

    struct guest_pinned_input inputs[GUEST_INPUT_COUNT];
    inputs[GUEST_INPUT_LEG_BUNDLE].name = "leg_guest_bundle";
    inputs[GUEST_INPUT_LEG_BUNDLE].url = "https://example.invalid/leg-guest-bundle.tar.gz";
    inputs[GUEST_INPUT_LEG_BUNDLE].filename = "bundle.tar.gz";
    inputs[GUEST_INPUT_ALPINE_ROOT].name = "alpine_root";
    inputs[GUEST_INPUT_ALPINE_ROOT].url = "https://example.invalid/alpine.tar.gz";
    inputs[GUEST_INPUT_ALPINE_ROOT].filename = "alpine.tar.gz";
    char bundle_sha[SHA256_HEX_LEN + 1];
    char alpine_sha[SHA256_HEX_LEN + 1];
    digest_of(bundle_path, bundle_sha);
    digest_of(alpine_path, alpine_sha);
    inputs[GUEST_INPUT_LEG_BUNDLE].sha256 = bundle_sha;
    inputs[GUEST_INPUT_ALPINE_ROOT].sha256 = alpine_sha;

    struct guest_pins pins;
    fill_pins(&pins, inputs, &fixture_compatibility);

    char error[1024];
    struct guest_provision_result result;

    /* --- a valid first run publishes an active root ------------------- */
    char roots[1024];
    snprintf(roots, sizeof(roots), "%s/roots", work);
    int status = guest_provision(&pins, bundle_path, alpine_path, roots, NULL, &result, error,
                                 sizeof(error));
    CHECK(status == GUEST_PROVISION_OK, "the first run failed: %s", error);
    CHECK(!result.reused, "the first run must not report a reuse");
    CHECK(path_exists(result.active_root), "the active root is missing");
    CHECK(path_exists("roots/root.active/meta.db"), "the fakefs database is missing");
    CHECK(path_exists("roots/root.active/data/usr/local/bin/leg"), "leg is missing from the guest");
    CHECK(path_exists("roots/root.active/data/usr/local/bin/leg-tui"),
          "leg-tui is missing from the guest");
    CHECK(path_exists("roots/root.active/data/usr/local/bin/leg-ui-supervisor"),
          "leg-ui-supervisor is missing from the guest");
    CHECK(path_exists("roots/root.active/data/usr/local/bin/leg-bridge"),
          "leg-bridge is missing from the guest");
    CHECK(!path_exists("roots/root.active/data/usr/local/bin/leg-web"),
          "the mobile guest must not stage leg-web");
    CHECK(path_exists("roots/root.active/data/etc/leg/guest-bundle.json"),
          "the guest bundle marker is missing");

    /* The marker has to record both input digests and every staged file. */
    {
        char *marker = read_file("roots/root.active/data/etc/leg/guest-bundle.json");
        CHECK(marker != NULL, "the guest bundle marker cannot be opened");
        if (marker != NULL) {
            CHECK(strstr(marker, "\"bundle_kind\": \"" FIXTURE_BUNDLE_KIND "\"") != NULL,
                  "the marker does not record the bundle kind");
            CHECK(strstr(marker, "\"leg_version\": \"" FIXTURE_LEG_VERSION "\"") != NULL,
                  "the marker does not record the leg version");
            CHECK(strstr(marker, "/usr/local/bin/leg-bridge") != NULL,
                  "the marker does not record leg-bridge");
            CHECK(strstr(marker, inputs[GUEST_INPUT_LEG_BUNDLE].sha256) != NULL,
                  "the marker does not record the guest bundle digest");
            CHECK(strstr(marker, inputs[GUEST_INPUT_ALPINE_ROOT].sha256) != NULL,
                  "the marker does not record the Alpine root digest");
            CHECK(strstr(marker, "/usr/local/bin/leg-tui") != NULL,
                  "the marker does not record the staged programs");
            CHECK(strstr(marker, "/bin/sh") != NULL,
                  "the marker does not record the staged root files");
            CHECK(strstr(marker, "/bin/ash") != NULL,
                  "the marker does not record the staged symlinks");
            free(marker);
        }
    }

    /* The staged root has to be complete: no partial binaries or marker. */
    for (int i = 0; i < GUEST_PROGRAM_COUNT; i++) {
        char path[1024];
        snprintf(path, sizeof(path), "roots/root.active/data/usr/local/bin/%s", guest_program_names[i]);
        CHECK(path_exists(path), "%s is missing from the staged root", guest_program_names[i]);
    }

    /* --- a second run verifies the marker and reuses the root --------- */
    struct guest_provision_result second;
    status = guest_provision(&pins, bundle_path, alpine_path, roots, NULL, &second, error,
                              sizeof(error));
    CHECK(status == GUEST_PROVISION_OK, "the second run failed: %s", error);
    CHECK(second.reused, "the second run must reuse the verified root");
    CHECK(strcmp(second.marker_sha256, result.marker_sha256) == 0,
          "the reused root has a different guest bundle marker");
    CHECK(strcmp(second.active_root, result.active_root) == 0,
          "the reused root is at a different path");

    /* --- a corrupted guest bundle fails its own digest check ---------- */
    {
        char corrupted[1024];
        snprintf(corrupted, sizeof(corrupted), "%s/corrupted-bundle.tar.gz", work);
        if (copy_file(bundle_path, corrupted))
            flip_one_byte(corrupted);

        char corrupted_roots[1024];
        snprintf(corrupted_roots, sizeof(corrupted_roots), "%s/roots-corrupted-bundle", work);
        struct guest_provision_result corrupted_result;
        status = guest_provision(&pins, corrupted, alpine_path, corrupted_roots, NULL,
                                 &corrupted_result, error, sizeof(error));
        CHECK(status == GUEST_PROVISION_ERR_CHECKSUM,
              "a corrupted guest bundle must fail the digest check, got %d", status);
        CHECK(!path_exists(corrupted_roots) || directory_is_empty(corrupted_roots),
              "a corrupted guest bundle must leave no staging state");
    }

    /* --- a corrupted Alpine root fails its own digest check ----------- */
    {
        char corrupted[1024];
        snprintf(corrupted, sizeof(corrupted), "%s/corrupted-alpine.tar.gz", work);
        if (copy_file(alpine_path, corrupted))
            flip_one_byte(corrupted);

        char corrupted_roots[1024];
        snprintf(corrupted_roots, sizeof(corrupted_roots), "%s/roots-corrupted-alpine", work);
        struct guest_provision_result corrupted_result;
        status = guest_provision(&pins, bundle_path, corrupted, corrupted_roots, NULL,
                                 &corrupted_result, error, sizeof(error));
        CHECK(status == GUEST_PROVISION_ERR_CHECKSUM,
              "a corrupted Alpine root must fail the digest check, got %d", status);
        CHECK(!path_exists(corrupted_roots) || directory_is_empty(corrupted_roots),
              "a corrupted Alpine root must leave no staging state");
    }

    /* --- every bundle off the ios-mobile contract is rejected ---------- */
    {
        const char *target = "i686-unknown-linux-musl";
        char other_hex[SHA256_HEX_LEN + 1];
        memset(other_hex, 'a', SHA256_HEX_LEN);
        other_hex[SHA256_HEX_LEN] = '\0';
        char self_listed[SHA256_HEX_LEN + 32];
        snprintf(self_listed, sizeof(self_listed), "%s  SHA256SUMS\n", other_hex);
        char wrong_digest[GUEST_PROGRAM_COUNT][SHA256_HEX_LEN + 1];
        memcpy(wrong_digest, program_sha256, sizeof(wrong_digest));
        memcpy(wrong_digest[3], other_hex, sizeof(other_hex));

        struct {
            const char *label;
            struct fixture_bundle fixture;
            int status;
            const char *message;
        } cases[] = {
            {"missing-manifest", {.target = target, .omit_file = FIXTURE_MANIFEST},
             GUEST_PROVISION_ERR_ARCHIVE, "is missing ish-compatibility-v1.json"},
            {"missing-bridge", {.target = target, .omit_file = "leg-bridge"},
             GUEST_PROVISION_ERR_ARCHIVE, "is missing leg-bridge"},
            {"extra-file", {.target = target, .extra_file = "leg-web"},
             GUEST_PROVISION_ERR_ARCHIVE, "unexpected file: leg-web"},
            {"generic-bundle", {.target = target, .generic = true},
             GUEST_PROVISION_ERR_ARCHIVE, "unexpected file: leg-web"},
            {"wrong-target", {.target = "x86_64-unknown-linux-musl"},
             GUEST_PROVISION_ERR_MANIFEST, "declares target = x86_64-unknown-linux-musl"},
            {"stale-version", {.target = target, .leg_version = "0.43.6"},
             GUEST_PROVISION_ERR_MANIFEST, "declares leg_version = 0.43.6"},
            {"missing-kind", {.target = target, .omit_bundle_kind = true},
             GUEST_PROVISION_ERR_MANIFEST, "declares bundle_kind = (missing)"},
            {"wrong-kind", {.target = target, .bundle_kind = "generic"},
             GUEST_PROVISION_ERR_MANIFEST, "declares bundle_kind = generic"},
            {"wrong-version-command", {.target = target, .wrong_version_command = "leg-bridge"},
             GUEST_PROVISION_ERR_MANIFEST, "wrong version command for leg-bridge"},
            {"malformed-sums", {.target = target, .sums_suffix = "not a checksum line\n"},
             GUEST_PROVISION_ERR_MANIFEST, "has a malformed line"},
            {"incomplete-sums", {.target = target, .omit_sum = "leg-bridge"},
             GUEST_PROVISION_ERR_MANIFEST, "does not list leg-bridge"},
            {"manifest-not-summed", {.target = target, .omit_sum = FIXTURE_MANIFEST},
             GUEST_PROVISION_ERR_MANIFEST, "does not list ish-compatibility-v1.json"},
            {"duplicate-sums", {.target = target, .duplicate_sum = "leg"},
             GUEST_PROVISION_ERR_MANIFEST, "lists leg twice"},
            {"unknown-sums", {.target = target, .unknown_sum = "leg-web"},
             GUEST_PROVISION_ERR_MANIFEST, "lists the unknown file leg-web"},
            {"self-listed-sums", {.target = target, .sums_suffix = self_listed},
             GUEST_PROVISION_ERR_MANIFEST, "lists the unknown file SHA256SUMS"},
            {"elf64", {.target = target, .elf64 = true},
             GUEST_PROVISION_ERR_ARCHIVE, "is not an ELF32 i386 executable"},
        };
        for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++)
            expect_bundle_rejected(work, cases[i].label, &cases[i].fixture, &fixture_compatibility, inputs,
                                   alpine_path, program_sha256, program_size, cases[i].status,
                                   cases[i].message);

        /* The manifest digest of leg-bridge disagrees with the archive. */
        expect_bundle_rejected(work, "wrong-binary-digest", &valid, &fixture_compatibility, inputs,
                               alpine_path, wrong_digest, program_size, GUEST_PROVISION_ERR_MANIFEST,
                               "wrong digest for leg-bridge");

        /* Pins without a bundle kind are a caller error, not a bundle error. */
        struct guest_compatibility unpinned = fixture_compatibility;
        unpinned.bundle_kind = NULL;
        expect_bundle_rejected(work, "unpinned-kind", &valid, &unpinned, inputs, alpine_path,
                               program_sha256, program_size, GUEST_PROVISION_ERR_USAGE,
                               "the pins do not declare bundle_kind");
    }

    /* --- a failed import rolls back every piece of its state ---------- */
    {
        char failed_roots[1024];
        snprintf(failed_roots, sizeof(failed_roots), "%s/roots-failed-import", work);
        struct guest_provision_options options = {.simulate_import_failure = true};
        struct guest_provision_result failed_result;
        status = guest_provision(&pins, bundle_path, alpine_path, failed_roots, &options,
                                 &failed_result, error, sizeof(error));
        CHECK(status == GUEST_PROVISION_ERR_IMPORT, "a failed import must be reported, got %d", status);
        CHECK(!path_exists(failed_roots) || directory_is_empty(failed_roots),
              "a failed import must leave no staging state behind");
        CHECK(!path_under(failed_roots, "root.active"),
              "a failed import must not publish an active root");
    }

    /* --- stale staging state from an earlier run is removed ------------ */
    {
        char stale_roots[1024];
        snprintf(stale_roots, sizeof(stale_roots), "%s/roots-stale", work);
        if (mkdir(stale_roots, 0755) == 0) {
            char stale_dir[1024];
            snprintf(stale_dir, sizeof(stale_dir), "%s/root.staging.deadbeef", stale_roots);
            if (mkdir(stale_dir, 0755) == 0)
                write_file("roots-stale/root.staging.deadbeef/leftover", "partial");
            char stale_archive[1024];
            snprintf(stale_archive, sizeof(stale_archive), "%s/root-staging-deadbeef.tar.gz",
                     stale_roots);
            write_file(stale_archive, "partial");
        }

        struct guest_provision_result stale_result;
        status = guest_provision(&pins, bundle_path, alpine_path, stale_roots, NULL, &stale_result,
                                 error, sizeof(error));
        CHECK(status == GUEST_PROVISION_OK, "the run after stale state failed: %s", error);
        CHECK(!path_under(stale_roots, "root.staging.deadbeef"),
              "the stale staging directory was not removed");
        CHECK(!path_under(stale_roots, "root-staging-deadbeef.tar.gz"),
              "the stale staging archive was not removed");
        CHECK(path_under(stale_roots, "root.active"), "the active root was not published");
    }

    /* --- an active root that no longer matches the pins is replaced --- */
    {
        char replaced_roots[1024];
        snprintf(replaced_roots, sizeof(replaced_roots), "%s/roots-replaced", work);
        struct guest_provision_result replaced_result;
        status = guest_provision(&pins, bundle_path, alpine_path, replaced_roots, NULL,
                                 &replaced_result, error, sizeof(error));
        CHECK(status == GUEST_PROVISION_OK, "the first run for the replaced root failed: %s", error);

        /* Corrupt one staged program so the reuse check has to reject it. */
        char corrupted_program[1024];
        snprintf(corrupted_program, sizeof(corrupted_program),
                 "%s/root.active/data/usr/local/bin/leg-tui", replaced_roots);
        flip_one_byte(corrupted_program);

        struct guest_provision_result reused_result;
        status = guest_provision(&pins, bundle_path, alpine_path, replaced_roots, NULL,
                                 &reused_result, error, sizeof(error));
        CHECK(status == GUEST_PROVISION_OK, "the run after the corruption failed: %s", error);
        CHECK(!reused_result.reused, "a corrupted active root must not be reused");
        CHECK(path_under(replaced_roots, "root.active/data/usr/local/bin/leg-tui"),
              "the corrupted active root was not replaced");
    }

    /* --- a root whose marker records another contract is replaced ------ */
    {
        struct {
            const char *label;
            const char *from;
            const char *to;
        } edits[] = {
            {"marker-kind", "\"bundle_kind\": \"ios-mobile\"", "\"bundle_kind\": \"ios-mobilf\""},
            {"marker-version", "\"leg_version\": \"0.44.0\"", "\"leg_version\": \"0.44.1\""},
        };
        for (size_t i = 0; i < sizeof(edits) / sizeof(edits[0]); i++) {
            char edited_roots[1024];
            snprintf(edited_roots, sizeof(edited_roots), "%s/roots-%s", work, edits[i].label);
            struct guest_provision_result first;
            status = guest_provision(&pins, bundle_path, alpine_path, edited_roots, NULL, &first, error,
                                     sizeof(error));
            CHECK(status == GUEST_PROVISION_OK, "%s: the first run failed: %s", edits[i].label, error);

            char marker[1024];
            snprintf(marker, sizeof(marker), "%s/root.active/data/etc/leg/guest-bundle.json", edited_roots);
            CHECK(rewrite_in_file(marker, edits[i].from, edits[i].to), "%s: cannot edit the marker",
                  edits[i].label);

            struct guest_provision_result again;
            status = guest_provision(&pins, bundle_path, alpine_path, edited_roots, NULL, &again, error,
                                     sizeof(error));
            CHECK(status == GUEST_PROVISION_OK, "%s: the run after the edit failed: %s", edits[i].label,
                  error);
            CHECK(!again.reused, "%s: a root with another marker contract must not be reused",
                  edits[i].label);
            CHECK(strcmp(again.marker_sha256, first.marker_sha256) == 0,
                  "%s: the replaced root must record the pinned contract again", edits[i].label);
        }
    }

    /* --- a root without leg-bridge is replaced ------------------------- */
    {
        char bridgeless_roots[1024];
        snprintf(bridgeless_roots, sizeof(bridgeless_roots), "%s/roots-bridgeless", work);
        struct guest_provision_result first;
        status = guest_provision(&pins, bundle_path, alpine_path, bridgeless_roots, NULL, &first, error,
                                 sizeof(error));
        CHECK(status == GUEST_PROVISION_OK, "the first run for the bridgeless root failed: %s", error);

        char bridge[1024];
        snprintf(bridge, sizeof(bridge), "%s/root.active/data/usr/local/bin/leg-bridge", bridgeless_roots);
        CHECK(unlink(bridge) == 0, "cannot remove leg-bridge from the active root");

        struct guest_provision_result again;
        status = guest_provision(&pins, bundle_path, alpine_path, bridgeless_roots, NULL, &again, error,
                                 sizeof(error));
        CHECK(status == GUEST_PROVISION_OK, "the run without leg-bridge failed: %s", error);
        CHECK(!again.reused, "a root without leg-bridge must not be reused");
        CHECK(path_exists(bridge), "the replaced root has no leg-bridge");
    }

    if (failures == 0) {
        printf("ok: %d checks passed\n", checks);
        return 0;
    }
    printf("FAILED: %d of %d checks failed\n", failures, checks);
    return 1;
}
