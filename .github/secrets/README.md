# .github/secrets

Age-encrypted secrets consumed by [.github/workflows/release.yml](../workflows/release.yml).

## Layout

| Filename                               | Plaintext                                                    |
| -------------------------------------- | ------------------------------------------------------------ |
| `recipients.txt`                       | age public keys that can decrypt (unencrypted — commit it).  |
| `secrets.env.age`                      | Shell-sourceable `KEY=VALUE` pairs for all string secrets.   |
| `apple-api-key.p8.age`                 | App Store Connect API private key.                           |
| `ios-dist.p12.age`                     | iOS Distribution certificate.                                |
| `ios-appstore.mobileprovision.age`     | iOS App Store provisioning profile.                          |
| `mas-app.p12.age`                      | 3rd Party Mac Developer Application cert.                    |
| `mas-installer.p12.age`                | 3rd Party Mac Developer Installer cert.                      |
| `mas-appstore.provisionprofile.age`    | Mac App Store provisioning profile.                          |
| `developer-id.p12.age`                 | Developer ID Application cert.                               |
| `android-release.jks.age`              | Android release keystore.                                    |

`secrets.env.age` decrypts to a file with lines like:

```
APPSTORE_API_KEY_ID=ABCD12EF34
APPSTORE_API_ISSUER_ID=00000000-0000-0000-0000-000000000000
APPLE_IOS_CERTIFICATE_PASSWORD=...
APPLE_MAS_APP_CERTIFICATE_PASSWORD=...
APPLE_MAS_INSTALLER_CERTIFICATE_PASSWORD=...
APPLE_DEVELOPER_ID_CERTIFICATE_PASSWORD=...
ANDROID_KEY_ALIAS=release
ANDROID_KEYSTORE_PASSWORD=...
ANDROID_KEY_PASSWORD=...
KEYCHAIN_PASSWORD=...
```

No quoting; one `KEY=VALUE` per line; no multiline values.

## Tooling

[`scripts/secrets.sh`](../../scripts/secrets.sh) wraps `age` with three commands:

```sh
scripts/secrets.sh encrypt path/to/plaintext.p12   # -> .github/secrets/plaintext.p12.age
scripts/secrets.sh decrypt                          # -> .secrets/* (gitignored)
scripts/secrets.sh rekey                            # re-encrypt all .age files after editing recipients.txt
```

Install `age` via mise (`mise install age`). Identity file default:
`~/.config/age/iroh-ble-chat.txt` (override with `AGE_IDENTITY_FILE`).
