# Phase 1 — Real Claude Code Resume Verification

Date: 2026-11-22  
Environment: macOS 27.0.1 (`arm64`), Claude Code `2.1.330`, Ghostty `1.4.0`, installed `restowa` binary at `/Users/dev/.cargo/bin/restowa`.

## Setup

1. Installed the release binary outside `target/` at `/Users/dev/.cargo/bin/restowa`.
2. Ran `restowa install --all` against the real `~/.claude/settings.json`:
   - Backed up existing settings to `~/.claude/settings.json.restowa.bak`.
   - Merged `SessionStart` (`restowa hook claude session-start`) and `SessionEnd` (`restowa hook claude session-end`) without disturbing existing settings (`model`, `enabledPlugins`, `modelSettings`, `theme`, `tui`, etc.).
3. Verified `restowa doctor` reported `[OK]` across `claude:hooks`, `claude:pid-files`, `registry`, `launchd`, and `terminal:ghostty`.

## Real End-to-End Resume Test

Tested with a fresh real Claude Code session in `/tmp/restowa-real-resume-test` (without touching any existing work sessions):

1. **Start + `SessionStart` hook:**
   - Launched `claude` in a Ghostty window in `/tmp/restowa-real-resume-test` (`PID 68204`).
   - `SessionStart` hook fired automatically and inserted:
     - `key`: `claude:0efe550b-0000-4000-8000-000000000007`
     - `source`: `hook`, `status`: `live`
     - `pid`: `68204`, `pid_start_us`: `1795382425368839`
     - `cwd`: `/private/tmp/restowa-real-resume-test`
     - `last_boot_id`: `1794307113.486877`
2. **Conversation turn:**
   - Sent: `"Remember the codeword MANGO-42 and reply with just OK."`
   - Claude replied: `"OK"`.
3. **Clean close + `SessionEnd` hook:**
   - Sent `/exit` (`EXIT=0`) and closed the Ghostty window.
   - `SessionEnd` hook fired automatically and updated the registry row to `status = closed`, `pid = null`, `pid_start_us = null`, preserving `last_boot_id = 1794307113.486877` and title `"Remember the codeword MANGO-42 and reply with just OK."`.
4. **Restore via Ghostty AppleScript backend:**
   - Ran `restowa restore 0efe550b`.
   - Ghostty opened a new window in `/private/tmp/restowa-real-resume-test` and executed `cd '/private/tmp/restowa-real-resume-test' && claude --resume '0efe550b-0000-4000-8000-000000000007'`.
   - `SessionStart` (`source = resume`) fired automatically for the new Claude process (`PID 69000`, `pid_start_us = 1795382581925852`), updating `status = live` on the same session ID.
5. **Conversation history confirmation:**
   - Sent: `"What was the codeword I asked you to remember? Reply with just the codeword."`
   - Claude replied: **`MANGO-42`**, confirming the conversation history was restored verbatim.
6. **Cleanup:**
   - Exited the test session cleanly via `/exit`, closed the test Ghostty window, ran `restowa forget 0efe550b`, and removed `/tmp/restowa-real-resume-test`.

## Additional Phase 1 Fix Verifications

- **`/clear`, `compact`, `resume`, `startup`:** Confirmed via Claude Code docs that `/compact` and `/resume` preserve `session_id`, while `/clear` issues a new `session_id` on the same `(pid, pid_start_us)`. Added unit tests for all four `source` values (`startup`, `resume`, `compact`, `clear`) plus `SessionEnd` (`reason = "clear"`).
- **Hook timeout + spool fallback:** Configured a dedicated `150 ms` `busy_timeout` for hooks; tested under an active `BEGIN EXCLUSIVE` lock that the hook falls back to `~/Library/Application Support/restowa/spool.jsonl`, exits `0`, and `restowa scan` imports and clears the spool.
- **Headless filter:** Tested that hooks invoked under any ancestor with `-p` or `--print` in `argv` are skipped without writing to SQLite or the spool.
- **Slow `.zshrc` test:** Tested Ghostty AppleScript `initial working directory` + `initial input` (`& linefeed`) with a 2.5-second `sleep` in `.zshrc` via `ZDOTDIR`; both window and tab cleanly buffered and ran commands after `.zshrc` loaded.
