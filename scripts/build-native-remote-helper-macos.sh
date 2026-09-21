#!/usr/bin/env sh
set -eu

# Build the Darwin helper used by fwm when the SSH server is macOS.  The
# executable uses only libproc/sysctl APIs present in stock macOS; Python,
# lsof, a compiler, and a resident service are never required on the target.
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
cc=${FWM_CLANG:-clang}
out=${FWM_MACOS_HELPER_OUT:-$root/crates/fwm-core/src/cleanup/native_helper_macos_universal}
mkdir -p "$(dirname -- "$out")"

"$cc" -std=c11 -O2 -Wall -Wextra -Werror -arch arm64 -arch x86_64 \
  -mmacosx-version-min=10.15 \
  -o "$out" "$root/crates/fwm-core/src/cleanup/native_helper_macos.c" -lproc

file "$out"
