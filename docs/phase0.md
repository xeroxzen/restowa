# Phase 0 — Spike Findings

Date: 2026-11-22  
Environment: macOS 27.0.1 (`26A434`, `arm64`), `rustc 1.96.1`, `cargo 1.96.1`, Apple Swift 6.4 (CLT; no `/Applications/Xcode.app` installed).

All tool inspections were strictly read-only against real user data folders (`~/.claude`, `~/.codex`, `~/.local/share/opencode`, `~/.gemini`). SQLite databases were inspected on temporary copies in `/tmp` so WAL/SHM headers in tool folders were never touched. No running agent processes were signaled or attached to.

---

## Summary of Outcomes

| Spike | Result | Impact on Spec |
| --- | --- | --- |
| **Claude Code (`2.1.330`)** | Verified (`claude --resume {id}`) | **Bonus:** `~/.claude/sessions/<pid>.json` provides exact live PID → `sessionId` + `cwd` even without hooks. **Pitfall:** native builds have `pbi_comm = <version>` (e.g. `2.1.328`) and `proc_pidpath` fails after auto-updates unlink old versions; must read `KERN_PROCARGS2` (`argv`). |
| **Codex (`0.171.0`)** | Verified (`codex resume {id}`) | Stores both legacy `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` and `~/.codex/state_5.sqlite` (`threads` table). Must exclude `app-server` / `exec-server` / `mcp` / `sandbox` / `remote-control` subcommands and npm `node` wrapper. |
| **OpenCode (`1.19.5`)** | Verified (`opencode --session {id}`) | **Contradiction:** uses SQLite WAL DB `~/.local/share/opencode/opencode.db` (`session`, `message`, `part`), not flat JSON files. No per-session open FD; exact ID only when `--session`/`-s` is in `argv`, else newest active top-level session for `directory = cwd`. |
| **Antigravity (`agy 1.5.5`)** | **Resumable!** (`agy --conversation {id}`) | **Spec update:** fully supported and **exact**! Live `agy` holds an open FD on `~/.gemini/antigravity-cli/presence/<id>.lock`; metadata in `conversation_summaries.db` and `conversations/<id>.db`. Must exclude `agy acp` and `agy --hub`. |
| **Ghostty (`1.4.0`)** | **Native tabs work!** | **Spec update:** Ghostty 1.4.0 ships `Ghostty.sdef` (`macos-applescript = true` by default) with `new surface configuration`, `new window`, and `new tab in <window>`. Verified live opening 1 window with 2 tabs in distinct folders. |
| **`libproc` + `sysctl`** | Verified, zero extra permissions | Full scan of ~923 processes in **18.22 ms** (budget: ≤ 150 ms). **Spec gap:** `sessions` table needs `pid_start_us` to persist process start time across scans for PID-reuse detection. |

---

## 1. Claude Code (`2.1.330`)

- **Binary:** `~/.local/bin/claude` → symlink to `~/.local/share/claude/versions/<version>` (e.g. `2.1.330`).
- **Resume command:** `claude --resume {id}` (`-r, --resume [value]`). Verified in `claude --help` and observed live on running sessions (`PID 2849`, `PID 3150`).
- **Session data paths:**
  1. **Transcripts:** `~/.claude/projects/<encoded-cwd>/<session_id>.jsonl`
     - Path encoding replaces every character outside `[A-Za-z0-9-]` (including `/`, `.`, `_`, and spaces) with `-`.
     - Verified examples:
       - `/Users/dev/Developer/Personal/example.com` → `-Users-dev-Developer-Personal-example-com`
       - `/Users/dev/Developer/Side Projects/demo-app` → `-Users-dev-Developer-Side-Projects-demo-app`
     - Because encoding is lossy, **never decode directory names back to `cwd`**; read `cwd` from hook JSON, `sessions/<pid>.json`, or transcript lines.
  2. **Live PID registry (new finding):** `~/.claude/sessions/<pid>.json`
     - Observed 7 live interactive `claude` PIDs on this machine; all 7 had matching `~/.claude/sessions/<pid>.json` files.
     - Gives exact `sessionId`, `cwd`, `startedAt`, `kind`, `entrypoint`, `name`, and `status` even when a session was started before hooks were installed or under `--bare` / `--safe-mode`.

### File format samples

**`~/.claude/sessions/<pid>.json`:**
```json
{
  "pid": 3964,
  "sessionId": "5e45591c-0000-4000-8000-000000000001",
  "cwd": "/Users/dev/Developer/Projects/sample-app",
  "startedAt": 1795075674950,
  "procStart": "Thu Nov 19 08:07:53 2026",
  "version": "2.1.326",
  "kind": "interactive",
  "entrypoint": "cli",
  "name": "sample-app-29",
  "status": "shell",
  "updatedAt": 1795223333092
}
```

**`~/.claude/projects/<encoded-cwd>/<session_id>.jsonl` (first 5 lines, trimmed):**
```json
{"type":"mode","mode":"normal","sessionId":"a5641cc1-0000-4000-8000-000000000003"}
{"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"a5641cc1-0000-4000-8000-000000000003"}
{"type":"atis-latch","atis":"c854faeb5c5d9d1f","sessionId":"a5641cc1-0000-4000-8000-000000000003"}
{"type":"file-history-snapshot","messageId":"b2a35b06-...","snapshot":{...},"isSnapshotUpdate":false}
{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"Summarize this repository."},"cwd":"/Users/dev/Developer/Side Projects/demo-app","sessionId":"a5641cc1-0000-4000-8000-000000000003","entrypoint":"cli"}
```

### Process matching & live ID resolution

- **Critical pitfall discovered under `libproc`:**
  - `pbi_comm` is the version number (e.g. `2.1.328`), **not** `claude`.
  - `proc_pidpath` returns `~/.local/share/claude/versions/2.1.328` while that version exists, and **fails (`ENOENT`/error)** once Claude auto-updates and deletes older versions (`PID 2849`, `3150`, `43196` all returned `exe: <err>`!).
  - `KERN_PROCARGS2` (`argv[0]`) remains intact (`claude` or `/.../claude`).
- **Match rule:** basename of `argv[0]` is `claude` OR `exe` contains `/claude/versions/`, **excluding**:
  - `exe` inside `/Applications/Claude.app/` (Desktop app)
  - `argv` containing `--chrome-native-host` (Chrome extension host; observed `PID 80635`)
  - Non-interactive one-shot invocations (`-p` / `--print`)
- **Live ID resolution order:**
  1. Exact: `~/.claude/sessions/<pid>.json` (`sessionId`, `cwd`; verify `kind == "interactive"` or `entrypoint == "cli"`).
  2. Exact: `argv` contains `--resume <uuid>` / `-r <uuid>`.
  3. Exact: already recorded by `SessionStart` hook for `(pid, pid_start_us)`.
  - Note: Claude Code does **not** hold its `.jsonl` transcript open persistently (`libproc` showed 0 `.jsonl` FDs on all 7 live processes).
- **Title extraction:** read `.jsonl` line-by-line until the first record with `"type": "user"` and string/text content in `message.content` (stop early; never load full file).

### Claude Code Hook Verification (Official Docs)

Confirmed against official Claude Code hook documentation (`code.claude.com` / `claude-code-settings.schema.json`):

- **Common stdin JSON fields (`BaseHookInput`):**
  - `session_id` (string)
  - `transcript_path` (string, absolute path to `.jsonl`)
  - `cwd` (string, absolute working directory)
  - `hook_event_name` (string: `"SessionStart"` or `"SessionEnd"`)
  - `permission_mode` (string, optional)
- **`SessionStart` extra fields:** `source` (string, e.g. `"startup"`, `"resume"`, `"clear"`, `"compact"`), `model` (string).
  - **Important:** `stdout` from `SessionStart` hooks is ingested into Claude's context window → `restowa hook` must **never** write to stdout and must always exit `0`.
- **`SessionEnd` extra fields:** `reason` (string, e.g. `"clear"`, `"logout"`, `"prompt_input_exit"`, `"other"`). Only fires on clean exit (never on crash or reboot), confirming the scanner is required.
- **Settings schema (`~/.claude/settings.json`):** `SessionStart` and `SessionEnd` are arrays of `HookMatcher` objects. Include `"matcher": ""` (matches all events):
  ```json
  {
    "hooks": {
      "SessionStart": [
        {
          "matcher": "",
          "hooks": [
            { "type": "command", "command": "restowa hook claude session-start" }
          ]
        }
      ],
      "SessionEnd": [
        {
          "matcher": "",
          "hooks": [
            { "type": "command", "command": "restowa hook claude session-end" }
          ]
        }
      ]
    }
  }
  ```
- **Existing `~/.claude/settings.json` on this machine:** valid JSON object with no `"hooks"` key yet (contains `model`, `enabledPlugins`, `modelSettings`, `tui`, `theme`, etc.).

---

## 2. Codex (`codex-cli 0.171.0`)

- **Binary:** `/opt/homebrew/bin/codex` → Node wrapper (`codex.js`) that spawns `.../vendor/aarch64-apple-darwin/bin/codex`. (Standalone installs also place a native binary at `~/.codex/packages/standalone/current/bin/codex`.)
- **Resume command:** `codex resume {id}` (verified via `codex resume --help` and PTY test with `CODEX_HOME=/tmp/...`).
- **Session data paths (`CODEX_HOME` defaults to `~/.codex`):**
  1. **SQLite state DB:** `~/.codex/state_5.sqlite` (WAL mode) — `threads` table.
  2. **Legacy / rollout transcripts:** `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<uuid>.jsonl` (verified against `codex migrate-rollouts --verbose --json` using a synthetic rollout in an isolated `CODEX_HOME`).

### File format samples

**`~/.codex/state_5.sqlite` (`threads` table, key columns):**
```sql
CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    rollout_path TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    source TEXT NOT NULL,              -- 'cli', 'vscode', etc.
    cwd TEXT NOT NULL,
    title TEXT NOT NULL,
    archived INTEGER NOT NULL DEFAULT 0,
    first_user_message TEXT NOT NULL DEFAULT '',
    updated_at_ms INTEGER,
    preview TEXT NOT NULL DEFAULT '',
    recency_at_ms INTEGER NOT NULL DEFAULT 0,
    name TEXT
);
```

**`~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<uuid>.jsonl` (lines 1–2):**
```json
{"timestamp":"2026-11-22T20:00:00.000Z","type":"session_meta","payload":{"id":"019a0000-0000-7000-8000-000000000001","timestamp":"2026-11-22T20:00:00.000Z","cwd":"/tmp/restowa-phase0-cx/work","originator":"codex_cli_rs","cli_version":"0.171.0","source":"cli"}}
{"timestamp":"2026-11-22T20:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello from phase0 test"}]}}
```
Verified by hand: `codex migrate-rollouts --verbose --json` recognized this exact path and JSONL structure (`"status": "eligible"`).

### Process matching & live ID resolution

- **Observed under `libproc`:**
  - Interactive binary: `comm = codex`, `exe = .../vendor/aarch64-apple-darwin/bin/codex`, `argv = [.../codex, resume, <uuid>]`.
  - Parent wrapper from npm/Homebrew is `node .../codex.js` (`comm = node`), which must be ignored so sessions aren't double-counted.
  - Background IDE daemon observed live (`PID 21398`): `.../.cursor/extensions/openai.chatgpt-.../bin/macos-aarch64/codex ... app-server ...`.
- **Match rule:** `comm == "codex"` (or basename of `exe`/`argv[0]` is `codex`, not `node`), **excluding**:
  - `argv` containing subcommands `app-server`, `exec-server`, `exec`, `review`, `mcp`, `sandbox`, `remote-control`, `debug`, `doctor`, `login`, `logout`, `update`.
  - `exe` inside `.cursor/extensions/` or IDE extension directories.
- **Live ID resolution order:**
  1. Exact: `argv` contains `resume <uuid>`.
  2. Exact: open FD via `libproc` matching `rollout-*-<uuid>.jsonl`.
  3. Fallback (best-guess if >1 active candidate in `cwd`): query `~/.codex/state_5.sqlite` read-only (`SELECT id, rollout_path, cwd, title, first_user_message FROM threads WHERE archived = 0 AND cwd = ? AND source = 'cli' ORDER BY updated_at_ms DESC`).
  4. Fallback if `state_5.sqlite` has no match: scan newest `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` reading only line 1 (`session_meta`) for matching `cwd`.
- **Session existence check:** `rollout_path` exists on disk OR row exists in `state_5.sqlite` with `archived = 0`.
- **Title extraction:** `threads.name` / `threads.title` / `threads.first_user_message`, else first `response_item` user text in `rollout-*.jsonl`.

---

## 3. OpenCode (`1.19.5`)

- **Binary:** `~/.opencode/bin/opencode`.
- **Resume command:** `opencode --session {id}` (`-s, --session`). Verified via `opencode --help`, `opencode session list`, and live PTY launch with `--session ses_0505f795dffewGmC0K9kioVBAP`.
- **Session data path:** `~/.local/share/opencode/opencode.db` (confirmed via `opencode db path`; WAL-mode SQLite DB).
  - **Contradiction with v0.1 spec:** OpenCode 1.19.5 no longer uses loose JSON files under `~/.local/share/opencode/storage/`; all session metadata and messages live in `opencode.db`.

### Database schema sample

```sql
CREATE TABLE `session` (
  `id` text PRIMARY KEY,           -- e.g. "ses_0505f795dffewGmC0K9kioVBAP"
  `project_id` text NOT NULL,
  `workspace_id` text,
  `parent_id` text,                -- NULL for top-level sessions
  `slug` text NOT NULL,
  `directory` text NOT NULL,       -- absolute cwd
  `path` text,
  `title` text NOT NULL,           -- e.g. "Fetch latest email" or "New session - <iso>"
  `time_created` integer NOT NULL, -- unix ms
  `time_updated` integer NOT NULL, -- unix ms
  `time_archived` integer
);

CREATE TABLE `message` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `time_created` integer NOT NULL,
  `data` text NOT NULL             -- JSON: {"role":"user", ...}
);

CREATE TABLE `part` (
  `id` text PRIMARY KEY,
  `message_id` text NOT NULL,
  `session_id` text NOT NULL,
  `data` text NOT NULL             -- JSON: {"type":"text","text":"..."}
);
```

### Process matching & live ID resolution

- **Observed under `libproc` (isolated `XDG_DATA_HOME`):**
  - `exe = ~/.opencode/bin/opencode`, `comm = opencode`, `cwd = /private/tmp/restowa-phase0-oc/work`.
  - Open FDs: `opencode.db`, `opencode.db-wal`, `opencode.db-shm`, `log/opencode.log` (no per-session FD).
- **Match rule:** `comm == "opencode"` or basename of `exe`/`argv[0]` is `opencode`, **excluding** non-TUI subcommands (`acp`, `serve`, `web`, `run`, `mcp`, `db`, `session`, `export`, `import`, `stats`, `models`, `upgrade`, `uninstall`).
- **Live ID resolution order:**
  1. Exact: `argv` contains `--session <id>` or `-s <id>` (always true for sessions restored by `restowa`).
  2. Fallback: query `opencode.db` read-only for `WHERE directory = ? AND parent_id IS NULL AND time_archived IS NULL ORDER BY time_updated DESC`. Mark as best-guess if multiple OpenCode sessions exist in the same directory.
- **Session existence check:** `SELECT 1 FROM session WHERE id = ? AND time_archived IS NULL` in `opencode.db`.
- **Title extraction:** `session.title` if it does not start with `"New session - "`; otherwise first text `part` of the earliest `"role":"user"` `message` for that `session_id`.

---

## 4. Antigravity CLI (`agy 1.5.5`)

- **Result:** **Resumable by exact ID!**
- **Binary:** `~/.local/bin/agy` (also aliased as `agy-cli`).
- **Resume command:** `agy --conversation {id}` (verified via `agy --help`: `--conversation: Resume a previous conversation by ID`).
- **Session data paths (`~/.gemini/antigravity-cli/`):**
  1. **Per-conversation DB:** `~/.gemini/antigravity-cli/conversations/<conversation_id>.db` (SQLite; `trajectory_meta`, `steps`, etc.).
  2. **Summaries index:** `~/.gemini/antigravity-cli/conversation_summaries.db` (SQLite; `conversation_summaries` table).
  3. **Live lock files:** `~/.gemini/antigravity-cli/presence/<conversation_id>.lock` — **held open by the live `agy` process!**
  4. **Workspace cache:** `~/.gemini/antigravity-cli/cache/last_conversations.json` (JSON map of `cwd -> conversation_id`).
  5. **Prompt history:** `~/.gemini/antigravity-cli/history.jsonl`.

### File format samples

**`~/.gemini/antigravity-cli/conversation_summaries.db`:**
```sql
CREATE TABLE `conversation_summaries` (
  `conversation_id` text PRIMARY KEY,
  `title` text NOT NULL DEFAULT "",
  `preview` text NOT NULL DEFAULT "",
  `step_count` integer NOT NULL DEFAULT 0,
  `last_modified_time` datetime NOT NULL,
  `workspace_uris` text NOT NULL,        -- JSON array: ["file:///Users/..."] (percent-encoded!)
  `status` text NOT NULL DEFAULT "",
  `parent_conversation_id` text NOT NULL DEFAULT "",
  `app_data_dir` text NOT NULL DEFAULT "" -- "antigravity-cli"
);
```
Note: `workspace_uris` uses percent-encoded `file://` URIs (e.g. `"file:///Users/dev/Developer/Side%20Projects/demo-app"`).

**`~/.gemini/antigravity-cli/cache/last_conversations.json`:**
```json
{
  "/Users/dev": "8c257602-0000-4000-8000-000000000002",
  "/Users/dev/Developer/Projects/sample-api": "9226b940-0000-4000-8000-000000000004"
}
```

### Process matching & live ID resolution

- **Observed live on `PID 15517` (`agy`) under `libproc`:**
  - `exe`: `/Users/dev/.local/bin/agy.1794331593374468000.old` (renamed during auto-update!)
  - `comm`: `agy`, `argv`: `["agy"]`, `cwd`: `/Users/dev`
  - **Exact open FD:** `fd=72 /Users/dev/.gemini/antigravity-cli/presence/8c257602-0000-4000-8000-000000000002.lock`
  - Also open: `fd=15 .../antigravity-cli/brain/8c257602-0000-4000-8000-000000000002`
- **False positives observed live and excluded:**
  - `PID 18225`: `~/.gemini/bin/agy --hub --hub-port=55437 --app_data_dir=antigravity ...` (Antigravity IDE hub)
  - `PID 49742`: `~/.local/bin/agy acp --gemini-dir ... --app-data-dir antigravity-acp` (T3 Code ACP server)
  - `/Applications/Antigravity*.app/...` (Desktop/IDE Electron helpers)
- **Match rule:** `comm == "agy"` or basename of `argv[0]` is `agy`/`agy-cli` (allowing `exe` ending in `agy` or `agy.*.old`), **excluding** `argv` containing `--hub`, `acp`, `-p`/`--print`, `remote-control`, `mcp`, `plugin`, `plugins`, `update`, `install`, `models`, `agent`, `agents`, `changelog`, `help`, `mic-serve`.
- **Live ID resolution order:**
  1. Exact: open FD matching `antigravity-cli/presence/<uuid>.lock` (or `antigravity-cli/conversations/<uuid>.db`).
  2. Exact: `argv` contains `--conversation <uuid>`.
  3. Fallback: `~/.gemini/antigravity-cli/cache/last_conversations.json[cwd]`.
- **Session existence check:** `~/.gemini/antigravity-cli/conversations/<id>.db` exists.
- **Title extraction:** `title` (else `preview`) from `conversation_summaries.db` for `conversation_id`.

---

## 5. Ghostty (`1.4.0`) Tab Scripting Spike

- **Result:** **YES — native AppleScript tab creation works out of the box!**
- **Version verified:** Ghostty `1.4.0` (`/Applications/Ghostty.app`).
- **Scripting dictionary:** `/Applications/Ghostty.app/Contents/Resources/Ghostty.sdef`.
- **Config toggle:** `macos-applescript = true` by default (verified via `ghostty +show-config --default --docs`).
- **Supported primitives in `Ghostty.sdef`:**
  - `new surface configuration` with `initial working directory`, `initial input`, `command`, `wait after command`, `environment variables`
  - `new window with configuration <cfg>` → returns `window`
  - `new tab in <window> with configuration <cfg>` → returns `tab`
  - `name override` on `tab` and `terminal` (optional tab naming!)
- **`initial input` vs `command`:**
  - Prefer `initial working directory` + `initial input` (terminated with `linefeed`) over `command`: Ghostty launches the user's normal login shell (loading `~/.zshrc`, `PATH`, Homebrew, `~/.local/bin`, `~/.opencode/bin`, nvm/asdf/mise shims), executes the command, and leaves the tab open at a shell prompt if the agent later exits.
  - Still pass `cd <shell-escaped-cwd> && <resume-command>` (or rely on `initial working directory` + `<resume-command>`) with strict POSIX shell escaping.

### Working verified AppleScript snippet

Tested live on Ghostty 1.4.0 (confirmed `tabs=2`, tab 1 ran in `/tmp`, tab 2 ran in `/Users/dev/Developer/OpenSource/restowa`, then closed the test window cleanly):

```applescript
tell application "Ghostty"
    set cfg1 to new surface configuration
    set initial working directory of cfg1 to "/path/to/project"
    set initial input of cfg1 to "cd '/path/to/project' && claude --resume '<id-1>'" & linefeed
    set win to new window with configuration cfg1

    set cfg2 to new surface configuration
    set initial working directory of cfg2 to "/path/to/project"
    set initial input of cfg2 to "cd '/path/to/project' && codex resume '<id-2>'" & linefeed
    new tab in win with configuration cfg2
end tell
```

Notes for implementation:
- First AppleScript call triggers macOS's Automation permission prompt for Ghostty (no Full Disk Access or Accessibility required).
- If `macos-applescript = false` in the user's `~/.config/ghostty/config`, `restowa doctor` should detect and report it, and restore can fall back to `Terminal.app` or Ghostty CLI windows.

---

## 6. `libproc` & `sysctl` Spike (macOS 27.0.1)

Compiled and ran a native macOS probe calling the exact C APIs exposed by `libproc` and `sysctl`:
- `sysctl([CTL_KERN, KERN_BOOTTIME])` → returned `1794307113.486877`
- `proc_listallpids` → returned 923 PIDs
- `proc_pidpath` → returned executable path (except unlinked self-updated binaries!)
- `proc_pidinfo(PROC_PIDTBSDINFO)` → returned `pbi_ppid`, `pbi_comm`, `pbi_start_tvsec`, `pbi_start_tvusec`
- `proc_pidinfo(PROC_PIDVNODEPATHINFO)` → returned exact `cwd` for every user-owned process (`claude`, `agy`, `codex`, `opencode`)
- `sysctl([CTL_KERN, KERN_PROCARGS2, pid])` → returned full `argv` for every user-owned process
- `proc_pidinfo(PROC_PIDLISTFDS)` + `proc_pidfdinfo(PROC_PIDFDVNODEPATHINFO)` → returned all open vnode paths (including `agy`'s `presence/<uuid>.lock` and `opencode.db`)

### Performance & permissions

- **Permissions required:** **None** (no `sudo`, no Full Disk Access, no Accessibility; worked even inside a restricted sandbox for all user-owned processes).
- **Measured runtime:** **18.22 ms** for a full system scan across 923 processes including FD enumeration on matched agents — well under the **150 ms** budget.

### Two implementation requirements surfaced by this spike

1. **Do not filter solely on `proc_pidpath`:**
   - Self-updating CLIs (`claude`, `agy`) unlink or rename their running binary on disk, causing `proc_pidpath` to fail (`<err>`) or return `*.old`.
   - The scanner must inspect `pbi_comm` + `KERN_PROCARGS2` (`argv`) in addition to `proc_pidpath`.
2. **Persist process start time in SQLite (`pid_start_us`):**
   - The v0.1 schema stored `pid INTEGER` without a process start timestamp, making cross-scan PID-reuse checks impossible since `restowa scan` exits between runs.
   - Add `pid_start_us INTEGER` (microseconds since epoch from `pbi_start_tvsec * 1_000_000 + pbi_start_tvusec`, `NULL` when closed) to the `sessions` table.
3. **Read-only SQLite reads against tool DBs (Codex, OpenCode, Antigravity):**
   - All three use WAL-mode SQLite databases. Opening a WAL database with `SQLITE_OPEN_READ_ONLY` fails or attempts `-shm` creation if `-shm` is absent; opening with `immutable=1` ignores uncheckpointed `-wal` rows.
   - In Phase 3/5, verify opening with `SQLITE_OPEN_READ_ONLY` when `-shm` exists (and fallback/temp snapshot only if needed), never writing inside tool directories.

---

## 7. Spec Contradictions & Updates Flagged

The following updates were applied to `restowa — Build Spec v0.1.md` based on Phase 0 findings:

1. **Data model:** Added `pid_start_us INTEGER` to `sessions` so stateless scans can detect PID reuse reliably.
2. **Tool adapters & config:**
   - Updated Claude Code to include `~/.claude/sessions/<pid>.json` as exact scanner source (in addition to hooks) and noted lossy directory encoding + `KERN_PROCARGS2` matching.
   - Updated Codex to include `~/.codex/state_5.sqlite` (`threads`) alongside `rollout-*.jsonl` and `argv`.
   - Updated OpenCode to reflect `~/.local/share/opencode/opencode.db` (SQLite) instead of generic folder storage.
   - Promoted Antigravity (`agy`) from "Unknown" to fully specified (`agy --conversation {id}`, exact live FD via `presence/<id>.lock`, metadata in `conversation_summaries.db` and `conversations/<id>.db`), and added `[tools.antigravity]` to `config.toml`.
3. **Claude Code hooks:** Added `"matcher": ""` to `SessionStart` and `SessionEnd` entries in the `settings.json` snippet to match the official `HookMatcher` schema.
4. **Ghostty backend:** Updated from "Verify in Phase 0" to confirmed native AppleScript tab support (`Ghostty.sdef`, Ghostty 1.3+, tested on 1.4.0) using `new surface configuration` + `new window` + `new tab in win`.
5. **Process info (`libproc`):** Clarified that `KERN_PROCARGS2` (`sysctl`) is used alongside `libproc` so auto-updated binaries (`claude`, `agy`) are never missed when `proc_pidpath` fails.
6. **Risks:** Updated Ghostty and Antigravity risks to resolved/mitigated and added a risk row for self-updating binaries + background daemon false positives (`agy --hub`, `agy acp`, `codex app-server`, `claude --chrome-native-host`).
7. **Phase 4 environment note:** `/Applications/Xcode.app` is not currently installed on this Mac (Swift 6.4 Command Line Tools are installed); Phase 4 can either use SwiftPM + `.app` bundle generation or Xcode if installed later.
