#ifndef VFSI_H
#define VFSI_H

#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

/**
 * ABI version implemented by this library.
 */
#define VFSI_ABI_VERSION 3

/**
 * No error occurred.
 */
#define VFSI_ERROR_NONE 0

/**
 * A filesystem/backend status was returned.
 */
#define VFSI_ERROR_FILESYSTEM 1

/**
 * The transport failed without a filesystem status.
 */
#define VFSI_ERROR_TRANSPORT 2

/**
 * The selected backend does not implement the requested operation.
 */
#define VFSI_ERROR_UNSUPPORTED 3

/**
 * The C request itself was malformed.
 */
#define VFSI_ERROR_INVALID_ARGUMENT 4

/**
 * The operation was not submitted because an earlier request was invalid.
 */
#define VFSI_ERROR_NOT_ATTEMPTED 5

/**
 * The backend batch failed and this element's final state cannot be proven.
 */
#define VFSI_ERROR_INDETERMINATE 6

/**
 * Fixed capacity of [`vfsi_result::message`], including its trailing NUL.
 */
#define VFSI_RESULT_MESSAGE_SIZE 160

#define VFSI_ATTR_MODE (1 << 0)

#define VFSI_ATTR_SIZE (1 << 1)

#define VFSI_ATTR_NLINK (1 << 2)

#define VFSI_ATTR_FILEID (1 << 3)

#define VFSI_ATTR_BLOCKS (1 << 4)

#define VFSI_ATTR_UID (1 << 5)

#define VFSI_ATTR_GID (1 << 6)

#define VFSI_ATTR_RDEV (1 << 7)

#define VFSI_ATTR_ATIME (1 << 8)

#define VFSI_ATTR_MTIME (1 << 9)

#define VFSI_ATTR_CTIME (1 << 10)

/**
 * The backend will currently attempt server-side COPY.
 */
#define VFSI_CAP_SERVER_COPY (1 << 0)

/**
 * The backend reports and honors Unix metadata such as modes and ownership.
 */
#define VFSI_CAP_POSIX_METADATA (1 << 1)

/**
 * The backend supports symbolic links.
 */
#define VFSI_CAP_SYMLINKS (1 << 2)

/**
 * The backend supports hard links.
 */
#define VFSI_CAP_HARDLINKS (1 << 3)

/**
 * The backend accepts arbitrary non-UTF-8 Unix path bytes.
 */
#define VFSI_CAP_NON_UTF8_PATHS (1 << 4)

/**
 * The backend implements no-follow metadata operations.
 */
#define VFSI_CAP_LSTAT (1 << 5)

/**
 * Opaque filesystem handle owned by C.
 */
typedef struct vfsi_fs vfsi_fs;

/**
 * Attributes returned by [`vfsi_stat`] and passed to listdir callbacks.
 */
typedef struct vfsi_attrs {
  /**
   * Size of this structure, for forward-compatible extension.
   */
  uint32_t struct_size;
  /**
   * ABI version used to populate this structure.
   */
  uint32_t abi_version;
  uint32_t ftype;
  uint32_t mode;
  uint64_t size;
  uint32_t nlink;
  uint64_t fileid;
  uint32_t uid;
  uint32_t gid;
  uint64_t blocks;
  int64_t atime_sec;
  uint32_t atime_nsec;
  int64_t mtime_sec;
  uint32_t mtime_nsec;
  int64_t ctime_sec;
  uint32_t ctime_nsec;
} vfsi_attrs;

/**
 * Uniform ABI-v3 result for scalar and vector operations.
 *
 * `index` is the completed count on success and the failing operation index
 * on error. Vector calls also populate a caller-owned result per element;
 * after a submitted concurrent batch fails, every non-failing element is
 * marked indeterminate because it may already have completed. `err_no`
 * retains the backend status while `category` is portable across protocols.
 */
typedef struct vfsi_result {
  uint32_t struct_size;
  uint32_t abi_version;
  size_t index;
  uint32_t category;
  uint32_t err_no;
  char message[VFSI_RESULT_MESSAGE_SIZE];
} vfsi_result;

typedef struct vfsi_open_op {
  const char *path;
  int flags;
  uint32_t mode;
  int fd;
} vfsi_open_op;

typedef struct vfsi_stat_op {
  const char *path;
  struct vfsi_attrs attrs;
} vfsi_stat_op;

typedef struct vfsi_setattr_op {
  const char *path;
  uint32_t mask;
  uint32_t mode;
  uint64_t size;
  int64_t atime_sec;
  uint32_t atime_nsec;
  int64_t mtime_sec;
  uint32_t mtime_nsec;
} vfsi_setattr_op;

typedef struct vfsi_pread_op {
  int fd;
  void *buf;
  size_t len;
  uint64_t offset;
  size_t got;
} vfsi_pread_op;

typedef struct vfsi_pwrite_op {
  int fd;
  const void *buf;
  size_t len;
  uint64_t offset;
  size_t wrote;
} vfsi_pwrite_op;

typedef struct vfsi_mkdir_op {
  const char *path;
  uint32_t mode;
} vfsi_mkdir_op;

typedef struct vfsi_rename_op {
  const char *oldpath;
  const char *newpath;
} vfsi_rename_op;

typedef struct vfsi_copy_op {
  const char *src;
  uint64_t src_offset;
  const char *dst;
  uint64_t dst_offset;
  uint64_t length;
  bool to_eof;
} vfsi_copy_op;

typedef bool (*vfsi_read_stream_cb)(const char *path,
                                    size_t index,
                                    uint64_t offset,
                                    const uint8_t *data,
                                    size_t len,
                                    bool eof,
                                    void *userdata);

typedef bool (*vfsi_listdir_cb)(const char *name, const struct vfsi_attrs *attrs, void *userdata);

typedef bool (*vfsi_listdirv_cb)(const char *dir,
                                 const char *name,
                                 const struct vfsi_attrs *attrs,
                                 void *userdata);

typedef bool (*vfsi_read_paths_cb)(const char *path, const uint8_t *data, size_t len, void *userdata);

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Return the ABI version implemented by the loaded library.
 */
uint32_t vfsi_abi_version(void);

/**
 * Return the negotiated NFS minor version, or zero for a non-NFS/invalid
 * handle.
 */
uint32_t vfsi_nfs_minorversion(const struct vfsi_fs *fs);

/**
 * Return the negotiated SMB dialect revision (`0x0202` through `0x0311`),
 * or zero for a non-SMB/invalid handle.
 */
uint16_t vfsi_smb_dialect(const struct vfsi_fs *fs);

/**
 * Return the current `VFSI_CAP_*` capability bitset.
 */
uint64_t vfsi_capabilities(const struct vfsi_fs *fs);

/**
 * Create a local-directory vfsi backend rooted at `root`.
 */
int vfsi_dummy_open(const char *root, struct vfsi_fs **out);

/**
 * Create a local-directory vfsi backend rooted at `root`, treating
 * `mountpoint` as the kernel-visible root of the same directory. Callers can
 * pass ordinary kernel paths under `mountpoint`, which are mapped to `/`-rooted
 * vfsi paths before they reach the backend.
 */
int vfsi_dummy_open_mount(const char *root, const char *mountpoint, struct vfsi_fs **out);

/**
 * Connect to an NFSv4.1 server at `host` and open the export root.
 */
int vfsi_nfs_open(const char *host, struct vfsi_fs **out);

/**
 * Connect using an explicit supported NFS minor version (1 or 2).
 */
int vfsi_nfs_open_minor(const char *host, uint32_t minorversion, struct vfsi_fs **out);

/**
 * Connect to an NFSv4.1 server and treat `mountpoint` (a local kernel mount
 * path) as the vfsi root. Callers can then pass ordinary kernel paths.
 */
int vfsi_nfs_open_mount(const char *host, const char *mountpoint, struct vfsi_fs **out);

/**
 * Connect to an NFSv4.1 server, mapping a local kernel `mountpoint` to the
 * server-side `export_root` beneath the NFSv4 pseudo-root.
 */
int vfsi_nfs_open_mount_export(const char *host,
                               const char *export_root,
                               const char *mountpoint,
                               struct vfsi_fs **out);

/**
 * Connect to an SMB2/3 share. `server` may omit port 445; empty username and
 * password strings request guest access. Paths are rooted at the share root.
 */
int vfsi_smb_open(const char *server,
                  const char *share,
                  const char *username,
                  const char *password,
                  const char *domain,
                  struct vfsi_fs **out);

/**
 * Connect to an SMB2/3 share and map a kernel-visible `mountpoint` onto
 * `share_root` within that share. Both paths must be absolute and may not
 * contain parent-directory components.
 */
int vfsi_smb_open_mount(const char *server,
                        const char *share,
                        const char *username,
                        const char *password,
                        const char *domain,
                        const char *share_root,
                        const char *mountpoint,
                        struct vfsi_fs **out);

/**
 * Destroy a filesystem handle returned by one of the `vfsi_*_open*`
 * functions.
 */
void vfsi_free(struct vfsi_fs *fs);

/**
 * Stat `path`, following a final symlink.
 */
int vfsi_stat(struct vfsi_fs *fs, const char *path, struct vfsi_attrs *out);

/**
 * Open a file and return a vfsi descriptor (`>= 0`), or a negative errno.
 */
int vfsi_open(struct vfsi_fs *fs, const char *path, int flags, uint32_t mode);

/**
 * Close a descriptor opened by [`vfsi_open`].
 */
int vfsi_close(struct vfsi_fs *fs, int fd);

/**
 * Read up to `len` bytes at `offset` into `buf`; writes the actual count to
 * `*got` when non-NULL.
 */
int vfsi_pread(struct vfsi_fs *fs, int fd, void *buf, size_t len, uint64_t offset, size_t *got);

/**
 * Write `len` bytes at `offset`; writes the actual count to `*wrote`.
 */
int vfsi_pwrite(struct vfsi_fs *fs,
                int fd,
                const void *buf,
                size_t len,
                uint64_t offset,
                size_t *wrote);

/**
 * Create a directory (`create_parents != 0` behaves like `mkdir -p`).
 */
int vfsi_mkdir(struct vfsi_fs *fs, const char *path, uint32_t mode, int create_parents);

/**
 * Remove a path (directories must be empty).
 */
int vfsi_remove(struct vfsi_fs *fs, const char *path);

/**
 * Rename `oldpath` to `newpath`.
 */
int vfsi_rename(struct vfsi_fs *fs, const char *oldpath, const char *newpath);

/**
 * Copy one extent. When `to_eof` is true, `length` is ignored and copying
 * continues to the source EOF. The backend uses NFSv4.2 COPY or SMB
 * server-side copy when available and falls back to client-side I/O.
 */
int vfsi_copy(struct vfsi_fs *fs,
              const char *src,
              uint64_t src_offset,
              const char *dst,
              uint64_t dst_offset,
              uint64_t length,
              bool to_eof);

/**
 * Open `count` files in one backend vector call. ABI-v2 functions remain
 * available; this and the other `*v` entry points use the uniform ABI-v3
 * overall and per-element result contract.
 */
struct vfsi_result vfsi_openv(struct vfsi_fs *fs,
                              struct vfsi_open_op *ops,
                              size_t count,
                              struct vfsi_result *item_results);

/**
 * Close a descriptor array in one backend vector call.
 */
struct vfsi_result vfsi_closev(struct vfsi_fs *fs,
                               const int *fds,
                               size_t count,
                               struct vfsi_result *item_results);

/**
 * Stat a path array in one backend vector call.
 */
struct vfsi_result vfsi_statv(struct vfsi_fs *fs,
                              struct vfsi_stat_op *ops,
                              size_t count,
                              struct vfsi_result *item_results);

/**
 * Set selected attributes for a path array in one backend vector call.
 */
struct vfsi_result vfsi_setattrv(struct vfsi_fs *fs,
                                 const struct vfsi_setattr_op *ops,
                                 size_t count,
                                 struct vfsi_result *item_results);

/**
 * Positioned vector read using caller-owned buffers.
 */
struct vfsi_result vfsi_preadv(struct vfsi_fs *fs,
                               struct vfsi_pread_op *ops,
                               size_t count,
                               struct vfsi_result *item_results);

/**
 * Positioned vector write using caller-owned buffers.
 */
struct vfsi_result vfsi_pwritev(struct vfsi_fs *fs,
                                struct vfsi_pwrite_op *ops,
                                size_t count,
                                struct vfsi_result *item_results);

/**
 * Create a directory array in one backend vector call.
 */
struct vfsi_result vfsi_mkdirv(struct vfsi_fs *fs,
                               const struct vfsi_mkdir_op *ops,
                               size_t count,
                               struct vfsi_result *item_results);

/**
 * Remove a path array in one backend vector call.
 */
struct vfsi_result vfsi_removev(struct vfsi_fs *fs,
                                const char *const *paths,
                                size_t count,
                                struct vfsi_result *item_results);

/**
 * Rename a path-pair array in one backend vector call.
 */
struct vfsi_result vfsi_renamev(struct vfsi_fs *fs,
                                const struct vfsi_rename_op *ops,
                                size_t count,
                                struct vfsi_result *item_results);

/**
 * Copy an extent-pair array in one backend vector call.
 */
struct vfsi_result vfsi_copyv(struct vfsi_fs *fs,
                              const struct vfsi_copy_op *ops,
                              size_t count,
                              struct vfsi_result *item_results);

/**
 * Stream several paths in bounded vectorized chunks. Returning `false` from
 * `cb` cancels successfully. The callback provides backpressure and must not
 * reenter the same filesystem handle.
 */
struct vfsi_result vfsi_read_streamv(struct vfsi_fs *fs,
                                     const char *const *paths,
                                     size_t count,
                                     size_t chunk_size,
                                     size_t memory_limit,
                                     vfsi_read_stream_cb cb,
                                     void *userdata);

/**
 * List `dir` and call `cb` for each entry. Returning `false` from `cb`
 * stops the listing.
 */
int vfsi_listdir(struct vfsi_fs *fs, const char *dir, vfsi_listdir_cb cb, void *userdata);

/**
 * List several directories in one vectorized batch, calling `cb` for each
 * entry with the directory the entry came from.
 */
int vfsi_listdirv(struct vfsi_fs *fs,
                  const char *const *dirs,
                  size_t count,
                  size_t max_entries,
                  bool recursive,
                  vfsi_listdirv_cb cb,
                  void *userdata);

/**
 * Read the full contents of several files in one vectorized batch, calling
 * `cb` for each file after its data has been fetched.
 */
int vfsi_read_paths(struct vfsi_fs *fs,
                    const char *const *paths,
                    size_t count,
                    vfsi_read_paths_cb cb,
                    void *userdata);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* VFSI_H */
