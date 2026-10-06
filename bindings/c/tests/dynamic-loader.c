/* Exercise loader error paths without depending on installed library versions.
 * The normal smoke test covers the real dlsym/RTLD_DEFAULT path.
 */
#include "vfsi.h"
#include <assert.h>
#include <dlfcn.h>
#include <errno.h>
#include <string.h>

enum fixture {
    OPTIONALS_ABSENT,
    OPTIONAL_PRESENT,
    WRONG_ABI,
    MISSING_VERSION,
    MISSING_FREE
};
static enum fixture fixture;

static uint32_t library_version(void)
{
    return fixture == WRONG_ABI ? VFSI_ABI_VERSION + 1 : VFSI_ABI_VERSION;
}

static void library_free(struct vfsi_fs *fs)
{
    (void)fs;
}

static int library_dummy_open_mount(const char *root, const char *mount,
                                  struct vfsi_fs **out)
{
    (void)root;
    (void)mount;
    (void)out;
    return ENOTSUP;
}

#define RETURN_SYMBOL(pointer)                                               \
    do {                                                                     \
        void *symbol;                                                        \
        _Static_assert(sizeof(pointer) == sizeof(symbol), "unsupported ABI"); \
        memcpy(&symbol, &(pointer), sizeof(symbol));                          \
        return symbol;                                                       \
    } while (0)

static void *fixture_dlsym(void *handle, const char *name)
{
    assert(handle == RTLD_DEFAULT || handle == &fixture);
    if (strcmp(name, "vfsi_abi_version") == 0 && fixture != MISSING_VERSION) {
        uint32_t (*function)(void) = library_version;
        RETURN_SYMBOL(function);
    }
    if (strcmp(name, "vfsi_free") == 0 && fixture != MISSING_FREE) {
        void (*function)(struct vfsi_fs *) = library_free;
        RETURN_SYMBOL(function);
    }
    if (strcmp(name, "vfsi_dummy_open_mount") == 0 && fixture == OPTIONAL_PRESENT) {
        int (*function)(const char *, const char *, struct vfsi_fs **) =
            library_dummy_open_mount;
        RETURN_SYMBOL(function);
    }
    return NULL;
}

#undef RETURN_SYMBOL
#define dlsym fixture_dlsym
#include "vfsi_dynamic.h"
#undef dlsym

int main(void)
{
    vfsi_dynamic_api api;
    assert(vfsi_dynamic_load(NULL, RTLD_DEFAULT) == EINVAL);

    fixture = OPTIONALS_ABSENT;
    assert(vfsi_dynamic_load(&api, RTLD_DEFAULT) == 0);
    assert(api.handle == RTLD_DEFAULT);
    assert(api.abi_version == library_version && api.free == library_free);
    assert(api.dummy_open_mount == NULL && api.nfs_open_mount_export == NULL);
    assert(api.nfs_from_mount == NULL && api.listdir == NULL && api.listdirs == NULL);
    assert(api.read_paths == NULL && api.read_paths_with_limit == NULL);

    fixture = OPTIONAL_PRESENT;
    assert(vfsi_dynamic_load(&api, &fixture) == 0);
    assert(api.handle == &fixture);
    assert(api.dummy_open_mount == library_dummy_open_mount);

    /* Reusing a table must clear optional pointers from a previous load. */
    fixture = OPTIONALS_ABSENT;
    assert(vfsi_dynamic_load(&api, &fixture) == 0);
    assert(api.dummy_open_mount == NULL);

    fixture = WRONG_ABI;
    assert(vfsi_dynamic_load(&api, &fixture) == EPROTO);
    fixture = MISSING_VERSION;
    assert(vfsi_dynamic_load(&api, &fixture) == ENOSYS);
    fixture = MISSING_FREE;
    assert(vfsi_dynamic_load(&api, &fixture) == ENOSYS);

    uint32_t (*version)(void) = NULL;
    fixture = OPTIONALS_ABSENT;
    assert(vfsi_dynamic_resolve(&fixture, "vfsi_abi_version", &version, 0) == EOVERFLOW);
    assert(version == NULL);
    return 0;
}
