CREATE TABLE IF NOT EXISTS users (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS tokens (
    hash       TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id),
    device     TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
);
CREATE TABLE IF NOT EXISTS memories (
    id            TEXT PRIMARY KEY,
    user_id       INTEGER NOT NULL REFERENCES users(id),
    scope         TEXT NOT NULL CHECK (scope IN ('personal', 'repo')),
    repo          TEXT,
    key           TEXT,
    text          TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    superseded_by TEXT,
    deleted_at    INTEGER
);
-- newest fact wins: one active memory per (owner, scope, repo, key)
CREATE UNIQUE INDEX IF NOT EXISTS memories_active_key
    ON memories (user_id, scope, ifnull(repo, ''), key)
    WHERE key IS NOT NULL AND superseded_by IS NULL AND deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS memories_owner ON memories (user_id, scope, repo, created_at);
CREATE TABLE IF NOT EXISTS memory_anchors (
    memory_id      TEXT NOT NULL REFERENCES memories(id),
    repo           TEXT,
    path           TEXT NOT NULL,
    symbol         TEXT,
    blob_at_save   TEXT,
    commit_at_save TEXT,
    PRIMARY KEY (memory_id, path)
);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts
    USING fts5(text, content = 'memories', content_rowid = 'rowid');
-- memory text never changes (a new fact is a new row), so inserts are all FTS needs
CREATE TRIGGER IF NOT EXISTS memories_fts_insert AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts (rowid, text) VALUES (new.rowid, new.text);
END;
