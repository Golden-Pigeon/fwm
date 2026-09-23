# Implementation conventions

- Prefer existing dependencies and maintained libraries for general capabilities. Do not reimplement functionality already provided by a suitable framework.
- Use `clap_complete` for completion parsing, filesystem candidates, and shell integration. Custom completion code should only supply fwm's saved server, rule, and group candidates and its configuration context.

## Documentation

- Keep `README.md` in English and short enough to read in roughly two GitHub page lengths. It is an introduction and quick start for people, not a specification or task log.
- Put detailed user instructions in `docs/` and link them from the README. Keep examples, limitations, and recovery steps with the relevant guide.
- Put agent-facing implementation and validation instructions here. Do not add session summaries, one-off test counts, coverage snapshots, or internal audit checklists to the README.
- `TESTING.md` contains the test matrix and historical validation evidence. `DESIGN.md` includes design history and future proposals; verify current behavior before treating a proposal as implemented. Preserve dated audit evidence under `audits/`.

- Keep public audit evidence free of workstation usernames, absolute home/temp paths, and real machine aliases. Use repository-relative source links and neutral fixtures; explain redaction without changing historical outcomes or recomputing captured checksums.
- Keep bundled-component license notices with checked-in helpers and binary distributions. `fwm licenses` embeds `crates/fwm/THIRD_PARTY_NOTICES.txt`; update the notices when their bundled components change.

## Code map

- `crates/fwm-core/src/`: domain models, configuration storage, selectors, event history, SSH handling, recovery helpers, and the forwarding engine.
- `crates/fwm-api/src/`: versioned protocol, bounded framing, and the transport-independent Rust client. Integrations should use this API or CLI JSON, not parse human-readable tables.
- `crates/fwm/src/cli/`: command arguments, completion, orchestration, and output.
- `crates/fwm/src/daemon/`: request dispatch, configuration application, status, and events.
- `crates/fwm/src/offline/` and `offline.rs`: configuration operations under an instance lock without starting the daemon.
- `crates/fwm/src/configuration/` and `ssh_actions.rs`: shared online/offline mutation validation and SSH actions.
- `crates/fwm/src/platform/`: IPC, daemon launch, and platform-specific user services.

## Behavior to preserve

- Saved running intent is not listener readiness. Do not report cached or offline state as a verified live connection. Status configuration and runtime snapshots must use the same revision.
- Configuration batches validate and commit together. Preserve stable IDs through renames, unapplied drafts through mutations, and stop/delete decisions through later reloads or recovery.
- Queries, diagnostics, completion, and offline edits must not start a stopped daemon. Validate enabling operations before starting it.
- Do not weaken host-key checks or IPC peer authentication. Authentication failures must not trigger offline-write fallback. The security boundary is the current OS account; see `SECURITY.md`.
- Verified remote cleanup requires ownership evidence for the same manager, rule, generation, and process identity. Never replace it with killing by port, process name, or bare PID. Unsupported cleanup must remain explicit.
- Keep rule/group changes isolated from unrelated forwards. Report unconfirmed remote cancellations and failed service rollback honestly.
- Shell completion must remain read-only and work without SSH access or a running daemon. Keep compatibility coverage for the pinned `clap_complete` dynamic interface.
- Preserve the structured-output contract: one final JSON result for a one-shot command, JSON Lines for watch/follow, and saved-but-not-ready outcomes on failed waits.

## Validation

For code changes, run checks appropriate to the affected components. The standard
Rust checks are:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Additional checks by area:

- Status/watch/history: `python3 tests/postfix_queries.py target/debug/fwm` after `cargo build --locked`.
- Completion/install scripts: `python3 tests/shell_completion_scripts.py` and `python3 tests/install_from_source.py -v`, and `python3 tests/install_release.py -v`.
- Remote helpers: `python3 -m unittest discover -s crates/fwm-core/src/cleanup -p 'test_*.py' -v` and `python3 tests/native_helper.py -v`.
- Native helper changes also require regenerating checked-in artifacts with the three `scripts/build-native-remote-helper*.sh` scripts. They require Zig and the relevant SDK/target support; see `TESTING.md`.
- Real SSH/recovery changes: `python3 tests/smoke.py target/debug/fwm`. It requires Unix, `sshd`, and `ssh-keygen`; Linux may need a test-environment `/run/sshd`. Use isolated temporary keys/configuration and loopback listeners. VM tests must use an explicitly selected isolated SSH endpoint.
- Documentation-only changes: check local links, command examples, and readability; do not run unrelated runtime suites.

Use `TESTING.md` and `.github/workflows/ci.yml` for the full platform and coverage
matrix. Cross-compilation and mocked service tests do not establish native
runtime behavior. Report the actual environment and checks used rather than
copying historical pass counts. On macOS, keep the compiler, linker, and SDK
from a compatible Apple toolchain; the source installer shows the `xcrun` setup.

## Releases

- `install.sh` downloads tagged GitHub releases; `install-from-source.sh` builds the checkout. Keep their names and documentation distinct, and reuse `scripts/install-shell-completions.sh`.
- Release tags must equal `v` plus the workspace version. `.github/workflows/release.yml` builds and tests each native target before publishing all archives and `SHA256SUMS` together. Never replace a published version's assets.
- Release archives carry the bundled-component notices, cargo-about's dependency notices, and unchanged MPL dependency sources. Preserve these files in packaging and installation tests.
