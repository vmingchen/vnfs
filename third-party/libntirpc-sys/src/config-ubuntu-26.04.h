/* Ubuntu 26.04 libntirpc 6.3 build settings. Its x86_64 development package
 * omits the generated config.h while public headers still include it. Keep
 * this fallback scoped in build.rs so other installations use their own. */
#ifndef CONFIG_H
#define CONFIG_H

#include <ntirpc/version.h>

#define HAVE_STDBOOL_H 1
#define HAVE_KRB5 1
#define LINUX 1
#define _HAVE_GSSAPI 1
#define HAVE_STRING_H 1
#define HAVE_STRINGS_H 1
#if defined(__BYTE_ORDER__) && __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
#define LITTLEEND 1
#else
#define BIGEND 1
#endif
#define TIRPC_EPOLL 1
#define USE_MONITORING 1

#define PACKAGE "libntirpc"
#define PACKAGE_NAME "libntirpc"
#define PACKAGE_STRING "libntirpc "
#define PACKAGE_TARNAME "libntirpc"
#define PACKAGE_VERSION ""

#endif
