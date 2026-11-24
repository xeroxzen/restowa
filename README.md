# restowa

Never lose an AI coding-agent session to a restart.

restowa keeps a live registry of every Claude Code, Codex, OpenCode and Antigravity session on your Mac: which tool, which folder, which session ID. After a reboot, crash or forced macOS update, one command reopens each session in its own terminal tab, in the right folder, resumed by its exact ID.

```
$ restowa list
KEY       TOOL      PROJECT          TITLE                          STATUS  SEEN
3f9a1c2e  claude    sample-api       Add audit event schema         live    1m
b71e04d9  codex     sample-lib       Port the TCP handshake         live    1m
0c4d88aa  opencode  restowa          Scaffold adapters              closed  3h

$ restowa restore --last-boot
```

The agent CLIs already save sessions to disk. What a restart loses is the set of sessions you had open. restowa tracks that set for you.

## Install

```sh
brew tap xeroxzen/tap
brew trust --formula xeroxzen/tap/restowa   # Homebrew 7+ asks you to trust third-party taps
brew install restowa
restowa install --all
restowa doctor
```

`restowa install --all` does two things:

- Merges `SessionStart`/`SessionEnd` hooks into `~/.claude/settings.json`, so Claude Code sessions are recorded exactly. Existing hooks are kept and a backup is written first.
- Loads a launchd agent that runs `restowa scan` every 2 minutes and exits. There is no resident daemon.

From source (Rust stable):

```sh
cargo install --path crates/restowa-cli
```

## Use

| Command | What it does |
| --- | --- |
| `restowa list [--live \| --last-boot \| --pinned \| --all]` | Sessions with tool, project, title, status and last seen. Default `--live` |
| `restowa restore --last-boot` | Reopen everything that was open before the last restart |
| `restowa restore KEY...` | Reopen specific sessions. Keys accept any unique ID prefix |
| `restowa restore --dry-run` | Print the commands without opening anything |
| `restowa focus KEY` | Bring a running session's Ghostty or Terminal.app tab to the front |
| `restowa pin KEY` / `unpin KEY` | Always include a session in restore sets |
| `restowa rename KEY "title"` | Set a title (titles default to the first prompt) |
| `restowa forget KEY` | Hide a session from restowa |
| `restowa doctor` | Check tools, hooks, the launchd agent, the registry and the terminal backend |
| `restowa config [path \| edit \| set]` | Show or change `config.toml` |
| `restowa uninstall [--purge]` | Remove hooks and the launchd agent; `--purge` also deletes the registry |

Every command takes `--json`.

Restore opens tabs in **Ghostty** (default, needs 1.3+ with `macos-applescript` enabled) or **Terminal.app** (`--terminal terminal`, or `restowa config set --terminal terminal`). The first restore asks for macOS Automation permission.

## Menu bar app

`apps/macos/Restowa` is a small SwiftUI menu bar app: open sessions grouped by project, the last-boot set with a one-click restore, pinned sessions, search, and per-session actions. Click a session to switch to its tab if it is running, or to reopen it if it is closed. It has no logic of its own and calls the `restowa` CLI for everything.

Build it with the Xcode Command Line Tools (macOS 14+):

```sh
apps/macos/Restowa/build-app.sh
open apps/macos/Restowa/build/Restowa.app
```

## Supported tools

| Tool | How sessions are identified | Resume command |
| --- | --- | --- |
| Claude Code | Hooks, plus `~/.claude/sessions/<pid>.json` | `claude --resume <id>` |
| Codex | Open rollout file, else `state_5.sqlite` | `codex resume <id>` |
| OpenCode | `opencode.db` | `opencode --session <id>` |
| Antigravity | Presence lock file, else last conversation per folder | `agy --conversation <id>` |

Resume commands are templates in `config.toml`, so a CLI that changes its flags can be fixed without a new release. When two sessions of one tool share a folder and restowa can't tell them apart for certain, it marks the match `(best guess)` instead of guessing silently.

## Guarantees

- Never writes, moves or deletes anything in `~/.claude`, `~/.codex`, or OpenCode's or Antigravity's data folders. The one exception is the hook merge into `~/.claude/settings.json`, which is backed up first.
- Never kills, signals or attaches to agent processes. It only observes them.
- No network access, no analytics.
- The Claude Code hook finishes in under 50 ms, never prints to stdout and always exits 0.

## How it works

A single Rust binary records sessions from Claude Code hooks and from a 2-minute launchd scan. The scan matches running agent processes to sessions by `argv`, executable path, cwd and open files. Everything lives in one SQLite file, `~/Library/Application Support/restowa/registry.db`, which also stores the boot ID each session was last seen live in. That boot ID is what lets `--last-boot` restore exactly the set that was open before the restart. See [`docs/spec.md`](docs/spec.md) for the full design.

macOS only for now.

## License

MIT
