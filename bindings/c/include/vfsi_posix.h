/* MIT OR Apache-2.0. Native POSIX projection helpers; not part of the C ABI.
 * Only fields requested in the supplying SDK operation are meaningful.
 * Missing fields cannot be inferred from ABI-v3 zero values; validate required
 * fields in that operation before using this projection.
 */
#ifndef VFSI_POSIX_H
#define VFSI_POSIX_H
#include <vfsi.h>
#include <errno.h>
#include <stdint.h>
#include <string.h>
#include <sys/stat.h>
#define VFSI_POSIX_FIELDS_VERSION 1

/* Populate native scalar fields, checking representation. st_dev is left zero:
 * callers must project a scoped object identity separately when they need it.
 * Synthetic VFSI device numbers must never be treated as kernel st_dev values.
 * Returns zero on success, an errno otherwise. On failure output is unspecified.
 */
static inline int vfsi_attrs_stat_fields(const vfsi_attrs *attrs, struct stat *out)
{
#if defined(__linux__) || defined(__APPLE__)
    mode_t type;
    if (!attrs || !out || attrs->abi_version != VFSI_ABI_VERSION ||
        attrs->struct_size < sizeof(*attrs))
        return EINVAL;
    if (attrs->size > INT64_MAX || attrs->blocks > INT64_MAX)
        return EOVERFLOW;
    if (attrs->atime_nsec >= 1000000000 || attrs->mtime_nsec >= 1000000000 ||
        attrs->ctime_nsec >= 1000000000)
        return EINVAL;
    /* ABI-v3 file type codes: regular=1, directory=2, symlink=5. */
    switch (attrs->ftype) {
    case 1: type = S_IFREG; break;
    case 2: type = S_IFDIR; break;
    case 5: type = S_IFLNK; break;
    default: return ENOTSUP;
    }
    memset(out, 0, sizeof(*out));
    out->st_mode = type | (attrs->mode & 07777);
    out->st_ino = attrs->fileid;
    out->st_nlink = attrs->nlink;
    out->st_uid = attrs->uid;
    out->st_gid = attrs->gid;
    out->st_size = attrs->size;
    out->st_blocks = attrs->blocks;
    out->st_atime = attrs->atime_sec;
    out->st_mtime = attrs->mtime_sec;
    out->st_ctime = attrs->ctime_sec;
    if ((uintmax_t)out->st_ino != attrs->fileid ||
        (uintmax_t)out->st_nlink != attrs->nlink ||
        (uintmax_t)out->st_uid != attrs->uid || (uintmax_t)out->st_gid != attrs->gid ||
        (uintmax_t)out->st_size != attrs->size || (uintmax_t)out->st_blocks != attrs->blocks ||
        (intmax_t)out->st_atime != attrs->atime_sec ||
        (intmax_t)out->st_mtime != attrs->mtime_sec ||
        (intmax_t)out->st_ctime != attrs->ctime_sec)
        return EOVERFLOW;
#if defined(__APPLE__) && defined(_POSIX_C_SOURCE) && !defined(_DARWIN_C_SOURCE)
    out->st_atimensec = attrs->atime_nsec;
    out->st_mtimensec = attrs->mtime_nsec;
    out->st_ctimensec = attrs->ctime_nsec;
#elif defined(__APPLE__)
    out->st_atimespec.tv_nsec = attrs->atime_nsec;
    out->st_mtimespec.tv_nsec = attrs->mtime_nsec;
    out->st_ctimespec.tv_nsec = attrs->ctime_nsec;
#else
    out->st_atim.tv_nsec = attrs->atime_nsec;
    out->st_mtim.tv_nsec = attrs->mtime_nsec;
    out->st_ctim.tv_nsec = attrs->ctime_nsec;
#endif
    return 0;
#else
    (void)attrs; (void)out;
    return ENOTSUP;
#endif
}
#endif
