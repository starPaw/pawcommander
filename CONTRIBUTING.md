# Contributing to PawCommander

Bug reports, focused fixes, translations, and documentation improvements are welcome.

## Report a problem

Use the issue templates and include your OS, app version or commit, steps to reproduce, and expected/actual behavior. For transfer bugs, say whether both locations are local or SFTP and whether an existing destination was involved. Use disposable sample files when reproducing file-operation problems.

Remove personal paths, hostnames, usernames, credentials, and file contents from logs and screenshots. For vulnerabilities, follow [SECURITY.md](SECURITY.md).

## Develop a change

Follow the build prerequisites in the [README](README.md), then create a branch and make a focused change. The Rust toolchain and Cargo lockfile are pinned. Use the [architecture and source map](docs/architecture.md) to find the owning modules, worker boundaries and focused test commands.

```sh
cargo run --locked
python3 scripts/check.py
```

The Python 3 script uses only the standard library and runs from the repository root on macOS, Linux and Windows (`python scripts/check.py` where that is your Python command). Its full mode runs `cargo fmt --check`, `cargo test --locked`, `cargo clippy --all-targets --locked -- -D warnings`, then `git diff --check`, stopping at the first failure. It prints each command's output and elapsed time. It does not install tools, clean build artifacts or modify Git state.

During iteration, run the relevant test filter from the source map. `python3 scripts/check.py --quick` checks formatting, compiles all targets with `cargo check --all-targets --locked`, and checks the diff; it does **not** run tests or Clippy and does not replace the full submission checks. Add `--offline` when Cargo dependencies are already cached.

For reproducible timing records, use `python3 scripts/check.py --offline --report target/check-report.json`. The optional JSON report records tool versions, UTC start/end times, platform, commands, monotonic elapsed times and exit codes, including a failed stage. Relative report paths are resolved from the repository root. Compare runs on the same host with the same check mode, dependency versions and build/cache state; a warm build and a clean build are different baselines. Keep reports local and measure before claiming an improvement in development speed.

Changes to file operations should cover cancellation, conflicts, partial failures, and preservation of existing source/destination data where relevant. Include meaningful regression tests for changed behavior. Mention the platforms and native integrations you actually tested in your pull request.

The UI supports English and Russian through `src/i18n.rs`. Keep both languages in sync when adding visible text. Keep the application and credential-store identifiers consistent with the macOS bundle identifier.

## Optional integration checks

On macOS/Linux with OpenSSH server tools and Python 3:

```sh
bash scripts/test-sftp.sh
PAWCOMMANDER_TEST_SFTP_HOST=::1 bash scripts/test-sftp.sh
PAWCOMMANDER_TEST_DISABLE_STATVFS=1 bash scripts/test-sftp.sh
PAWCOMMANDER_TEST_SFTP_RECONNECT=1 bash scripts/test-sftp.sh
PAWCOMMANDER_TEST_SFTP_METADATA_DENIED=1 bash scripts/test-sftp.sh
```

The script creates a temporary loopback-only SSH server, generates disposable keys, and cleans up on exit. It does not change your SSH configuration. Local OS policy may prevent launching `sshd`.

The reconnect mode starts a temporary SSH agent and uses an OpenSSH SFTP subsystem wrapper to withhold rename replies. It checks reconnect and inspection after disconnects before or after publication. It uses `/usr/libexec/sftp-server` by default. Set `SFTP_SERVER_BIN` if your OpenSSH server uses another path.

The metadata-denied mode disables `setstat` and `fsetstat` on the temporary server. It checks that the copy still completes with field-specific warnings. The standard mode checks supported dates and permissions and cancellation after metadata writes.

Run `bash scripts/test-ssh-interactive.sh` with a C compiler, `pkg-config`, OpenSSH key generation tools and a libssh build with server support. The loopback fixture exercises first-use trust, cancellation, hashed OpenSSH host records, key rotation, keyboard-interactive rounds and a second factor after key authentication. It compares the fingerprints with `ssh-keygen` and writes trust records only in its temporary directory.

For local moves, set `PAWCOMMANDER_TEST_MOVE_SOURCE_ROOT` and `PAWCOMMANDER_TEST_MOVE_TARGET_ROOT` to writable directories on **different filesystems**, then run `cargo test --locked actual_cross_volume_routing_counts_only_removed_sources -- --ignored`. The test creates its own disposable subdirectories, checks replacement and a nested move, and stops a later move after publication to verify that its source remains and cannot be retried automatically. It refuses two paths on the same volume. This filesystem check does not replace native GUI acceptance.

The real credential-store test is separate and explicitly opt-in:

```sh
cargo test --locked os_vault_round_trip -- --ignored
```

It creates and removes a disposable entry in the OS vault, including a key-sized payload and a Unicode passphrase. On Windows it also checks the previous stored format and preservation of an existing entry when a larger value exceeds the platform limit. Default tests use a mock vault instead.

## Before submitting

Keep generated builds, IDE state, private settings, and credentials out of commits. Review `git diff --cached` before committing. Run the full checks above; for UI changes, include a screenshot using synthetic files. Exercise affected native UI, clipboard and drag-and-drop behavior on the relevant OS with disposable files and isolated app state; compilation and shared tests do not replace native acceptance. State any untested platforms. Keep the PR description focused on the behavior and validation.

Contributions are distributed under the project's [MIT license](LICENSE).

## Package a macOS release

Install [cargo-about 0.9.3](https://github.com/EmbarkStudios/cargo-about/releases/tag/0.9.3) and Python 3, then run:

```sh
bash scripts/package-macos.sh
```

This builds for the current Mac's architecture and writes a ZIP plus `SHA256SUMS.txt` to `dist/release`. The archive includes license notices and corresponding source for MPL-licensed components. `CARGO_ABOUT_BIN` can select a cargo-about executable; `SOURCE_COMMIT` identifies the commit when building from an exported source tree.

For public releases, build from a clean, reviewed source snapshot, verify the source commit in `BUILD_INFO.txt`, inspect the archive for private paths or files, and test the extracted app. The package is ad-hoc signed and not notarized. Publish it as an asset on a versioned GitHub Release; update the README's versioned download link when releasing a new version.
