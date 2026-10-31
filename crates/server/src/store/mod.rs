//! All SQL lives here. One writer thread owns the only write connection and
//! handlers send it work over a channel; reads use a small connection pool.
//! Every memory query takes the principal: there is no unscoped read path.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use sm_proto::{Anchor, Memory, PathBlob, Scope};

mod migrate;

/// Who is calling, from their token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub user_id: i64,
    pub device: String,
}

pub struct NewMemory {
    pub scope: Scope,
    pub repo: Option<String>,
    pub key: Option<String>,
    pub text: String,
    pub anchors: Vec<String>,
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

const MAX_IDLE_READERS: usize = 4;

#[derive(Clone)]
pub struct Store {
    writer: mpsc::Sender<Job>,
    readers: Arc<Mutex<Vec<Connection>>>,
    path: Arc<PathBuf>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        let mut conn = open(path).with_context(|| format!("open {}", path.display()))?;
        // memories are primary data: an acknowledged remember must survive a power cut
        conn.pragma_update(None, "synchronous", "FULL")?;
        migrate::run(&mut conn, path, migrate::MIGRATIONS)?;
        // after migrating, so a refused newer schema is left byte-for-byte untouched
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let (writer, jobs) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("sm-store-writer".into())
            .spawn(move || {
                for job in jobs {
                    job(&mut conn);
                }
            })?;
        Ok(Store {
            writer,
            readers: Arc::default(),
            path: Arc::new(path.to_path_buf()),
        })
    }

    async fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.writer
            .send(Box::new(move |conn| {
                let _ = tx.send(f(conn));
            }))
            .map_err(|_| anyhow!("store writer is gone"))?;
        Ok(rx.await??)
    }

    async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let (readers, path) = (self.readers.clone(), self.path.clone());
        tokio::task::spawn_blocking(move || -> anyhow::Result<T> {
            let pooled = readers.lock().unwrap().pop();
            let conn = match pooled {
                Some(conn) => conn,
                None => {
                    let conn = open(&path)?;
                    conn.pragma_update(None, "query_only", "ON")?;
                    conn
                }
            };
            let result = f(&conn);
            let mut idle = readers.lock().unwrap();
            if idle.len() < MAX_IDLE_READERS {
                idle.push(conn);
            }
            Ok(result?)
        })
        .await?
    }

    /// Creates the user if missing and stores the token's hash.
    pub async fn add_token(&self, user: &str, device: &str, hash: &str) -> anyhow::Result<()> {
        let (user, device, hash) = (user.to_string(), device.to_string(), hash.to_string());
        self.write(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO users (name) VALUES (?1) ON CONFLICT (name) DO NOTHING",
                [&user],
            )?;
            let user_id: i64 =
                tx.query_row("SELECT id FROM users WHERE name = ?1", [&user], |r| {
                    r.get(0)
                })?;
            tx.execute(
                "INSERT INTO tokens (hash, user_id, device, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![hash, user_id, device, now_ms()],
            )?;
            tx.commit()
        })
        .await
    }

    pub async fn principal(&self, token_hash: &str) -> anyhow::Result<Option<Principal>> {
        let hash = token_hash.to_string();
        self.read(move |conn| {
            conn.query_row(
                "SELECT user_id, device FROM tokens WHERE hash = ?1 AND revoked_at IS NULL",
                [hash],
                |r| {
                    Ok(Principal {
                        user_id: r.get(0)?,
                        device: r.get(1)?,
                    })
                },
            )
            .optional()
        })
        .await
    }

    /// Saves a memory; with a key it supersedes the owner's active memory with
    /// the same `(scope, repo, key)`.
    pub async fn insert_memory(&self, p: &Principal, m: NewMemory) -> anyhow::Result<Memory> {
        let memory = Memory {
            id: uuid::Uuid::now_v7().to_string(),
            scope: m.scope,
            repo: m.repo,
            key: m.key,
            text: m.text,
            created_at: now_ms(),
            anchors: m
                .anchors
                .into_iter()
                .map(|path| Anchor {
                    path,
                    blob_at_save: None,
                    commit_at_save: None,
                })
                .collect(),
        };
        let user_id = p.user_id;
        self.write(move |conn| {
            let m = &memory;
            let tx = conn.transaction()?;
            if let Some(key) = &m.key {
                tx.execute(
                    "UPDATE memories SET superseded_by = ?1
                     WHERE user_id = ?2 AND scope = ?3 AND ifnull(repo, '') = ifnull(?4, '')
                       AND key = ?5 AND superseded_by IS NULL AND deleted_at IS NULL",
                    params![m.id, user_id, m.scope.as_str(), m.repo, key],
                )?;
            }
            tx.execute(
                "INSERT INTO memories (id, user_id, scope, repo, key, text, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![m.id, user_id, m.scope.as_str(), m.repo, m.key, m.text, m.created_at],
            )?;
            for a in &m.anchors {
                tx.execute(
                    "INSERT OR IGNORE INTO memory_anchors (memory_id, repo, path) VALUES (?1, ?2, ?3)",
                    params![m.id, m.repo, a.path],
                )?;
            }
            tx.commit()?;
            Ok(memory)
        })
        .await
    }

    /// Full-text search over the caller's active memories, best match first;
    /// memories of `repo` rank above the rest.
    pub async fn recall(
        &self,
        p: &Principal,
        query: &str,
        repo: Option<&str>,
        limit: u32,
    ) -> anyhow::Result<Vec<Memory>> {
        let Some(fts) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let (user_id, repo) = (p.user_id, repo.map(String::from));
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT m.id, m.scope, m.repo, m.key, m.text, m.created_at
                 FROM memories_fts JOIN memories m ON m.rowid = memories_fts.rowid
                 WHERE memories_fts MATCH ?1 AND m.user_id = ?2
                   AND m.superseded_by IS NULL AND m.deleted_at IS NULL
                 ORDER BY (m.repo IS NOT NULL AND m.repo = ?3) DESC, bm25(memories_fts)
                 LIMIT ?4",
            )?;
            let rows = stmt.query_map(params![fts, user_id, repo, limit], memory_row)?;
            with_anchors(conn, rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    /// Owner only, soft delete. False when there is no such active memory of theirs.
    pub async fn forget(&self, p: &Principal, id: &str) -> anyhow::Result<bool> {
        let (user_id, id) = (p.user_id, id.to_string());
        self.write(move |conn| {
            conn.execute(
                "UPDATE memories SET deleted_at = ?1
                 WHERE id = ?2 AND user_id = ?3 AND deleted_at IS NULL",
                params![now_ms(), id, user_id],
            )
        })
        .await
        .map(|n| n > 0)
    }

    /// The repo's memories, then personal ones, newest first.
    pub async fn context(
        &self,
        p: &Principal,
        repo: Option<&str>,
        limit: u32,
    ) -> anyhow::Result<Vec<Memory>> {
        let (user_id, repo) = (p.user_id, repo.map(String::from));
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT id, scope, repo, key, text, created_at FROM memories
                 WHERE user_id = ?1 AND superseded_by IS NULL AND deleted_at IS NULL
                   AND (scope = 'personal' OR (scope = 'repo' AND repo = ?2))
                 ORDER BY scope = 'repo' DESC, created_at DESC, id DESC
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![user_id, repo, limit], memory_row)?;
            with_anchors(conn, rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    /// Records what anchored files hashed to when the memory was saved. The
    /// first baseline wins, so a late post cannot make a stale memory look
    /// fresh. False when the memory is not the caller's.
    pub async fn set_baselines(
        &self,
        p: &Principal,
        id: &str,
        commit: Option<String>,
        blobs: Vec<PathBlob>,
    ) -> anyhow::Result<bool> {
        let (user_id, id) = (p.user_id, id.to_string());
        self.write(move |conn| {
            let tx = conn.transaction()?;
            let owned = tx
                .query_row(
                    "SELECT 1 FROM memories WHERE id = ?1 AND user_id = ?2",
                    params![id, user_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if owned {
                for b in &blobs {
                    tx.execute(
                        "UPDATE memory_anchors SET blob_at_save = ?1, commit_at_save = ?2
                         WHERE memory_id = ?3 AND path = ?4 AND blob_at_save IS NULL",
                        params![b.blob, commit, id, b.path],
                    )?;
                }
            }
            tx.commit()?;
            Ok(owned)
        })
        .await
    }
}

fn memory_row(r: &rusqlite::Row) -> rusqlite::Result<Memory> {
    let scope: String = r.get(1)?;
    Ok(Memory {
        id: r.get(0)?,
        scope: Scope::parse(&scope).unwrap_or(Scope::Personal),
        repo: r.get(2)?,
        key: r.get(3)?,
        text: r.get(4)?,
        created_at: r.get(5)?,
        anchors: Vec::new(),
    })
}

fn with_anchors(conn: &Connection, mut memories: Vec<Memory>) -> rusqlite::Result<Vec<Memory>> {
    let mut stmt = conn.prepare_cached(
        "SELECT path, blob_at_save, commit_at_save FROM memory_anchors
         WHERE memory_id = ?1 ORDER BY path",
    )?;
    for m in &mut memories {
        m.anchors = stmt
            .query_map([&m.id], |r| {
                Ok(Anchor {
                    path: r.get(0)?,
                    blob_at_save: r.get(1)?,
                    commit_at_save: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
    }
    Ok(memories)
}

/// User text → FTS5 query that cannot break FTS syntax: each word becomes a
/// quoted token, joined with OR. `None` when there is nothing to search for.
fn fts_query(text: &str) -> Option<String> {
    let tokens: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();
    (!tokens.is_empty()).then(|| tokens.join(" OR "))
}

#[cfg(test)]
mod tests {
    #[test]
    fn fts_query_quotes_every_token() {
        assert_eq!(
            super::fts_query("foo\"bar AND (").as_deref(),
            Some("\"foo\" OR \"bar\" OR \"AND\"")
        );
        assert_eq!(super::fts_query(" ( \" "), None);
    }
}
