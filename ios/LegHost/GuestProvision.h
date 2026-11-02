/*
 * Deterministic guest provisioning for the LegHost iOS host.
 *
 * The host ships two pinned archives (the leg guest bundle for i386 and an
 * Alpine i386 root) as app resources. On first launch this provisioner
 * verifies both archives, merges them into one staging archive, imports that
 * archive into a fresh fakefs root with the vendored iSH importer, verifies
 * the result, and only then publishes it as the persistent active root.
 *
 * Nothing here downloads anything: the archives are the embedded resources,
 * and every digest is checked against the pin manifest before the importer is
 * allowed to run.
 */

#ifndef LEGHOST_GUEST_PROVISION_H
#define LEGHOST_GUEST_PROVISION_H

#include <stdbool.h>
#include <stddef.h>

#include "Sha256.h"

#define GUEST_PATH_MAX 4096

/* The pinned inputs, in the order the provisioner consumes them. */
enum {
    GUEST_INPUT_LEG_BUNDLE = 0,
    GUEST_INPUT_ALPINE_ROOT = 1,
    GUEST_INPUT_COUNT = 2,
};

/* One pinned download: where it comes from and what it must hash to. */
struct guest_pinned_input {
    const char *name;
    const char *url;
    const char *filename;
    const char *sha256;
};

/* The compatibility contract the guest bundle manifest has to declare. */
struct guest_compatibility {
    const char *target;
    const char *architecture;
    const char *os;
    const char *libc;
    const char *alpine_release;
    const char *docker_platform;
};

struct guest_pins {
    const struct guest_pinned_input *inputs; /* GUEST_INPUT_COUNT entries */
    const struct guest_compatibility *compatibility;
};

/* The guest programs staged into the guest root. */
#define GUEST_PROGRAM_COUNT 4
extern const char *const guest_program_names[GUEST_PROGRAM_COUNT];

/* Where the staged programs and the guest bundle marker live in the guest. */
#define GUEST_PROGRAMS_DIR "/usr/local/bin"
#define GUEST_MARKER_PATH "/etc/leg/guest-bundle.json"

/* The guest bundle archive holds exactly these six files. */
#define GUEST_BUNDLE_FILE_COUNT 6
extern const char *const guest_bundle_file_names[GUEST_BUNDLE_FILE_COUNT];

enum guest_provision_status {
    GUEST_PROVISION_OK = 0,
    GUEST_PROVISION_ERR_USAGE,    /* the caller passed inconsistent arguments */
    GUEST_PROVISION_ERR_INPUT,    /* an input file is missing or unreadable */
    GUEST_PROVISION_ERR_CHECKSUM, /* an input or staged file failed its digest */
    GUEST_PROVISION_ERR_MANIFEST, /* the guest bundle manifest is missing or wrong */
    GUEST_PROVISION_ERR_ARCHIVE,  /* an archive is unreadable or has the wrong file set */
    GUEST_PROVISION_ERR_IMPORT,   /* the fakefs import failed */
    GUEST_PROVISION_ERR_VERIFY,   /* the staged root did not verify */
    GUEST_PROVISION_ERR_IO,       /* a filesystem operation failed */
};

struct guest_provision_options {
    /* Test hook: truncate the staging archive right before the import so the
     * importer fails after it has already created the staging root. The
     * provisioner then has to remove every piece of that partial state. */
    bool simulate_import_failure;
};

struct guest_provision_result {
    /* True when an existing active root was verified and reused instead of
     * being imported again. */
    bool reused;
    /* The persistent root directory, always valid on success. */
    char active_root[GUEST_PATH_MAX];
    /* Digest of the guest bundle marker, for logging and for the tests. */
    char marker_sha256[SHA256_HEX_LEN + 1];
};

/*
 * Provision the guest root.
 *
 * `roots_dir` holds the persistent state: the active root lives in
 * `<roots_dir>/root.active`, staging directories in `<roots_dir>/root.staging.*`
 * and the merged staging archive next to them. Both inputs are verified
 * against `pins` before anything is imported.
 *
 * Returns a `guest_provision_status`; on failure `error` holds a
 * human-readable reason and no active root is left behind.
 */
int guest_provision(const struct guest_pins *pins,
                    const char *guest_bundle_path,
                    const char *alpine_root_path,
                    const char *roots_dir,
                    const struct guest_provision_options *options,
                    struct guest_provision_result *result,
                    char *error, size_t error_size);

const char *guest_provision_status_string(int status);

#endif
