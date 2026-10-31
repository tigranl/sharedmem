//! Forward-only schema migrations, counted in SQLite's `PRAGMA user_version`.
//! To change the schema, add the next numbered file to `migrations/`; never
//! edit one that has shipped. A file must not contain BEGIN or COMMIT.

use std::path::Path;

use anyhow::{Context as _, bail};
use rusqlite::{Connection, TransactionBehavior};

/// `(file name, SQL)` in order; applying the Nth sets `user_version` to N.
pub(super) const MIGRATIONS: &[(&str, &str)] = &[(
    "0001_init.sql",
    include_str!("../../migrations/0001_init.sql"),
)];

/// Applies the pending migrations, each in one transaction with its version
/// bump. An existing database is first copied to `<db>.bak-v<version>`.
pub(super) fn run(
    conn: &mut Connection,
    db: &Path,
    migrations: &[(&str, &str)],
) -> anyhow::Result<()> {
    let from: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let latest = migrations.len() as u32;
    if from > latest {
        bail!(
            "{} has a newer schema (v{from}) than this sm-server knows (v{latest}); upgrade sm-server",
            db.display()
        );
    }
    if from == latest {
        return Ok(());
    }
    // version 0 with tables is a database created before migrations existed
    let existing =
        from > 0 || conn.query_row("SELECT count(*) > 0 FROM sqlite_schema", [], |r| r.get(0))?;
    if existing {
        // VACUUM INTO refuses to overwrite, so an older backup is never lost
        let backup = format!("{}.bak-v{from}", db.display());
        conn.execute("VACUUM INTO ?1", [&backup])
            .with_context(|| format!("back up {} to {backup}", db.display()))?;
        tracing::info!(from, %backup, "backed up database before migrating");
    }
    for (version, (name, sql)) in (1u32..).zip(migrations).skip(from as usize) {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // on error `tx` drops and rolls back, so a migration applies whole or not at all
        tx.execute_batch(sql)
            .and_then(|()| tx.pragma_update(None, "user_version", version))
            .and_then(|()| tx.commit())
            .with_context(|| format!("migration {name} failed and was rolled back"))?;
        tracing::info!(migration = %name, version, "applied migration");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use rusqlite::Connection;
    use sm_proto::Scope;

    use super::{MIGRATIONS, run};
    use crate::store::{NewMemory, Principal, Store};

    /// What `Store::open` created inline before migrations existed.
    const OLD_INLINE_SCHEMA: &str = "
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
";

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_db() -> (TempDir, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sm-migrate-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("sm.db");
        (TempDir(dir), db)
    }

    fn backup(db: &Path, version: u32) -> PathBuf {
        PathBuf::from(format!("{}.bak-v{version}", db.display()))
    }

    fn version(db: &Path) -> u32 {
        Connection::open(db)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    /// Every schema object with the SQL SQLite stored for it.
    fn schema(db: &Path) -> Vec<(String, String)> {
        let conn = Connection::open(db).unwrap();
        let mut stmt = conn
            .prepare("SELECT name, ifnull(sql, '') FROM sqlite_schema ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn row_counts(db: &Path) -> Vec<i64> {
        let conn = Connection::open(db).unwrap();
        [
            "users",
            "tokens",
            "memories",
            "memory_anchors",
            "memories_fts",
        ]
        .iter()
        .map(|t| {
            conn.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
                .unwrap()
        })
        .collect()
    }

    async fn recall(store: &Store, user_id: i64, query: &str) -> Vec<String> {
        let p = Principal {
            user_id,
            device: "laptop".into(),
        };
        let hits = store.recall(&p, query, None, 10).await.unwrap();
        hits.into_iter().map(|m| m.text).collect()
    }

    #[tokio::test]
    async fn fresh_db_gets_the_latest_schema_and_works() {
        let (_dir, db) = temp_db();
        let store = Store::open(&db).unwrap();
        assert_eq!(version(&db), MIGRATIONS.len() as u32);
        let names: Vec<String> = schema(&db).into_iter().map(|(n, _)| n).collect();
        for table in [
            "users",
            "tokens",
            "memories",
            "memory_anchors",
            "memories_fts",
        ] {
            assert!(
                names.iter().any(|n| n == table),
                "{table} missing: {names:?}"
            );
        }
        assert!(!backup(&db, 0).exists(), "a new database needs no backup");

        store.add_token("a", "laptop", "hash").await.unwrap();
        let p = store.principal("hash").await.unwrap().unwrap();
        let memory = NewMemory {
            scope: Scope::Personal,
            repo: None,
            key: None,
            text: "deploys go through CI".into(),
            anchors: Vec::new(),
        };
        store.insert_memory(&p, memory).await.unwrap();
        assert_eq!(
            recall(&store, p.user_id, "deploys").await,
            ["deploys go through CI"]
        );
    }

    #[test]
    fn init_creates_exactly_the_old_inline_schema() {
        let (_dir, db) = temp_db();
        run(&mut Connection::open(&db).unwrap(), &db, &MIGRATIONS[..1]).unwrap();
        let (_old_dir, old) = temp_db();
        Connection::open(&old)
            .unwrap()
            .execute_batch(OLD_INLINE_SCHEMA)
            .unwrap();
        assert_eq!(schema(&db), schema(&old));
    }

    /// A live database made by the inline schema: `user_version` 0, real rows.
    #[tokio::test]
    async fn pre_migration_db_is_backed_up_and_kept_intact() {
        let (_dir, db) = temp_db();
        let conn = Connection::open(&db).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(OLD_INLINE_SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO users (id, name) VALUES (1, 'tigran');
             INSERT INTO tokens (hash, user_id, device, created_at) VALUES ('h', 1, 'laptop', 1);
             INSERT INTO memories (id, user_id, scope, repo, text, created_at)
                 VALUES ('m1', 1, 'repo', 'x/y', 'seeded ATS slugs are often wrong', 1);
             INSERT INTO memory_anchors (memory_id, repo, path) VALUES ('m1', 'x/y', 'a.yaml');",
        )
        .unwrap();
        drop(conn);
        let before = row_counts(&db);
        assert_eq!(before, [1, 1, 1, 1, 1]);

        let store = Store::open(&db).unwrap();
        assert_eq!(version(&db), 1);
        assert_eq!(row_counts(&db), before);
        assert_eq!(version(&backup(&db, 0)), 0);
        assert_eq!(row_counts(&backup(&db, 0)), before);
        assert_eq!(
            recall(&store, 1, "ATS slugs").await,
            ["seeded ATS slugs are often wrong"]
        );
    }

    #[test]
    fn newer_schema_is_refused_and_left_untouched() {
        let (_dir, db) = temp_db();
        Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE from_the_future (x); PRAGMA user_version = 99;")
            .unwrap();
        let bytes = std::fs::read(&db).unwrap();
        let err = Store::open(&db).err().expect("opening must fail");
        assert!(format!("{err:#}").contains("newer schema"), "{err:#}");
        assert_eq!(std::fs::read(&db).unwrap(), bytes);
        assert!(!backup(&db, 99).exists());
    }

    #[test]
    fn failed_migration_rolls_back_and_names_its_file() {
        let (_dir, db) = temp_db();
        let broken = [
            MIGRATIONS[0],
            (
                "0002_broken.sql",
                "CREATE TABLE half_done (x); NOT SQL AT ALL;",
            ),
        ];
        let err = run(&mut Connection::open(&db).unwrap(), &db, &broken).unwrap_err();
        assert!(format!("{err:#}").contains("0002_broken.sql"), "{err:#}");
        assert_eq!(version(&db), 1);
        assert!(!schema(&db).iter().any(|(name, _)| name == "half_done"));
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let (_dir, db) = temp_db();
        let mut conn = Connection::open(&db).unwrap();
        run(&mut conn, &db, MIGRATIONS).unwrap();
        let first = schema(&db);
        run(&mut conn, &db, MIGRATIONS).unwrap();
        assert_eq!(schema(&db), first);
        assert_eq!(version(&db), MIGRATIONS.len() as u32);
        assert!(!backup(&db, MIGRATIONS.len() as u32).exists());
    }
}
