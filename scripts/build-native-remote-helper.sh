#!/usr/bin/env sh
set -eu

# Build the Linux x86_64 helper embedded by fwm. A static musl binary keeps
# remote execution independent of the target's libc, Python, compiler, and
# package manager. Zig is used because it supplies a cross-target musl libc;
# callers may override it with FWM_ZIG.

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
zig=${FWM_ZIG:-zig}
cache=${FWM_ZIG_CACHE:-${TMPDIR:-/tmp}/fwm-zig-cache}
mkdir -p "$cache"
mkdir -p "$root/crates/fwm-core/src/cleanup"

ZIG_GLOBAL_CACHE_DIR="$cache" "$zig" cc \
  -target x86_64-linux-musl \
  -static -s -O2 -std=c11 -Wall -Wextra -Werror \
  -o "$root/crates/fwm-core/src/cleanup/native_helper_linux_x86_64" \
  "$root/crates/fwm-core/src/cleanup/native_helper.c"

file "$root/crates/fwm-core/src/cleanup/native_helper_linux_x86_64"
