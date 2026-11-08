/*
 * Deterministic guest provisioning for the LegHost iOS host.
 *
 * See GuestProvision.h for the contract. The flow is:
 *
 *   1. verify the digest of both pinned archives;
 *   2. verify the guest bundle: exact file set, compatibility manifest,
 *      ELF32/i386 program headers, per-file digests and the inner SHA256SUMS;
 *   3. verify the Alpine root is a readable archive;
 *   4. merge the root entries, the four guest programs and the bundle marker
 *      into one staging archive;
 *   5. import that archive once with the vendored iSH importer into a staging
 *      directory;
 *   6. verify the staged root;
 *   7. publish the staging directory as the active root with a rename.
 *
 * Every failure path removes the staging archive and the staging directory and
 * leaves no active root behind.
 */

#include "GuestProvision.h"

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include <archive.h>
#include <archive_entry.h>

#include "tools/fakefs.h"

const char *const guest_program_names[GUEST_PROGRAM_COUNT] = {
    "leg", "leg-tui", "leg-ui-supervisor", "leg-bridge",
};

const char *const guest_bundle_file_names[GUEST_BUNDLE_FILE_COUNT] = {
    "SHA256SUMS", "ish-compatibility-v1.json", "leg", "leg-tui", "leg-ui-supervisor", "leg-bridge",
};

/*
 * The shell the guest program runs its commands with. The pinned Alpine root
 * has no bash and the guest program asks for one by name, so the provisioner
 * stages a shim that runs the shell the root does have.
 */
static const char guest_shell_shim[] =
    "#!/bin/sh\n"
    "exec /bin/busybox sh \"$@\"\n";

#define GUEST_COMPATIBILITY_MANIFEST "ish-compatibility-v1.json"
#define GUEST_COMPATIBILITY_SUMS "SHA256SUMS"
#define GUEST_STAGING_DIR_PREFIX "root.staging."
#define GUEST_STAGING_ARCHIVE_PREFIX "root-staging-"
#define GUEST_REPLACED_DIR_PREFIX "root.replaced."
#define GUEST_ACTIVE_DIR_NAME "root.active"
#define GUEST_MAX_PATH_FIELD 1024
#define GUEST_MAX_TEXT_FIELD (64 * 1024)

/*
 * The compatibility fields, in the order the manifest check, the marker and
 * the marker check use them.
 */
#define GUEST_COMPATIBILITY_FIELD_COUNT 8
static const char *const compatibility_keys[GUEST_COMPATIBILITY_FIELD_COUNT] = {
    "bundle_kind", "leg_version", "target", "architecture",
    "os", "libc", "alpine_release", "docker_platform",
};

static void compatibility_values(const struct guest_compatibility *compatibility,
                                 const char *values[GUEST_COMPATIBILITY_FIELD_COUNT]) {
    values[0] = compatibility->bundle_kind;
    values[1] = compatibility->leg_version;
    values[2] = compatibility->target;
    values[3] = compatibility->architecture;
    values[4] = compatibility->os;
    values[5] = compatibility->libc;
    values[6] = compatibility->alpine_release;
    values[7] = compatibility->docker_platform;
}

/* ------------------------------------------------------------------ errors */

static void set_error(char *error, size_t error_size, const char *fmt, ...) {
    if (error == NULL || error_size == 0)
        return;
    va_list args;
    va_start(args, fmt);
    vsnprintf(error, error_size, fmt, args);
    va_end(args);
}

const char *guest_provision_status_string(int status) {
    switch (status) {
    case GUEST_PROVISION_OK: return "ok";
    case GUEST_PROVISION_ERR_USAGE: return "invalid arguments";
    case GUEST_PROVISION_ERR_INPUT: return "pinned input unreadable";
    case GUEST_PROVISION_ERR_CHECKSUM: return "digest mismatch";
    case GUEST_PROVISION_ERR_MANIFEST: return "guest bundle manifest rejected";
    case GUEST_PROVISION_ERR_ARCHIVE: return "archive rejected";
    case GUEST_PROVISION_ERR_IMPORT: return "guest root import failed";
    case GUEST_PROVISION_ERR_VERIFY: return "guest root verification failed";
    case GUEST_PROVISION_ERR_IO: return "filesystem error";
    default: return "unknown error";
    }
}

/* ------------------------------------------------------------ file helpers */

static bool read_whole_file(const char *path, char **out, size_t *out_len, char *error, size_t error_size) {
    FILE *file = fopen(path, "rb");
    if (file == NULL) {
        set_error(error, error_size, "cannot open %s: %s", path, strerror(errno));
        return false;
    }

    size_t capacity = 8192;
    size_t length = 0;
    char *buffer = malloc(capacity);
    if (buffer == NULL) {
        fclose(file);
        set_error(error, error_size, "out of memory reading %s", path);
        return false;
    }

    for (;;) {
        if (length == capacity) {
            size_t next = capacity * 2;
            char *grown = realloc(buffer, next);
            if (grown == NULL) {
                free(buffer);
                fclose(file);
                set_error(error, error_size, "out of memory reading %s", path);
                return false;
            }
            buffer = grown;
            capacity = next;
        }
        size_t got = fread(buffer + length, 1, capacity - length, file);
        length += got;
        if (got == 0) {
            if (ferror(file)) {
                free(buffer);
                fclose(file);
                set_error(error, error_size, "read error on %s: %s", path, strerror(errno));
                return false;
            }
            break;
        }
    }
    fclose(file);

    char *grown = realloc(buffer, length + 1);
    if (grown != NULL)
        buffer = grown;
    buffer[length] = '\0';
    *out = buffer;
    *out_len = length;
    return true;
}

static void sha256_to_hex(const uint8_t digest[SHA256_DIGEST_LEN], char out[SHA256_HEX_LEN + 1]) {
    static const char hex[] = "0123456789abcdef";
    for (int i = 0; i < SHA256_DIGEST_LEN; i++) {
        out[i * 2] = hex[digest[i] >> 4];
        out[i * 2 + 1] = hex[digest[i] & 0xf];
    }
    out[SHA256_HEX_LEN] = '\0';
}

static bool sha256_of_file(const char *path, char out[SHA256_HEX_LEN + 1], char *error, size_t error_size) {
    FILE *file = fopen(path, "rb");
    if (file == NULL) {
        set_error(error, error_size, "cannot open %s: %s", path, strerror(errno));
        return false;
    }

    struct sha256_ctx ctx;
    sha256_init(&ctx);

    uint8_t buffer[64 * 1024];
    for (;;) {
        size_t got = fread(buffer, 1, sizeof(buffer), file);
        if (got > 0)
            sha256_update(&ctx, buffer, got);
        if (got < sizeof(buffer)) {
            if (ferror(file)) {
                fclose(file);
                set_error(error, error_size, "read error on %s: %s", path, strerror(errno));
                return false;
            }
            break;
        }
    }
    fclose(file);

    uint8_t digest[SHA256_DIGEST_LEN];
    sha256_final(&ctx, digest);
    sha256_to_hex(digest, out);
    return true;
}

static bool path_exists(const char *path) {
    struct stat info;
    return stat(path, &info) == 0;
}

static bool remove_tree(const char *path) {
    struct stat info;
    if (lstat(path, &info) != 0)
        return errno == ENOENT;

    if (!S_ISDIR(info.st_mode))
        return unlink(path) == 0;

    DIR *dir = opendir(path);
    if (dir == NULL)
        return false;

    bool ok = true;
    struct dirent *entry;
    while ((entry = readdir(dir)) != NULL) {
        if (strcmp(entry->d_name, ".") == 0 || strcmp(entry->d_name, "..") == 0)
            continue;
        char child[GUEST_PATH_MAX];
        if (snprintf(child, sizeof(child), "%s/%s", path, entry->d_name) >= (int) sizeof(child)) {
            ok = false;
            continue;
        }
        if (!remove_tree(child))
            ok = false;
    }
    closedir(dir);

    if (rmdir(path) != 0)
        ok = false;
    return ok;
}

static void make_staging_id(char *out, size_t size) {
    struct timespec now;
    clock_gettime(CLOCK_REALTIME, &now);
    unsigned long long value = (unsigned long long) now.tv_nsec;
    value ^= (unsigned long long) now.tv_sec << 21;
    value ^= (unsigned long long) getpid() << 41;
    value ^= (unsigned long long) arc4random() << 7;
    snprintf(out, size, "%016llx", value);
}

/* ------------------------------------------------------------- json reader */

enum json_type { JSON_NULL, JSON_BOOL, JSON_NUMBER, JSON_STRING, JSON_ARRAY, JSON_OBJECT };

struct json_value;

struct json_member {
    char *name;
    struct json_value *value;
    struct json_member *next;
};

struct json_value {
    enum json_type type;
    bool boolean;
    double number;
    char *string;
    struct json_value **items;
    size_t count;
    struct json_member *members;
};

struct json_parser {
    const char *cursor;
    char *error;
    size_t error_size;
};

static struct json_value *json_parse_value(struct json_parser *parser);

static void json_skip_space(struct json_parser *parser) {
    while (*parser->cursor == ' ' || *parser->cursor == '\t' || *parser->cursor == '\n' ||
           *parser->cursor == '\r')
        parser->cursor++;
}

static void json_fail(struct json_parser *parser, const char *message) {
    if (parser->error != NULL && parser->error_size > 0 && parser->error[0] == '\0')
        set_error(parser->error, parser->error_size, "%s", message);
}

static struct json_value *json_new(enum json_type type) {
    struct json_value *value = calloc(1, sizeof(*value));
    if (value != NULL)
        value->type = type;
    return value;
}

static void json_free(struct json_value *value) {
    if (value == NULL)
        return;
    switch (value->type) {
    case JSON_STRING:
        free(value->string);
        break;
    case JSON_ARRAY:
        for (size_t i = 0; i < value->count; i++)
            json_free(value->items[i]);
        free(value->items);
        break;
    case JSON_OBJECT:
        for (struct json_member *member = value->members; member != NULL; member = member->next) {
            free(member->name);
            json_free(member->value);
        }
        break;
    default:
        break;
    }
    free(value);
}

static bool json_append_utf8(char **buffer, size_t *length, size_t *capacity, unsigned int code_point) {
    char encoded[4];
    size_t count;
    if (code_point < 0x80) {
        encoded[0] = (char) code_point;
        count = 1;
    } else if (code_point < 0x800) {
        encoded[0] = (char) (0xc0 | (code_point >> 6));
        encoded[1] = (char) (0x80 | (code_point & 0x3f));
        count = 2;
    } else if (code_point < 0x10000) {
        encoded[0] = (char) (0xe0 | (code_point >> 12));
        encoded[1] = (char) (0x80 | ((code_point >> 6) & 0x3f));
        encoded[2] = (char) (0x80 | (code_point & 0x3f));
        count = 3;
    } else {
        encoded[0] = (char) (0xf0 | (code_point >> 18));
        encoded[1] = (char) (0x80 | ((code_point >> 12) & 0x3f));
        encoded[2] = (char) (0x80 | ((code_point >> 6) & 0x3f));
        encoded[3] = (char) (0x80 | (code_point & 0x3f));
        count = 4;
    }

    if (*length + count + 1 > *capacity) {
        size_t next = *capacity == 0 ? 64 : *capacity * 2;
        while (next < *length + count + 1)
            next *= 2;
        char *grown = realloc(*buffer, next);
        if (grown == NULL)
            return false;
        *buffer = grown;
        *capacity = next;
    }
    memcpy(*buffer + *length, encoded, count);
    *length += count;
    (*buffer)[*length] = '\0';
    return true;
}

static bool json_parse_hex4(struct json_parser *parser, unsigned int *out) {
    unsigned int value = 0;
    for (int i = 0; i < 4; i++) {
        char c = *parser->cursor++;
        value <<= 4;
        if (c >= '0' && c <= '9')
            value |= (unsigned int) (c - '0');
        else if (c >= 'a' && c <= 'f')
            value |= (unsigned int) (c - 'a' + 10);
        else if (c >= 'A' && c <= 'F')
            value |= (unsigned int) (c - 'A' + 10);
        else {
            json_fail(parser, "invalid \\u escape in JSON string");
            return false;
        }
    }
    *out = value;
    return true;
}

static char *json_parse_string_raw(struct json_parser *parser) {
    if (*parser->cursor != '"') {
        json_fail(parser, "expected a JSON string");
        return NULL;
    }
    parser->cursor++;

    char *buffer = NULL;
    size_t length = 0;
    size_t capacity = 0;

    while (*parser->cursor != '"') {
        if (*parser->cursor == '\0') {
            json_fail(parser, "unterminated JSON string");
            free(buffer);
            return NULL;
        }
        char c = *parser->cursor++;
        if (c != '\\') {
            if (!json_append_utf8(&buffer, &length, &capacity, (unsigned char) c)) {
                json_fail(parser, "out of memory");
                free(buffer);
                return NULL;
            }
            continue;
        }

        char escape = *parser->cursor++;
        switch (escape) {
        case '"':
        case '\\':
        case '/':
            if (!json_append_utf8(&buffer, &length, &capacity, (unsigned char) escape))
                goto out_of_memory;
            break;
        case 'b':
            if (!json_append_utf8(&buffer, &length, &capacity, '\b'))
                goto out_of_memory;
            break;
        case 'f':
            if (!json_append_utf8(&buffer, &length, &capacity, '\f'))
                goto out_of_memory;
            break;
        case 'n':
            if (!json_append_utf8(&buffer, &length, &capacity, '\n'))
                goto out_of_memory;
            break;
        case 'r':
            if (!json_append_utf8(&buffer, &length, &capacity, '\r'))
                goto out_of_memory;
            break;
        case 't':
            if (!json_append_utf8(&buffer, &length, &capacity, '\t'))
                goto out_of_memory;
            break;
        case 'u': {
            unsigned int code_point;
            if (!json_parse_hex4(parser, &code_point))
                goto failed;
            if (code_point >= 0xd800 && code_point <= 0xdbff && parser->cursor[0] == '\\' &&
                parser->cursor[1] == 'u') {
                parser->cursor += 2;
                unsigned int low;
                if (!json_parse_hex4(parser, &low))
                    goto failed;
                if (low >= 0xdc00 && low <= 0xdfff)
                    code_point = 0x10000 + ((code_point - 0xd800) << 10) + (low - 0xdc00);
                else
                    code_point = 0xfffd;
            }
            if (!json_append_utf8(&buffer, &length, &capacity, code_point))
                goto out_of_memory;
            break;
        }
        default:
            json_fail(parser, "invalid escape in JSON string");
            goto failed;
        }
        continue;

    out_of_memory:
        json_fail(parser, "out of memory");
        goto failed;
    }

    parser->cursor++; /* closing quote */
    if (buffer == NULL) {
        buffer = calloc(1, 1);
        if (buffer == NULL) {
            json_fail(parser, "out of memory");
            return NULL;
        }
    }
    return buffer;

failed:
    free(buffer);
    return NULL;
}

static struct json_value *json_parse_number(struct json_parser *parser) {
    char *end = NULL;
    double value = strtod(parser->cursor, &end);
    if (end == parser->cursor) {
        json_fail(parser, "invalid number in JSON");
        return NULL;
    }
    parser->cursor = end;
    struct json_value *node = json_new(JSON_NUMBER);
    if (node == NULL) {
        json_fail(parser, "out of memory");
        return NULL;
    }
    node->number = value;
    return node;
}

static struct json_value *json_parse_array(struct json_parser *parser) {
    parser->cursor++; /* '[' */
    struct json_value *node = json_new(JSON_ARRAY);
    if (node == NULL) {
        json_fail(parser, "out of memory");
        return NULL;
    }

    json_skip_space(parser);
    if (*parser->cursor == ']') {
        parser->cursor++;
        return node;
    }

    for (;;) {
        struct json_value *item = json_parse_value(parser);
        if (item == NULL)
            goto failed;

        struct json_value **grown = realloc(node->items, (node->count + 1) * sizeof(*grown));
        if (grown == NULL) {
            json_free(item);
            json_fail(parser, "out of memory");
            goto failed;
        }
        node->items = grown;
        node->items[node->count++] = item;

        json_skip_space(parser);
        if (*parser->cursor == ',') {
            parser->cursor++;
            json_skip_space(parser);
            continue;
        }
        if (*parser->cursor == ']') {
            parser->cursor++;
            return node;
        }
        json_fail(parser, "expected ',' or ']' in JSON array");
        goto failed;
    }

failed:
    json_free(node);
    return NULL;
}

static struct json_value *json_parse_object(struct json_parser *parser) {
    parser->cursor++; /* '{' */
    struct json_value *node = json_new(JSON_OBJECT);
    if (node == NULL) {
        json_fail(parser, "out of memory");
        return NULL;
    }

    struct json_member *tail = NULL;

    json_skip_space(parser);
    if (*parser->cursor == '}') {
        parser->cursor++;
        return node;
    }

    for (;;) {
        char *name = json_parse_string_raw(parser);
        if (name == NULL)
            goto failed;

        json_skip_space(parser);
        if (*parser->cursor != ':') {
            free(name);
            json_fail(parser, "expected ':' in JSON object");
            goto failed;
        }
        parser->cursor++;
        json_skip_space(parser);

        struct json_value *value = json_parse_value(parser);
        if (value == NULL) {
            free(name);
            goto failed;
        }

        struct json_member *member = calloc(1, sizeof(*member));
        if (member == NULL) {
            free(name);
            json_free(value);
            json_fail(parser, "out of memory");
            goto failed;
        }
        member->name = name;
        member->value = value;
        if (tail == NULL)
            node->members = member;
        else
            tail->next = member;
        tail = member;

        json_skip_space(parser);
        if (*parser->cursor == ',') {
            parser->cursor++;
            json_skip_space(parser);
            continue;
        }
        if (*parser->cursor == '}') {
            parser->cursor++;
            return node;
        }
        json_fail(parser, "expected ',' or '}' in JSON object");
        goto failed;
    }

failed:
    json_free(node);
    return NULL;
}

static struct json_value *json_parse_value(struct json_parser *parser) {
    json_skip_space(parser);
    switch (*parser->cursor) {
    case '{':
        return json_parse_object(parser);
    case '[':
        return json_parse_array(parser);
    case '"': {
        char *string = json_parse_string_raw(parser);
        if (string == NULL)
            return NULL;
        struct json_value *node = json_new(JSON_STRING);
        if (node == NULL) {
            free(string);
            json_fail(parser, "out of memory");
            return NULL;
        }
        node->string = string;
        return node;
    }
    case 't':
        if (strncmp(parser->cursor, "true", 4) != 0)
            break;
        parser->cursor += 4;
        {
            struct json_value *node = json_new(JSON_BOOL);
            if (node == NULL) {
                json_fail(parser, "out of memory");
                return NULL;
            }
            node->boolean = true;
            return node;
        }
    case 'f':
        if (strncmp(parser->cursor, "false", 5) != 0)
            break;
        parser->cursor += 5;
        {
            struct json_value *node = json_new(JSON_BOOL);
            if (node == NULL) {
                json_fail(parser, "out of memory");
                return NULL;
            }
            node->boolean = false;
            return node;
        }
    case 'n':
        if (strncmp(parser->cursor, "null", 4) != 0)
            break;
        parser->cursor += 4;
        {
            struct json_value *node = json_new(JSON_NULL);
            if (node == NULL)
                json_fail(parser, "out of memory");
            return node;
        }
    default:
        break;
    }
    if (*parser->cursor == '-' || (*parser->cursor >= '0' && *parser->cursor <= '9'))
        return json_parse_number(parser);

    json_fail(parser, "unexpected token in JSON");
    return NULL;
}

static struct json_value *json_parse(const char *text, char *error, size_t error_size) {
    struct json_parser parser = {.cursor = text, .error = error, .error_size = error_size};
    if (error != NULL && error_size > 0)
        error[0] = '\0';

    struct json_value *value = json_parse_value(&parser);
    if (value == NULL)
        return NULL;

    json_skip_space(&parser);
    if (*parser.cursor != '\0') {
        json_free(value);
        set_error(error, error_size, "trailing data after the JSON value");
        return NULL;
    }
    return value;
}

static const struct json_value *json_member_get(const struct json_value *object, const char *name) {
    if (object == NULL || object->type != JSON_OBJECT)
        return NULL;
    for (struct json_member *member = object->members; member != NULL; member = member->next)
        if (strcmp(member->name, name) == 0)
            return member->value;
    return NULL;
}

static const char *json_as_string(const struct json_value *value) {
    if (value == NULL || value->type != JSON_STRING)
        return NULL;
    return value->string;
}

static bool json_as_number(const struct json_value *value, double *out) {
    if (value == NULL || value->type != JSON_NUMBER)
        return false;
    *out = value->number;
    return true;
}

/* ---------------------------------------------------------- staged records */

struct staged_file {
    char path[GUEST_MAX_PATH_FIELD];
    bool symlink;
    uint64_t size;
    char sha256[SHA256_HEX_LEN + 1];
};

struct staged_list {
    struct staged_file *items;
    size_t count;
    size_t capacity;
};

static bool staged_list_add(struct staged_list *list, const struct staged_file *record) {
    if (list->count == list->capacity) {
        size_t next = list->capacity == 0 ? 64 : list->capacity * 2;
        struct staged_file *grown = realloc(list->items, next * sizeof(*grown));
        if (grown == NULL)
            return false;
        list->items = grown;
        list->capacity = next;
    }
    list->items[list->count++] = *record;
    return true;
}

static const struct staged_file *staged_list_find(const struct staged_list *list, const char *path) {
    for (size_t i = 0; i < list->count; i++)
        if (strcmp(list->items[i].path, path) == 0)
            return &list->items[i];
    return NULL;
}

static void staged_list_free(struct staged_list *list) {
    free(list->items);
    list->items = NULL;
    list->count = 0;
    list->capacity = 0;
}

/* -------------------------------------------------------- archive plumbing */

/* Turns a tar entry path into a guest path and rejects traversal. */
static bool normalize_archive_path(const char *path, char *out, size_t out_size) {
    while (path[0] == '.' && path[1] == '/')
        path += 2;

    size_t length = 0;
    if (out_size < 2)
        return false;
    out[length++] = '/';
    while (*path != '\0') {
        const char *component = path;
        while (*path != '\0' && *path != '/')
            path++;
        size_t component_length = (size_t) (path - component);
        if (component_length == 0) {
            if (*path == '/')
                path++;
            continue;
        }
        if (component_length == 1 && component[0] == '.') {
            if (*path == '/')
                path++;
            continue;
        }
        if (component_length == 2 && component[0] == '.' && component[1] == '.')
            return false;
        if (length > 1) {
            if (length + 1 >= out_size)
                return false;
            out[length++] = '/';
        }
        if (length + component_length >= out_size)
            return false;
        memcpy(out + length, component, component_length);
        length += component_length;
        if (*path == '/')
            path++;
    }
    out[length] = '\0';
    return true;
}

struct archive *open_read_archive(const char *path, char *error, size_t error_size) {
    struct archive *archive = archive_read_new();
    if (archive == NULL) {
        set_error(error, error_size, "cannot create an archive reader");
        return NULL;
    }
    archive_read_support_filter_gzip(archive);
    archive_read_support_format_tar(archive);
    if (archive_read_open_filename(archive, path, 65536) != ARCHIVE_OK) {
        set_error(error, error_size, "cannot open %s: %s", path, archive_error_string(archive));
        archive_read_free(archive);
        return NULL;
    }
    return archive;
}

/* What one archive entry contributes to the checks. */
struct entry_capture {
    uint64_t size;
    char sha256[SHA256_HEX_LEN + 1];
    uint8_t head[64];
    size_t head_length;
    char *text; /* the whole entry, only for the small metadata files */
    size_t text_length;
};

/*
 * Streams the current entry, hashing it and capturing the first bytes (and,
 * for the small metadata files, the whole content).
 */
static bool read_entry(struct archive *archive, struct entry_capture *capture, bool capture_text,
                       char *error, size_t error_size) {
    memset(capture, 0, sizeof(*capture));

    char *text = NULL;
    size_t text_length = 0;
    size_t text_capacity = 0;

    struct sha256_ctx sha;
    sha256_init(&sha);

    for (;;) {
        const void *block = NULL;
        size_t block_size = 0;
        la_int64_t offset = 0;
        int result = archive_read_data_block(archive, &block, &block_size, &offset);
        if (result == ARCHIVE_EOF)
            break;
        if (result != ARCHIVE_OK) {
            set_error(error, error_size, "cannot read archive entry data: %s", archive_error_string(archive));
            free(text);
            return false;
        }

        sha256_update(&sha, block, block_size);
        capture->size += block_size;

        size_t want = sizeof(capture->head) - capture->head_length;
        size_t take = block_size < want ? block_size : want;
        if (take > 0) {
            memcpy(capture->head + capture->head_length, block, take);
            capture->head_length += take;
        }

        if (capture_text) {
            if (text_length + block_size + 1 > text_capacity) {
                size_t next = text_capacity == 0 ? 4096 : text_capacity;
                while (next < text_length + block_size + 1)
                    next *= 2;
                char *grown = realloc(text, next);
                if (grown == NULL) {
                    set_error(error, error_size, "out of memory reading an archive entry");
                    free(text);
                    return false;
                }
                text = grown;
                text_capacity = next;
            }
            memcpy(text + text_length, block, block_size);
            text_length += block_size;
            text[text_length] = '\0';
        }
    }

    uint8_t digest[SHA256_DIGEST_LEN];
    sha256_final(&sha, digest);
    sha256_to_hex(digest, capture->sha256);

    if (capture_text) {
        if (text == NULL) {
            text = calloc(1, 1);
            if (text == NULL) {
                set_error(error, error_size, "out of memory reading an archive entry");
                return false;
            }
        }
        capture->text = text;
        capture->text_length = text_length;
    }
    return true;
}

static bool elf_is_i386(const uint8_t *head, size_t length) {
    if (length < 20)
        return false;
    if (memcmp(head, "\x7f" "ELF", 4) != 0)
        return false;
    if (head[4] != 1) /* ELFCLASS32 */
        return false;
    if (head[5] != 1) /* ELFDATA2LSB */
        return false;
    uint16_t type = (uint16_t) (head[16] | ((uint16_t) head[17] << 8));
    uint16_t machine = (uint16_t) (head[18] | ((uint16_t) head[19] << 8));
    if (machine != 3) /* EM_386 */
        return false;
    if (type != 2 && type != 3) /* ET_EXEC, ET_DYN */
        return false;
    return true;
}

/* ------------------------------------------------------ guest bundle checks */

struct bundle_entry {
    bool present;
    uint64_t size;
    char sha256[SHA256_HEX_LEN + 1];
};

struct guest_bundle {
    struct bundle_entry entries[GUEST_BUNDLE_FILE_COUNT];
    char *manifest;
    size_t manifest_length;
    char *sums;
    size_t sums_length;
};

static void guest_bundle_free(struct guest_bundle *bundle) {
    free(bundle->manifest);
    free(bundle->sums);
    bundle->manifest = NULL;
    bundle->sums = NULL;
}

static int bundle_file_index(const char *name) {
    for (int i = 0; i < GUEST_BUNDLE_FILE_COUNT; i++)
        if (strcmp(guest_bundle_file_names[i], name) == 0)
            return i;
    return -1;
}

static bool check_compatibility_manifest(const struct guest_bundle *bundle,
                                         const struct guest_compatibility *expected, char *error,
                                         size_t error_size) {
    struct json_value *root = json_parse(bundle->manifest, error, error_size);
    if (root == NULL) {
        set_error(error, error_size, "the guest bundle manifest is not valid JSON");
        return false;
    }

    double version = 0;
    if (!json_as_number(json_member_get(root, "schema_version"), &version) || version != 1) {
        set_error(error, error_size, "the guest bundle manifest has an unsupported schema version");
        json_free(root);
        return false;
    }

    const char *values[GUEST_COMPATIBILITY_FIELD_COUNT];
    compatibility_values(expected, values);
    for (size_t i = 0; i < GUEST_COMPATIBILITY_FIELD_COUNT; i++) {
        const char *actual = json_as_string(json_member_get(root, compatibility_keys[i]));
        if (actual == NULL || strcmp(actual, values[i]) != 0) {
            set_error(error, error_size, "the guest bundle manifest declares %s = %s, expected %s",
                      compatibility_keys[i], actual != NULL ? actual : "(missing)", values[i]);
            json_free(root);
            return false;
        }
    }

    const struct json_value *binaries = json_member_get(root, "binaries");
    if (binaries == NULL || binaries->type != JSON_ARRAY || binaries->count != GUEST_PROGRAM_COUNT) {
        set_error(error, error_size, "the guest bundle manifest does not list the four guest programs");
        json_free(root);
        return false;
    }

    for (int i = 0; i < GUEST_PROGRAM_COUNT; i++) {
        int index = bundle_file_index(guest_program_names[i]);
        const struct json_value *entry = NULL;
        for (size_t j = 0; j < binaries->count; j++) {
            const char *name = json_as_string(json_member_get(binaries->items[j], "name"));
            if (name != NULL && strcmp(name, guest_program_names[i]) == 0) {
                entry = binaries->items[j];
                break;
            }
        }
        if (entry == NULL) {
            set_error(error, error_size, "the guest bundle manifest does not list %s",
                      guest_program_names[i]);
            json_free(root);
            return false;
        }

        /* The release smoke runs exactly this command, so the host only
         * accepts the command the release verified. */
        char version_command[64];
        snprintf(version_command, sizeof(version_command), "%s --version", guest_program_names[i]);
        const char *command = json_as_string(json_member_get(entry, "version_command"));
        if (command == NULL || strcmp(command, version_command) != 0) {
            set_error(error, error_size, "the guest bundle manifest declares the wrong version command for %s",
                      guest_program_names[i]);
            json_free(root);
            return false;
        }

        double size = 0;
        if (!json_as_number(json_member_get(entry, "size"), &size) ||
            (uint64_t) size != bundle->entries[index].size) {
            set_error(error, error_size, "the guest bundle manifest declares the wrong size for %s",
                      guest_program_names[i]);
            json_free(root);
            return false;
        }

        const char *sha = json_as_string(json_member_get(entry, "sha256"));
        if (sha == NULL || strcmp(sha, bundle->entries[index].sha256) != 0) {
            set_error(error, error_size, "the guest bundle manifest declares the wrong digest for %s",
                      guest_program_names[i]);
            json_free(root);
            return false;
        }
    }

    json_free(root);
    return true;
}

/*
 * SHA256SUMS has to list every other bundle file exactly once, with the digest
 * the archive entry has, and nothing else.
 */
static bool check_inner_sums(const struct guest_bundle *bundle, char *error, size_t error_size) {
    const char *cursor = bundle->sums;
    bool seen[GUEST_BUNDLE_FILE_COUNT] = {false};

    while (*cursor != '\0') {
        const char *line_end = strchr(cursor, '\n');
        size_t line_length = line_end != NULL ? (size_t) (line_end - cursor) : strlen(cursor);

        if (line_length > 0) {
            char line[512];
            if (line_length >= sizeof(line)) {
                set_error(error, error_size, "the guest bundle SHA256SUMS has an overlong line");
                return false;
            }
            memcpy(line, cursor, line_length);
            line[line_length] = '\0';

            /* The format is "<digest>  <name>", with an optional '*' before
             * the name for binary mode. */
            if (strlen(line) < SHA256_HEX_LEN) {
                set_error(error, error_size, "the guest bundle SHA256SUMS has a malformed line");
                return false;
            }
            char digest[SHA256_HEX_LEN + 1];
            memcpy(digest, line, SHA256_HEX_LEN);
            digest[SHA256_HEX_LEN] = '\0';
            for (size_t i = 0; i < SHA256_HEX_LEN; i++) {
                char c = digest[i];
                if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f'))) {
                    set_error(error, error_size,
                              "the guest bundle SHA256SUMS has a malformed digest");
                    return false;
                }
            }

            const char *name = line + SHA256_HEX_LEN;
            if (*name != ' ') {
                set_error(error, error_size, "the guest bundle SHA256SUMS has a malformed line");
                return false;
            }
            while (*name == ' ' || *name == '*')
                name++;
            if (*name == '\0') {
                set_error(error, error_size, "the guest bundle SHA256SUMS has a malformed line");
                return false;
            }

            int index = bundle_file_index(name);
            if (index < 0 || strcmp(name, GUEST_COMPATIBILITY_SUMS) == 0) {
                set_error(error, error_size, "the guest bundle SHA256SUMS lists the unknown file %s", name);
                return false;
            }
            if (seen[index]) {
                set_error(error, error_size, "the guest bundle SHA256SUMS lists %s twice", name);
                return false;
            }
            if (strcmp(digest, bundle->entries[index].sha256) != 0) {
                set_error(error, error_size, "the guest bundle SHA256SUMS disagrees about %s", name);
                return false;
            }
            seen[index] = true;
        }

        if (line_end == NULL)
            break;
        cursor = line_end + 1;
    }

    for (int i = 0; i < GUEST_BUNDLE_FILE_COUNT; i++) {
        if (!seen[i] && strcmp(guest_bundle_file_names[i], GUEST_COMPATIBILITY_SUMS) != 0) {
            set_error(error, error_size, "the guest bundle SHA256SUMS does not list %s",
                      guest_bundle_file_names[i]);
            return false;
        }
    }
    return true;
}

/*
 * Reads the guest bundle and checks everything the pin manifest promises:
 * the exact file set, ELF32/i386 program headers, the compatibility manifest
 * and the inner SHA256SUMS.
 */
static bool validate_guest_bundle(const char *path, const struct guest_compatibility *expected,
                                  struct guest_bundle *bundle, int *status, char *error,
                                  size_t error_size) {
    memset(bundle, 0, sizeof(*bundle));

    struct archive *archive = open_read_archive(path, error, error_size);
    if (archive == NULL) {
        *status = GUEST_PROVISION_ERR_ARCHIVE;
        return false;
    }

    struct archive_entry *entry;
    int result;
    while ((result = archive_read_next_header(archive, &entry)) == ARCHIVE_OK) {
        const char *raw_path = archive_entry_pathname(entry);
        char normalized[GUEST_MAX_PATH_FIELD];
        if (!normalize_archive_path(raw_path, normalized, sizeof(normalized))) {
            set_error(error, error_size, "the guest bundle contains an unusable path: %s", raw_path);
            goto archive_error;
        }

        const char *name = normalized + 1; /* skip the leading slash */
        if (strchr(name, '/') != NULL) {
            set_error(error, error_size, "the guest bundle contains a nested path: %s", raw_path);
            goto archive_error;
        }

        int index = bundle_file_index(name);
        if (index < 0) {
            set_error(error, error_size, "the guest bundle contains an unexpected file: %s", name);
            goto archive_error;
        }
        if (bundle->entries[index].present) {
            set_error(error, error_size, "the guest bundle contains %s twice", name);
            goto archive_error;
        }
        if (archive_entry_filetype(entry) != AE_IFREG) {
            set_error(error, error_size, "the guest bundle entry %s is not a regular file", name);
            goto archive_error;
        }

        bool is_manifest = strcmp(name, GUEST_COMPATIBILITY_MANIFEST) == 0;
        bool is_sums = strcmp(name, GUEST_COMPATIBILITY_SUMS) == 0;

        struct entry_capture capture;
        if (!read_entry(archive, &capture, is_manifest || is_sums, error, error_size)) {
            archive_read_free(archive);
            *status = GUEST_PROVISION_ERR_ARCHIVE;
            return false;
        }

        bundle->entries[index].present = true;
        bundle->entries[index].size = capture.size;
        memcpy(bundle->entries[index].sha256, capture.sha256, sizeof(capture.sha256));

        if (is_manifest) {
            bundle->manifest = capture.text;
            bundle->manifest_length = capture.text_length;
        } else if (is_sums) {
            bundle->sums = capture.text;
            bundle->sums_length = capture.text_length;
        } else {
            for (int i = 0; i < GUEST_PROGRAM_COUNT; i++) {
                if (strcmp(name, guest_program_names[i]) != 0)
                    continue;
                if (!elf_is_i386(capture.head, capture.head_length)) {
                    set_error(error, error_size, "%s is not an ELF32 i386 executable",
                              guest_program_names[i]);
                    archive_read_free(archive);
                    *status = GUEST_PROVISION_ERR_ARCHIVE;
                    return false;
                }
            }
        }
    }

    if (result != ARCHIVE_EOF) {
        set_error(error, error_size, "cannot read the guest bundle: %s", archive_error_string(archive));
        goto archive_error;
    }
    archive_read_free(archive);

    for (int i = 0; i < GUEST_BUNDLE_FILE_COUNT; i++) {
        if (!bundle->entries[i].present) {
            set_error(error, error_size, "the guest bundle is missing %s", guest_bundle_file_names[i]);
            *status = GUEST_PROVISION_ERR_ARCHIVE;
            return false;
        }
    }
    if (bundle->manifest == NULL || bundle->sums == NULL) {
        set_error(error, error_size, "the guest bundle metadata is unreadable");
        *status = GUEST_PROVISION_ERR_ARCHIVE;
        return false;
    }

    if (!check_compatibility_manifest(bundle, expected, error, error_size)) {
        *status = GUEST_PROVISION_ERR_MANIFEST;
        return false;
    }
    if (!check_inner_sums(bundle, error, error_size)) {
        *status = GUEST_PROVISION_ERR_MANIFEST;
        return false;
    }

    return true;

archive_error:
    archive_read_free(archive);
    *status = GUEST_PROVISION_ERR_ARCHIVE;
    return false;
}

/* ---------------------------------------------------------------- markers */

static bool append_to_buffer(char **buffer, size_t *length, size_t *capacity, const char *text) {
    size_t extra = strlen(text);
    if (*length + extra + 1 > *capacity) {
        size_t next = *capacity == 0 ? 4096 : *capacity;
        while (next < *length + extra + 1)
            next *= 2;
        char *grown = realloc(*buffer, next);
        if (grown == NULL)
            return false;
        *buffer = grown;
        *capacity = next;
    }
    memcpy(*buffer + *length, text, extra);
    *length += extra;
    (*buffer)[*length] = '\0';
    return true;
}

static bool append_json_string(char **buffer, size_t *length, size_t *capacity, const char *text) {
    if (!append_to_buffer(buffer, length, capacity, "\""))
        return false;
    for (const char *cursor = text; *cursor != '\0'; cursor++) {
        char escaped[8];
        switch (*cursor) {
        case '"': memcpy(escaped, "\\\"", 3); break;
        case '\\': memcpy(escaped, "\\\\", 3); break;
        case '\n': memcpy(escaped, "\\n", 3); break;
        case '\r': memcpy(escaped, "\\r", 3); break;
        case '\t': memcpy(escaped, "\\t", 3); break;
        default:
            if ((unsigned char) *cursor < 0x20)
                snprintf(escaped, sizeof(escaped), "\\u%04x", (unsigned char) *cursor);
            else {
                escaped[0] = *cursor;
                escaped[1] = '\0';
            }
            break;
        }
        if (!append_to_buffer(buffer, length, capacity, escaped))
            return false;
    }
    return append_to_buffer(buffer, length, capacity, "\"");
}

/*
 * Builds the guest bundle marker. It records both pinned input digests, the
 * compatibility contract and the digest of every staged file. The marker
 * itself is the one staged file it cannot describe.
 */
static bool build_marker(const struct guest_pins *pins, const struct staged_list *files, char **out,
                         size_t *out_length, char *error, size_t error_size) {
    char *buffer = NULL;
    size_t length = 0;
    size_t capacity = 0;

    if (!append_to_buffer(&buffer, &length, &capacity, "{\n  \"schema_version\": 1,\n")) goto oom;
    if (!append_to_buffer(&buffer, &length, &capacity, "  \"generator\": \"leg-ios-host\",\n")) goto oom;
    if (!append_to_buffer(&buffer, &length, &capacity, "  \"inputs\": [\n")) goto oom;
    for (int i = 0; i < GUEST_INPUT_COUNT; i++) {
        const struct guest_pinned_input *input = &pins->inputs[i];
        if (!append_to_buffer(&buffer, &length, &capacity, "    {\"name\": ")) goto oom;
        if (!append_json_string(&buffer, &length, &capacity, input->name)) goto oom;
        if (!append_to_buffer(&buffer, &length, &capacity, ", \"url\": ")) goto oom;
        if (!append_json_string(&buffer, &length, &capacity, input->url)) goto oom;
        if (!append_to_buffer(&buffer, &length, &capacity, ", \"filename\": ")) goto oom;
        if (!append_json_string(&buffer, &length, &capacity, input->filename)) goto oom;
        if (!append_to_buffer(&buffer, &length, &capacity, ", \"sha256\": ")) goto oom;
        if (!append_json_string(&buffer, &length, &capacity, input->sha256)) goto oom;
        if (!append_to_buffer(&buffer, &length, &capacity, i + 1 < GUEST_INPUT_COUNT ? "},\n" : "}\n"))
            goto oom;
    }
    if (!append_to_buffer(&buffer, &length, &capacity, "  ],\n")) goto oom;

    const char *values[GUEST_COMPATIBILITY_FIELD_COUNT];
    compatibility_values(pins->compatibility, values);
    if (!append_to_buffer(&buffer, &length, &capacity, "  \"compatibility\": {")) goto oom;
    for (size_t i = 0; i < GUEST_COMPATIBILITY_FIELD_COUNT; i++) {
        if (!append_to_buffer(&buffer, &length, &capacity, i == 0 ? "" : ", ")) goto oom;
        if (!append_json_string(&buffer, &length, &capacity, compatibility_keys[i])) goto oom;
        if (!append_to_buffer(&buffer, &length, &capacity, ": ")) goto oom;
        if (!append_json_string(&buffer, &length, &capacity, values[i])) goto oom;
    }
    if (!append_to_buffer(&buffer, &length, &capacity, "},\n")) goto oom;

    if (!append_to_buffer(&buffer, &length, &capacity, "  \"files\": [\n")) goto oom;
    for (size_t i = 0; i < files->count; i++) {
        const struct staged_file *record = &files->items[i];
        if (!append_to_buffer(&buffer, &length, &capacity, "    {\"path\": ")) goto oom;
        if (!append_json_string(&buffer, &length, &capacity, record->path)) goto oom;
        if (!append_to_buffer(&buffer, &length, &capacity,
                               record->symlink ? ", \"type\": \"symlink\", \"size\": "
                                               : ", \"type\": \"file\", \"size\": "))
            goto oom;
        char number[32];
        snprintf(number, sizeof(number), "%llu", (unsigned long long) record->size);
        if (!append_to_buffer(&buffer, &length, &capacity, number)) goto oom;
        if (!append_to_buffer(&buffer, &length, &capacity, ", \"sha256\": ")) goto oom;
        if (!append_json_string(&buffer, &length, &capacity, record->sha256)) goto oom;
        if (!append_to_buffer(&buffer, &length, &capacity,
                               i + 1 < files->count ? "},\n" : "}\n"))
            goto oom;
    }
    if (!append_to_buffer(&buffer, &length, &capacity, "  ]\n}\n")) goto oom;

    *out = buffer;
    *out_length = length;
    return true;

oom:
    free(buffer);
    set_error(error, error_size, "out of memory building the guest bundle marker");
    return false;
}

/* --------------------------------------------------------- staging archive */

static bool write_entry_header(struct archive *out, const char *path, unsigned int filetype,
                               unsigned int permissions, uint64_t size, const char *symlink_target,
                               const char *hardlink_target, time_t mtime, char *error,
                               size_t error_size) {
    struct archive_entry *entry = archive_entry_new();
    if (entry == NULL) {
        set_error(error, error_size, "out of memory writing an archive entry");
        return false;
    }

    archive_entry_set_pathname(entry, path);
    archive_entry_set_filetype(entry, filetype);
    /* The mode carries the type bits, so it has to be set after the type. */
    archive_entry_set_mode(entry, (mode_t) (filetype | (permissions & 07777)));
    archive_entry_set_size(entry, (la_int64_t) size);
    archive_entry_set_mtime(entry, mtime, 0);
    if (symlink_target != NULL)
        archive_entry_set_symlink(entry, symlink_target);
    if (hardlink_target != NULL)
        archive_entry_set_hardlink(entry, hardlink_target);

    int result = archive_write_header(out, entry);
    archive_entry_free(entry);
    if (result != ARCHIVE_OK) {
        set_error(error, error_size, "cannot write an archive entry header: %s",
                  archive_error_string(out));
        return false;
    }
    return true;
}

/* Streams the current entry of `in` into `out` and returns its digest. */
static bool copy_entry(struct archive *in, struct archive *out, uint64_t *size_out,
                       char sha_out[SHA256_HEX_LEN + 1], char *error, size_t error_size) {
    struct sha256_ctx sha;
    sha256_init(&sha);
    uint64_t size = 0;

    for (;;) {
        const void *block = NULL;
        size_t block_size = 0;
        la_int64_t offset = 0;
        int result = archive_read_data_block(in, &block, &block_size, &offset);
        if (result == ARCHIVE_EOF)
            break;
        if (result != ARCHIVE_OK) {
            set_error(error, error_size, "cannot read an archive entry: %s", archive_error_string(in));
            return false;
        }
        sha256_update(&sha, block, block_size);
        size += block_size;
        if (archive_write_data(out, block, block_size) < 0) {
            set_error(error, error_size, "cannot write an archive entry: %s", archive_error_string(out));
            return false;
        }
    }

    uint8_t digest[SHA256_DIGEST_LEN];
    sha256_final(&sha, digest);
    sha256_to_hex(digest, sha_out);
    if (size_out != NULL)
        *size_out = size;
    return true;
}

/*
 * Merges the Alpine root, the four guest programs and the guest bundle marker
 * into one staging archive. The Alpine entries are streamed straight through,
 * so the archive is never held in memory.
 */
static bool build_staging_archive(const char *alpine_path, const char *bundle_path,
                                  const struct guest_pins *pins, const struct guest_bundle *bundle,
                                  const char *archive_path, struct staged_list *files, char *error,
                                  size_t error_size) {
    struct archive *out = archive_write_new();
    if (out == NULL) {
        set_error(error, error_size, "cannot create an archive writer");
        return false;
    }
    archive_write_set_format_pax(out);
    archive_write_add_filter_gzip(out);
    if (archive_write_open_filename(out, archive_path) != ARCHIVE_OK) {
        set_error(error, error_size, "cannot create the staging archive %s: %s", archive_path,
                  archive_error_string(out));
        archive_write_free(out);
        return false;
    }

    bool ok = false;
    struct archive *alpine = open_read_archive(alpine_path, error, error_size);
    if (alpine == NULL)
        goto done;

    struct archive_entry *entry;
    int result;
    while ((result = archive_read_next_header(alpine, &entry)) == ARCHIVE_OK) {
        const char *raw_path = archive_entry_pathname(entry);
        char guest_path[GUEST_MAX_PATH_FIELD];
        if (!normalize_archive_path(raw_path, guest_path, sizeof(guest_path))) {
            set_error(error, error_size, "the Alpine root contains an unusable path: %s", raw_path);
            goto done;
        }
        if (strcmp(guest_path, "/") == 0)
            continue; /* the root directory itself */

        unsigned int filetype = archive_entry_filetype(entry);
        unsigned int permissions = (unsigned int) (archive_entry_mode(entry) & 07777);
        time_t mtime = archive_entry_mtime(entry);

        if (filetype == AE_IFDIR) {
            if (!write_entry_header(out, guest_path, AE_IFDIR, permissions, 0, NULL, NULL, mtime,
                                    error, error_size))
                goto done;
            continue;
        }

        if (filetype == AE_IFLNK) {
            const char *target = archive_entry_symlink(entry);
            if (target == NULL) {
                set_error(error, error_size, "the Alpine root has a symlink without a target: %s",
                          guest_path);
                goto done;
            }
            if (!write_entry_header(out, guest_path, AE_IFLNK, 0777, 0, target, NULL, mtime, error,
                                    error_size))
                goto done;

            struct staged_file record;
            memset(&record, 0, sizeof(record));
            snprintf(record.path, sizeof(record.path), "%s", guest_path);
            record.symlink = true;
            record.size = strlen(target);
            sha256_hex(target, strlen(target), record.sha256);
            if (!staged_list_add(files, &record)) {
                set_error(error, error_size, "out of memory recording the staged files");
                goto done;
            }
            continue;
        }

        if (filetype == AE_IFREG) {
            const char *hardlink = archive_entry_hardlink(entry);
            if (hardlink != NULL) {
                char target_path[GUEST_MAX_PATH_FIELD];
                if (!normalize_archive_path(hardlink, target_path, sizeof(target_path))) {
                    set_error(error, error_size, "the Alpine root has an unusable hardlink target");
                    goto done;
                }
                const struct staged_file *source = staged_list_find(files, target_path);
                if (source == NULL) {
                    set_error(error, error_size, "the Alpine root hardlinks to the unknown path %s",
                              target_path);
                    goto done;
                }
                if (!write_entry_header(out, guest_path, AE_IFREG, permissions, source->size, NULL,
                                        target_path, mtime, error, error_size))
                    goto done;

                struct staged_file record = *source;
                snprintf(record.path, sizeof(record.path), "%s", guest_path);
                if (!staged_list_add(files, &record)) {
                    set_error(error, error_size, "out of memory recording the staged files");
                    goto done;
                }
                continue;
            }

            if (!write_entry_header(out, guest_path, AE_IFREG, permissions,
                                    (uint64_t) archive_entry_size(entry), NULL, NULL, mtime, error,
                                    error_size))
                goto done;

            struct staged_file record;
            memset(&record, 0, sizeof(record));
            snprintf(record.path, sizeof(record.path), "%s", guest_path);
            if (!copy_entry(alpine, out, &record.size, record.sha256, error, error_size))
                goto done;
            if (!staged_list_add(files, &record)) {
                set_error(error, error_size, "out of memory recording the staged files");
                goto done;
            }
            continue;
        }

        set_error(error, error_size, "the Alpine root contains an unsupported entry type: %s",
                  guest_path);
        goto done;
    }

    if (result != ARCHIVE_EOF) {
        set_error(error, error_size, "cannot read the Alpine root: %s", archive_error_string(alpine));
        goto done;
    }
    archive_read_free(alpine);
    alpine = NULL;

    /* The four guest programs, taken from the verified bundle. */
    struct archive *bundle_archive = open_read_archive(bundle_path, error, error_size);
    if (bundle_archive == NULL)
        goto done;

    while ((result = archive_read_next_header(bundle_archive, &entry)) == ARCHIVE_OK) {
        const char *raw_path = archive_entry_pathname(entry);
        const char *base = strrchr(raw_path, '/');
        base = base != NULL ? base + 1 : raw_path;

        int program = -1;
        for (int i = 0; i < GUEST_PROGRAM_COUNT; i++)
            if (strcmp(base, guest_program_names[i]) == 0)
                program = i;
        if (program < 0)
            continue;

        char guest_path[GUEST_MAX_PATH_FIELD];
        snprintf(guest_path, sizeof(guest_path), "%s/%s", GUEST_PROGRAMS_DIR, guest_program_names[program]);
        if (!write_entry_header(out, guest_path, AE_IFREG, 0755,
                                (uint64_t) archive_entry_size(entry), NULL, NULL,
                                archive_entry_mtime(entry), error, error_size))
            goto done;

        struct staged_file record;
        memset(&record, 0, sizeof(record));
        snprintf(record.path, sizeof(record.path), "%s", guest_path);
        if (!copy_entry(bundle_archive, out, &record.size, record.sha256, error, error_size))
            goto done;

        int index = bundle_file_index(guest_program_names[program]);
        if (strcmp(record.sha256, bundle->entries[index].sha256) != 0) {
            set_error(error, error_size, "%s changed while the staging archive was built",
                      guest_program_names[program]);
            goto done;
        }
        if (!staged_list_add(files, &record)) {
            set_error(error, error_size, "out of memory recording the staged files");
            goto done;
        }
    }
    if (result != ARCHIVE_EOF) {
        set_error(error, error_size, "cannot read the guest bundle: %s",
                  archive_error_string(bundle_archive));
        goto done;
    }
    archive_read_free(bundle_archive);
    bundle_archive = NULL;

    /*
     * The shell the guest program runs its commands with. The pinned Alpine
     * root has no bash and the guest program asks for one by name, so the
     * provisioner stages a shim that runs the shell the root does have.
     */
    if (!write_entry_header(out, GUEST_SHELL_SHIM_PATH, AE_IFREG, 0755,
                            (uint64_t) (sizeof(guest_shell_shim) - 1), NULL, NULL, 0, error,
                            error_size))
        goto done;
    if (archive_write_data(out, guest_shell_shim, sizeof(guest_shell_shim) - 1) < 0) {
        set_error(error, error_size, "cannot write the shell shim: %s", archive_error_string(out));
        goto done;
    }

    struct staged_file shim;
    memset(&shim, 0, sizeof(shim));
    snprintf(shim.path, sizeof(shim.path), "%s", GUEST_SHELL_SHIM_PATH);
    shim.size = sizeof(guest_shell_shim) - 1;
    sha256_hex(guest_shell_shim, sizeof(guest_shell_shim) - 1, shim.sha256);
    if (!staged_list_add(files, &shim)) {
        set_error(error, error_size, "out of memory recording the staged files");
        goto done;
    }

    /* The marker goes in last: it describes everything staged before it. */
    char *marker = NULL;
    size_t marker_length = 0;
    if (!build_marker(pins, files, &marker, &marker_length, error, error_size))
        goto done;

    if (!write_entry_header(out, GUEST_MARKER_PATH, AE_IFREG, 0644, marker_length, NULL, NULL, 0,
                            error, error_size)) {
        free(marker);
        goto done;
    }
    if (archive_write_data(out, marker, marker_length) < 0) {
        set_error(error, error_size, "cannot write the guest bundle marker: %s",
                  archive_error_string(out));
        free(marker);
        goto done;
    }
    free(marker);

    ok = true;

done:
    if (alpine != NULL)
        archive_read_free(alpine);
    if (archive_write_close(out) != ARCHIVE_OK && ok) {
        set_error(error, error_size, "cannot close the staging archive: %s", archive_error_string(out));
        ok = false;
    }
    archive_write_free(out);
    return ok;
}

/* ------------------------------------------------------------ verification */

static bool verify_file_digest(const char *path, const char *expected, char *error, size_t error_size) {
    char actual[SHA256_HEX_LEN + 1];
    if (!sha256_of_file(path, actual, error, error_size))
        return false;
    if (strcmp(actual, expected) != 0) {
        set_error(error, error_size, "%s has the digest %s, expected %s", path, actual, expected);
        return false;
    }
    return true;
}

/* Looks up a staged file record in the marker. */
static const struct json_value *marker_find_file(const struct json_value *marker, const char *path) {
    const struct json_value *files = json_member_get(marker, "files");
    if (files == NULL || files->type != JSON_ARRAY)
        return NULL;
    for (size_t i = 0; i < files->count; i++) {
        const char *candidate = json_as_string(json_member_get(files->items[i], "path"));
        if (candidate != NULL && strcmp(candidate, path) == 0)
            return files->items[i];
    }
    return NULL;
}

static bool marker_matches_pins(const struct json_value *marker, const struct guest_pins *pins,
                                char *error, size_t error_size) {
    double version = 0;
    if (!json_as_number(json_member_get(marker, "schema_version"), &version) || version != 1) {
        set_error(error, error_size, "the guest bundle marker has an unsupported schema version");
        return false;
    }

    const struct json_value *inputs = json_member_get(marker, "inputs");
    if (inputs == NULL || inputs->type != JSON_ARRAY || inputs->count != GUEST_INPUT_COUNT) {
        set_error(error, error_size, "the guest bundle marker does not record both pinned inputs");
        return false;
    }
    for (int i = 0; i < GUEST_INPUT_COUNT; i++) {
        const char *sha = json_as_string(json_member_get(inputs->items[i], "sha256"));
        if (sha == NULL || strcmp(sha, pins->inputs[i].sha256) != 0) {
            set_error(error, error_size, "the guest bundle marker records a different digest for %s",
                      pins->inputs[i].name);
            return false;
        }
    }

    const char *values[GUEST_COMPATIBILITY_FIELD_COUNT];
    compatibility_values(pins->compatibility, values);
    const struct json_value *object = json_member_get(marker, "compatibility");
    for (size_t i = 0; i < GUEST_COMPATIBILITY_FIELD_COUNT; i++) {
        const char *value = json_as_string(json_member_get(object, compatibility_keys[i]));
        if (value == NULL || strcmp(value, values[i]) != 0) {
            set_error(error, error_size, "the guest bundle marker records a different %s",
                      compatibility_keys[i]);
            return false;
        }
    }
    return true;
}

/*
 * Verifies a staged or active root: the fakefs database has to be there, the
 * four guest programs have to be there with the expected digests, and the
 * marker has to match the pins.
 */
static bool verify_root(const char *root, const struct guest_pins *pins,
                        const struct guest_bundle *bundle, char marker_sha[SHA256_HEX_LEN + 1],
                        char *error, size_t error_size) {
    char path[GUEST_PATH_MAX];

    snprintf(path, sizeof(path), "%s/meta.db", root);
    if (!path_exists(path)) {
        set_error(error, error_size, "the guest root has no fakefs database");
        return false;
    }

    snprintf(path, sizeof(path), "%s/data%s", root, GUEST_MARKER_PATH);
    if (!path_exists(path)) {
        set_error(error, error_size, "the guest root has no guest bundle marker");
        return false;
    }
    {
        char digest[SHA256_HEX_LEN + 1];
        if (!sha256_of_file(path, digest, error, error_size))
            return false;
        if (marker_sha != NULL)
            memcpy(marker_sha, digest, sizeof(digest));
    }

    /* The marker has to describe the pins this build was made for. */
    {
        char *text = NULL;
        size_t text_length = 0;
        if (!read_whole_file(path, &text, &text_length, error, error_size))
            return false;
        struct json_value *marker = json_parse(text, error, error_size);
        free(text);
        if (marker == NULL) {
            set_error(error, error_size, "the guest bundle marker of the guest root is not valid JSON");
            return false;
        }
        bool matches = marker_matches_pins(marker, pins, error, error_size);
        json_free(marker);
        if (!matches)
            return false;
    }

    for (int i = 0; i < GUEST_PROGRAM_COUNT; i++) {
        int index = bundle_file_index(guest_program_names[i]);
        snprintf(path, sizeof(path), "%s/data%s/%s", root, GUEST_PROGRAMS_DIR, guest_program_names[i]);
        if (!path_exists(path)) {
            set_error(error, error_size, "the guest root is missing %s", guest_program_names[i]);
            return false;
        }
        if (!verify_file_digest(path, bundle->entries[index].sha256, error, error_size))
            return false;
    }
    return true;
}

/* ------------------------------------------------------------- reuse check */

enum reuse_state { REUSE_ABSENT, REUSE_VALID, REUSE_INVALID };

/*
 * Checks whether the active root can be reused: the marker has to match the
 * pins and the four guest programs have to have the digests the marker and
 * the guest bundle manifest recorded.
 */
static enum reuse_state check_active_root(const char *active, const struct guest_pins *pins,
                                          const struct guest_bundle *bundle,
                                          char marker_sha[SHA256_HEX_LEN + 1], char *error,
                                          size_t error_size) {
    char path[GUEST_PATH_MAX];

    snprintf(path, sizeof(path), "%s/meta.db", active);
    if (!path_exists(path))
        return REUSE_ABSENT;

    snprintf(path, sizeof(path), "%s/data%s", active, GUEST_MARKER_PATH);
    char *text = NULL;
    size_t text_length = 0;
    if (!read_whole_file(path, &text, &text_length, error, error_size))
        return REUSE_INVALID;

    char digest[SHA256_HEX_LEN + 1];
    sha256_hex(text, text_length, digest);
    if (marker_sha != NULL)
        memcpy(marker_sha, digest, sizeof(digest));

    struct json_value *marker = json_parse(text, error, error_size);
    free(text);
    if (marker == NULL) {
        set_error(error, error_size, "the guest bundle marker of the active root is not valid JSON");
        return REUSE_INVALID;
    }

    bool ok = marker_matches_pins(marker, pins, error, error_size);
    if (ok) {
        for (int i = 0; i < GUEST_PROGRAM_COUNT; i++) {
            char guest_path[GUEST_MAX_PATH_FIELD];
            snprintf(guest_path, sizeof(guest_path), "%s/%s", GUEST_PROGRAMS_DIR, guest_program_names[i]);
            const struct json_value *record = marker_find_file(marker, guest_path);
            const char *sha = json_as_string(json_member_get(record, "sha256"));
            int index = bundle_file_index(guest_program_names[i]);
            if (sha == NULL || strcmp(sha, bundle->entries[index].sha256) != 0) {
                set_error(error, error_size,
                          "the active root records a different digest for %s", guest_program_names[i]);
                ok = false;
                break;
            }
        }
    }
    json_free(marker);
    if (!ok)
        return REUSE_INVALID;

    for (int i = 0; i < GUEST_PROGRAM_COUNT; i++) {
        char guest_path[GUEST_MAX_PATH_FIELD];
        snprintf(guest_path, sizeof(guest_path), "%s/data%s/%s", active, GUEST_PROGRAMS_DIR,
                 guest_program_names[i]);
        int index = bundle_file_index(guest_program_names[i]);
        if (!verify_file_digest(guest_path, bundle->entries[index].sha256, error, error_size))
            return REUSE_INVALID;
    }

    /* The shell shim is staged by this build, so its digest is known here. */
    snprintf(path, sizeof(path), "%s/data%s", active, GUEST_SHELL_SHIM_PATH);
    char shim_sha[SHA256_HEX_LEN + 1];
    sha256_hex(guest_shell_shim, sizeof(guest_shell_shim) - 1, shim_sha);
    if (!verify_file_digest(path, shim_sha, error, error_size))
        return REUSE_INVALID;

    return REUSE_VALID;
}

/* ------------------------------------------------------------ housekeeping */

/* Removes the staging directories and archives a previous run left behind. */
static void cleanup_stale_staging(const char *roots_dir) {
    DIR *dir = opendir(roots_dir);
    if (dir == NULL)
        return;

    struct dirent *entry;
    while ((entry = readdir(dir)) != NULL) {
        bool staging_dir = strncmp(entry->d_name, GUEST_STAGING_DIR_PREFIX,
                                   strlen(GUEST_STAGING_DIR_PREFIX)) == 0;
        bool staging_archive = strncmp(entry->d_name, GUEST_STAGING_ARCHIVE_PREFIX,
                                       strlen(GUEST_STAGING_ARCHIVE_PREFIX)) == 0;
        bool replaced_dir = strncmp(entry->d_name, GUEST_REPLACED_DIR_PREFIX,
                                    strlen(GUEST_REPLACED_DIR_PREFIX)) == 0;
        if (!staging_dir && !staging_archive && !replaced_dir)
            continue;

        char path[GUEST_PATH_MAX];
        if (snprintf(path, sizeof(path), "%s/%s", roots_dir, entry->d_name) >= (int) sizeof(path))
            continue;
        remove_tree(path);
    }
    closedir(dir);
}

/* Publishes the staging directory as the active root. */
static bool publish_active_root(const char *staging, const char *active, const char *roots_dir,
                                char *error, size_t error_size) {
    if (!path_exists(active))
        return rename(staging, active) == 0;

    char replaced[GUEST_PATH_MAX];
    char id[32];
    make_staging_id(id, sizeof(id));
    if (snprintf(replaced, sizeof(replaced), "%s/%s%s", roots_dir, GUEST_REPLACED_DIR_PREFIX, id) >=
        (int) sizeof(replaced)) {
        set_error(error, error_size, "the replaced root path is too long");
        return false;
    }

    if (rename(active, replaced) != 0) {
        set_error(error, error_size, "cannot move the previous guest root aside: %s", strerror(errno));
        return false;
    }
    if (rename(staging, active) != 0) {
        int saved = errno;
        rename(replaced, active);
        errno = saved;
        set_error(error, error_size, "cannot publish the guest root: %s", strerror(errno));
        return false;
    }
    remove_tree(replaced);
    return true;
}

/* ------------------------------------------------------------------ driver */

int guest_provision(const struct guest_pins *pins, const char *guest_bundle_path,
                    const char *alpine_root_path, const char *roots_dir,
                    const struct guest_provision_options *options,
                    struct guest_provision_result *result, char *error, size_t error_size) {
    struct guest_provision_options defaults = {false};
    if (options == NULL)
        options = &defaults;

    if (pins == NULL || pins->inputs == NULL || pins->compatibility == NULL || guest_bundle_path == NULL ||
        alpine_root_path == NULL || roots_dir == NULL || result == NULL) {
        set_error(error, error_size, "the provisioner was called with missing arguments");
        return GUEST_PROVISION_ERR_USAGE;
    }
    {
        const char *values[GUEST_COMPATIBILITY_FIELD_COUNT];
        compatibility_values(pins->compatibility, values);
        for (size_t i = 0; i < GUEST_COMPATIBILITY_FIELD_COUNT; i++) {
            if (values[i] == NULL) {
                set_error(error, error_size, "the pins do not declare %s", compatibility_keys[i]);
                return GUEST_PROVISION_ERR_USAGE;
            }
        }
    }

    memset(result, 0, sizeof(*result));

    char status_message[512];
    status_message[0] = '\0';
    if (error != NULL && error_size > 0)
        error[0] = '\0';

    /* 1. Both pinned inputs have to hash to the pinned digest. */
    for (int i = 0; i < GUEST_INPUT_COUNT; i++) {
        const char *path = i == GUEST_INPUT_LEG_BUNDLE ? guest_bundle_path : alpine_root_path;
        char digest[SHA256_HEX_LEN + 1];
        if (!sha256_of_file(path, digest, status_message, sizeof(status_message)))
            return GUEST_PROVISION_ERR_INPUT;
        if (strcmp(digest, pins->inputs[i].sha256) != 0) {
            set_error(error, error_size, "%s has the digest %s, expected %s", pins->inputs[i].name,
                      digest, pins->inputs[i].sha256);
            return GUEST_PROVISION_ERR_CHECKSUM;
        }
    }

    /* 2. The guest bundle has to be exactly what the manifest promises. */
    struct guest_bundle bundle;
    int status = GUEST_PROVISION_OK;
    if (!validate_guest_bundle(guest_bundle_path, pins->compatibility, &bundle, &status,
                               status_message, sizeof(status_message))) {
        guest_bundle_free(&bundle);
        set_error(error, error_size, "%s", status_message);
        return status;
    }

    /* 3. The Alpine root has to be a readable archive. */
    {
        struct archive *alpine = open_read_archive(alpine_root_path, status_message,
                                                   sizeof(status_message));
        if (alpine == NULL) {
            guest_bundle_free(&bundle);
            set_error(error, error_size, "%s", status_message);
            return GUEST_PROVISION_ERR_ARCHIVE;
        }
        struct archive_entry *entry;
        int result_code;
        while ((result_code = archive_read_next_header(alpine, &entry)) == ARCHIVE_OK) {
            const char *path = archive_entry_pathname(entry);
            char normalized[GUEST_MAX_PATH_FIELD];
            if (!normalize_archive_path(path, normalized, sizeof(normalized))) {
                set_error(error, error_size, "the Alpine root contains an unusable path: %s", path);
                archive_read_free(alpine);
                guest_bundle_free(&bundle);
                return GUEST_PROVISION_ERR_ARCHIVE;
            }
        }
        if (result_code != ARCHIVE_EOF) {
            set_error(error, error_size, "cannot read the Alpine root: %s",
                      archive_error_string(alpine));
            archive_read_free(alpine);
            guest_bundle_free(&bundle);
            return GUEST_PROVISION_ERR_ARCHIVE;
        }
        archive_read_free(alpine);
    }

    if (mkdir(roots_dir, 0755) != 0 && errno != EEXIST) {
        set_error(error, error_size, "cannot create the guest root directory %s: %s", roots_dir,
                  strerror(errno));
        guest_bundle_free(&bundle);
        return GUEST_PROVISION_ERR_IO;
    }

    /* 4. A previous run may have left staging state behind. */
    cleanup_stale_staging(roots_dir);

    char active[GUEST_PATH_MAX];
    snprintf(active, sizeof(active), "%s/%s", roots_dir, GUEST_ACTIVE_DIR_NAME);

    /* 5. An existing root is reused when the marker and the four guest
     * programs still verify against the pins. */
    char marker_sha[SHA256_HEX_LEN + 1] = {0};
    enum reuse_state reuse = check_active_root(active, pins, &bundle, marker_sha, status_message,
                                               sizeof(status_message));
    if (reuse == REUSE_VALID) {
        guest_bundle_free(&bundle);
        snprintf(result->active_root, sizeof(result->active_root), "%s", active);
        memcpy(result->marker_sha256, marker_sha, sizeof(marker_sha));
        result->reused = true;
        return GUEST_PROVISION_OK;
    }
    if (reuse == REUSE_INVALID) {
        /* The active root does not match the pins any more, so it is stale
         * state and has to go before a new root can be published. */
        remove_tree(active);
    }

    /* 6. Build the staging archive and import it. */
    char id[32];
    make_staging_id(id, sizeof(id));

    char staging[GUEST_PATH_MAX];
    char staging_archive[GUEST_PATH_MAX];
    if (snprintf(staging, sizeof(staging), "%s/%s%s", roots_dir, GUEST_STAGING_DIR_PREFIX, id) >=
            (int) sizeof(staging) ||
        snprintf(staging_archive, sizeof(staging_archive), "%s/%s%s.tar.gz", roots_dir,
                 GUEST_STAGING_ARCHIVE_PREFIX, id) >= (int) sizeof(staging_archive)) {
        set_error(error, error_size, "the guest root paths are too long");
        guest_bundle_free(&bundle);
        return GUEST_PROVISION_ERR_IO;
    }

    struct staged_list files;
    memset(&files, 0, sizeof(files));

    if (!build_staging_archive(alpine_root_path, guest_bundle_path, pins, &bundle, staging_archive,
                               &files, status_message, sizeof(status_message))) {
        set_error(error, error_size, "%s", status_message);
        remove_tree(staging_archive);
        staged_list_free(&files);
        guest_bundle_free(&bundle);
        return GUEST_PROVISION_ERR_ARCHIVE;
    }
    staged_list_free(&files);

    if (options->simulate_import_failure) {
        /* Truncate the staging archive so the importer fails after it has
         * already created the staging root. */
        int fd = open(staging_archive, O_WRONLY | O_TRUNC);
        if (fd < 0) {
            set_error(error, error_size, "cannot truncate the staging archive: %s", strerror(errno));
            remove_tree(staging_archive);
            guest_bundle_free(&bundle);
            return GUEST_PROVISION_ERR_IO;
        }
        close(fd);
    }

    struct fakefsify_error import_error;
    memset(&import_error, 0, sizeof(import_error));
    struct progress no_progress;
    memset(&no_progress, 0, sizeof(no_progress));

    if (!fakefs_import(staging_archive, staging, &import_error, no_progress)) {
        set_error(error, error_size, "the guest root import failed: %s",
                  import_error.message != NULL ? import_error.message : "unknown error");
        free(import_error.message);
        remove_tree(staging);
        remove_tree(staging_archive);
        guest_bundle_free(&bundle);
        return GUEST_PROVISION_ERR_IMPORT;
    }

    /* 7. The staged root has to verify before it is published. */
    char staged_marker_sha[SHA256_HEX_LEN + 1] = {0};
    if (!verify_root(staging, pins, &bundle, staged_marker_sha, status_message,
                     sizeof(status_message))) {
        set_error(error, error_size, "%s", status_message);
        remove_tree(staging);
        remove_tree(staging_archive);
        guest_bundle_free(&bundle);
        return GUEST_PROVISION_ERR_VERIFY;
    }

    /* 8. Publish the staged root. */
    if (!publish_active_root(staging, active, roots_dir, status_message, sizeof(status_message))) {
        set_error(error, error_size, "%s", status_message);
        remove_tree(staging);
        remove_tree(staging_archive);
        guest_bundle_free(&bundle);
        return GUEST_PROVISION_ERR_IO;
    }

    remove_tree(staging_archive);
    guest_bundle_free(&bundle);

    snprintf(result->active_root, sizeof(result->active_root), "%s", active);
    memcpy(result->marker_sha256, staged_marker_sha, sizeof(staged_marker_sha));
    result->reused = false;
    return GUEST_PROVISION_OK;
}
