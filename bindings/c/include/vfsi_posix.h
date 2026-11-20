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
#if defined(__linux__) || defined(__APPLE__)
#if defined(__APPLE__) && defined(_POSIX_C_SOURCE) && !defined(_DARWIN_C_SOURCE)
#define VFSI_STAT_ATIME_NSEC st_atimensec
#define VFSI_STAT_MTIME_NSEC st_mtimensec
#define VFSI_STAT_CTIME_NSEC st_ctimensec
#elif defined(__APPLE__)
#define VFSI_STAT_ATIME_NSEC st_atimespec.tv_nsec
#define VFSI_STAT_MTIME_NSEC st_mtimespec.tv_nsec
#define VFSI_STAT_CTIME_NSEC st_ctimespec.tv_nsec
#else
#define VFSI_STAT_ATIME_NSEC st_atim.tv_nsec
#define VFSI_STAT_MTIME_NSEC st_mtim.tv_nsec
#define VFSI_STAT_CTIME_NSEC st_ctim.tv_nsec
#endif

/* Use the destination's actual field widths for both POSIX layouts. */
#define VFSI_DEFINE_STAT_FIELDS(function, stat_type)                             \
static inline int function(const vfsi_attrs *attrs, stat_type *out)              \
{                                                                                \
    mode_t type;                                                                 \
    if (!attrs || !out || attrs->abi_version != VFSI_ABI_VERSION ||              \
        attrs->struct_size < sizeof(*attrs))                                     \
        return EINVAL;                                                           \
    if (attrs->size > INT64_MAX || attrs->blocks > INT64_MAX)                    \
        return EOVERFLOW;                                                        \
    if (attrs->atime_nsec >= 1000000000 || attrs->mtime_nsec >= 1000000000 ||    \
        attrs->ctime_nsec >= 1000000000)                                         \
        return EINVAL;                                                           \
    /* ABI-v3 file type codes: regular=1, directory=2, symlink=5. */             \
    switch (attrs->ftype) {                                                      \
    case 1: type = S_IFREG; break;                                               \
    case 2: type = S_IFDIR; break;                                               \
    case 5: type = S_IFLNK; break;                                               \
    default: return ENOTSUP;                                                     \
    }                                                                            \
    memset(out, 0, sizeof(*out));                                                \
    out->st_mode = type | (attrs->mode & 07777);                                 \
    out->st_ino = attrs->fileid;                                                 \
    out->st_nlink = attrs->nlink;                                                \
    out->st_uid = attrs->uid;                                                    \
    out->st_gid = attrs->gid;                                                    \
    out->st_size = attrs->size;                                                  \
    out->st_blocks = attrs->blocks;                                              \
    out->st_atime = attrs->atime_sec;                                            \
    out->st_mtime = attrs->mtime_sec;                                            \
    out->st_ctime = attrs->ctime_sec;                                            \
    if ((uintmax_t)out->st_ino != attrs->fileid ||                               \
        (uintmax_t)out->st_nlink != attrs->nlink ||                              \
        (uintmax_t)out->st_uid != attrs->uid || (uintmax_t)out->st_gid != attrs->gid ||\
        (uintmax_t)out->st_size != attrs->size || (uintmax_t)out->st_blocks != attrs->blocks ||\
        (intmax_t)out->st_atime != attrs->atime_sec ||                           \
        (intmax_t)out->st_mtime != attrs->mtime_sec ||                           \
        (intmax_t)out->st_ctime != attrs->ctime_sec)                             \
        return EOVERFLOW;                                                        \
    out->VFSI_STAT_ATIME_NSEC = attrs->atime_nsec;                               \
    out->VFSI_STAT_MTIME_NSEC = attrs->mtime_nsec;                               \
    out->VFSI_STAT_CTIME_NSEC = attrs->ctime_nsec;                               \
    return 0;                                                                    \
}

VFSI_DEFINE_STAT_FIELDS(vfsi_attrs_stat_fields, struct stat)

/* Optional helper for consumers that use glibc's explicit large-file layout. */
#if defined(__GLIBC__) && defined(__USE_LARGEFILE64)
#define VFSI_POSIX_STAT64_FIELDS 1
VFSI_DEFINE_STAT_FIELDS(vfsi_attrs_stat64_fields, struct stat64)
#endif
#undef VFSI_DEFINE_STAT_FIELDS
#undef VFSI_STAT_ATIME_NSEC
#undef VFSI_STAT_MTIME_NSEC
#undef VFSI_STAT_CTIME_NSEC
#else
static inline int vfsi_attrs_stat_fields(const vfsi_attrs *attrs, struct stat *out)
{
    (void)attrs; (void)out;
    return ENOTSUP;
}
#endif
#endif
