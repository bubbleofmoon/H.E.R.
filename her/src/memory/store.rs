//! Reader-bound access to the memory database. ACL scope, the private rule and owner authority are
//! applied here; callers name a conversation, never an `is_owner` flag.
use rusqlite::Connection;
use serde_json::Value;

use super::{Hit, MemoryInput, ReaderScope, UpsertOutcome};
use crate::runtime::RuntimeContext;

pub struct MemoryStore {
    conn: Connection,
    owner: Option<String>,
}

/// One conversation's view: search, write, forget and index see only what it may read.
pub struct Reader<'a> {
    pub(super) conn: &'a Connection,
    pub(super) key: String,
    pub(super) is_owner: bool,
}

/// ACL and archive administration, for callers that authorized first.
pub struct Admin<'a> {
    conn: &'a Connection,
}

/// A memory a conversation writes; its owner is always the reader's conversation.
pub struct NewMemory<'a> {
    pub owner_type: &'a str,
    /// `None`: `private` for a private conversation, otherwise `group`.
    pub scope: Option<&'a str>,
    pub kind: &'a str,
    pub subject: Option<&'a str>,
    pub content: &'a str,
    pub tags: Option<&'a str>,
    pub source: &'a str,
    pub importance: f64,
}

/// Why `Reader::forget` failed. Both stages run in one transaction, so a failed delete rolls back.
#[derive(Debug)]
pub enum ForgetError {
    /// The visibility check could not be read; nothing was deleted.
    Check(rusqlite::Error),
    /// The delete or its index removal failed; the transaction rolled back.
    Delete(rusqlite::Error),
}

impl MemoryStore {
    /// The selected runtime's database at the current schema version; the owner is `cfg.ownerQQ`.
    pub fn open(context: &RuntimeContext, cfg: &Value) -> Result<Self, String> {
        Ok(Self {
            conn: super::open_ready_for_runtime(context)?,
            owner: crate::conversation_authority::owner_conversation(cfg),
        })
    }

    /// An already-open connection (probes and tests); no migration runs.
    pub fn from_connection(conn: Connection, owner: Option<String>) -> Self {
        Self { conn, owner }
    }

    pub fn for_reader(&self, conversation: &str) -> Reader<'_> {
        Reader {
            conn: &self.conn,
            key: conversation.to_string(),
            is_owner: self.owner.as_deref() == Some(conversation),
        }
    }

    pub fn admin(&self) -> Admin<'_> {
        Admin { conn: &self.conn }
    }
}

impl Reader<'_> {
    pub fn is_owner(&self) -> bool {
        self.is_owner
    }

    /// The owner ids this conversation may read (diagnostic probes).
    pub fn scope(&self) -> rusqlite::Result<ReaderScope> {
        super::resolve_reader_scope(self.conn, &self.key, self.is_owner)
    }

    /// Explicit search; counts a hit on every returned memory.
    pub fn search(&self, query: &str, kinds: &[String], limit: usize) -> rusqlite::Result<Vec<Hit>> {
        super::search(self.conn, &self.key, self.is_owner, query, kinds, limit)
    }

    pub fn index(&self, max_terms: usize) -> rusqlite::Result<(i64, Vec<String>)> {
        super::index_hint(self, &[], max_terms)
    }

    pub fn write(&self, m: &NewMemory<'_>) -> rusqlite::Result<UpsertOutcome> {
        let default_scope = if self.key.starts_with("private:") { "private" } else { "group" };
        super::upsert_memory(self.conn, &MemoryInput {
            owner_type: m.owner_type,
            owner_id: &self.key,
            scope: m.scope.unwrap_or(default_scope),
            kind: m.kind,
            subject: m.subject,
            content: m.content,
            tags: m.tags,
            source: m.source,
            importance: m.importance,
        })
    }

    /// Digest progress in one sequence space (`msg`, `inbound`): (through sequence, updated at ms).
    pub fn digest_cursor(&self, space: &str) -> rusqlite::Result<Option<(i64, i64)>> {
        use rusqlite::OptionalExtension;
        self.conn.query_row("SELECT through_seq, updated_at FROM digest_cursor WHERE conversation=?1 AND space=?2",
            rusqlite::params![self.key, space], |r| Ok((r.get(0)?, r.get(1)?))).optional()
    }

    /// Write a digest's memories and advance its cursor in one transaction. `Ok(None)`: the cursor is no
    /// longer `expected` (another run digested first); nothing is written.
    pub fn write_digest(&self, space: &str, expected: Option<i64>, through: i64, memories: &[NewMemory<'_>])
        -> rusqlite::Result<Option<Vec<UpsertOutcome>>> {
        super::atomic(self.conn, |conn| {
            use rusqlite::OptionalExtension;
            let current: Option<i64> = conn.query_row("SELECT through_seq FROM digest_cursor WHERE conversation=?1 AND space=?2",
                rusqlite::params![self.key, space], |r| r.get(0)).optional()?;
            if current != expected {
                return Ok(None);
            }
            let mut written = Vec::with_capacity(memories.len());
            for memory in memories {
                written.push(self.write(memory)?);
            }
            conn.execute("INSERT INTO digest_cursor(conversation,space,through_seq,updated_at) VALUES(?1,?2,?3,?4)
                ON CONFLICT(conversation,space) DO UPDATE SET through_seq=excluded.through_seq, updated_at=excluded.updated_at",
                rusqlite::params![self.key, space, through, super::now_ms()])?;
            Ok(Some(written))
        })
    }

    /// Delete one memory this conversation can read, checking and deleting in one transaction.
    /// `Ok(None)`: absent or not visible; nothing changed.
    pub fn forget(&self, id: i64) -> Result<Option<bool>, ForgetError> {
        let mut deleting = false;
        super::atomic(self.conn, |conn| {
            if !super::can_read_id(conn, &self.key, self.is_owner, id)? {
                return Ok(None);
            }
            deleting = true;
            super::forget_memory(conn, id, &self.key).map(Some)
        })
        .map_err(|error| if deleting { ForgetError::Delete(error) } else { ForgetError::Check(error) })
    }
}

impl Admin<'_> {
    pub fn grant(&self, reader_type: &str, reader_id: &str, read_type: &str, read_id: &str, level: &str, actor: &str) -> rusqlite::Result<i64> {
        super::grant_acl(self.conn, reader_type, reader_id, read_type, read_id, level, actor)
    }

    pub fn revoke(&self, reader_type: &str, reader_id: &str, read_type: &str, read_id: &str, actor: &str) -> rusqlite::Result<bool> {
        super::revoke_acl(self.conn, reader_type, reader_id, read_type, read_id, actor)
    }

    pub fn acl(&self) -> rusqlite::Result<Vec<Value>> {
        super::list_acl_json(self.conn)
    }

    pub fn archive(&self, id: i64, archived: bool, actor: &str) -> rusqlite::Result<bool> {
        super::archive_memory(self.conn, id, archived, actor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "private:9";

    /// Rows 1 group:1 (group), 2 private:2 (private), 3 group:3 (group), 4 the owner's private row.
    /// ACL: group:1 reads group:3; group:7 holds a wildcard grant.
    fn store(owner: Option<&str>) -> MemoryStore {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../../spec/memory-schema.sql")).unwrap();
        for (owner_id, scope) in [("group:1", "group"), ("private:2", "private"), ("group:3", "group"), (OWNER, "private")] {
            crate::memory::write_memory(&conn, &MemoryInput {
                owner_type: "session", owner_id, scope, kind: "fact", subject: None,
                content: &format!("needle {owner_id}"), tags: None, source: "fixture", importance: 0.5,
            }).unwrap();
        }
        conn.execute_batch("INSERT INTO memory_acl(reader_type,reader_id,read_type,read_id,level,created_at) VALUES
            ('session','group:1','session','group:3','read',1),
            ('session','group:7','*','*','read',1);").unwrap();
        MemoryStore::from_connection(conn, owner.map(str::to_string))
    }

    fn visible(store: &MemoryStore, reader: &str) -> Vec<i64> {
        let mut ids: Vec<i64> = store.for_reader(reader).search("needle", &[], 50).unwrap()
            .into_iter().map(|h| h.id).collect();
        ids.sort();
        ids
    }

    fn exists(store: &MemoryStore, id: i64) -> bool {
        store.conn.query_row("SELECT EXISTS(SELECT 1 FROM memory WHERE id=?1)", [id], |r| r.get(0)).unwrap()
    }

    #[test]
    fn private_rule_hides_other_private_rows_even_with_wildcard_grant() {
        let store = store(Some(OWNER));
        assert_eq!(visible(&store, "group:7"), vec![1, 3]);
        let reader = store.for_reader("group:7");
        assert_eq!(reader.index(12).unwrap().0, 2);
        assert_eq!(reader.forget(2).unwrap(), None);
        assert_eq!(reader.forget(4).unwrap(), None);
        assert!(exists(&store, 2) && exists(&store, 4));
        assert_eq!(visible(&store, "private:2"), vec![2], "a private conversation reads only its own private rows");
    }

    #[test]
    fn scope_rule_limits_reader_to_its_rows_and_grants() {
        let store = store(Some(OWNER));
        assert_eq!(visible(&store, "group:1"), vec![1, 3]);
        assert_eq!(visible(&store, "group:3"), vec![3]);
        let reader = store.for_reader("group:3");
        assert_eq!(reader.index(12).unwrap().0, 1);
        assert_eq!(reader.forget(1).unwrap(), None);
        assert!(exists(&store, 1));
        assert_eq!(reader.forget(3).unwrap(), Some(true));
        assert!(!exists(&store, 3));
    }

    #[test]
    fn owner_rule_reads_everything_only_for_the_configured_owner() {
        let owned = store(Some(OWNER));
        assert_eq!(visible(&owned, OWNER), vec![1, 2, 3, 4]);
        assert_eq!(owned.for_reader(OWNER).index(12).unwrap().0, 4);
        assert_eq!(owned.for_reader(OWNER).forget(2).unwrap(), Some(true));
        let unowned = store(None);
        assert_eq!(visible(&unowned, OWNER), vec![4], "without a configured owner it is an ordinary private reader");
        assert_eq!(unowned.for_reader(OWNER).forget(2).unwrap(), None);
    }

    #[test]
    fn writes_belong_to_the_reader_conversation() {
        let store = store(Some(OWNER));
        let new = |scope: Option<&'static str>| NewMemory {
            owner_type: "session", scope, kind: "fact", subject: None, content: "written",
            tags: None, source: "fixture", importance: 0.5,
        };
        let group = store.for_reader("group:3").write(&new(None)).unwrap();
        let private = store.for_reader("private:2").write(&new(None)).unwrap();
        let row = |id: i64| -> (String, String) {
            store.conn.query_row("SELECT owner_id, scope FROM memory WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
        };
        assert_eq!(row(group.id), ("group:3".to_string(), "group".to_string()));
        assert_eq!(row(private.id), ("private:2".to_string(), "private".to_string()));
    }

    #[test]
    fn digest_write_commits_memories_with_its_cursor_and_refuses_a_moved_cursor() {
        let store = store(Some(OWNER));
        store.conn.execute_batch(crate::memory::DIGEST_CURSOR_V2).unwrap();
        let new = |content: &'static str| NewMemory { owner_type: "session", scope: None, kind: "inference",
            subject: Some("s"), content, tags: None, source: "digest:group:3:msg:1-4", importance: 0.5 };
        let reader = store.for_reader("group:3");
        assert_eq!(reader.digest_cursor("msg").unwrap(), None);
        assert_eq!(reader.write_digest("msg", None, 4, &[new("digested")]).unwrap().map(|w| w.len()), Some(1));
        assert_eq!(reader.digest_cursor("msg").unwrap().map(|c| c.0), Some(4));
        assert!(reader.write_digest("msg", None, 9, &[new("stale")]).unwrap().is_none(), "a stale expected cursor writes nothing");
        let rows: i64 = store.conn.query_row("SELECT count(*) FROM memory WHERE source LIKE 'digest:%'", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 1);
        assert_eq!(reader.digest_cursor("msg").unwrap().map(|c| c.0), Some(4));
        assert_eq!(store.for_reader("group:1").digest_cursor("msg").unwrap(), None, "cursors are per conversation");
        assert_eq!(reader.digest_cursor("inbound").unwrap(), None, "cursors are per sequence space");
    }
}
