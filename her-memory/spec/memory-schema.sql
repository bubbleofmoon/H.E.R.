
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS memory (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  owner_type   TEXT NOT NULL,              -- session | person
  owner_id     TEXT NOT NULL,              -- group:7002 / private:7001
  scope        TEXT NOT NULL,              -- private | group
  kind         TEXT NOT NULL,              -- fact | topic | thought | impression | event | preference
  subject      TEXT,                       -- 话题名/人名（模糊指代时靠它命中）
  content      TEXT NOT NULL,
  tags         TEXT,
  source       TEXT NOT NULL,              -- agents-md | qq-memory | auto-digest | manual | import
  importance   REAL NOT NULL DEFAULT 0.5,
  hits         INTEGER NOT NULL DEFAULT 0,
  pinned       INTEGER NOT NULL DEFAULT 0,
  archived     INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL,
  last_used_at INTEGER,
  expires_at   INTEGER
);
CREATE INDEX IF NOT EXISTS idx_memory_owner ON memory(owner_type, owner_id, kind, archived);
CREATE INDEX IF NOT EXISTS idx_memory_recent ON memory(created_at DESC);

-- 检索索引：seg 是 segment() 的结果（空格连接）
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(seg, tokenize='unicode61');

-- ACL：reader 能读哪一类 owner 的记忆
CREATE TABLE IF NOT EXISTS memory_acl (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  reader_type TEXT NOT NULL,               -- owner | session | person | team
  reader_id   TEXT NOT NULL,
  read_type   TEXT NOT NULL,               -- session | person | team | *
  read_id     TEXT NOT NULL,               -- 具体 id 或 '*'
  level       TEXT NOT NULL DEFAULT 'read',
  created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_acl_reader ON memory_acl(reader_type, reader_id);

CREATE TABLE IF NOT EXISTS team (
  id TEXT PRIMARY KEY, title TEXT, note TEXT, created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS team_member (
  team_id TEXT NOT NULL, member_type TEXT NOT NULL, member_id TEXT NOT NULL,
  PRIMARY KEY (team_id, member_type, member_id)
);

CREATE TABLE IF NOT EXISTS role_group (
  id TEXT PRIMARY KEY, role_name TEXT NOT NULL, title TEXT, note TEXT
);
CREATE TABLE IF NOT EXISTS role_group_session (
  role_group_id TEXT NOT NULL, session_key TEXT NOT NULL,
  PRIMARY KEY (role_group_id, session_key)
);

CREATE TABLE IF NOT EXISTS memory_audit (
  id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL,
  actor TEXT, action TEXT, memory_id INTEGER, detail TEXT
);
