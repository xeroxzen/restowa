CREATE TABLE conversation_summaries (
  conversation_id TEXT PRIMARY KEY,
  title TEXT NOT NULL DEFAULT '',
  preview TEXT NOT NULL DEFAULT '',
  step_count INTEGER NOT NULL DEFAULT 0,
  last_modified_time TEXT NOT NULL DEFAULT '',
  workspace_uris TEXT NOT NULL DEFAULT '[]',
  status TEXT NOT NULL DEFAULT '',
  parent_conversation_id TEXT NOT NULL DEFAULT '',
  app_data_dir TEXT NOT NULL DEFAULT 'antigravity-cli'
);

INSERT INTO conversation_summaries VALUES
  (
    '8c257602-0000-4000-8000-000000000002',
    'Inspect Security Research Repo',
    'check endpoints',
    12,
    '2026-11-22T10:00:00Z',
    '["file:///Users/dev/Security%20Research/demo"]',
    'CASCADE_RUN_STATUS_IDLE',
    '',
    'antigravity-cli'
  ),
  (
    '9226b940-0000-4000-8000-000000000004',
    'Sample API Dashboard Cleanup',
    'fix layout',
    8,
    '2026-11-22T09:00:00Z',
    '["file:///Users/dev/sample-api"]',
    'CASCADE_RUN_STATUS_IDLE',
    '',
    'antigravity-cli'
  );
