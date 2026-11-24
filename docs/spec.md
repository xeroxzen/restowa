# restowa — Build Spec v0.1

Nov 22, 2026 · @Andile Mbele

## Overview

restowa keeps a live registry of every AI coding-agent session on a Mac, so a restart never costs you a session. After a reboot, one click (or `restowa restore --last-boot`) reopens each session in a terminal tab, in the right folder, resumed by its exact session ID.

**Problem.** Claude Code, Codex, OpenCode and Antigravity already persist sessions to disk. What gets lost on restart is the set of sessions that were open: which tool, which folder, which session ID. Tracking that by hand fails once you run 5+ sessions that live for days.

**Goals**

- Capture every session automatically, with zero manual steps, including crashes and forced macOS update restarts.
- Restore all sessions that were open at last shutdown with one action.
- Be as light as possible: no resident daemon; the Rust binary uses a few MB per run.
- Never modify or delete any tool's session data.

**Non-goals (v1)**

- Linux and Windows (design for them, do not build them).
- Restoring terminal layout (splits, window positions). Tabs only.
- Syncing sessions across machines.
- Managing session content (search, export, summarise).

**Target user.** A developer on macOS running several agent CLIs in Ghostty, Terminal.app or iTerm2, who restarts weekly.

## Architecture

One Rust binary does everything except the UI; a SQLite file is the single source of truth that the CLI and the menu bar app both read.

| Component | Responsibility | Runs when |
| --- | --- | --- |
| Hook handler (`restowa hook …`) | Records Claude Code sessions exactly, from hook JSON on stdin | On every Claude Code session start/end |
| Scanner (`restowa scan`) | Matches running agent processes to sessions; marks sessions live or closed; stamps the current boot ID | Every 2 min via launchd, then exits |
| Registry | SQLite at `~/Library/Application Support/restowa/registry.db`, WAL mode | Always on disk; never a server |
| Adapters | One per tool: discover sessions, read metadata, build the resume command | Inside scan, hook and restore |
| Restorer (`restowa restore`) | Opens one terminal tab per session and runs the resume command in its folder | On demand |
| CLI | `list`, `restore`, `pin`, `forget`, `install`, `doctor` | On demand |
| Menu bar app | Shows sessions; calls the CLI to restore | At login, idle otherwise |

**Key rule: no long-running Rust process.** Everything is short-lived and triggered by a hook, launchd or a user action.

**How “open at last shutdown” works.** Each scan stamps live sessions with the current boot ID (from `sysctl kern.boottime`). After a reboot, the restore set is every session last seen live under the previous boot ID. This works for clean restarts, crashes and forced update restarts alike, because nothing has to run at shutdown.

&#91;embedded content: restowa architecture · 6 components\]

The hook and scanner are the only writers; the menu bar app never writes, it calls the CLI, which owns restore.

## Tech stack

Rust for the core and CLI, SwiftUI for the menu bar app; no async runtime, no Electron, no web view.

| Area | Choice | Notes |
| --- | --- | --- |
| Language | Rust, stable, edition 2021 | Single binary `restowa` |
| CLI parsing | `clap` (derive) |  |
| Registry | `rusqlite` with `bundled` | WAL mode, `busy_timeout` 2 s |
| Process info | `libproc` + `sysctl` | Process list, cwd (`pidcwd`), open files, `pbi_start_*` via `libproc`; `argv` (`KERN_PROCARGS2`) and boot ID (`KERN_BOOTTIME`) via `sysctl`. Do not shell out to `lsof` or `ps` |
| Serialisation | `serde`, `serde_json`, `toml` |  |
| Paths | `directories` |  |
| Errors | `anyhow` (bin), `thiserror` (core) |  |
| Logging | `log` + a tiny file logger | Log to `~/Library/Logs/restowa/restowa.log`, rotate at 1 MB |
| UI | SwiftUI `MenuBarExtra`, macOS 14+ | Calls the CLI for every read and write; never opens SQLite |

**Not allowed without a written reason:** `tokio` or any async runtime, `reqwest` or any network crate, `sysinfo` (use `libproc` first).

**Release profile**

```toml
[profile.release]
opt-level = 3
lto = true
codegen-units = 1
strip = true
panic = "abort"
```

**Budgets:** binary ≤ 5 MB; `restowa scan` ≤ 10 MB peak RSS and ≤ 150 ms with 10 live sessions; menu bar app ≤ 40 MB RSS idle.

## Data model

Two tables and a schema version are enough for v1; migrations are plain numbered SQL files run at startup.

```sql
PRAGMA journal_mode = WAL;
PRAGMA user_version = 2;

CREATE TABLE sessions (
  key            TEXT PRIMARY KEY,      -- "<tool>:<session_id>"
  tool           TEXT NOT NULL,         -- claude | codex | opencode | antigravity
  session_id     TEXT NOT NULL,
  cwd            TEXT NOT NULL,         -- absolute path the session runs in
  title          TEXT,                  -- user-set, else first prompt (≤ 80 chars)
  transcript     TEXT,                  -- path to the tool's session file, if known
  pid            INTEGER,               -- last known process, NULL when closed
  pid_start_us   INTEGER,               -- process start time (unix µs) for PID-reuse detection, NULL when closed
  status         TEXT NOT NULL,         -- live | closed | missing
  source         TEXT NOT NULL,         -- hook | scan
  first_seen     INTEGER NOT NULL,      -- unix seconds
  last_seen_live INTEGER,               -- unix seconds
  last_boot_id   TEXT,                  -- boot ID when last seen live
  ended_by_user  INTEGER NOT NULL DEFAULT 0, -- 1 when user exited (prompt_input_exit, logout, clear)
  best_guess     INTEGER NOT NULL DEFAULT 0, -- 1 when scan fallback was ambiguous
  pinned         INTEGER NOT NULL DEFAULT 0,
  hidden         INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_sessions_boot ON sessions(last_boot_id);
CREATE INDEX idx_sessions_cwd  ON sessions(cwd);

CREATE TABLE boots (
  boot_id      TEXT PRIMARY KEY,        -- kern.boottime as "<sec>.<usec>"
  first_seen   INTEGER NOT NULL,
  last_scan_at INTEGER                  -- unix seconds of the most recent scan in this boot
);
```

**Rules**

- `status = missing` means the tool's session file is gone; never offer it for restore.
- `boots.last_scan_at` is updated on every scan pass.
- "Previous boot" (`prev`) is the most recent earlier boot with at least one eligible session.
- A `SessionEnd` hook with `reason` in `prompt_input_exit`, `logout`, or `clear` sets `ended_by_user = 1` and is always excluded from the automatic restore set. Reason `other`, or no `SessionEnd` (e.g. crash or OS shutdown), leaves `ended_by_user = 0` so the session stays eligible. Resuming or seeing the session live again resets `ended_by_user = 0`.
- The last-boot restore set is `status IN ('live','closed') AND last_boot_id = prev.boot_id AND hidden = 0 AND ended_by_user = 0 AND last_seen_live >= prev.last_scan_at - (scan_interval + 30s)`, unioned with all non-hidden, non-missing pinned sessions (`pinned = 1`).
- Scans only ever change `pid`, `pid_start_us`, `status`, `ended_by_user`, `best_guess`, `last_seen_live` and `last_boot_id`. User fields (`title`, `pinned`, `hidden`) change only through the CLI.
- Writes happen in one short transaction per scan or hook; the UI opens the file read-only.

## Tool adapters

Each tool gets one adapter behind a common trait. Paths and flags below are the starting assumptions; Phase 0 verifies each one against the installed CLI before any code depends on it.

```rust
pub trait Adapter {
    fn tool(&self) -> Tool;
    /// Is this running process one of ours? (match on executable path and argv only — never process name/comm)
    fn matches_process(&self, p: &ProcInfo) -> bool;
    /// Given a live process, return the session it is running, if knowable.
    fn session_for_process(&self, p: &ProcInfo) -> Result<Option<SessionRef>>;
    /// Does the tool's session data still exist?
    fn session_exists(&self, s: &Session) -> bool;
    /// Optional: title from the first user prompt.
    fn title(&self, s: &Session) -> Option<String>;
    /// Command to resume, run inside s.cwd.
    fn resume_command(&self, s: &Session, cfg: &ToolConfig) -> String;
}
```

| Tool | How the session is identified | Session data (verified in Phase 0) | Resume command (default template) |
| --- | --- | --- | --- |
| Claude Code | Exact: `SessionStart` hook (`session_id`, `cwd`, `transcript_path`), `~/.claude/sessions/<pid>.json`, or `--resume <id>` in `argv` | `~/.claude/projects/<encoded-cwd>/<id>.jsonl` and `~/.claude/sessions/<pid>.json` | `claude --resume {id}` |
| Codex | Exact: `resume <id>` in `argv` or open `rollout-*.jsonl` FD via `libproc`; fallback: newest active CLI thread in `state_5.sqlite` (or `sessions/**/*.jsonl`) for `cwd` | `~/.codex/state_5.sqlite` (`threads`) and `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | `codex resume {id}` |
| OpenCode | Exact: `--session <id>` / `-s <id>` in `argv`; fallback: newest active top-level session in `opencode.db` for `directory = cwd` | `~/.local/share/opencode/opencode.db` (SQLite WAL: `session`, `message`, `part`) | `opencode --session {id}` |
| Antigravity | Exact: open FD on `presence/<id>.lock` via `libproc` or `--conversation <id>` in `argv`; fallback: `cache/last_conversations.json[cwd]` | `~/.gemini/antigravity-cli/conversations/<id>.db` and `conversation_summaries.db` | `agy --conversation {id}` |

**Rules for every adapter**

- Read-only. Never write, move or delete anything in a tool’s data folder.
- Open other tools’ SQLite databases read-only, with short queries, no open transactions, and no writes.
- If a table or schema is missing, fall back to the next detection method and report it in `restowa doctor` — never crash.
- Identify processes by `argv` and executable path only, never by process name (`comm`).
- Parse only what is needed (ID, cwd, first prompt). Read session files line by line and stop early; some are hundreds of MB.
- Resume commands come from `config.toml`, so a CLI flag change is a config edit, not a release:

```toml
[tools.claude]
resume = "claude --resume {id}"

[tools.codex]
resume = "codex resume {id}"

[tools.opencode]
resume = "opencode --session {id}"

[tools.antigravity]
resume = "agy --conversation {id}"
```

- Ambiguous fallbacks (two sessions in one folder, no exact match) are recorded with `source = scan`, and the UI marks them “best guess”.

## CLI

Every command supports `--json` for the menu bar app and scripts. Exit codes: 0 ok, 1 error, 2 nothing to do.

| Command | What it does |
| --- | --- |
| `restowa list [--live \| --last-boot \| --pinned \| --all] [--tool T] [--json]` | Table of sessions: short ID, tool, project (cwd basename), title, status, last seen. Default `--live` |
| `restowa restore [KEY...] [--last-boot] [--terminal ghostty\|terminal\|iterm] [--dry-run]` | Opens one tab per session. `--dry-run` prints the commands only |
| `restowa focus KEY` | Bring a running session's tab to the front: reads the process's tty and selects the Ghostty or Terminal.app tab with that tty. Never launches either app |
| `restowa scan` | One scan pass (what launchd runs). Silent unless `-v` |
| `restowa hook claude <session-start\|session-end>` | Reads hook JSON from stdin, upserts the session. Must finish in < 50 ms and always exit 0 so it never blocks Claude Code |
| `restowa pin KEY` / `unpin KEY` | Always include in restore sets |
| `restowa rename KEY "title"` | Set a title |
| `restowa forget KEY` | Set `hidden = 1` (never deletes tool data) |
| `restowa install [--hooks] [--launchd] [--all]` | Merge hooks into `~/.claude/settings.json` (backup first) and load the launchd agent |
| `restowa uninstall` | Reverse `install`; keep the registry unless `--purge` |
| `restowa doctor` | Checks: tools found, hook installed, launchd loaded, registry writable, terminal backend available |
| `restowa config [path\|edit]` | Print or open `config.toml` |

**KEY** accepts the full `tool:session_id` or any unique prefix of the session ID (like git hashes).

**Example `restowa list` output**

```
KEY       TOOL      PROJECT          TITLE                          STATUS  SEEN
3f9a1c2e  claude    sample-api   Add audit event schema         live    1m
b71e04d9  codex     sample-lib       Port the TCP handshake        live    1m
0c4d88aa  opencode  restowa          Scaffold adapters              closed  3h
```

## Capture

Hooks give exact IDs for Claude Code; the 2-minute scanner covers every tool and keeps the live/closed state honest.

**Claude Code hooks.** `restowa install --hooks` merges this into `~/.claude/settings.json`, keeping any existing hooks and writing `settings.json.restowa.bak` first:

```json
{
  "hooks": {
    "SessionStart": [
      { "matcher": "", "hooks": [ { "type": "command", "command": "restowa hook claude session-start" } ] }
    ],
    "SessionEnd": [
      { "matcher": "", "hooks": [ { "type": "command", "command": "restowa hook claude session-end" } ] }
    ]
  }
}
```

The handler reads `session_id`, `cwd` and `transcript_path` from stdin, upserts the row with `source = hook`, and stores the parent PID and its start time (`pid_start_us`) as `pid` / `pid_start_us`. On `session-end` it sets `status = closed`. It must never print to stdout (Claude injects `SessionStart` stdout into context) or exit non-zero. Confirmed in Phase 0 (`docs/phase0.md`).

**Scanner.** A per-user launchd agent runs `restowa scan` every 120 s and at login:

```xml
<!-- ~/Library/LaunchAgents/dev.restowa.scan.plist -->
<plist version="1.0"><dict>
  <key>Label</key><string>dev.restowa.scan</string>
  <key>ProgramArguments</key>
  <array><string>/opt/homebrew/bin/restowa</string><string>scan</string></array>
  <key>StartInterval</key><integer>120</integer>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>LowPriorityIO</key><true/>
</dict></plist>
```

**One scan pass**

1. Read the boot ID; insert it into `boots` if new.
2. List processes with `libproc`; keep those any adapter matches.
3. For each, resolve the session (adapter) and upsert: `status = live`, `pid`, `last_seen_live = now`, `last_boot_id = current`.
4. Any row marked `live` whose PID is gone or reused becomes `closed`. Check PID reuse by comparing process start time.
5. Any row whose session data no longer exists becomes `missing`.
6. Exit. Target ≤ 150 ms.

## Restore

Restore is a pluggable terminal backend; Ghostty is the default, with native AppleScript tab support confirmed in Phase 0 (`Ghostty 1.4.0`, `Ghostty.sdef`).

```rust
pub trait TerminalBackend {
    fn name(&self) -> &'static str;
    fn available(&self) -> bool;
    /// Open sessions grouped by project; one window per project, one tab per session.
    fn open(&self, groups: &[ProjectGroup]) -> Result<RestoreReport>;
}
```

| Backend | Mechanism | Tabs? | Status |
| --- | --- | --- | --- |
| Ghostty | Native AppleScript (`Ghostty.sdef`: `new surface configuration` with `initial working directory` + `initial input`, `new window`, `new tab in win`) | Yes (verified on 1.4.0) | Default |
| Terminal.app | AppleScript via `osascript` (`do script` in a new tab) | Yes | Fallback |
| iTerm2 | AppleScript (`create tab with default profile`, `write text`) | Yes | Fallback |

**Restore behaviour**

1. Group sessions by `cwd`; order groups by most recent activity.
2. Each tab runs `cd <cwd> && <resume command>` inside the user’s login shell, so PATH and nvm/asdf shims load.
3. Shell-escape every path and ID. Never build commands by naive string concatenation.
4. Skip sessions that are already live (a matching process exists) and say so.
5. If `cwd` no longer exists, skip the session and report it; do not create the folder.
6. Print a report: restored, skipped (already live), failed (with reason).

**Permissions.** The first AppleScript call triggers macOS’s Automation prompt for the chosen terminal. `restowa doctor` explains this. Never request Full Disk Access or Accessibility.

## Menu bar app

A thin SwiftUI `MenuBarExtra` built with SwiftPM plus a `.app` bundle build script under `apps/macos/Restowa/`. It contains no capture, restore, or SQLite logic of its own: it never opens `registry.db` directly and routes every read and write through the `restowa` CLI.

**Layout (window-style menu, \~360 pt wide)**

1. Header: “Restore N sessions from last boot” button, shown only when the last-boot set has sessions that are not live.
2. Section **Open now**: sessions grouped by project, each row showing a tool icon, title, `(best guess)` indicator when applicable, and relative time.
3. Section **Last boot**: same layout, each row with a Restore button.
4. Section **Pinned**.
5. Footer: search field, Settings, Quit.

**Clicking a row** asks first, inline: “Switch to …?” for a running session (`restowa focus`) or “Open …?” for a closed one (`restowa restore`).

**Row actions** (the ⋯ button on hover, or right-click): Switch to Tab (running) or Restore (closed), Copy resume command, Reveal folder in Finder, Pin/Unpin, Rename, Forget.

**Settings:** default terminal backend, scan interval (1/2/5 min), launch at login (`SMAppService.mainApp`, falling back to a per-user LaunchAgent when running a locally/ad-hoc signed build), show tools (checkboxes), `restowa` binary path override, “Run doctor”.

**Implementation rules**

- Never read SQLite in Swift. When the menu opens, fetch sessions via `restowa list --live --json`, `restowa list --last-boot --json`, and `restowa list --pinned --json` through `Process`. Every write or action (`restore`, `focus`, `pin`, `unpin`, `rename`, `forget`, `doctor`) goes through the CLI.
- Refresh only while the menu is open: watch the registry file's modification time (`~/Library/Application Support/restowa/registry.db` and `-wal` via `DispatchSource`) strictly while open to trigger a CLI refresh; stop watching immediately when closed (zero polling or work while closed).
- Zero third-party Swift packages (Foundation, SwiftUI, AppKit, ServiceManagement only).
- Set `LSUIElement = true` in `Info.plist` (menu-bar-only agent app, no Dock icon).
- No web views, no background networking, no analytics.
- Locate the binary from Settings, else `/opt/homebrew/bin/restowa`, else `~/.cargo/bin/restowa`, else `/usr/local/bin/restowa`.
- Budget: ≤ 40 MB RSS idle, menu opens in < 150 ms with 50 sessions.

## Repo layout

A Cargo workspace for the core and CLI, with the macOS app as a sibling SwiftPM package + `.app` bundle script.

```
restowa/
├── Cargo.toml                  # workspace + release profile
├── crates/
│   ├── restowa-core/           # model, registry, migrations, adapters, backends
│   │   ├── src/registry/
│   │   ├── src/adapters/       # claude.rs, codex.rs, opencode.rs, antigravity.rs
│   │   ├── src/terminal/       # ghostty.rs, terminal_app.rs, iterm.rs
│   │   └── src/platform/macos/ # libproc, boot ID
│   └── restowa-cli/            # main.rs, clap commands only
├── apps/macos/Restowa/         # SwiftUI menu bar app (SwiftPM + build-app.sh)
├── packaging/
│   ├── dev.restowa.scan.plist
│   └── homebrew/restowa.rb
├── tests/fixtures/             # sample session files per tool, hook JSON
└── docs/                       # this spec, ADRs, Phase 0-4 findings
```

Keep all macOS-specific code under `platform/macos` and `terminal/` behind traits, so Linux later means new modules rather than rewrites.

## Phases and acceptance criteria

Ship in six phases; each ends with something Andile uses daily, and nothing starts before Phase 0’s findings are written down.

**Phase 0: Spikes (verify assumptions)** — *Complete (see `docs/phase0.md`)*

- [x] `docs/phase0.md` records, per tool: session data path, file format sample, how to get the ID and cwd, the exact resume command, tested by hand
- [x] Claude Code hook input fields confirmed against the current docs
- [x] Ghostty: can a script open a new tab with a set folder and command? Answer with a working snippet, or “windows only”
- [x] Antigravity: how its CLI stores and resumes sessions, or “not resumable”
- [x] `libproc` returns cwd and open files for these processes without extra permissions

**Phase 1: Claude Code end to end** — *Complete*

- [x] `restowa install --hooks` merges hooks without touching existing ones and writes a backup
- [x] Starting 3 Claude Code sessions in 2 folders produces 3 rows with exact IDs
- [x] `restowa list` shows them; `restowa restore --dry-run` prints correct commands
- [x] `restowa restore` reopens all 3 in the right folders (Ghostty AppleScript backend)
- [x] Hook handler completes in < 50 ms

**Phase 2: Scanner and the last-boot set** — *Ready for real reboot verification*

- [x] launchd agent installed via `restowa install --launchd`; `restowa doctor` reports it loaded
- [ ] Reboot test: 4 live sessions before a restart → `restowa restore --last-boot` restores exactly those 4 *(requires real macOS reboot)*
- [x] Crash test: force-quitting the terminal marks sessions closed within one scan interval, and they still appear in the last-boot set
- [x] PID reuse does not mark a dead session live
- [x] Scan ≤ 150 ms and ≤ 10 MB peak RSS with 10 sessions

**Phase 3: Codex, OpenCode and Antigravity** — *Complete*

- [x] Codex, OpenCode and Antigravity adapters pass fixture-based unit and integration tests
- [x] Two Codex sessions in the same folder resolve to the correct IDs
- [x] Ambiguous matches are flagged as best guesses

**Phase 4: Menu bar app** — *Built; awaiting manual UI verification (see `docs/phase4.md`)*

- [ ] Shows Open now, Last boot and Pinned; restore works from a row and from the header button *(sections, restore from a row, switch to tab and forget verified 2026-11-24; header button needs a reboot)*
- [ ] Launch at login works; idle RSS ≤ 40 MB
- [ ] No work happens while the menu is closed (verified in Activity Monitor)

**Phase 5: Polish and distribution** — *In progress (see `docs/releasing.md`)*

- [ ] Titles from first prompt; pin, rename and forget in CLI and UI *(done in CLI and app; ticks with the Phase 4 UI check)*
- [x] Homebrew tap `brew install xeroxzen/tap/restowa` *(clean install 2026-11-23; upgrades through v0.1.3 verified)*
- [ ] Signed and notarised app *(scripts ready: `build-app.sh` with `CODESIGN_IDENTITY`, `notarize.sh`; needs a Developer ID certificate)*
- [ ] README with a 30-second GIF *(README done; GIF after the Phase 4 UI check)*

## Risks

Phase 0 resolved the two design-shaping risks (Ghostty native tabs and Antigravity resumability both work) and surfaced one new operational risk around self-updating binaries and background daemons.

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Ghostty AppleScript disabled (`macos-applescript = false`) or older Ghostty (< 1.3) | Tab scripting fails | `restowa doctor` checks `macos-applescript`; fall back to Terminal.app or Ghostty windows |
| Antigravity CLI storage or flags change | Adapter breaks | Resume template in `config.toml`; fixture tests + `restowa doctor` checks |
| Self-updating CLIs (`claude`, `agy`) unlink/rename binaries; IDE daemons share binary names | Scanner misses long-lived sessions or falsely captures `agy --hub`, `agy acp`, `codex app-server`, `claude --chrome-native-host` | Match via `KERN_PROCARGS2` (`argv`) and executable path only (never process name/`comm`); explicitly filter daemon/helper subcommands and `.app` bundles |
| Tool CLIs change paths or flags | Capture or restore breaks silently | Resume templates in config; `restowa doctor` checks each tool; fixture tests per adapter |
| Two sessions of one tool in one folder | Wrong session resumed | Exact IDs from hooks, `sessions/<pid>.json`, `presence/<id>.lock`, open rollouts, or `argv`; fall back to “best guess” flag, never silently |
| Claude Code hook slows session start | Bad experience, users uninstall | < 50 ms budget, no network, never write stdout, always exit 0 |
| macOS Automation prompts confuse users | Restore appears broken | `doctor` explains it; first restore prints what to approve |
| Huge session files | Slow scans | Read only the first lines; never load whole files |

## Working agreement for Gemini Neon

Build one phase at a time, prove it against the acceptance criteria, and stop for review before the next phase.

1. Start with Phase 0. Write findings to `docs/phase0.md`; if a finding contradicts this spec, update the spec and flag it. Do not work around it silently.
2. Never write, move or delete anything inside `~/.claude`, `~/.codex`, OpenCode’s or Antigravity’s data folders. The one exception is the hook merge into `~/.claude/settings.json`, with a backup first.
3. Never kill, signal or attach to agent processes. Observe only.
4. No new dependency outside the stack table without a one-line justification in the PR.
5. Every adapter has fixture-based unit tests in `tests/fixtures/`; no test touches the real home folder (use a temp `HOME`).
6. Check performance budgets in CI where possible (binary size, scan time on fixtures).
7. `cargo fmt`, `cargo clippy -D warnings` and tests pass before each commit; conventional commit messages.
8. Keep macOS-specific code behind the `platform` and `terminal` traits.
9. When unsure about a tool’s behaviour, test it by hand and write down what you saw rather than guessing.
