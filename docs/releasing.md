# Releasing Knobyte

Releases are built by `.github/workflows/release.yml` when a GitHub release is published (or a
`v*` tag is pushed, or the workflow is run manually with a tag). It attaches one archive and one
`.sha256` checksum per platform:

| Platform | Asset |
|---|---|
| macOS (Apple Silicon) | `knobyte-aarch64-apple-darwin.tar.gz` |
| Linux (x86-64) | `knobyte-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (arm64) | `knobyte-aarch64-unknown-linux-gnu.tar.gz` |
| Windows (x86-64) | `knobyte-x86_64-pc-windows-msvc.zip` |

No signing keys or other credentials are stored on GitHub; the workflow uses only the repository's
built-in token to upload assets.

## Cutting a release

1. Bump `version` in `Cargo.toml` (and the version badge in `README.md`), commit and push to `main`.
2. Tag and publish:

   ```bash
   git tag -a v0.9.6 -m "Knobyte 0.9.6"
   git push origin v0.9.6
   gh release create v0.9.6 --verify-tag --title "Knobyte 0.9.6" --notes-file notes.md
   ```

To rebuild the assets for an existing tag, run the workflow manually: **Actions → Release → Run
workflow**, with the tag (e.g. `v0.9.5`).

Include the macOS install note below in every release's notes.

## macOS downloads

The macOS binary is not signed with an Apple Developer ID or notarized. When it is downloaded with
a browser, macOS marks it as quarantined and refuses to open it ("cannot be opened because the
developer cannot be verified"). Users clear the flag once after extracting it:

```bash
xattr -d com.apple.quarantine /path/to/knobyte
```

Downloads made with `curl` or `wget`, and binaries built from source with `cargo`, are not
quarantined.

### Signing your own download (optional)

To offer a ready-to-run macOS download, for example on the website, sign and notarize the binary
on your own Mac, where the certificate and key stay in your keychain. With a *Developer ID
Application* certificate installed and a notarization profile stored in the keychain once
(`xcrun notarytool store-credentials knobyte-notary`):

```bash
codesign --force --sign "Developer ID Application: Your Name (TEAMID)" \
  --options runtime --timestamp --identifier ai.knobyte.cli knobyte
ditto -c -k --keepParent knobyte knobyte-notarize.zip
xcrun notarytool submit knobyte-notarize.zip --keychain-profile knobyte-notary --wait
codesign --verify --strict --verbose=2 knobyte
```

Then archive and publish that binary yourself. A bare command-line binary cannot have its
notarization ticket stapled, so Gatekeeper confirms it with Apple online on first launch.
