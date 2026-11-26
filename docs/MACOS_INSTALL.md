# Install PawCommander on macOS

The 0.3.0 preview is for Apple Silicon (M-series Macs). It targets macOS 11 or later and has been tested on macOS 27.0.1.

1. Download `PawCommander-0.3.0-macOS-arm64.zip`, `SHA256SUMS.txt` and `SHA256SUMS.txt.sig` into the same folder. Verify the download as described below. [Official release](https://github.com/starPaw/pawcommander/releases/tag/v0.3.0)
2. Double-click the ZIP to extract it.
3. Drag **PawCommander.app** into **Applications** and open it.

Rust, Cargo, Homebrew, and Xcode are not required to run this build. `locate` and Zed are optional tools for indexed search and editor integration.

## First launch

The app has a local ad-hoc signature, without an Apple Developer ID certificate or notarization. macOS may block its first launch because the developer cannot be verified.

If you trust the official download, first attempt to open the app, then choose **System Settings → Privacy & Security → Open Anyway** and confirm **Open**. This creates an exception for this app. See [Apple's guide](https://support.apple.com/en-us/102445). If macOS reports detected malware or a damaged app, do not bypass that warning; check the download and report the problem.

## Verify the download

PawCommander signs the checksum with an Ed25519 release key. Its SHA-256 fingerprint is `SHA256:YT6I9NljjE+IWKMBYtYyN9eNC9mtUDjgJyqSPeFSONc`. Authenticate the key using a previously trusted fingerprint or the project's Git signing identity. For example, if you already trust the Git signer of `v0.2.0`, run `git verify-tag v0.3.0` in a project checkout and confirm the same signer. The signed tag contains both the release key and the verification script.

From that trusted checkout, verify the original ZIP beside its two checksum files. Adjust the download path to match your Mac.

```sh
git show v0.3.0:docs/keys/macos-release-ed25519.pub > macos-release-ed25519.pub
git show v0.3.0:scripts/verify-macos-release.sh > verify-release.sh
ssh-keygen -E sha256 -lf macos-release-ed25519.pub
bash verify-release.sh macos-release-ed25519.pub ~/Downloads/PawCommander-0.3.0-macOS-arm64.zip
```

Proceed after `Verified release archive` appears. This authenticates the package with the PawCommander release key. macOS evaluates the app's code signature and notarization separately on launch.

The ZIP contains the app, this guide, release notes, the MIT license, third-party license notices, corresponding source for MPL-licensed components, and a build manifest identifying the source commit.

## Updates

Quit PawCommander before replacing it with a newer copy in Applications. Automatic app updates are not included. Check the [Releases page](https://github.com/starPaw/pawcommander/releases) for updates.

With a version that provides `update-macos` in `--help`, use the installed command to verify and replace the app. Keep the ZIP, `SHA256SUMS.txt` and `SHA256SUMS.txt.sig` together. Use the public key you verified in the previous section. For example, with PawCommander installed in Applications, run this in Terminal after quitting the app.

```sh
app=/Applications/PawCommander.app
"$app/Contents/MacOS/pawcommander" update-macos "$app" \
  /path/to/PawCommander-VERSION-macOS-arm64.zip /path/to/trusted-release-key.pub
```

The command accepts a newer version for your architecture and checks its macOS requirement. It prepares the signed app and a copy of the previous installation in private folders before swapping the app bundles. Use a writable local volume that supports atomic swaps, such as APFS. Use the signed ZIP and manual installation above when your installed version predates the update command.

Save the printed backup path. If the command is interrupted, check the installed version in Finder using **Get Info**. The installation path contains a complete app before and after the swap. After quitting the app, restore from the saved path. Run the command from the backup so recovery also works when the new app cannot launch. If the backup predates the command, use a separate trusted copy with `restore-macos` in `--help`.

```sh
app=/Applications/PawCommander.app
backup=/Applications/.pawcommander-previous-EXAMPLE/PawCommander.app
"$backup/Contents/MacOS/pawcommander" restore-macos "$app" "$backup"
```

Keep the backup in the original installation folder until you have checked that the updated app opens. The same restore command also works if the installed app has been removed. If another item appears at the missing app's path during recovery, restore stops and preserves that item. The backup remains available for another attempt. A forcibly stopped command may leave a private `.pawcommander-update-*` working folder beside the app. After confirming the installed version and backup, you can remove that folder.

Version 0.3.0 keeps the application and credential-store identifiers from the 0.1.0 and 0.2.0 public previews. Replacing the app preserves settings and saved connections from those releases.
