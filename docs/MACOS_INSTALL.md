# Install PawCommander on macOS

The first downloadable preview is for Apple Silicon (M-series Macs). It is built for macOS 11 or later and tested on macOS 27.0.1. Older macOS versions have not been manually verified. There is no Intel binary in this release.

1. Download `PawCommander-0.1.0-macOS-arm64.zip` from the [official release](https://github.com/starPaw/pawcommander/releases/tag/v0.1.0).
2. Double-click the ZIP to extract it.
3. Drag **PawCommander.app** into **Applications** and open it.

Rust, Cargo, Homebrew, and Xcode are not required to run this build. `locate` and Zed are optional tools for indexed search and editor integration.

## First launch

The app has a local ad-hoc signature, without an Apple Developer ID certificate or notarization. macOS may block its first launch because the developer cannot be verified.

If you trust the official download, first attempt to open the app, then choose **System Settings → Privacy & Security → Open Anyway** and confirm **Open**. This creates an exception for this app. See [Apple's guide](https://support.apple.com/en-us/102445). If macOS reports detected malware or a damaged app, do not bypass that warning; check the download and report the problem.

## Verify the download

Download `SHA256SUMS.txt` from the same release and put it beside the ZIP. In that folder, run:

```sh
shasum -a 256 -c SHA256SUMS.txt
```

The result should be `OK`. A checksum verifies the downloaded bytes against the published file; it is not Apple notarization.

The ZIP contains the app, this guide, the MIT license, third-party license notices, corresponding source for MPL-licensed components, and a build manifest identifying the source commit.

## Updates

Quit PawCommander before replacing it with a newer copy in Applications. Automatic app updates are not included. Check the [Releases page](https://github.com/starPaw/pawcommander/releases) for updates.

The public preview uses new PawCommander application and credential-store identifiers. Settings and saved keys from older development builds are not migrated.
