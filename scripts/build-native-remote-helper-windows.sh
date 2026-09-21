#!/usr/bin/env sh
set -eu

# Build the Windows x86_64 helper from the macOS/Linux development host. Zig
# supplies the MinGW headers and runtime; the resulting PE executable only
# imports the Windows system DLLs and needs no runtime on the SSH server.
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
zig=${FWM_ZIG:-zig}
cache=${FWM_ZIG_CACHE:-${TMPDIR:-/tmp}/fwm-zig-cache}
mkdir -p "$cache" "$root/crates/fwm-core/src/cleanup"

ZIG_GLOBAL_CACHE_DIR="$cache" "$zig" cc \
  -target x86_64-windows-gnu \
  -O2 -s -std=c11 -Wall -Wextra -Werror \
  -Wl,--subsystem,console \
  -o "$root/crates/fwm-core/src/cleanup/native_helper_windows_x86_64.exe" \
  "$root/crates/fwm-core/src/cleanup/native_helper_windows.c" \
  -lws2_32 -liphlpapi -ladvapi32

file "$root/crates/fwm-core/src/cleanup/native_helper_windows_x86_64.exe"
