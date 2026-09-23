# Pre-publication review

Reviewed on 2026-09-23 at commit `2c7a8d8` on `master`.
That commit contains the English README and documentation reorganization.

The findings below record the pre-remediation state. The license and audit
sanitization findings have since been addressed; see the resolution notes under
each finding. The runtime fixes and evidence from earlier audits are preserved.

## Findings

### Resolved: Include musl's notice with the distributed Linux helper

Evidence: [the helper build command](../../scripts/build-native-remote-helper.sh#L15)
uses `x86_64-linux-musl` and `-static`. The tracked
`crates/fwm-core/src/cleanup/native_helper_linux_x86_64` is a stripped, statically
linked ELF executable, and it is embedded into fwm for upload to SSH servers.
The repository includes the project's MIT license, the word-list attribution,
and jsmn's notice in its header, but no musl copyright/license notice. The Linux
artifact itself does not contain that notice either.

The [musl copyright file distributed by Zig 0.15.2](https://raw.githubusercontent.com/ziglang/zig/0.15.2/lib/libc/musl/COPYRIGHT)
requires its notice with copies or substantial portions of the library. Merely
publishing fwm's MIT notice does not supply musl's attribution. This affects the
source repository as published today, because it already contains a compiled
helper; it is not limited to a future binary release.

Add third-party notices covering the bundled helper runtime and ship them with
source and binary distributions. Also inventory the Windows helper's linked
runtime against [Zig's MinGW license file](https://raw.githubusercontent.com/ziglang/zig/0.15.2/lib/libc/mingw/COPYING)
and the actual linked components; do not assume every compiler runtime has the
same terms. The confirmed omission here is musl's notice.

Resolution: [bundled-component notices](../../THIRD_PARTY_NOTICES.md) now include
musl's complete copyright file, MinGW-w64 and gdtoa notices, the Zig runtime
license, jsmn, and the word-list attribution. `fwm licenses` embeds these notices
in the executable. The source installer copies them alongside the installed
binary under `share/licenses/fwm/`.

### Resolved: Prepare a sanitized public copy of audit evidence

Evidence: [a saved SSH-parser result](../2026-09-20-postfix/ssh-parser-round1.json#L21)
contains the development account name and absolute SSH identity/known-hosts
paths. Similar local paths occur 290 times in tracked text. Audit Markdown links
such as those in [cli-api.md](../2026-09-20-postfix/cli-api.md#L15) also point to
one developer's filesystem and will not navigate to source on GitHub.

These are environment details, not evidence of leaked private-key material.
Still, publishing the raw evidence discloses unnecessary workstation information
and leaves readers with unusable source links. Replace local paths with neutral
fixture paths and use repository-relative links in the public copy. Preserve
original dated evidence privately if needed. These records also exist in earlier
commits: changing only HEAD will not remove them from a published Git history.
Resolution: local paths and machine aliases have been replaced with neutral
fixtures across the working tree and all nine existing commits. Markdown source
links now use repository-relative paths. Standalone Python probes derive their
binary path from the checkout. Original evidence and Git history were backed up
outside the repository before rewriting; no audit files were removed. See the
[audit archive notes](../README.md) for interpreting historical checksums.

## Dependency advisory, already disclosed

An OSV query of all 266 registry packages in `Cargo.lock` returned one advisory:
`rsa 0.10.0-rc.18`, [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html).
The current advisory still lists no patched version. It is already documented
in [SECURITY.md](../../SECURITY.md#L21), including the distinction between the
SSH signing path and a network-accessible decryption oracle.

This review did not establish a usable timing attack against fwm. Do not describe
the dependency as fixed or infer exploitability from the package match alone.
Keep the exception scoped and documented, and consider automated advisory
checks so new issues are not hidden behind this known one.

## Checks performed

Environment: macOS arm64, installed stable Rust toolchain. Rust linking used the
installed macOS 26.5 SDK to avoid the local default SDK/linker mismatch.

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed. |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Passed. |
| `cargo test --workspace --locked` | 570 passed; none ignored. |
| Python legacy helper tests | 33 passed. |
| `tests/native_helper.py -v` | 10 passed. |
| `tests/postfix_queries.py target/debug/fwm` | 3 passed. |
| `tests/install_from_source.py -v` | 25 passed, using its isolated fixtures. |
| `tests/shell_completion_scripts.py` | 18 cases: 12 passed and 6 expected failures for documented upstream compatibility limits. |
| Gitleaks 8.30.1, all Git history | 9 commits / about 4.17 MB scanned. All 25 alerts were source-file SHA-256 digests, manually classified using their historical JSON entries. No credential leak was confirmed. |
| Additional historical pattern scan | No private-key blocks or common provider-token patterns found in historical text blobs. |
| OSV lockfile scan | 266 registry packages checked; one existing RSA advisory. |
| Dependency metadata | 219 packages in the local macOS dependency graph inspected for declared license and minimum Rust version; no declared minimum above 1.90. This is not an independent Rust 1.90 build. |

Gitleaks was downloaded from its official release and checked against the
published SHA-256 checksum. Scan reports used redacted output. No private data
was submitted to a scanning service; the OSV query contained registry package
names and versions only.

The Rust dependency graph also contains mixed permissive licenses and
`option-ext` under MPL-2.0. This alone is not a finding against the project's MIT
license; binary release packaging needs its own complete notice/source-offer
review for the actual distribution.

## Limits and follow-up

No fresh Linux/Windows native run, remote-machine recovery test, or complete
OpenSSH smoke run was performed in this review. Existing dated evidence is
available in [TESTING.md](../../TESTING.md). The local checks do not certify
three-platform runtime behavior, absence of all secrets, or absence of security
bugs. Repository hosting settings and release artifacts were not audited because
this checkout has no configured Git remote.

Before distributing prebuilt releases, pin and record the native-helper build
toolchains and hashes, verify regenerated artifacts in CI, and preserve the
third-party notices in release packaging. The existing CI runs functional tests but does not
currently provide those artifact provenance checks or automated secret/advisory
scans. These are follow-up improvements, separate from the concrete findings
above.

## Remediation verification

After the license and redaction changes: 570 Rust tests, strict Clippy, and
format checks passed. Installer tests passed all 26 cases, including checking
the installed notice file. Completion tests passed 12 cases with the same six
documented expected failures. Both `fwm licenses` and its JSON output matched
the complete embedded notice text without creating configuration.

All nine rewritten historical trees were compared file-by-file with the intended
redaction of their originals; no files were dropped. All 442 reachable historical
blobs passed the workstation-identifier scan. The current public files passed
JSON and Python parsing checks, and 468 relative Markdown links resolved.
Gitleaks on the rewritten history returned only the same 25 source-hash false
positives. Original history, the old/new commit map, and the original review are
kept in a private backup outside this repository.
