# Phase 4 findings: menu bar app

Nov 23, 2026

## Build

- `apps/macos/Restowa/` is a SwiftPM package: `RestowaKit` (models, CLI client, process runner, registry watcher, grouping) and the `Restowa` executable (SwiftUI). `build-app.sh` wraps the release binary into `build/Restowa.app` and signs it ad-hoc (`CODESIGN_IDENTITY` overrides).
- Builds with the Command Line Tools alone (Swift 6.4, macOS 27 SDK). No Xcode on the dev machine.
- **CLT limitation:** SwiftUI's `@State` is a macro in this SDK and its plugin (`SwiftUIMacros`) ships only with Xcode. The app therefore keeps view state in `@Observable` classes (`MenuState`, `SettingsModel`); the Observation macro plugin is in the CLT. Same applies to `#Preview` and `@Entry`: don't use them.
- SwiftPM prints two `ld: warning: search path ... not found` lines because it adds XCTest search paths the CLT doesn't have. They come from the toolchain, not this code; Swift warnings are errors (`treatAllWarnings(as: .error)`).
- Tests use Swift Testing (`swift test`), which the CLT include.

## Findings that shaped the design

- **`restowa list --live` takes ~0.44 s** (wall clock, almost no CPU). The 150 ms menu-open budget can't wait on it, so the app shows its last snapshot instantly and refreshes in the background.
- **Reads touch the WAL.** A read-only `restowa list` updates the mtime of an empty `registry.db-wal`. Watching `-wal` naively makes every refresh trigger the next. `RegistrySignature` counts the WAL only while it holds data, plus `registry.db` mtime and size.
- **Minimal PATH.** An app launched from Finder or at login gets launchd's PATH, so `restowa doctor` would report every agent CLI as missing. The app asks the login shell for its PATH once (3 s timeout), merged with well-known dirs.
- **Menu open/close** is tracked through the panel window's key status (`WindowVisibility`), not `onAppear`/`onDisappear`. The registry watcher and refreshes run only while the panel is key.
- **Memory:** idle physical footprint 17 MB (what Activity Monitor shows). `ps` RSS reads ~76 MB because it counts shared system framework pages. The spec's "≤ 40 MB RSS" should say footprint.
- `NSAppleEventsUsageDescription` is set so the Ghostty/Terminal Automation prompt is attributed to Restowa when it runs `restowa restore`.

## Acceptance (to verify by hand)

Automating clicks on the menu bar item from a terminal didn't show the panel (likely hidden behind the notch on a crowded menu bar), so the UI criteria still need a manual pass:

1. `apps/macos/Restowa/build-app.sh && open apps/macos/Restowa/build/Restowa.app`
2. Panel shows Open now, Last boot, Pinned; restore works from a row and from the header button.
3. Context menu: Restore, Copy Resume Command, Reveal in Finder, Pin/Unpin, Rename (inline), Forget (inline confirm).
4. Settings: terminal and scan interval write through `restowa config set`; launch at login; Run doctor shows all checks ok.
5. With the menu closed, Activity Monitor shows 0% CPU and no `restowa` child processes.
