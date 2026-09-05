#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

/**
 * Opaque filesystem handle owned by C.
 */
typedef struct vfsi_fs vfsi_fs;

/**
 * Attributes returned by [`vfsi_stat`] and passed to listdir callbacks.
 */
typedef struct vfsi_attrs {
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

typedef bool (*vfsi_listdir_cb)(const char *name, const struct vfsi_attrs *attrs, void *userdata);

/**
 * Create a local-directory vfsi backend rooted at `root`.
 */
int vfsi_dummy_open(const char *root, struct vfsi_fs **out);

/**
 * Connect to an NFSv4.1 server at `host` and open the export root.
 */
int vfsi_nfs_open(const char *host, struct vfsi_fs **out);

/**
 * Destroy a filesystem handle returned by [`vfsi_dummy_open`] /
 * [`vfsi_nfs_open`].
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
int vfsi_pread(struct vfsi_fs *fs,
               int fd,
               void *buf,
               uintptr_t len,
               uint64_t offset,
               uintptr_t *got);

/**
 * Write `len` bytes at `offset`; writes the actual count to `*wrote`.
 */
int vfsi_pwrite(struct vfsi_fs *fs,
                int fd,
                const void *buf,
                uintptr_t len,
                uint64_t offset,
                uintptr_t *wrote);

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
 * List `dir` and call `cb` for each entry. Returning `false` from `cb`
 * stops the listing.
 */
int vfsi_listdir(struct vfsi_fs *fs, const char *dir, vfsi_listdir_cb cb, void *userdata);
