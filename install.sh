#!/usr/bin/env bash
# Install a prebuilt GitHub release. Compatible with macOS Bash 3.2.
set -euo pipefail

usage() {
    cat <<'HELP'
Usage: bash install.sh [--version TAG] [--root PATH] [--shell bash|zsh|none]
                       [--rc-file PATH] [--no-start]

Download and verify a prebuilt fwm release for macOS or Linux.
Defaults: latest release, ~/.local/bin, detected Bash/Zsh completion,
and start/restart the default daemon after installation.

  --version TAG   Install a particular release, e.g. v0.1.0.
  --root PATH     Install into PATH/bin instead of ~/.local/bin.
  --shell SHELL   Configure bash/zsh, or use none to leave startup files alone.
  --rc-file PATH  Use one specific shell startup file (backed up before edits).
  --no-start      Install without starting or restarting the daemon.
  -h, --help      Print this help without downloading or installing anything.
HELP
}
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }
argument_error() { printf 'error: %s\n' "$*" >&2; usage >&2; exit 2; }

repo=Golden-Pigeon/fwm
version=latest
install_root=
completion_shell=${SHELL:-}
completion_shell=${completion_shell##*/}
completion_explicit=false
completion_rc=
start_daemon=true
while (( $# )); do
    case "$1" in
        --version|--root|--shell|--rc-file)
            (( $# >= 2 )) && [[ -n $2 && $2 != -* ]] || argument_error "$1 requires a value"
            case "$1" in
                --version) version=$2 ;;
                --root) install_root=$2 ;;
                --shell) completion_shell=$2; completion_explicit=true ;;
                --rc-file) completion_rc=$2 ;;
            esac
            shift 2 ;;
        --version=*) version=${1#*=}; shift ;;
        --root=*) install_root=${1#*=}; [[ -n $install_root ]] || argument_error '--root requires a value'; shift ;;
        --shell=*) completion_shell=${1#*=}; completion_explicit=true; shift ;;
        --rc-file=*) completion_rc=${1#*=}; [[ -n $completion_rc ]] || argument_error '--rc-file requires a value'; shift ;;
        --no-start) start_daemon=false; shift ;;
        -h|--help) usage; exit 0 ;;
        *) argument_error "unknown argument: $1" ;;
    esac
done
case "$completion_shell" in
    bash|zsh|none) ;;
    *)
        if [[ $completion_explicit == true || -n $completion_rc ]]; then
            argument_error 'use --shell bash, --shell zsh, or --shell none'
        fi
        completion_shell=none ;;
esac
[[ $completion_shell != none || -z $completion_rc ]] || argument_error '--rc-file cannot be combined with --shell none'
[[ $version == latest || $version =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || argument_error 'version must look like v0.1.0'

case "$(uname -s)" in
    Darwin) platform=apple-darwin ;;
    Linux) platform=unknown-linux-musl ;;
    *) fail 'this installer supports macOS and Linux; download the Windows zip from GitHub Releases' ;;
esac
case "$(uname -m)" in
    x86_64|amd64) architecture=x86_64 ;;
    arm64|aarch64) architecture=aarch64 ;;
    *) fail 'unsupported CPU architecture; supported architectures are x86_64 and arm64' ;;
esac
target=$architecture-$platform
for required in curl tar mktemp; do command -v "$required" >/dev/null || fail "$required is required"; done
if command -v sha256sum >/dev/null; then
    checksum=(sha256sum)
elif command -v shasum >/dev/null; then
    checksum=(shasum -a 256)
else
    fail 'sha256sum or shasum is required'
fi
curl_options=(--fail --location --silent --show-error --retry 3 --proto '=https' --tlsv1.2)
if [[ $version == latest ]]; then
    # Resolve once so an intervening release cannot mix the archive and checksum.
    resolved=$(curl "${curl_options[@]}" --output /dev/null --write-out '%{url_effective}' "https://github.com/$repo/releases/latest")
    [[ $resolved == "https://github.com/$repo/releases/tag/"* ]] || fail 'could not resolve the latest release'
    version=${resolved##*/}
    [[ $version =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || fail 'the latest release has an unsupported tag'
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/fwm-install.XXXXXX")
new_binary=
cleanup() {
    rm -rf -- "$work"
    [[ -z $new_binary ]] || rm -f -- "$new_binary"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
archive=fwm-$target.tar.gz
base=https://github.com/$repo/releases/download/$version
printf 'Downloading fwm %s for %s...\n' "$version" "$target"
curl "${curl_options[@]}" --output "$work/$archive" "$base/$archive"
curl "${curl_options[@]}" --output "$work/SHA256SUMS" "$base/SHA256SUMS"
expected=$(awk -v archive="$archive" '$2 == archive { print $1 }' "$work/SHA256SUMS")
[[ $expected =~ ^[0-9a-f]{64}$ ]] || fail 'release checksum is missing or ambiguous'
actual=$("${checksum[@]}" "$work/$archive")
actual=${actual%% *}
[[ $actual == "$expected" ]] || fail 'SHA-256 verification failed; nothing was installed'
tar -xzf "$work/$archive" -C "$work"
package=$work/fwm-$target
for required in fwm LICENSE THIRD_PARTY_NOTICES.txt DEPENDENCY_LICENSES.txt scripts/install-shell-completions.sh; do
    [[ -f $package/$required && ! -L $package/$required ]] || fail "release archive is missing $required"
done
[[ -d $package/dependency-sources ]] || fail 'release archive is missing dependency sources'
[[ $("$package/fwm" --version) == "fwm ${version#v}" ]] || fail 'downloaded binary version does not match the release'

install_root=${install_root:-${HOME:?HOME must be set when --root is omitted}/.local}
mkdir -p -- "$install_root/bin" "$install_root/share/licenses/fwm"
install_root=$(CDPATH= cd -- "$install_root" && pwd -P)
new_binary=$(mktemp "$install_root/bin/.fwm.XXXXXX")
cp "$package/fwm" "$new_binary"
chmod 755 "$new_binary"
mv -f -- "$new_binary" "$install_root/bin/fwm"
new_binary=
for file in LICENSE THIRD_PARTY_NOTICES.txt DEPENDENCY_LICENSES.txt; do
    cp "$package/$file" "$install_root/share/licenses/fwm/$file"
done
cp -R "$package/dependency-sources" "$install_root/share/licenses/fwm/"
source "$package/scripts/install-shell-completions.sh"
install_shell_completions "$install_root" "$completion_shell" "$completion_rc"
if [[ $start_daemon == true ]]; then
    printf '\nStarting or restarting the fwm daemon...\n'
    if ! "$install_root/bin/fwm" daemon restart; then
        printf 'error: fwm was installed, but daemon restart failed. Retry with:\n  %q daemon restart\n' "$install_root/bin/fwm" >&2
        exit 1
    fi
fi
printf '\nInstalled fwm %s at %s/bin/fwm\n' "$version" "$install_root"
printf 'Open a new terminal, or add %s/bin to PATH.\n' "$install_root"
