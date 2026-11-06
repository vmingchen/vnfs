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
    attrs.mtime_sec = 1600000001;
    attrs.mtime_nsec = 123456789;
    attrs.ctime_sec = 1600000002;
    attrs.ctime_nsec = 987654321;
    assert(vfsi_attrs_stat_fields(&attrs, &st) == 0);
    assert(S_ISREG(st.st_mode) && (st.st_mode & 07777) == 0640);
    assert(st.st_ino == 17 && st.st_nlink == 2 && st.st_dev == 0);
    assert(st.st_uid == 0 && st.st_size == 0); /* valid zero values */
    assert(st.st_atime == -1);
    assert(st.st_mtime == 1600000001 && st.st_ctime == 1600000002);
#if defined(__APPLE__) && defined(_POSIX_C_SOURCE) && !defined(_DARWIN_C_SOURCE)
    assert(st.st_atimensec == 999999999);
    assert(st.st_mtimensec == 123456789 && st.st_ctimensec == 987654321);
#elif defined(__APPLE__)
    assert(st.st_atimespec.tv_nsec == 999999999);
    assert(st.st_mtimespec.tv_nsec == 123456789 && st.st_ctimespec.tv_nsec == 987654321);
#else
    assert(st.st_atim.tv_nsec == 999999999);
    assert(st.st_mtim.tv_nsec == 123456789 && st.st_ctim.tv_nsec == 987654321);
#endif
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
