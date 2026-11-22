CREATE TABLE IF NOT EXISTS sessions (
  key            TEXT PRIMARY KEY,
  tool           TEXT NOT NULL,
  session_id     TEXT NOT NULL,
  cwd            TEXT NOT NULL,
  title          TEXT,
  transcript     TEXT,
  pid            INTEGER,
  pid_start_us   INTEGER,
  status         TEXT NOT NULL,
  source         TEXT NOT NULL,
  first_seen     INTEGER NOT NULL,
  last_seen_live INTEGER,
  last_boot_id   TEXT,
  pinned         INTEGER NOT NULL DEFAULT 0,
  hidden         INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_sessions_boot ON sessions(last_boot_id);
CREATE INDEX IF NOT EXISTS idx_sessions_cwd  ON sessions(cwd);

CREATE TABLE IF NOT EXISTS boots (
  boot_id    TEXT PRIMARY KEY,
  first_seen INTEGER NOT NULL
);
