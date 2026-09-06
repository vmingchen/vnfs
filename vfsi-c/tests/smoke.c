#include "vfsi.h"

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

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
    if (argc != 2) {
        fprintf(stderr, "usage: smoke <root>\n");
        return 2;
    }

    if (vfsi_abi_version() != VFSI_ABI_VERSION) {
        fprintf(stderr, "vfsi ABI mismatch: library=%u header=%u\n",
                vfsi_abi_version(), VFSI_ABI_VERSION);
        return 1;
    }

    struct vfsi_fs *fs = NULL;
    int rc = vfsi_dummy_open(argv[1], &fs);
    if (rc != 0 || fs == NULL) {
        fprintf(stderr, "dummy_open failed: %d\n", rc);
        return 1;
    }
    if (vfsi_nfs_minorversion(fs) != 0 || vfsi_smb_dialect(fs) != 0 ||
        vfsi_capabilities(fs) != 0) {
        fprintf(stderr, "dummy backend reported network capabilities\n");
        return 1;
    }

    rc = vfsi_mkdir(fs, "/d", 0755, 1);
    if (rc != 0) {
        fprintf(stderr, "mkdir failed: %d\n", rc);
        return 1;
    }

    int fd = vfsi_open(fs, "/d/f.bin", O_CREAT | O_RDWR | O_TRUNC, 0644);
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

    rc = vfsi_copy(fs, "/d/f.bin", 0, "/d/copied.bin", 0, 0, true);
    if (rc != 0) {
        fprintf(stderr, "copy failed: %d\n", rc);
        return 1;
    }

    fd = vfsi_open(fs, "/d/copied.bin", O_RDONLY, 0);
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
    if (vfsi_remove(fs, "/d/copied.bin") != 0)
        return 1;

    char *seen = NULL;
    rc = vfsi_listdir(fs, "/d", list_one, &seen);
    if (rc != 0 || seen == NULL || strcmp(seen, "f.bin") != 0) {
        fprintf(stderr, "listdir failed: rc=%d seen=%s\n", rc, seen ? seen : "(null)");
        return 1;
    }
    free(seen);
    vfsi_free(fs);
    puts("smoke ok");
    return 0;
}
