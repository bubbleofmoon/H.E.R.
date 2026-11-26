//! Bridge-specific database selection, containment and owner configuration.

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::PathBuf;

use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
pub use her_memory::{
    index_hint, record_hits, search_error_message, DigestCursor, ForgetError, Hit, MemoryStore,
    NewMemory, Provenance, VerifiedMessage,
};
#[cfg(test)]
pub use her_memory::{list_acl_json, segment, write_memory, MemoryInput};
#[cfg(test)]
pub(crate) use fixture::{
    install as install_fixture_embeddings, install_loader as install_fixture_embedding_loader,
};

pub const DB_PATH: &str = crate::legacy_path!("data/memory.db");

struct RuntimeModelResources(RuntimeContext);

impl her_memory::ModelResources for RuntimeModelResources {
    fn resources_dir(&self) -> &std::path::Path {
        self.0.resources_dir()
    }

    fn read_model_file(&self, relative: &str) -> Option<Vec<u8>> {
        self.0.read_file(RuntimeArea::Resources, relative).ok()
    }

    fn model_file_stamp(&self, relative: &str) -> Option<her_memory::FileStamp> {
        let path = self
            .0
            .resolve_path(RuntimeArea::Resources, relative, PathIntent::ReadFile)
            .ok()?;
        let metadata = std::fs::metadata(path).ok()?;
        Some(her_memory::FileStamp::from_metadata(&metadata))
    }
}

pub fn recall(
    reader: &her_memory::Reader<'_>,
    text: &str,
    semantic: Option<(&RuntimeContext, &str)>,
    limit: usize,
    exclude: &[i64],
) -> rusqlite::Result<Vec<Hit>> {
    #[cfg(test)]
    let fixture_model = semantic.and_then(|(context, _)| fixture::model(context));
    #[cfg(not(test))]
    let fixture_model: Option<her_memory::DenseModel> = None;
    let semantic = semantic.map(|(context, query)| her_memory::SemanticQuery {
        query,
        resources: std::sync::Arc::new(RuntimeModelResources(context.clone())),
        model: fixture_model.as_ref(),
    });
    her_memory::recall(reader, text, semantic, limit, exclude)
}

/// A selected root always owns its database. Legacy MCP/console calls honor an explicit
/// Rust override, then the frozen JS override, before using the deployment default.
pub fn db_path_for_runtime(context: &RuntimeContext) -> Result<PathBuf, String> {
    if context.is_isolated() {
        context
            .resolve_path(RuntimeArea::Memory, "memory.db", PathIntent::WriteFile)
            .map_err(str::to_string)
    } else {
        Ok(std::env::var_os("HER_MEMORY_DB")
            .or_else(|| std::env::var_os("MEMORY_DB_PATH").filter(|p| !p.is_empty()))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DB_PATH)))
    }
}

fn validated_db_path_for_runtime(context: &RuntimeContext) -> Result<PathBuf, String> {
    let path = db_path_for_runtime(context)?;
    if context.is_isolated() {
        // SQLite opens its own sidecars outside Rust's filesystem wrappers. Validate every
        // existing leaf immediately before opening, then ask SQLite not to follow a reparse
        // point at the database itself. The runtime directory remains a trusted local root;
        // this preflight does not pin Windows directory handles against a concurrent replacer.
        for suffix in ["-wal", "-shm", "-journal"] {
            context
                .resolve_path(
                    RuntimeArea::Memory,
                    &format!("memory.db{suffix}"),
                    PathIntent::WriteFile,
                )
                .map_err(|_| "selected memory database is unavailable".to_string())?;
        }
    }
    Ok(path)
}

/// Open an existing database read-only, sharing runtime selection and sidecar containment
/// with the bridge. Never creates the source or follows a redirected database leaf.
pub fn open_read_only_for_runtime(context: &RuntimeContext) -> Result<Connection, String> {
    let path = validated_db_path_for_runtime(context)?;
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|_| "cannot open memory backup source".to_string())
}

/// Open the selected runtime's memory database without imposing caller-specific PRAGMA policy.
/// Console, MCP, and other callers share path selection while retaining their existing connection
/// settings and legacy override contract.
pub fn open_for_runtime(context: &RuntimeContext) -> Result<Connection, String> {
    let _restore_gate = crate::restore::writer_gate_shared(context)?;
    open_for_runtime_unlocked(context)
}

fn open_for_runtime_unlocked(context: &RuntimeContext) -> Result<Connection, String> {
    let path = validated_db_path_for_runtime(context)?;
    if context.is_isolated() {
        Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| "cannot open selected memory database".to_string())
    } else {
        Connection::open(&path).map_err(|e| format!("cannot open memory database: {e}"))
    }
}

/// Open the selected runtime's database at the current memory schema version.
pub fn open_ready_for_runtime(context: &RuntimeContext) -> Result<Connection, String> {
    let _restore_gate = crate::restore::writer_gate_shared(context)?;
    her_memory::prepare_connection(open_for_runtime_unlocked(context)?)
}

/// Open the selected runtime's database at the current memory schema version, retaining the
/// shared writer restore gate until the caller drops the returned guard.
pub(crate) fn open_ready_guarded_for_runtime(
    context: &RuntimeContext,
) -> Result<(Connection, Option<std::fs::File>), String> {
    let restore_gate = crate::restore::writer_gate_shared(context)?;
    let conn = her_memory::prepare_connection(open_for_runtime_unlocked(context)?)?;
    Ok((conn, restore_gate))
}

pub fn open_store(context: &RuntimeContext, cfg: &Value) -> Result<MemoryStore, String> {
    let restore_gate = crate::restore::writer_gate_shared(context)?;
    let store = MemoryStore::open_prepared(
        open_for_runtime_unlocked(context)?,
        crate::social_state::owner_conversation(cfg),
    )?;
    Ok(match restore_gate {
        Some(guard) => store.with_guard(guard),
        None => store,
    })
}

#[cfg(test)]
mod fixture {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{LazyLock, Mutex};

    use her_memory::{DenseModel, EmbeddingFn};

    use crate::runtime::RuntimeContext;

    type Encoder = fn(&[String]) -> Option<Vec<Vec<f32>>>;
    static RUNNERS: LazyLock<Mutex<HashMap<PathBuf, DenseModel>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    pub(crate) struct FixtureEncoder(PathBuf);

    impl Drop for FixtureEncoder {
        fn drop(&mut self) {
            RUNNERS.lock().unwrap().remove(&self.0);
        }
    }

    pub(crate) fn install(
        context: &RuntimeContext,
        embed: impl FnMut(&[String]) -> Option<Vec<Vec<f32>>> + Send + 'static,
    ) -> FixtureEncoder {
        let embed = Mutex::new(Some(embed));
        install_model(
            context,
            DenseModel::with_loader(move |_| {
                let f = embed.lock().unwrap().take()?;
                Some(Box::new(f) as EmbeddingFn)
            }),
        )
    }

    pub(crate) fn install_loader(
        context: &RuntimeContext,
        load: impl Fn() -> Option<Encoder> + Send + Sync + 'static,
    ) -> FixtureEncoder {
        install_model(
            context,
            DenseModel::with_loader(move |_| {
                let f = load()?;
                Some(Box::new(f) as EmbeddingFn)
            }),
        )
    }

    fn install_model(context: &RuntimeContext, model: DenseModel) -> FixtureEncoder {
        let path = context.runtime_root().unwrap().to_owned();
        assert!(RUNNERS.lock().unwrap().insert(path.clone(), model).is_none());
        FixtureEncoder(path)
    }

    pub(super) fn model(context: &RuntimeContext) -> Option<DenseModel> {
        RUNNERS.lock().unwrap().get(context.runtime_root()?).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_dot_recall_logs_failure_without_payload_and_keeps_index_hint() {
        const PROBE: &str = "FTS_DOT_RECALL_PROBE";
        if std::env::var_os(PROBE).is_some() {
            let (_root, context) = isolated_fixture();
            let conn = open_ready_for_runtime(&context).unwrap();
            write_memory(&conn, &MemoryInput {
                owner_type: "session", owner_id: "group:101", scope: "group", kind: "fact",
                subject: Some("retained-topic"), content: "private-query-token-path", tags: None,
                source: "fixture", importance: 0.5,
            }).unwrap();
            conn.execute_batch("CREATE TRIGGER fail_hits BEFORE UPDATE OF hits ON memory BEGIN SELECT RAISE(FAIL, 'private-query-token-path'); END;").unwrap();
            drop(conn);
            // Rendering never updates `hits`, so the refused update surfaces when the delivery is counted.
            let block = crate::prompt::recall_block_with_runtime(&context,
                &serde_json::json!({"memory":{"enabled":true}}),
                &serde_json::json!({"unread":[{"text":"private-query-token-path"}]}), "group:101");
            assert!(block.contains("retained-topic"), "keep the available index hint: {block}");
            crate::prompt::record_recall_hits(&context, &[1]);
            return;
        }
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        #[cfg(windows)] {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let output = command.args(["--exact", "memory::tests::fts_dot_recall_logs_failure_without_payload_and_keeps_index_hint", "--nocapture"])
            .env(PROBE, "1").output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(diagnostic.contains("memory recall hit count failed"), "missing diagnostic: {diagnostic}");
        assert!(!diagnostic.contains("private-query-token-path"), "payload leaked: {diagnostic}");
        assert!(!diagnostic.contains("ticket03-memory-fixture"), "path leaked: {diagnostic}");
    }

    #[test]
    fn ticket17_legacy_memory_override_is_used_without_opening_default() {
        const PROBE: &str = "TICKET17_MEMORY_OVERRIDE_PROBE";
        if let Some(path) = std::env::var_os(PROBE) {
            let context = RuntimeContext::legacy();
            // Assert routing before opening: even the red run cannot touch production.
            assert_eq!(db_path_for_runtime(&context).unwrap(), PathBuf::from(&path));
            let conn = open_ready_for_runtime(&context).unwrap();
            conn.execute("INSERT INTO memory(owner_type,owner_id,scope,kind,content,source,created_at) VALUES('session','group:17','group','fact','override marker','test',1)", []).unwrap();
            assert_eq!(
                std::fs::canonicalize(
                    conn.query_row("PRAGMA database_list", [], |r| r.get::<_, String>(2))
                        .unwrap()
                )
                .unwrap(),
                std::fs::canonicalize(path).unwrap()
            );
            return;
        }
        let (root, _) = isolated_fixture();
        let path = std::fs::canonicalize(&root.0).unwrap().join("override.db");
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let output = command
            .args([
                "--exact",
                "memory::tests::ticket17_legacy_memory_override_is_used_without_opening_default",
                "--nocapture",
            ])
            .env(PROBE, &path)
            .env("HER_MEMORY_DB", &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            conn.query_row("SELECT content FROM memory", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "override marker"
        );
    }

    #[test]
    fn ticket17_interrupted_schema_recovers_without_losing_rows_or_grants() {
        let (root, context) = isolated_fixture();
        let path = root.0.join("data/memory.db");
        let conn = Connection::open(&path).unwrap();
        let schema = include_str!("../../her-memory/spec/memory-schema.sql");
        // Simulate an old first-run interruption immediately after the memory table.
        conn.execute_batch(schema.split("CREATE INDEX").next().unwrap())
            .unwrap();
        conn.execute("INSERT INTO memory(owner_type,owner_id,scope,kind,content,source,created_at) VALUES('session','group:17','group','fact','recoverable marker','test',1)", []).unwrap();
        drop(conn);
        let conn = open_ready_for_runtime(&context).unwrap();
        for table in [
            "memory_fts",
            "memory_acl",
            "team",
            "team_member",
            "role_group",
            "role_group_session",
            "memory_audit",
        ] {
            conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
        }
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM memory_fts WHERE memory_fts MATCH 'recoverable'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        conn.execute("DELETE FROM memory_acl", []).unwrap();
        conn.execute("INSERT INTO memory_acl(reader_type,reader_id,read_type,read_id,created_at) VALUES('session','group:19','session','group:17',1)", []).unwrap();
        drop(conn);
        let conn = open_ready_for_runtime(&context).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM memory", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT reader_id FROM memory_acl", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "group:19"
        );
    }

    #[test]
    fn ticket17_schema_failure_rolls_back_and_retry_completes() {
        let (_root, context) = isolated_fixture();
        let conn = open_for_runtime(&context).unwrap();
        conn.execute_batch("CREATE TABLE idx_acl_reader(value TEXT);")
            .unwrap();
        drop(conn);
        assert!(open_ready_for_runtime(&context).is_err());
        let conn = open_for_runtime(&context).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='memory'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0,
            "failed initialization must roll back all schema DDL"
        );
        conn.execute_batch("DROP TABLE idx_acl_reader;").unwrap();
        drop(conn);
        let conn = open_ready_for_runtime(&context).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM memory_acl WHERE reader_type='owner'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        conn.prepare("SELECT * FROM role_group_session").unwrap();
    }

    fn rows(conn: &Connection) -> Vec<String> {
        let mut out = Vec::new();
        for sql in ["SELECT * FROM memory", "SELECT * FROM memory_acl", "SELECT * FROM memory_audit",
                    "SELECT * FROM team", "SELECT * FROM team_member", "SELECT * FROM role_group",
                    "SELECT * FROM role_group_session", "SELECT rowid, seg FROM memory_fts"] {
            let mut stmt = conn.prepare(&format!("{sql} ORDER BY rowid")).unwrap();
            let columns = stmt.column_count();
            let mapped = stmt.query_map([], |r| (0..columns)
                .map(|i| r.get::<_, rusqlite::types::Value>(i))
                .collect::<rusqlite::Result<Vec<_>>>()).unwrap();
            for row in mapped { out.push(format!("{sql}: {:?}", row.unwrap())); }
        }
        out
    }

    #[test]
    fn schema_version_stamps_unversioned_database_without_changing_rows() {
        let (root, context) = isolated_fixture();
        let conn = Connection::open(root.0.join("data/memory.db")).unwrap();
        conn.execute_batch(include_str!("../../her-memory/spec/memory-schema.sql")).unwrap();
        write_memory(&conn, &MemoryInput {
            owner_type: "session", owner_id: "group:55", scope: "group", kind: "fact",
            subject: Some("topic"), content: "versioned marker", tags: None, source: "fixture", importance: 0.5,
        }).unwrap();
        conn.execute_batch("INSERT INTO memory_acl(reader_type,reader_id,read_type,read_id,level,created_at) VALUES('session','group:56','session','group:55','read',1);
            INSERT INTO team_member(team_id,member_type,member_id) VALUES('t','session','group:55');
            INSERT INTO memory_audit(at,actor,action,memory_id,detail) VALUES(1,'fixture','write',1,NULL);").unwrap();
        let before = rows(&conn);
        drop(conn);
        let conn = open_ready_for_runtime(&context).unwrap();
        assert_eq!(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(), her_memory::SCHEMA_VERSION);
        assert_eq!(rows(&conn), before, "migration to version 1 must not change rows");
    }

    #[test]
    fn schema_version_1_skips_completeness_repair_and_newer_versions_refuse() {
        let (root, context) = isolated_fixture();
        drop(open_ready_for_runtime(&context).unwrap());
        let conn = Connection::open(root.0.join("data/memory.db")).unwrap();
        conn.execute_batch("DROP TABLE role_group_session;").unwrap();
        drop(conn);
        let conn = open_ready_for_runtime(&context).unwrap();
        assert!(conn.prepare("SELECT * FROM role_group_session").is_err(),
                "a version 1 open must not rerun the unversioned completion");
        let newer = her_memory::SCHEMA_VERSION + 1;
        conn.execute_batch(&format!("PRAGMA user_version={newer};")).unwrap();
        drop(conn);
        assert_eq!(open_ready_for_runtime(&context).err().as_deref(), Some("unsupported memory database version"));
        let conn = Connection::open(root.0.join("data/memory.db")).unwrap();
        assert_eq!(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(), newer);
    }

    #[test]
    fn schema_version_2_adds_the_digest_cursor_to_a_version_1_database() {
        let (root, context) = isolated_fixture();
        drop(open_ready_for_runtime(&context).unwrap());
        let conn = Connection::open(root.0.join("data/memory.db")).unwrap();
        conn.execute_batch("DROP TABLE memory_embedding_cache; DROP TRIGGER memory_embedding_delete; DROP TRIGGER memory_embedding_update; DROP TABLE memory_provenance; DROP TRIGGER remove_memory_provenance; DROP TABLE digest_cursor; PRAGMA user_version=1;").unwrap();
        drop(conn);
        let conn = open_ready_for_runtime(&context).unwrap();
        assert_eq!(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(), her_memory::SCHEMA_VERSION);
        conn.prepare("SELECT conversation, space, through_seq, updated_at, replies_ms, replies_seq, failed_at FROM digest_cursor").unwrap();
        conn.prepare("SELECT memory_id, message_id, attributed_to, author_id, message_at, message_seq, superseded_by FROM memory_provenance").unwrap();
        conn.prepare("SELECT memory_id, model, embedding FROM memory_embedding_cache").unwrap();
    }

    #[test]
    fn schema_version_2_keeps_every_row_and_grant_of_a_populated_version_1_database() {
        let (root, context) = isolated_fixture();
        drop(open_ready_for_runtime(&context).unwrap());
        let conn = Connection::open(root.0.join("data/memory.db")).unwrap();
        conn.execute_batch("DROP TABLE memory_embedding_cache; DROP TRIGGER memory_embedding_delete; DROP TRIGGER memory_embedding_update; DROP TABLE memory_provenance; DROP TRIGGER remove_memory_provenance; DROP TABLE digest_cursor; PRAGMA user_version=1; PRAGMA wal_autocheckpoint=0;").unwrap();
        write_memory(&conn, &MemoryInput {
            owner_type: "session", owner_id: "group:55", scope: "group", kind: "fact",
            subject: Some("topic"), content: "version one marker", tags: None, source: "fixture", importance: 0.5,
        }).unwrap();
        conn.execute_batch("INSERT INTO memory_acl(reader_type,reader_id,read_type,read_id,level,created_at) VALUES('session','group:56','session','group:55','read',1);
            INSERT INTO team_member(team_id,member_type,member_id) VALUES('t','session','group:55');
            INSERT INTO memory_audit(at,actor,action,memory_id,detail) VALUES(1,'fixture','write',1,NULL);").unwrap();
        let before = rows(&conn);
        // The version 1 writer stays open with its rows still in the WAL, as a crashed older bridge leaves them.
        let migrated = open_ready_for_runtime(&context).unwrap();
        assert_eq!(migrated.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(), her_memory::SCHEMA_VERSION);
        assert_eq!(rows(&migrated), before, "migration to version 2 must not change rows");
        migrated.prepare("SELECT conversation, space, through_seq, updated_at, replies_ms, replies_seq, failed_at FROM digest_cursor").unwrap();
        drop((migrated, conn));
    }

    #[test]
    fn schema_version_newer_database_is_refused_before_journal_mode_changes() {
        let (root, context) = isolated_fixture();
        let conn = Connection::open(root.0.join("data/memory.db")).unwrap();
        let newer = her_memory::SCHEMA_VERSION + 1;
        conn.execute_batch(&format!("PRAGMA journal_mode=DELETE; PRAGMA user_version={newer};")).unwrap();
        drop(conn);
        assert_eq!(open_ready_for_runtime(&context).err().as_deref(), Some("unsupported memory database version"));
        let conn = Connection::open(root.0.join("data/memory.db")).unwrap();
        assert_eq!(conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0)).unwrap(), "delete",
                   "a refused newer database must keep its journal mode");
    }

    #[test]
    fn schema_version_migrates_unversioned_database_missing_fts_with_rows_grants_and_audit() {
        let (root, context) = isolated_fixture();
        let conn = Connection::open(root.0.join("data/memory.db")).unwrap();
        conn.execute_batch(include_str!("../../her-memory/spec/memory-schema.sql")).unwrap();
        write_memory(&conn, &MemoryInput {
            owner_type: "session", owner_id: "group:55", scope: "group", kind: "fact",
            subject: Some("topic"), content: "fts marker", tags: None, source: "fixture", importance: 0.5,
        }).unwrap();
        conn.execute_batch("INSERT INTO memory_acl(reader_type,reader_id,read_type,read_id,level,created_at) VALUES('session','group:56','session','group:55','read',1);
            INSERT INTO team_member(team_id,member_type,member_id) VALUES('t','session','group:55');
            INSERT INTO memory_audit(at,actor,action,memory_id,detail) VALUES(1,'fixture','write',1,NULL);").unwrap();
        let before: Vec<String> = rows(&conn).into_iter().filter(|r| !r.starts_with("SELECT rowid, seg FROM memory_fts")).collect();
        conn.execute_batch("DROP TABLE memory_fts;").unwrap();
        drop(conn);
        let conn = open_ready_for_runtime(&context).unwrap();
        assert_eq!(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(), her_memory::SCHEMA_VERSION);
        let after: Vec<String> = rows(&conn).into_iter().filter(|r| !r.starts_with("SELECT rowid, seg FROM memory_fts")).collect();
        assert_eq!(after, before, "migration must keep rows, grants and audit unchanged");
        assert_eq!(conn.query_row("SELECT count(*) FROM memory_fts WHERE memory_fts MATCH 'marker'", [], |r| r.get::<_, i64>(0)).unwrap(), 1,
                   "missing FTS must be repopulated from the rows");
    }

    struct FixtureRoot(PathBuf);

    impl Drop for FixtureRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn isolated_fixture() -> (FixtureRoot, RuntimeContext) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(".scratch")
            .join(format!(
                "ticket03-memory-fixture-{}-{sequence}",
                std::process::id()
            ));
        let root = crate::install_root::development_root(root);
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("resources")).unwrap();
        std::fs::write(
            root.join("config/config.json"),
            br#"{"consoleToken":"synthetic-memory-fixture-token"}"#,
        )
        .unwrap();
        std::fs::write(root.join("data/social-v2.json"), b"{}").unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            br#"{"dsh":"http://127.0.0.1:49101","model":"http://127.0.0.1:49102","onebot":"http://127.0.0.1:49103"}"#,
        )
        .unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let context = RuntimeContext::from_root(&root).expect("synthetic runtime must validate");
        (FixtureRoot(root), context)
    }

    #[cfg(windows)]
    #[test]
    fn isolated_database_rejects_external_sqlite_sidecar_reparse_point() {
        let (root, context) = isolated_fixture();
        let outside = root
            .0
            .parent()
            .unwrap()
            .join(format!("ticket03-memory-sentinel-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        let outside_root = FixtureRoot(outside.clone());
        let sentinel = outside.join("external-sentinel.txt");
        let expected = b"synthetic external sentinel remains unchanged";
        std::fs::write(&sentinel, expected).unwrap();
        let database = root.0.join("data/memory.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute("CREATE TABLE fixture_sentinel(value TEXT)", [])
            .unwrap();
        drop(connection);

        let sidecar = root.0.join("data/memory.db-wal");
        let mut junction = std::process::Command::new("cmd.exe");
        use std::os::windows::process::CommandExt;
        junction.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        junction
            .arg("/d")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(&sidecar)
            .arg(&outside);
        let output = junction.output().expect("junction helper must start");
        if !output.status.success() {
            eprintln!(
                "SKIP: Windows directory junction unavailable for isolated SQLite sidecar check: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let result = open_for_runtime(&context);
        assert!(
            result.is_err(),
            "isolated opener must reject an external SQLite sidecar reparse point"
        );
        assert_eq!(std::fs::read(&sentinel).unwrap(), expected);
        drop((root, outside_root));
    }
}
