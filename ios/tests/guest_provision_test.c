/*
 * Host side fixtures for the LegHost guest provisioner.
 *
 * The fixtures build their own pinned inputs, so they are hermetic and do not
 * need the network. They cover the paths the plan calls out: valid inputs,
 * one byte corruption of each input, a bundle for the wrong target, a
 * checksum failure, a failed import, stale staging cleanup, a reload with no
 * partial state, and a verified reuse of an existing root.
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

/*
 * A guest bundle with the four programs, the compatibility manifest and the
 * inner SHA256SUMS. `target` and `elf64` let the fixtures build the rejected
 * variants.
 */
static bool write_fixture_bundle(const char *path, const char *target, bool elf64,
                                 const char program_sha256[GUEST_PROGRAM_COUNT][SHA256_HEX_LEN + 1],
                                 const uint64_t program_size[GUEST_PROGRAM_COUNT]) {
    uint8_t program[FIXTURE_PROGRAM_SIZE];
    char manifest[4096];
    size_t manifest_length = 0;

    manifest_length += (size_t) snprintf(manifest + manifest_length, sizeof(manifest) - manifest_length,
                                         "{\n  \"schema_version\": 1,\n  \"leg_version\": \"0.43.1\",\n"
                                         "  \"target\": \"%s\",\n  \"architecture\": \"%s\",\n"
                                         "  \"os\": \"linux\",\n  \"libc\": \"musl\",\n  \"host\": \"ish\",\n"
                                         "  \"alpine_release\": \"3.20.3\",\n"
                                         "  \"docker_platform\": \"linux/386\",\n  \"binaries\": [\n",
                                         target, elf64 ? "x86_64" : "i386");
    for (int i = 0; i < GUEST_PROGRAM_COUNT; i++) {
        manifest_length += (size_t) snprintf(
            manifest + manifest_length, sizeof(manifest) - manifest_length,
            "    {\"name\": \"%s\", \"version_command\": \"%s --version\", \"size\": %llu, "
            "\"sha256\": \"%s\"}%s\n",
            guest_program_names[i], guest_program_names[i], (unsigned long long) program_size[i],
            program_sha256[i], i + 1 < GUEST_PROGRAM_COUNT ? "," : "");
    }
    manifest_length += (size_t) snprintf(manifest + manifest_length,
                                         sizeof(manifest) - manifest_length, "  ]\n}\n");

    char manifest_sha[SHA256_HEX_LEN + 1];
    sha256_hex(manifest, manifest_length, manifest_sha);

    char sums[1024];
    size_t sums_length = 0;
    for (int i = 0; i < GUEST_PROGRAM_COUNT; i++) {
        sums_length += (size_t) snprintf(sums + sums_length, sizeof(sums) - sums_length,
                                         "%s  %s\n", program_sha256[i], guest_program_names[i]);
    }
    sums_length += (size_t) snprintf(sums + sums_length, sizeof(sums) - sums_length, "%s  %s\n",
                                     manifest_sha, "ish-compatibility-v1.json");

    struct archive *archive = archive_write_new();
    archive_write_set_format_pax(archive);
    archive_write_add_filter_gzip(archive);
    if (archive_write_open_filename(archive, path) != ARCHIVE_OK) {
        archive_write_free(archive);
        return false;
    }

    bool ok = write_archive_entry(archive, "SHA256SUMS", sums, sums_length, AE_IFREG, 0644, NULL) &&
              write_archive_entry(archive, "ish-compatibility-v1.json", manifest, manifest_length,
                                  AE_IFREG, 0644, NULL);
    for (int i = 0; ok && i < GUEST_PROGRAM_COUNT; i++) {
        write_elf_header(program, elf64);
        ok = write_archive_entry(archive, guest_program_names[i], program, sizeof(program), AE_IFREG,
                                 0755, NULL);
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

/* ------------------------------------------------------------------ main */

/*
 * The pinned input mode: run the provisioner against the real pinned
 * archives. The pins come from a small "key=value" file that the shell
 * script writes from the pin manifest, so there is one source of truth.
 */
struct pinned_values {
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
        if (strcmp(line, "target") == 0)
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

    return complete && values->target[0] != '\0' && values->bundle_sha256[0] != '\0' &&
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
    inputs[GUEST_INPUT_LEG_BUNDLE].filename = "leg-0.43.1-i686-unknown-linux-musl.tar.gz";
    inputs[GUEST_INPUT_LEG_BUNDLE].sha256 = values.bundle_sha256;
    inputs[GUEST_INPUT_ALPINE_ROOT].name = "alpine_root";
    inputs[GUEST_INPUT_ALPINE_ROOT].url = "https://example.invalid/alpine.tar.gz";
    inputs[GUEST_INPUT_ALPINE_ROOT].filename = "alpine-minirootfs-3.20.3-x86.tar.gz";
    inputs[GUEST_INPUT_ALPINE_ROOT].sha256 = values.alpine_sha256;

    struct guest_compatibility compatibility = {
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
    if (!write_fixture_bundle(bundle_path, "i686-unknown-linux-musl", false,
                              program_sha256, program_size)) {
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
    CHECK(path_exists("roots/root.active/data/usr/local/bin/leg-web"),
          "leg-web is missing from the guest");
    CHECK(path_exists("roots/root.active/data/usr/local/bin/leg-ui-supervisor"),
          "leg-ui-supervisor is missing from the guest");
    CHECK(path_exists("roots/root.active/data/etc/leg/guest-bundle.json"),
          "the guest bundle marker is missing");

    /* The marker has to record both input digests and every staged file. */
    {
        char *marker = NULL;
        FILE *file = fopen("roots/root.active/data/etc/leg/guest-bundle.json", "rb");
        CHECK(file != NULL, "the guest bundle marker cannot be opened");
        if (file != NULL) {
            fseek(file, 0, SEEK_END);
            long size = ftell(file);
            fseek(file, 0, SEEK_SET);
            marker = malloc((size_t) size + 1);
            size_t got = fread(marker, 1, (size_t) size, file);
            marker[got] = '\0';
            fclose(file);

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

    /* --- a bundle for the wrong target is rejected -------------------- */
    {
        char wrong[1024];
        snprintf(wrong, sizeof(wrong), "%s/wrong-target.tar.gz", work);
        if (write_fixture_bundle(wrong, "x86_64-unknown-linux-musl", false,
                                 program_sha256, program_size)) {
            struct guest_pinned_input wrong_inputs[GUEST_INPUT_COUNT];
            memcpy(wrong_inputs, inputs, sizeof(inputs));
            char wrong_sha[SHA256_HEX_LEN + 1];
            digest_of(wrong, wrong_sha);
            wrong_inputs[GUEST_INPUT_LEG_BUNDLE].sha256 = wrong_sha;

            struct guest_pins wrong_pins;
            fill_pins(&wrong_pins, wrong_inputs, &fixture_compatibility);

            char wrong_roots[1024];
            snprintf(wrong_roots, sizeof(wrong_roots), "%s/roots-wrong-target", work);
            struct guest_provision_result wrong_result;
            status = guest_provision(&wrong_pins, wrong, alpine_path, wrong_roots, NULL,
                                     &wrong_result, error, sizeof(error));
            CHECK(status == GUEST_PROVISION_ERR_MANIFEST,
                  "a bundle for the wrong target must be rejected, got %d", status);
            CHECK(!path_exists(wrong_roots) || directory_is_empty(wrong_roots),
                  "a rejected bundle must leave no staging state");
        }
    }

    /* --- a program that is not an ELF32 i386 binary is rejected -------- */
    {
        char wrong[1024];
        snprintf(wrong, sizeof(wrong), "%s/wrong-elf.tar.gz", work);
        if (write_fixture_bundle(wrong, "i686-unknown-linux-musl", true,
                                 program_sha256, program_size)) {
            struct guest_pinned_input wrong_inputs[GUEST_INPUT_COUNT];
            memcpy(wrong_inputs, inputs, sizeof(inputs));
            char wrong_sha[SHA256_HEX_LEN + 1];
            digest_of(wrong, wrong_sha);
            wrong_inputs[GUEST_INPUT_LEG_BUNDLE].sha256 = wrong_sha;

            struct guest_pins wrong_pins;
            fill_pins(&wrong_pins, wrong_inputs, &fixture_compatibility);

            char wrong_roots[1024];
            snprintf(wrong_roots, sizeof(wrong_roots), "%s/roots-wrong-elf", work);
            struct guest_provision_result wrong_result;
            status = guest_provision(&wrong_pins, wrong, alpine_path, wrong_roots, NULL,
                                     &wrong_result, error, sizeof(error));
            CHECK(status == GUEST_PROVISION_ERR_ARCHIVE,
                  "a bundle with a 64 bit program must be rejected, got %d", status);
        }
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

    if (failures == 0) {
        printf("ok: %d checks passed\n", checks);
        return 0;
    }
    printf("FAILED: %d of %d checks failed\n", failures, checks);
    return 1;
}
