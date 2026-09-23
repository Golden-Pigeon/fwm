# Installation and shell completion

[Back to README](../README.md)

## Prebuilt releases

On macOS or Linux:

```sh
curl -fsSL https://github.com/Golden-Pigeon/fwm/releases/latest/download/install.sh -o install.sh
bash install.sh
bash install.sh --version v0.1.0 --root "$HOME/apps/fwm"
bash install.sh --shell none --no-start
```

The installer supports Intel and ARM64 on macOS/Linux. It resolves the latest
tag once, downloads the matching archive, and checks its SHA-256 against the
release manifest before running or installing the binary. It uses Bash 3.2+,
`curl`, `tar`, and either `sha256sum` or `shasum`; Rust is not required.

Installation defaults to `~/.local/bin/fwm` and sets up Bash/Zsh completion with
backups of changed startup files. `--root`, `--shell`, and `--rc-file` work as
shown in the shell-completion section below. It starts or restarts the default
`~/.fwm` daemon after installation; pass `--no-start` to install without doing so.
`--shell none` alone only disables startup-file edits.

On Windows, download `fwm-x86_64-pc-windows-msvc.zip` from
[Releases](https://github.com/Golden-Pigeon/fwm/releases), compare its SHA-256 with
`SHA256SUMS` using `Get-FileHash`, extract it, and add the directory containing
`fwm.exe` to your user PATH. Keep the included license and dependency-source
files with the executable. Run `fwm daemon restart` after upgrading an existing
installation.

Linux packages use musl and do not require a particular glibc version. macOS
packages require macOS 11 or newer. The Windows package targets Windows x64.
These describe the machine running fwm; supported remote recovery helpers are
listed in [operations](operations.md#reverse-tunnel-recovery).

## Build from source

Build requirements are Rust/Cargo 1.90 or newer and a platform C compiler. The
`ring` dependency includes native code. fwm uses a Rust SSH implementation and
does not need the local `ssh` executable for normal operation.

### macOS and Linux

Run the installer from a complete source checkout:

```sh
./install-from-source.sh
./install-from-source.sh --root "$HOME/apps/fwm"
./install-from-source.sh --offline
./install-from-source.sh --shell none
```

The default executable is `~/.local/bin/fwm`. `--root PATH` installs into
`PATH/bin`; relative paths use the directory from which you invoke the script.
You can invoke the script by its path from outside the checkout. It requires
Bash 3.2 or newer.

The script builds a release binary with `cargo install --locked --force`,
installs completion files, and configures Bash or Zsh according to `$SHELL`.
Repeating it updates the installation. `--offline` uses cached Cargo dependencies
only; otherwise Cargo may download missing dependencies.

After installation, the script starts or restarts the daemon for `~/.fwm`, keeping
saved rules and their running/stopped settings. It also refreshes an installed
login service's executable path. `--root` changes the install location, not the
configuration directory. `--shell none` skips shell startup edits but still
restarts the daemon. If restart fails, the binary remains installed; the script
exits with an error and prints a command to retry it.

On macOS, the script uses `xcrun` to select a matching compiler, linker, and SDK.
It preserves explicit toolchain overrides, including `SDKROOT` and
`DEVELOPER_DIR`. If linking fails with an SDK or architecture error, check
`xcode-select -p` and any compiler/SDK overrides for a mismatched toolchain.

### Windows or manual installation

```sh
cargo install --path crates/fwm --locked --force
```

This uses Cargo's installation directory. To build without installing:

```sh
cargo build --release --locked
./target/release/fwm --help
```

The Windows executable is `target\release\fwm.exe`. After replacing an installed
binary, run `fwm daemon restart` to load it into the background process. See
[services and upgrades](operations.md#daemon-and-login-services).

## License notices

`fwm licenses` prints the project and bundled-component notices without loading
configuration or starting the daemon. The text is embedded in the executable,
including on Windows and after a manual Cargo installation. The source installer
also saves a copy in `ROOT/share/licenses/fwm/THIRD_PARTY_NOTICES.txt`.
Release installations also keep `DEPENDENCY_LICENSES.txt` and
`dependency-sources/` in that directory. See
[third-party notices](../THIRD_PARTY_NOTICES.md) when redistributing binaries.

## Shell completion

The source installer enables completion in new terminals. In an existing
terminal, load the appropriate file for the default install:

```sh
# Zsh
source ~/.local/share/fwm/shell-init.zsh

# Bash
source ~/.local/share/fwm/shell-init.bash
```

For a custom root, use the `source` path printed by the installer. Completion
includes command options, files, and saved server, rule, and group names. It
reads the selected `--config-dir` and works while the daemon is stopped.

The installer updates its own marked block and backs up changed startup files as
`FILE.fwm-backup.*`. Defaults are `${ZDOTDIR:-$HOME}/.zshrc` for Zsh and
`~/.bashrc` plus the first existing Bash login file (`.bash_profile`,
`.bash_login`, or `.profile`). If none exists, it creates `.bash_profile`.
Use `--shell bash` or `--shell zsh` to override detection, `--rc-file PATH` to
choose one startup file, or `--shell none` to manage loading yourself.

Completion files live under `ROOT/share/zsh/site-functions/_fwm` and
`ROOT/share/bash-completion/completions/fwm`. The supplied loaders register
completion from the current binary each time a shell starts. The Zsh loader
initializes `compinit` if necessary. For your own shell setup:

```sh
# Zsh: load explicitly, rather than only adding the file to fpath
source <(fwm completions zsh)

# Bash
eval "$(fwm completions bash)"
```

With the pinned completion engine (including Bash 3.2 and 5.2), prefer `--server NAME` over `--server=NAME` and complete at the end of
the word. Colon-containing IDs and completion in the middle of a word have
upstream limitations. Shell metacharacters in a candidate can also cause Bash
to reject the completed command; enter such IDs as a quoted argument manually.
If a quoted path does not complete in Bash or Zsh, start
with an unquoted prefix and let the shell quote the result. On older Bash, a
candidate matching a local directory can be treated as a directory.
