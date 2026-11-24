# Releasing

## CLI (Homebrew)

The formula lives in `xeroxzen/homebrew-tap` as `Formula/restowa.rb`, with a copy in `packaging/homebrew/restowa.rb`. It installs a universal (arm64 + x86_64) binary that the `Release binaries` workflow attaches to each GitHub release, so users don't need Rust. `brew install --HEAD` still builds from source.

1. Bump `version` in the workspace `Cargo.toml`, and `CFBundleShortVersionString` and `CFBundleVersion` in `apps/macos/Restowa/Resources/Info.plist`.
2. `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `swift test` (in `apps/macos/Restowa`).
3. Commit, then `git tag vX.Y.Z && git push origin main vX.Y.Z`.
4. `gh release create vX.Y.Z --title vX.Y.Z --notes "..."`. Publishing the release starts the `Release binaries` workflow, which attaches `restowa-vX.Y.Z-macos-universal.tar.gz` and its `.sha256`. Wait for it: `gh run watch $(gh run list --workflow release.yml --limit 1 --json databaseId --jq '.[0].databaseId')`.
5. Get the checksum: `gh release download vX.Y.Z -p '*.sha256' -O -`.
6. Update `url` and `sha256` in both formula copies, then commit and push the tap.
7. Check: `brew update && brew audit --strict --online xeroxzen/tap/restowa && brew upgrade restowa`.

To attach a binary to an existing release, run the workflow by hand: `gh workflow run release.yml -f tag=vX.Y.Z`.

`restowa install --launchd` records the `bin/restowa` symlink, not the versioned Cellar path, so the launchd agent survives `brew upgrade`.

## Menu bar app

Needs a "Developer ID Application" certificate and a notarytool keychain profile. Setup steps are at the top of `apps/macos/Restowa/notarize.sh`.

```sh
CODESIGN_IDENTITY="Developer ID Application: Name (TEAMID)" apps/macos/Restowa/notarize.sh
gh release upload vX.Y.Z apps/macos/Restowa/build/Restowa-X.Y.Z.zip
```

Without a certificate, `build-app.sh` signs ad-hoc. That build runs on the machine that built it but Gatekeeper blocks it elsewhere.
