/* Standalone ABI-v3 consumer: link only libdl, never libvfsi_c.
 * Resolve both optional functions BEFORE choosing any I/O path. */
#include "vfsi.h"
#include "vfsi_dynamic.h"

#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#ifdef NDEBUG
#error "Conformance assertions must be enabled"
#endif

struct operations {
    int (*dummy_open)(const char *, struct vfsi_fs **);
    int (*nfs_open_minor)(const char *, uint32_t, struct vfsi_fs **);
    int (*open)(struct vfsi_fs *, const char *, int, uint32_t);
    int (*close)(struct vfsi_fs *, int);
    int (*remove)(struct vfsi_fs *, const char *);
    struct vfsi_result (*preadv)(struct vfsi_fs *, struct vfsi_pread_op *, size_t, struct vfsi_result *);
    struct vfsi_result (*pwritev)(struct vfsi_fs *, struct vfsi_pwrite_op *, size_t, struct vfsi_result *);
};

/* Catch a legacy signature drift without referring to a linked symbol. */
#if defined(__GNUC__) || defined(__clang__)
_Static_assert(__builtin_types_compatible_p(
    __typeof__(((struct operations *)0)->preadv), __typeof__(&vfsi_preadv)),
    "legacy preadv declaration differs from packaged header");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(((struct operations *)0)->pwritev), __typeof__(&vfsi_pwritev)),
    "legacy pwritev declaration differs from packaged header");
#endif

#define RESOLVE(name) assert(vfsi_dynamic_resolve(handle, "vfsi_" #name, \
                                &ops.name, sizeof(ops.name)) == 0)

static void accepted(struct vfsi_result result, size_t count)
{
    assert(result.struct_size == sizeof(result) && result.abi_version == VFSI_ABI_VERSION);
    assert(result.category == VFSI_ERROR_NONE && result.index == count);
}

static void item(struct vfsi_io_result result, size_t index, uint32_t state)
{
    assert(result.status.struct_size == sizeof(result.status));
    assert(result.status.abi_version == VFSI_ABI_VERSION);
    assert(result.status.index == index);
    if (result.failure_state != state)
        fprintf(stderr, "item %zu: state=%u expected=%u category=%u status=%u\n",
                index, result.failure_state, state, result.status.category, result.status.err_no);
    assert(result.failure_state == state);
    assert((result.status.category == VFSI_ERROR_NONE) == (state == VFSI_IO_SUCCESS));
    if (state == VFSI_IO_SUCCESS)
        assert(result.offset_known == 1);
}

/* The fallback choice is fixed before OPEN or any read/write.
 * Legacy operations remain scalar here: their first-error vector contract does
 * not promise independent outcomes. Do not synthesize extended results. */
static void legacy(struct operations *ops, struct vfsi_fs *fs, const char *path)
{
    int fd = ops->open(fs, path, O_CREAT | O_EXCL | O_RDWR, 0600);
    assert(fd >= 0);
    struct vfsi_pwrite_op write = {fd, "abc", 3, 0, 0};
    struct vfsi_result outcome;
    accepted(ops->pwritev(fs, &write, 1, &outcome), 1);
    assert(outcome.category == VFSI_ERROR_NONE);
    assert(write.wrote == 3);
    char buffer[8] = {0};
    struct vfsi_pread_op read = {fd, buffer, sizeof(buffer), 0, 0};
    accepted(ops->preadv(fs, &read, 1, &outcome), 1);
    assert(outcome.category == VFSI_ERROR_NONE);
    assert(read.got == 3 && memcmp(buffer, "abc", 3) == 0);
    assert(ops->close(fs, fd) == 0);
    assert(ops->close(fs, fd) == EBADF);
    assert(ops->remove(fs, path) == 0);
}

static void posix(const char *root, const char *name)
{
    char path[4096];
    assert(snprintf(path, sizeof(path), "%s%s", root, name) < (int)sizeof(path));
    int fd = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
    assert(fd >= 0 && pwrite(fd, "abc", 3, 0) == 3);
    char bytes[8] = {0};
    assert(pread(fd, bytes, sizeof(bytes), 0) == 3);
    assert(memcmp(bytes, "abc", 3) == 0);
    assert(close(fd) == 0 && unlink(path) == 0);
}

static void extended(vfsi_dynamic_api *api, struct operations *ops,
                     struct vfsi_fs *fs, const char *path)
{
    assert(ops->open(fs, path, O_RDONLY, 0) < 0);
    int fd = ops->open(fs, path, O_CREAT | O_EXCL | O_RDWR, 0600);
    assert(fd >= 0);
    int retired = ops->open(fs, path, O_RDONLY, 0);
    assert(retired >= 0 && ops->close(fs, retired) == 0);
    int readonly = ops->open(fs, path, O_RDONLY, 0);
    assert(readonly >= 0);
    struct vfsi_pwrite_op writes[] = {
        {fd, "abc", 3, 0, 99},
        {-1, "bad", 3, 0, 99},
        {retired, "bad", 3, 0, 99},
        {fd, NULL, 0, 0, 99},
        {readonly, "X", 1, 0, 99},
    };
    struct vfsi_io_result results[5];
    memset(results, 0xa5, sizeof(results));
    accepted(api->pwritev_ex(fs, writes, 5, results), 5);
    item(results[4], 4, VFSI_IO_FAILED);
    assert(writes[4].wrote == 0 && results[4].status.err_no != 0);
    assert(!results[4].offset_known);
    assert(ops->close(fs, readonly) == 0 && ops->close(fs, readonly) == EBADF);
    for (size_t i = 0; i < 4; ++i) {
        item(results[i], i, i == 1 || i == 2 ? VFSI_IO_NOT_ATTEMPTED : VFSI_IO_SUCCESS);
        assert(writes[i].wrote == (i == 0 ? 3 : 0));
        if (i == 1 || i == 2) {
            assert(results[i].status.err_no == EBADF);
            assert(!results[i].offset_known);
        } else {
            assert(results[i].offset == 0);
        }
    }
    char first[8], missing[8], closed[8], eof[8];
    memset(missing, 0x5a, sizeof(missing));
    memset(closed, 0x5a, sizeof(closed));
    struct vfsi_pread_op reads[] = {
        {fd, first, sizeof(first), 0, 99},
        {-1, missing, sizeof(missing), 0, 99},
        {retired, closed, sizeof(closed), 0, 99},
        {fd, eof, sizeof(eof), 100, 99},
    };
    accepted(api->preadv_ex(fs, reads, 4, results), 4);
    for (size_t i = 0; i < 4; ++i) {
        item(results[i], i, i == 1 || i == 2 ? VFSI_IO_NOT_ATTEMPTED : VFSI_IO_SUCCESS);
        assert(reads[i].got == (i == 0 ? 3 : 0));
        if (i == 1 || i == 2) {
            assert(results[i].status.err_no == EBADF);
            assert(!results[i].offset_known);
            for (size_t j = 0; j < sizeof(missing); ++j)
                assert((i == 1 ? missing[j] : closed[j]) == 0x5a);
        } else {
            assert(results[i].offset == reads[i].offset);
            if (i == 3) assert(results[i].eof);
        }
    }
    assert(memcmp(first, "abc", 3) == 0);

    int append = ops->open(fs, path, O_WRONLY | O_APPEND, 0);
    assert(append >= 0);
    struct vfsi_pwrite_op appended = {append, "de", 2, UINT64_MAX, 99};
    accepted(api->pwritev_ex(fs, &appended, 1, results), 1);
    item(results[0], 0, VFSI_IO_SUCCESS);
    assert(appended.wrote == 2 && results[0].offset == 3);
    assert(ops->close(fs, append) == 0);
    reads[0].len = 5;
    accepted(api->preadv_ex(fs, reads, 1, results), 1);
    assert(reads[0].got == 5 && memcmp(first, "abcde", 5) == 0);

    /* A malformed batch is rejected before modifying its first valid input. */
    struct vfsi_pwrite_op rejected[] = {{fd, "X", 1, 0, 99}, {fd, NULL, 1, 0, 99}};
    struct vfsi_result batch = api->pwritev_ex(fs, rejected, 2, results);
    assert(batch.category == VFSI_ERROR_INVALID_ARGUMENT && batch.index == 1);
    for (size_t i = 0; i < 2; ++i) {
        item(results[i], i, VFSI_IO_NOT_ATTEMPTED);
        assert(rejected[i].wrote == 0);
    }
    accepted(api->preadv_ex(fs, reads, 1, results), 1);
    assert(reads[0].got == 5 && memcmp(first, "abcde", 5) == 0);

    assert(ops->close(fs, fd) == 0 && ops->close(fs, fd) == EBADF);
    accepted(api->preadv_ex(fs, reads, 1, results), 1);
    item(results[0], 0, VFSI_IO_NOT_ATTEMPTED);
    assert(reads[0].got == 0 && results[0].status.err_no == EBADF);
    assert(ops->remove(fs, path) == 0);
}

/* The external proxy arms at this boundary, after all OPEN traffic. */
static void fault(vfsi_dynamic_api *api, struct operations *ops,
                  struct vfsi_fs *fs, const char *path, const char *mode)
{
    int fd = ops->open(fs, path, O_CREAT | O_EXCL | O_RDWR, 0600);
    assert(fd >= 0);
    puts("READY");
    fflush(stdout);
    assert(getchar() == '\n');
    int later = strcmp(mode, "later") == 0 || strcmp(mode, "uncached-later") == 0;
    int independent = strcmp(mode, "uncached-later") == 0;
    size_t length = later ? 16 * 1024 * 1024 : 16;
    char *payload = malloc(length);
    assert(payload);
    if (later) memset(payload, 'a', length);
    else memcpy(payload, "abcdefghijklmnop", length);
    struct vfsi_pwrite_op writes[] = {
        {fd, payload, length, 0, 99},
        {later ? fd : -1, "b", 1, 32 * 1024 * 1024, 99},
        {-1, "bad", 3, 0, 99},
    };
    size_t count = later ? 3 : 2;
    struct vfsi_io_result results[3];
    accepted(api->pwritev_ex(fs, writes, count, results), count);
    free(payload);
    int short_only = strcmp(mode, "short") == 0;
    int prefix = strcmp(mode, "prefix") == 0;
    item(results[0], 0, short_only ? VFSI_IO_SUCCESS : prefix ? VFSI_IO_FAILED : VFSI_IO_UNKNOWN);
    assert(writes[0].wrote == (short_only ? 16 : prefix ? 4 : 0));
    assert(results[0].offset_known == short_only);
    if (short_only) assert(results[0].offset == 0);
    if (prefix) assert(results[0].status.err_no == 28); /* native NFS4ERR_NOSPC */
    if (strncmp(mode, "uncached", 8) == 0) {
        assert(results[0].status.err_no == 10068);
        assert(results[0].status.category == VFSI_ERROR_FILESYSTEM);
    }
    if (strcmp(mode, "lost") == 0) assert(results[0].status.category == VFSI_ERROR_TRANSPORT);
    item(results[1], 1, independent ? VFSI_IO_SUCCESS : VFSI_IO_NOT_ATTEMPTED);
    assert(writes[1].wrote == (independent ? 1 : 0));
    if (independent) {
        assert(results[1].offset_known && results[1].offset == 32 * 1024 * 1024);
    } else {
        assert(!results[1].offset_known);
        if (later) {
            assert(results[0].status.category == VFSI_ERROR_TRANSPORT);
            assert(results[1].status.category == results[0].status.category);
            assert(results[1].status.err_no == results[0].status.err_no);
        } else {
            assert(results[1].status.err_no == EBADF);
        }
    }
    if (later) {
        item(results[2], 2, VFSI_IO_NOT_ATTEMPTED);
        assert(writes[2].wrote == 0 && results[2].status.err_no == EBADF);
        assert(!results[2].offset_known);
    }

    static const char *states[] = {"success", "not_attempted", "failed", "unknown"};
    printf("OUTCOMES");
    for (size_t i = 0; i < count; ++i) {
        assert(results[i].failure_state < sizeof(states) / sizeof(states[0]));
        printf(" %s:%zu", states[results[i].failure_state], writes[i].wrote);
    }
    puts("");

    /* Never replay or fall back here, even when confirmed bytes are zero.
     * The old session owns cleanup of uncertain remote state. */
    api->free(fs);
    const char *server = getenv("VFSI_OUTCOME_VERIFY_SERVER");
    const char *minor_text = getenv("VFSI_OUTCOME_MINOR");
    assert(server && minor_text);
    struct vfsi_fs *fresh = NULL;
    assert(ops->nfs_open_minor(server, (uint32_t)strtoul(minor_text, NULL, 10), &fresh) == 0);
    int reader = ops->open(fresh, path, O_RDONLY, 0);
    assert(reader >= 0);
    char bytes[32] = {0};
    struct vfsi_pread_op read = {reader, bytes, sizeof(bytes), 0, 99};
    accepted(api->preadv_ex(fresh, &read, 1, results), 1);
    item(results[0], 0, VFSI_IO_SUCCESS);
    assert(read.got == (later ? sizeof(bytes) : prefix ? 4 : 16));
    if (later) {
        for (size_t i = 0; i < read.got; ++i) assert(bytes[i] == 'a');
    } else {
        assert(memcmp(bytes, "abcdefghijklmnop", read.got) == 0);
    }
    if (independent) {
        read.offset = 32 * 1024 * 1024;
        read.len = 1;
        accepted(api->preadv_ex(fresh, &read, 1, results), 1);
        assert(read.got == 1 && bytes[0] == 'b');
    }
    assert(ops->close(fresh, reader) == 0 && ops->close(fresh, reader) == EBADF);
    assert(ops->remove(fresh, path) == 0);
    api->free(fresh);
}

int main(int argc, char **argv)
{
    if (argc < 4 || argc > 5) {
        fprintf(stderr, "usage: %s LIB local|legacy|posix ROOT\n"
                        "       %s LIB nfs HOST MINOR\n", argv[0], argv[0]);
        return 2;
    }
    void *handle = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (!handle) {
        if (strcmp(argv[2], "posix") == 0 && argc == 4) {
            char name[128];
            assert(snprintf(name, sizeof(name), "/vfsi-outcomes-%ld", (long)getpid()) < (int)sizeof(name));
            posix(argv[3], name);
            puts("C outcome consumer: PASS");
            return 0;
        }
        fprintf(stderr, "%s\n", dlerror());
        return 1;
    }
    vfsi_dynamic_api api;
    assert(vfsi_dynamic_load(&api, handle) == 0);
    /* Decide once. An error after this point MUST NOT trigger fallback. */
    int available = api.preadv_ex != NULL && api.pwritev_ex != NULL;
    int old = strcmp(argv[2], "legacy") == 0;
    int native = strcmp(argv[2], "posix") == 0;
    int nfs = strcmp(argv[2], "nfs") == 0;
    assert(available == !(old || native));
    char path[128];
    assert(snprintf(path, sizeof(path), "/vfsi-outcomes-%ld", (long)getpid()) < (int)sizeof(path));
    if (native) {
        posix(argv[3], path);
    } else {
        struct operations ops;
        RESOLVE(dummy_open); RESOLVE(nfs_open_minor); RESOLVE(open);
        RESOLVE(close); RESOLVE(remove); RESOLVE(preadv); RESOLVE(pwritev);
        struct vfsi_fs *fs = NULL;
        if (nfs) {
            assert(argc == 5);
            unsigned minor = (unsigned)strtoul(argv[4], NULL, 10);
            assert(minor == 1 || minor == 2);
            assert(ops.nfs_open_minor(argv[3], minor, &fs) == 0);
        } else {
            assert(argc == 4 && (old || strcmp(argv[2], "local") == 0));
            assert(ops.dummy_open(argv[3], &fs) == 0);
        }
        assert(fs);
        const char *mode = getenv("VFSI_OUTCOME_FAULT");
        if (mode) {
            assert(nfs && available);
            fault(&api, &ops, fs, path, mode);
        } else {
            if (available) extended(&api, &ops, fs, path);
            else legacy(&ops, fs, path);
            api.free(fs);
        }
    }
    assert(dlclose(handle) == 0);
    puts("C outcome consumer: PASS");
    return 0;
}
