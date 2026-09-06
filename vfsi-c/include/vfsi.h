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
#define VFSI_ABI_VERSION 2

/**
 * The backend will currently attempt server-side COPY.
 */
#define VFSI_CAP_SERVER_COPY (1 << 0)

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
