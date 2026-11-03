/* Frozen vfsi-c 0.3.11 ABI-v3 declarations. Deliberately do not include the
 * current header: a freshly generated header would hide old-client breaks. */
#include <assert.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
struct vfsi_fs;
struct vfsi_attrs {
    uint32_t struct_size, abi_version, ftype, mode;
    uint64_t size;
    uint32_t nlink;
    uint64_t fileid;
    uint32_t uid, gid;
    uint64_t blocks;
    int64_t atime_sec;
    uint32_t atime_nsec;
    int64_t mtime_sec;
    uint32_t mtime_nsec;
    int64_t ctime_sec;
    uint32_t ctime_nsec;
};
extern uint32_t vfsi_abi_version(void);
extern int vfsi_dummy_open(const char *, struct vfsi_fs **);
extern int vfsi_stat(struct vfsi_fs *, const char *, struct vfsi_attrs *);
extern void vfsi_free(struct vfsi_fs *);
#if UINTPTR_MAX == UINT64_MAX
_Static_assert(sizeof(struct vfsi_attrs) == 104, "frozen ABI-v3 size");
_Static_assert(offsetof(struct vfsi_attrs, fileid) == 32, "frozen ABI-v3 offset");
#endif
int main(int argc, char **argv)
{
    assert(argc == 2 && vfsi_abi_version() == 3);
    struct vfsi_fs *fs = NULL;
    assert(vfsi_dummy_open(argv[1], &fs) == 0);
    struct { uint64_t before[8]; struct vfsi_attrs attrs; uint64_t after[8]; } output;
    memset(&output, 0xa5, sizeof(output));
    assert(vfsi_stat(fs, "/input", &output.attrs) == 0);
    assert(output.attrs.struct_size == sizeof(struct vfsi_attrs));
    assert(output.attrs.abi_version == 3 && output.attrs.size == 3);
    for (size_t i = 0; i < 8; ++i) {
        assert(output.before[i] == UINT64_C(0xa5a5a5a5a5a5a5a5));
        assert(output.after[i] == UINT64_C(0xa5a5a5a5a5a5a5a5));
    }
    vfsi_free(fs);
    return 0;
}
