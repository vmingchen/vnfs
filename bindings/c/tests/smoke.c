#include "vfsi.h"

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static bool list_one(const char *name, const struct vfsi_attrs *attrs, void *userdata)
{
    (void)attrs;
    char **out = (char **)userdata;
    size_t len = strlen(name) + 1;
    *out = malloc(len);
    if (*out == NULL)
        return false;
    memcpy(*out, name, len);
    return true;
}

int main(int argc, char **argv)
{
    bool smb = (argc == 4 || argc == 7) && strcmp(argv[1], "--smb") == 0;
    bool nfs = argc == 4 && strcmp(argv[1], "--nfs") == 0;
    unsigned long nfs_minor = nfs ? strtoul(argv[3], NULL, 10) : 0;
    if ((argc != 2 && !smb && !nfs) || (nfs && nfs_minor != 1 && nfs_minor != 2)) {
        fprintf(stderr, "usage: smoke <root>\n"
                        "       smoke --nfs <server> <minor-version>\n"
                        "       smoke --smb <server> <share> [username password domain]\n");
        return 2;
    }

    if (vfsi_abi_version() != VFSI_ABI_VERSION) {
        fprintf(stderr, "vfsi ABI mismatch: library=%u header=%u\n",
                vfsi_abi_version(), VFSI_ABI_VERSION);
        return 1;
    }

    struct vfsi_fs *fs = NULL;
    int rc;
    const char *backend;
    if (smb) {
        backend = "SMB";
        rc = vfsi_smb_open(argv[2], argv[3], argc == 7 ? argv[4] : "",
                           argc == 7 ? argv[5] : "", argc == 7 ? argv[6] : "", &fs);
    } else if (nfs) {
        backend = "NFS";
        rc = vfsi_nfs_open_minor(argv[2], (uint32_t)nfs_minor, &fs);
    } else {
        backend = "dummy";
        rc = vfsi_dummy_open(argv[1], &fs);
    }
    if (rc != 0 || fs == NULL) {
        fprintf(stderr, "%s open failed: %d\n", backend, rc);
        return 1;
    }
    uint16_t dialect = vfsi_smb_dialect(fs);
    uint64_t capabilities = vfsi_capabilities(fs);
    uint64_t unix_capabilities = VFSI_CAP_POSIX_METADATA | VFSI_CAP_SYMLINKS |
                                 VFSI_CAP_HARDLINKS | VFSI_CAP_NON_UTF8_PATHS |
                                 VFSI_CAP_LSTAT;
    bool identity_ok = nfs
                           ? vfsi_nfs_minorversion(fs) == nfs_minor && dialect == 0 &&
                                 (capabilities & unix_capabilities) == unix_capabilities &&
                                 ((nfs_minor == 2) ==
                                  ((capabilities & VFSI_CAP_SERVER_COPY) != 0))
                       : smb ? vfsi_nfs_minorversion(fs) == 0 && dialect >= 0x0202 &&
                                   dialect <= 0x0311 &&
                                   (capabilities & unix_capabilities) == 0
                             : vfsi_nfs_minorversion(fs) == 0 && dialect == 0 &&
                                   (capabilities & unix_capabilities) == unix_capabilities &&
                                   (capabilities & VFSI_CAP_SERVER_COPY) == 0;
    if (!identity_ok) {
        fprintf(stderr, "backend capability report is inconsistent\n");
        return 1;
    }

    char dir[96];
    char source[128];
    char copied[128];
    snprintf(dir, sizeof(dir), "/vfsi-c-smoke-%ld", (long)getpid());
    snprintf(source, sizeof(source), "%s/f.bin", dir);
    snprintf(copied, sizeof(copied), "%s/copied.bin", dir);

    struct vfsi_mkdir_op mkdir_op = {.path = dir, .mode = 0755};
    struct vfsi_result item_result;
    struct vfsi_result batch = vfsi_mkdirv(fs, &mkdir_op, 1, &item_result);
    if (batch.category != VFSI_ERROR_NONE || batch.index != 1) {
        fprintf(stderr, "vector mkdir failed: category=%u status=%u index=%zu\n",
                batch.category, batch.err_no, batch.index);
        return 1;
    }

    int fd = vfsi_open(fs, source, O_CREAT | O_RDWR | O_TRUNC, 0644);
    if (fd < 0) {
        fprintf(stderr, "open failed: %d\n", fd);
        return 1;
    }
    size_t wrote = 0;
    rc = vfsi_pwrite(fs, fd, "hello", 5, 0, &wrote);
    if (rc != 0 || wrote != 5) {
        fprintf(stderr, "write failed: %d\n", rc);
        return 1;
    }
    rc = vfsi_close(fs, fd);
    if (rc != 0)
        return 1;

    rc = vfsi_copy(fs, source, 0, copied, 0, 0, true);
    if (rc != 0) {
        fprintf(stderr, "copy failed: %d\n", rc);
        return 1;
    }
    if (nfs && nfs_minor == 2 && getenv("VFSI_REQUIRE_SERVER_COPY") != NULL &&
        (vfsi_capabilities(fs) & VFSI_CAP_SERVER_COPY) == 0) {
        fprintf(stderr, "NFSv4.2 copy fell back to the client\n");
        return 1;
    }

    fd = vfsi_open(fs, copied, O_RDONLY, 0);
    if (fd < 0) {
        fprintf(stderr, "reopen failed: %d\n", fd);
        return 1;
    }
    char buf[8] = {0};
    size_t got = 0;
    rc = vfsi_pread(fs, fd, buf, sizeof(buf), 0, &got);
    if (rc != 0 || got != 5 || memcmp(buf, "hello", 5) != 0) {
        fprintf(stderr, "read failed: rc=%d got=%zu\n", rc, got);
        return 1;
    }
    vfsi_close(fs, fd);
    if (vfsi_remove(fs, copied) != 0)
        return 1;

    char *seen = NULL;
    rc = vfsi_listdir(fs, dir, list_one, &seen);
    if (rc != 0 || seen == NULL || strcmp(seen, "f.bin") != 0) {
        fprintf(stderr, "listdir failed: rc=%d seen=%s\n", rc, seen ? seen : "(null)");
        return 1;
    }
    free(seen);
    if (vfsi_remove(fs, source) != 0 || vfsi_remove(fs, dir) != 0)
        return 1;
    vfsi_free(fs);
    puts("smoke ok");
    return 0;
}
