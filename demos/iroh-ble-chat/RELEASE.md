# Releasing iroh-ble-chat

The release pipeline lives in [.github/workflows/release.yml](../../.github/workflows/release.yml).
All credentials (certificates, keystores, provisioning profiles, API keys, passwords) are
stored as age-encrypted files in [.github/secrets/](../../.github/secrets/) and committed to
the repo. A single GitHub Actions secret, `AGE_IDENTITY`, holds the private key that decrypts
them at build time.

## Artifacts

| Job                     | Artifact                           | Destination                        |
| ----------------------- | ---------------------------------- | ---------------------------------- |
| `ios-testflight`        | `.ipa`                             | App Store Connect → TestFlight     |
| `macos-testflight`      | `.pkg` (sandboxed, MAS-signed)     | App Store Connect → TestFlight     |
| `macos-release-build`   | `.app.zip` + `.dmg` (Developer ID) | GitHub Release (via `publish`)     |
| `android-release-build` | universal `.apk` (signed)          | GitHub Release (via `publish`)     |
| `publish`               | —                                  | Creates/updates the GitHub Release |

Triggers:

- Tag push `iroh-ble-chat-v*` → every build job runs, then `publish` collects the `release-*`
  artifacts and creates a single GitHub Release for the tag (with auto-generated notes).
  Tags containing `-alpha`, `-beta`, or `-rc` are marked prerelease.
- `workflow_dispatch` → pick a subset via the `targets` input. Manual runs never create a
  GitHub Release — they upload workflow artifacts for download/debugging.

## Cutting a release

```sh
cd demos/iroh-ble-chat/src-tauri
cargo release 0.2.0            # edits tauri.conf.json via scripts/bump-apple-version.sh
git push && git push --tags     # tag triggers the workflow
```

`scripts/bump-apple-version.sh` is a `cargo-release` pre-release hook that writes:

- `version` — `X.Y.Z` short version (prerelease suffix stripped).
- `bundle.iOS.bundleVersion` — `git rev-list --count HEAD` (monotonic).
- `bundle.macOS.bundleVersion` — same as iOS (TestFlight needs strictly-increasing builds).

Android uses `autoIncrementVersionCode: true` in `tauri.conf.json`, so the APK `versionCode`
advances automatically on every build.

## Manually triggering

GitHub UI → Actions → "Release" → "Run workflow":

| `targets`           | Runs                                                              |
| ------------------- | ----------------------------------------------------------------- |
| `all`               | everything                                                        |
| `testflight-all`    | iOS + macOS TestFlight uploads                                    |
| `release-all`       | macOS Developer ID + Android APK (no TestFlight)                  |
| `ios-testflight`    | iOS only                                                          |
| `macos-testflight`  | macOS MAS only                                                    |
| `macos-release`     | macOS Developer ID only                                           |
| `android-release`   | Android only                                                      |

Manual runs reuse whatever `version`/`bundleVersion` is currently in `tauri.conf.json` — re-run
`cargo release` (or edit the file by hand) if you need a bumped build number before re-uploading.

## Secret management (age)

All secrets live in [.github/secrets/](../../.github/secrets/) as age-encrypted files. The helper
script wraps the common operations.

### One-time setup (repo owner)

1. Install age via mise: `mise install age` (or `brew install age`).
2. Generate an identity and add it as a recipient:
   ```sh
   mkdir -p ~/.config/age
   age-keygen -o ~/.config/age/iroh-ble-chat.txt
   # copy the "# public key: ageXXX" line into .github/secrets/recipients.txt
   ```
3. Encrypt every credential once (see the full list below):
   ```sh
   scripts/secrets.sh encrypt ~/certs/ios-dist.p12
   scripts/secrets.sh encrypt ~/certs/mas-app.p12
   # ...etc
   ```
4. Put the contents of `~/.config/age/iroh-ble-chat.txt` into a single GitHub Actions secret
   named `AGE_IDENTITY`.

### Adding a collaborator

1. They run `age-keygen -o ~/.config/age/iroh-ble-chat.txt` on their machine.
2. They send you the `# public key: …` line.
3. You append it to `.github/secrets/recipients.txt` and run `scripts/secrets.sh rekey`.
4. Commit the re-encrypted `.age` files.

### Local use

```sh
scripts/secrets.sh decrypt   # writes to .secrets/ (gitignored)
scripts/secrets.sh encrypt path/to/new.p12   # adds .github/secrets/new.p12.age
scripts/secrets.sh rekey                     # re-encrypt all files after editing recipients.txt
```

`AGE_IDENTITY_FILE` overrides the default path (`~/.config/age/iroh-ble-chat.txt`).

## Required files in `.github/secrets/`

### Env-var bundle: `secrets.env.age`

A shell-sourceable file with every string secret. Lines should be `KEY=VALUE` with no quoting:

```
APPSTORE_API_KEY_ID=ABCD12EF34
APPSTORE_API_ISSUER_ID=00000000-0000-0000-0000-000000000000
APPLE_IOS_CERTIFICATE_PASSWORD=…
APPLE_MAS_APP_CERTIFICATE_PASSWORD=…
APPLE_MAS_INSTALLER_CERTIFICATE_PASSWORD=…
APPLE_DEVELOPER_ID_CERTIFICATE_PASSWORD=…
ANDROID_KEY_ALIAS=release
ANDROID_KEYSTORE_PASSWORD=…
ANDROID_KEY_PASSWORD=…
KEYCHAIN_PASSWORD=…
```

Encrypt with:
```sh
scripts/secrets.sh encrypt path/to/secrets.env
```

### Apple

| File                                     | Source                                                 |
| ---------------------------------------- | ------------------------------------------------------ |
| `apple-api-key.p8.age`                   | App Store Connect API key (.p8 downloaded from ASC).   |
| `ios-dist.p12.age`                       | iOS Distribution cert (.p12 with private key).         |
| `ios-appstore.mobileprovision.age`       | iOS App Store provisioning profile (bundle id matches).|
| `mas-app.p12.age`                        | 3rd Party Mac Developer Application cert (.p12).       |
| `mas-installer.p12.age`                  | 3rd Party Mac Developer Installer cert (.p12).         |
| `mas-appstore.provisionprofile.age`      | Mac App Store provisioning profile.                    |
| `developer-id.p12.age`                   | Developer ID Application cert (.p12).                  |

Get the API key from [App Store Connect → Users and Access → Integrations](https://appstoreconnect.apple.com/access/integrations/api)
with role Developer or App Manager. Export `.p12` certs from Keychain Access
(Export → "Personal Information Exchange (.p12)"). Provisioning profiles come from the
[Apple Developer portal](https://developer.apple.com/account/resources/profiles/list).

Sandbox entitlements for MAS live in [src-tauri/Entitlements.mas.plist](src-tauri/Entitlements.mas.plist);
the MAS bundle overlay is [src-tauri/tauri.mas.conf.json](src-tauri/tauri.mas.conf.json).

### Android

| File                          | Source                                                      |
| ----------------------------- | ----------------------------------------------------------- |
| `android-release.jks.age`     | Java KeyStore containing the release key.                   |

Generate a keystore once with:

```sh
keytool -genkey -v \
  -keystore release.jks \
  -keyalg RSA -keysize 2048 -validity 10000 \
  -alias release
# answer the prompts, then encrypt:
scripts/secrets.sh encrypt release.jks
```

**Back up the plaintext keystore offline.** If you lose it, Google Play will not let you publish
updates to the same package name. For pure sideload releases you can replace it, but existing
installs will refuse to upgrade without an uninstall first.

The signing config wiring lives in [src-tauri/gen/android/app/build.gradle.kts](src-tauri/gen/android/app/build.gradle.kts);
it reads `gen/android/keystore.properties` (gitignored), which the CI job generates from the
decrypted keystore + the `ANDROID_*` env vars. If the file is absent, release builds fall back
to Android's debug signing (useful for local `tauri android build` runs).

The universal APK is built with ABIs `arm64-v8a`, `armeabi-v7a`, and `x86_64` — selected via the
`androidTargetAbis` Gradle property (passed through `ORG_GRADLE_PROJECT_androidTargetAbis` in
CI). Local `pnpm tauri android dev` uses the default single ABI (`arm64-v8a`) for speed.

## Legacy: Xcode Cloud

The Xcode Cloud entry point ([src-tauri/gen/apple/ci_scripts/ci_post_clone.sh](src-tauri/gen/apple/ci_scripts/ci_post_clone.sh))
and the `CI_XCODE_CLOUD` branch in [gen/apple/project.yml](src-tauri/gen/apple/project.yml) are kept
as a fallback for now. Once the GitHub Actions pipeline has shipped a successful TestFlight build,
those can be deleted along with any Xcode Cloud workflows configured in App Store Connect.
