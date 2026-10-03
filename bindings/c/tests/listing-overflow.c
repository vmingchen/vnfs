/* Test-only ABI-v3 adapter: simulate a successful truncated legacy listing.
 * Load the real adapter privately so optional symbols stay absent here. */
#include <vfsi.h>
#include <dlfcn.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

static void *symbol(const char *name)
{
    static void *handle;
    void *result;
    if (!handle) handle = dlopen(getenv("VFSI_REAL_LIBRARY"), RTLD_NOW | RTLD_LOCAL);
    result = handle ? dlsym(handle, name) : NULL;
    if (!result) { fprintf(stderr, "missing test adapter symbol: %s\n", name); abort(); }
    return result;
}
#define REAL(name) ((__typeof__(&name))symbol(#name))
uint32_t vfsi_abi_version(void) { return VFSI_ABI_VERSION; }
int vfsi_dummy_open_mount(const char *root, const char *mount, struct vfsi_fs **out)
{ return REAL(vfsi_dummy_open_mount)(root, mount, out); }
int vfsi_nfs_open_mount_export(const char *host, const char *root, const char *mount, struct vfsi_fs **out)
{ return REAL(vfsi_nfs_open_mount_export)(host, root, mount, out); }
void vfsi_free(struct vfsi_fs *fs) { REAL(vfsi_free)(fs); }
int vfsi_listdir(struct vfsi_fs *fs, const char *path, vfsi_listdir_cb cb, void *data)
{ return REAL(vfsi_listdir)(fs, path, cb, data); }
int vfsi_read_paths(struct vfsi_fs *fs, const char *const *paths, size_t count, vfsi_read_paths_cb cb, void *data)
{ return REAL(vfsi_read_paths)(fs, paths, count, cb, data); }
int vfsi_listdirv(struct vfsi_fs *fs, const char *const *dirs, size_t count,
                 size_t max_entries, bool recursive, vfsi_listdirv_cb cb, void *data)
{
    struct vfsi_attrs attrs = { .struct_size = sizeof(attrs), .abi_version = VFSI_ABI_VERSION,
                               .ftype = 1, .mode = 0100644, .nlink = 1 };
    const char *configured = getenv("VFSI_TEST_ENTRIES");
    size_t i, total = configured ? strtoull(configured, NULL, 10) : 200001;
    (void)fs; (void)recursive;
    if (!count) return 0;
    if (max_entries && max_entries < total) total = max_entries;
    for (i = 0; i < total; i++)
        if (!cb(dirs[i % count], i == 200000 ? "tail" : "prefix", &attrs, data)) break;
    return 0; /* The legacy contract considers a limit stop successful. */
}
