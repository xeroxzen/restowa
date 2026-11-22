CREATE TABLE session (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  parent_id TEXT,
  slug TEXT NOT NULL,
  directory TEXT NOT NULL,
  title TEXT NOT NULL,
  version TEXT NOT NULL,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  time_archived INTEGER
);

CREATE TABLE message (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  data TEXT NOT NULL
);

CREATE TABLE part (
  id TEXT PRIMARY KEY,
  message_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  data TEXT NOT NULL
);

INSERT INTO session VALUES
  ('ses_single_01', 'global', NULL, 's1', '/Users/dev/restowa', 'Scaffold adapters', '1.19.5', 1793982000000, 1793982900000, NULL),
  ('ses_ambig_new', 'global', NULL, 's2', '/Users/dev/shared-repo', 'New session - 2026-11-22T10:00:00.000Z', '1.19.5', 1793983000000, 1793983900000, NULL),
  ('ses_ambig_old', 'global', NULL, 's3', '/Users/dev/shared-repo', 'Refactor parser', '1.19.5', 1793981000000, 1793981500000, NULL);

INSERT INTO message VALUES
  ('msg_u1', 'ses_ambig_new', 1793983000100, 1793983000100, '{"role":"user"}');

INSERT INTO part VALUES
  ('prt_u1', 'msg_u1', 'ses_ambig_new', 1793983000200, 1793983000200, '{"type":"text","text":"Audit SQLite WAL read-only flags"}');
