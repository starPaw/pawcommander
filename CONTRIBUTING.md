# Contributing to PawCommander

Bug reports, focused fixes, translations, and documentation improvements are welcome.

## Report a problem

Use the issue templates and include your OS, app version or commit, steps to reproduce, and expected/actual behavior. For transfer bugs, say whether both locations are local or SFTP and whether an existing destination was involved. Use disposable sample files when reproducing file-operation problems.

Remove personal paths, hostnames, usernames, credentials, and file contents from logs and screenshots. For vulnerabilities, follow [SECURITY.md](SECURITY.md).

## Develop a change

Follow the build prerequisites in the [README](README.md), then create a branch and make a focused change. The Rust toolchain and Cargo lockfile are pinned.

```sh
cargo run --locked
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

Changes to file operations should cover cancellation, conflicts, partial failures, and preservation of existing source/destination data where relevant. Include meaningful regression tests for changed behavior. Mention the platforms and native integrations you actually tested in your pull request.

The UI supports English and Russian through `src/i18n.rs`. Keep both languages in sync when adding visible text. Keep the application and credential-store identifiers consistent with the macOS bundle identifier.

## Optional integration checks

On macOS/Linux with OpenSSH server tools and Python 3:

```sh
bash scripts/test-sftp.sh
PAWCOMMANDER_TEST_DISABLE_STATVFS=1 bash scripts/test-sftp.sh
```

The script creates a temporary loopback-only SSH server, generates disposable keys, and cleans up on exit. It does not change your SSH configuration. Local OS policy may prevent launching `sshd`.

The real credential-store test is separate and explicitly opt-in:

```sh
cargo test --locked os_vault_round_trip -- --ignored
```

It creates and removes a disposable entry in the OS vault. Default tests use a mock vault instead.

## Before submitting

Keep generated builds, IDE state, private settings, and credentials out of commits. Review `git diff --cached` before committing. Run the checks above; for UI changes, include a screenshot using synthetic files. Keep the PR description focused on the behavior and validation.

Contributions are distributed under the project's [MIT license](LICENSE).

## Package a macOS release

Install [cargo-about 0.9.3](https://github.com/EmbarkStudios/cargo-about/releases/tag/0.9.3) and Python 3, then run:

```sh
bash scripts/package-macos.sh
```

This builds for the current Mac's architecture and writes a ZIP plus `SHA256SUMS.txt` to `dist/release`. The archive includes license notices and corresponding source for MPL-licensed components. `CARGO_ABOUT_BIN` can select a cargo-about executable; `SOURCE_COMMIT` identifies the commit when building from an exported source tree.

For public releases, build from a clean, reviewed source snapshot, verify the source commit in `BUILD_INFO.txt`, inspect the archive for private paths or files, and test the extracted app. The package is ad-hoc signed and not notarized. Publish it as an asset on a versioned GitHub Release; update the README's versioned download link when releasing a new version.
