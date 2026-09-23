# Installation and shell completion

[Back to README](../README.md)

Build requirements are Rust/Cargo 1.90 or newer and a platform C compiler. The
`ring` dependency includes native code. fwm uses a Rust SSH implementation and
does not need the local `ssh` executable for normal operation.

## macOS and Linux

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
only; otherwise Cargo may download missing dependencies. There is no prebuilt
release download installer.

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

## Windows or manual installation

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

On Bash 3.2, prefer `--server NAME` over `--server=NAME` and complete at the end of
the word. Colon-containing IDs and completion in the middle of a word have
upstream limitations. If a quoted path does not complete in Bash or Zsh, start
with an unquoted prefix and let the shell quote the result. On older Bash, a
candidate matching a local directory can be treated as a directory.
