#!/bin/sh
# Native C/C++ header, optional-loader, and stat-projection checks; no Rust SDK build.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/vfsi-c-headers.XXXXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM
cat >"$work/headers.c" <<'C'
#define _DEFAULT_SOURCE 1
#define _POSIX_C_SOURCE 200809L
#include <vfsi.h>
#include <vfsi_dynamic.h>
#include <vfsi_posix.h>
int main(void) { return VFSI_ABI_VERSION != 3; }
C
for standard in c99 c11; do
    "${CC:-cc}" -std="$standard" -Wall -Wextra -Werror \
        -I "$root/bindings/c/include" -fsyntax-only "$work/headers.c"
done
"${CXX:-c++}" -std=c++11 -Wall -Wextra -Werror -x c++ \
    -I "$root/bindings/c/include" -fsyntax-only "$work/headers.c"
for test in posix-fields dynamic-loader; do
    "${CC:-cc}" -std=c11 -Wall -Wextra -Werror \
        -I "$root/bindings/c/include" "$root/bindings/c/tests/$test.c" -o "$work/$test"
    "$work/$test"
done
# Darwin changes struct stat's timestamp members when strict POSIX is selected.
# Exercise its extension layout as well as the strict layout tested above.
"${CC:-cc}" -std=c11 -D_DARWIN_C_SOURCE -Wall -Wextra -Werror \
    -I "$root/bindings/c/include" "$root/bindings/c/tests/posix-fields.c" -o "$work/darwin-fields"
"$work/darwin-fields"
printf '%s\n' 'Native C99/C11/C++ headers, optional loader, and POSIX fields passed.'
