/* Optional convenience loader for applications that dlopen libvfsi_c.
 *
 * Include vfsi.h first. This does not own or close `handle`; the caller keeps
 * the library loaded for as long as any function pointer may be used.
 * Linux consumers should link with -ldl (required on older glibc releases).
 */
#ifndef VFSI_DYNAMIC_H
#define VFSI_DYNAMIC_H

#include <dlfcn.h>
#include <errno.h>
#include <string.h>
#include <vfsi.h>

typedef struct vfsi_dynamic_api {
  void *handle;
  uint32_t (*abi_version)(void);
  void (*free)(struct vfsi_fs *);
  int (*dummy_open_mount)(const char *, const char *, struct vfsi_fs **);
  int (*nfs_open_mount_export)(const char *, const char *, const char *,
                               struct vfsi_fs **);
  int (*nfs_from_mount)(const char *, struct vfsi_fs **);
  int (*listdir)(struct vfsi_fs *, const char *, vfsi_listdir_cb, void *);
  int (*listdirs)(struct vfsi_fs *, const char *const *, size_t,
                  const struct vfsi_listing_options *,
                  vfsi_indexed_listdir_cb, void *,
                  struct vfsi_listing_result *);
  int (*read_paths)(struct vfsi_fs *, const char *const *, size_t,
                    vfsi_read_paths_cb, void *);
  int (*read_paths_with_limit)(struct vfsi_fs *, const char *const *, size_t,
                               size_t, vfsi_read_paths_cb, void *);
} vfsi_dynamic_api;

/* Resolve an optional symbol into a function pointer without an ISO C cast.
 * Returns 0 when found, ENOSYS when absent, or EOVERFLOW when pointer sizes
 * differ on an unsupported platform ABI.
 */
static inline int vfsi_dynamic_resolve(void *handle, const char *name,
                                       void *target, size_t target_size) {
  void *symbol = dlsym(handle, name);
  if (!symbol)
    return ENOSYS;
  if (target_size != sizeof(symbol))
    return EOVERFLOW;
  memcpy(target, &symbol, sizeof(symbol));
  return 0;
}

/* Load the common port-facing symbol set and validate its ABI. `handle` may
 * be a dlopen handle or RTLD_DEFAULT (which is null on some platforms). Only
 * abi_version and free are required; other operations remain optional so a
 * caller can select a feature based on the resulting null function pointer.
 * Does not call dlclose on failure or success.
 */
static inline int vfsi_dynamic_load(vfsi_dynamic_api *api, void *handle) {
  if (!api)
    return EINVAL;
  memset(api, 0, sizeof(*api));
  api->handle = handle;

#define VFSI_DYNAMIC_SYMBOL(member, required)                               \
  do {                                                                       \
    int error = vfsi_dynamic_resolve(handle, "vfsi_" #member,               \
                                     &api->member, sizeof(api->member));      \
    if ((required) && error)                                                 \
      return error;                                                          \
  } while (0)

  VFSI_DYNAMIC_SYMBOL(abi_version, 1);
  if (api->abi_version() != VFSI_ABI_VERSION)
    return EPROTO;
  VFSI_DYNAMIC_SYMBOL(free, 1);
  VFSI_DYNAMIC_SYMBOL(dummy_open_mount, 0);
  VFSI_DYNAMIC_SYMBOL(nfs_open_mount_export, 0);
  VFSI_DYNAMIC_SYMBOL(nfs_from_mount, 0);
  VFSI_DYNAMIC_SYMBOL(listdir, 0);
  VFSI_DYNAMIC_SYMBOL(listdirs, 0);
  VFSI_DYNAMIC_SYMBOL(read_paths, 0);
  VFSI_DYNAMIC_SYMBOL(read_paths_with_limit, 0);

#undef VFSI_DYNAMIC_SYMBOL
  return 0;
}

#endif /* VFSI_DYNAMIC_H */
