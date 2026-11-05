#define _DEFAULT_SOURCE 1
#define _POSIX_C_SOURCE 200809L
#include <vfsi_posix.h>
#include <assert.h>
#include <limits.h>

int main(void)
{
    vfsi_attrs attrs = {0};
    struct stat st;
    attrs.abi_version = VFSI_ABI_VERSION;
    attrs.struct_size = sizeof(attrs);
    attrs.ftype = 1;
    attrs.mode = 0640;
    attrs.nlink = 2;
    attrs.fileid = 17;
    attrs.atime_sec = -1;
    attrs.atime_nsec = 999999999;
    assert(vfsi_attrs_stat_fields(&attrs, &st) == 0);
    assert(S_ISREG(st.st_mode) && (st.st_mode & 07777) == 0640);
    assert(st.st_ino == 17 && st.st_nlink == 2 && st.st_dev == 0);
    assert(st.st_uid == 0 && st.st_size == 0); /* valid zero values */
    assert(st.st_atime == -1);
    attrs.mtime_nsec = 1000000000;
    assert(vfsi_attrs_stat_fields(&attrs, &st) == EINVAL);
    attrs.mtime_nsec = 0;
    attrs.size = UINT64_MAX;
    assert(vfsi_attrs_stat_fields(&attrs, &st) == EOVERFLOW);
    attrs.size = 0;
    attrs.ftype = 99;
    assert(vfsi_attrs_stat_fields(&attrs, &st) == ENOTSUP);
    attrs.ftype = 2;
    assert(vfsi_attrs_stat_fields(&attrs, &st) == 0 && S_ISDIR(st.st_mode));
    attrs.ftype = 5;
    assert(vfsi_attrs_stat_fields(&attrs, &st) == 0 && S_ISLNK(st.st_mode));
    attrs.struct_size = 0;
    assert(vfsi_attrs_stat_fields(&attrs, &st) == EINVAL);
    return 0;
}
