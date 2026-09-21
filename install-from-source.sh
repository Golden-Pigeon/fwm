#!/usr/bin/env bash
# Build and install this local checkout; release-download installers are separate.
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: ./install-from-source.sh [--root PATH] [--offline] [--shell bash|zsh|none]
                              [--rc-file PATH]

Build fwm from this source checkout in release mode and install or update it.
Requires Rust/Cargo 1.90+ and a platform C compiler.

Options:
  --root PATH   Install into PATH/bin instead of ~/.local/bin.
                Relative paths are resolved from the directory where you run this script.
  --offline     Use cached Cargo dependencies only.
  --shell NAME  Enable completions for bash or zsh (default: detect from $SHELL).
                Use none to install completion files without editing startup files.
  --rc-file PATH
                Configure this startup file instead of the shell's default files.
  -h, --help    Show this help without building or installing anything.

Without --root, install into ~/.local/bin, regardless of Cargo's installation-root settings.
This script builds local source; it does not download a prebuilt GitHub release.
Cargo may download dependencies unless --offline is supplied.
EOF
}

argument_error() {
    printf 'error: %s\n\n' "$1" >&2
    usage >&2
    exit 2
}

install_args=(install --locked --force)
install_root=
completion_shell=${SHELL:-}
completion_shell=${completion_shell##*/}
completion_shell_explicit=false
completion_rc=
while (( $# > 0 )); do
    case "$1" in
        --root)
            (( $# >= 2 )) && [[ -n $2 && $2 != -* ]] || argument_error '--root requires a path'
            install_root=$2
            shift 2
            ;;
        --root=*)
            [[ -n ${1#--root=} ]] || argument_error '--root requires a path'
            install_root=${1#--root=}
            shift
            ;;
        --offline)
            install_args+=(--offline)
            shift
            ;;
        --shell)
            (( $# >= 2 )) || argument_error '--shell requires bash, zsh, or none'
            completion_shell=$2
            completion_shell_explicit=true
            shift 2
            ;;
        --shell=*)
            completion_shell=${1#--shell=}
            completion_shell_explicit=true
            shift
            ;;
        --rc-file)
            (( $# >= 2 )) && [[ -n $2 && $2 != -* ]] || argument_error '--rc-file requires a path'
            completion_rc=$2
            shift 2
            ;;
        --rc-file=*)
            completion_rc=${1#--rc-file=}
            [[ -n $completion_rc ]] || argument_error '--rc-file requires a path'
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            argument_error "unknown argument: $1"
            ;;
    esac
done

case "$completion_shell" in
    bash|zsh|none) ;;
    *)
        if [[ $completion_shell_explicit == true || -n $completion_rc ]]; then
            argument_error 'use --shell bash, --shell zsh, or --shell none for an unsupported login shell'
        fi
        completion_shell=none
        ;;
esac
[[ $completion_shell != none || -z $completion_rc ]] || argument_error '--rc-file cannot be combined with --shell none'
install_root=${install_root:-${HOME:?HOME must be set when --root is omitted}/.local}
source_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]:-$0}")" && pwd -P)
if [[ ! -f $source_dir/Cargo.lock || ! -f $source_dir/crates/fwm/Cargo.toml || ! -f $source_dir/scripts/install-shell-completions.sh ]]; then
    printf 'error: run this script from a complete fwm source checkout\n' >&2
    exit 1
fi
if ! command -v cargo >/dev/null 2>&1; then
    printf 'error: cargo is not on PATH; install Rust/Cargo 1.90+ before running this script\n' >&2
    exit 1
fi

printf 'Building and installing fwm from %s\n' "$source_dir"
# Keep the caller's working directory so relative --root paths keep their meaning.
# Cargo install builds release binaries by default; --force also updates the same version.
cargo "${install_args[@]}" "--root=$install_root" --path "$source_dir/crates/fwm"

install_root=$(CDPATH= cd -- "$install_root" && pwd -P)
source "$source_dir/scripts/install-shell-completions.sh"
install_shell_completions "$install_root" "$completion_shell" "$completion_rc"

printf '\nInstalled fwm from source. Add %s/bin to PATH if needed.\n' "$install_root"
printf 'Verify with: fwm --version\n'
