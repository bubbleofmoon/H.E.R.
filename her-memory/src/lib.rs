//! SQLite memory store (`data/memory.db`): ACL scope resolution, FTS5 bigram search,
//! and strict `private` visibility (limited to owners and the originating private chat).

use rusqlite::{params, Connection, OptionalExtension, ToSql};

mod dense;
pub mod parity_clock;
mod store;

pub use dense::{DenseModel, EmbeddingFn, FileStamp, ModelResources, SemanticQuery};
use parity_clock::now_ms;
pub use store::{
    Admin, DigestCursor, ForgetError, MemoryStore, NewMemory, Reader, VerifiedMessage,
};

/// Current memory schema version (`PRAGMA user_version`). Version 1 is the frozen
/// `spec/memory-schema.sql`, version 2 adds `digest_cursor`, version 3 adds `memory_provenance`,
/// and version 4 adds the derived embedding cache.
pub const SCHEMA_VERSION: i64 = 4;
const PROVENANCE_V3: &str = "CREATE TABLE IF NOT EXISTS memory_provenance (
  memory_id INTEGER PRIMARY KEY, message_id TEXT, attributed_to TEXT, author_id TEXT, message_at INTEGER, message_seq INTEGER, superseded_by INTEGER
);
CREATE INDEX IF NOT EXISTS memory_provenance_message ON memory_provenance(message_id);
CREATE TRIGGER IF NOT EXISTS remove_memory_provenance AFTER DELETE ON memory
BEGIN DELETE FROM memory_provenance WHERE memory_id=OLD.id; END;";
/// Version 2: the Digest's progress per conversation and sequence space, written with its memories.
const DIGEST_CURSOR_V2: &str = "CREATE TABLE IF NOT EXISTS digest_cursor (
  conversation TEXT NOT NULL, space TEXT NOT NULL, through_seq INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  replies_ms INTEGER NOT NULL DEFAULT 0, replies_seq INTEGER NOT NULL DEFAULT 0, failed_at INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (conversation, space));";

/// Prepare an opened memory connection for the current schema. A newer version refuses to open.
pub fn prepare_connection(conn: Connection) -> Result<Connection, String> {
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    // A newer schema is refused before the first write: the journal_mode change below rewrites the header.
    let stored: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if stored > SCHEMA_VERSION {
        return Err("unsupported memory database version".to_string());
    }
    // journal_mode cannot change in a transaction. Set it first; the schema's same-mode
    // PRAGMA is then harmless inside the transaction containing every DDL/seed operation.
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| e.to_string())?;
    let version = atomic(&conn, migrate).map_err(|e| e.to_string())?;
    if version != SCHEMA_VERSION {
        return Err("unsupported memory database version".to_string());
    }
    Ok(conn)
}

/// Bring the schema to `SCHEMA_VERSION` inside one IMMEDIATE transaction and return the version
/// in force. Version 0 is every database made before versioning, including ones an older
/// initializer left incomplete: it is completed in place and stamped. A newer version is untouched.
fn migrate(conn: &Connection) -> rusqlite::Result<i64> {
    let mut version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == 0 {
        complete_unversioned(conn)?;
        version = 1;
        conn.pragma_update(None, "user_version", version)?;
    }
    if version == 1 {
        conn.execute_batch(DIGEST_CURSOR_V2)?;
        version = 2;
        conn.pragma_update(None, "user_version", version)?;
    }
    if version == 2 {
        conn.execute_batch(PROVENANCE_V3)?;
        version = 3;
        conn.pragma_update(None, "user_version", version)?;
    }
    if version == 3 {
        conn.execute_batch(dense::CACHE_SCHEMA_V4)?;
        version = 4;
        conn.pragma_update(None, "user_version", version)?;
    }
    Ok(version)
}

/// Existing rows, grants and FTS entries survive; only a missing FTS table is populated from rows.
fn complete_unversioned(conn: &Connection) -> rusqlite::Result<()> {
    let exists = |name: &str| -> rusqlite::Result<bool> {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
            [name],
            |r| r.get(0),
        )
    };
    let had_fts = exists("memory_fts")?;
    let had_acl = exists("memory_acl")?;
    let mut complete = true;
    for name in [
        "memory",
        "memory_fts",
        "memory_acl",
        "team",
        "team_member",
        "role_group",
        "role_group_session",
        "memory_audit",
        "idx_memory_owner",
        "idx_memory_recent",
        "idx_acl_reader",
    ] {
        complete &= exists(name)?;
    }
    if complete {
        return Ok(());
    }
    conn.execute_batch(include_str!("../spec/memory-schema.sql"))?;
    if !had_fts {
        let mut stmt = conn.prepare("SELECT id, subject, content, tags FROM memory")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (id, subject, content, tags) = row?;
            let seg = segment(&format!(
                "{} {} {}",
                subject.unwrap_or_default(),
                content,
                tags.unwrap_or_default()
            ))
            .join(" ");
            conn.execute(
                "INSERT INTO memory_fts(rowid,seg) VALUES(?1,?2)",
                params![id, seg],
            )?;
        }
    }
    if !had_acl {
        conn.execute("INSERT INTO memory_acl(reader_type,reader_id,read_type,read_id,level,created_at) VALUES('owner','main','*','*','read',?1)", [now_ms()])?;
    }
    Ok(())
}

fn atomic<T>(
    conn: &Connection,
    operation: impl FnOnce(&Connection) -> rusqlite::Result<T>,
) -> rusqlite::Result<T> {
    if !conn.is_autocommit() {
        return operation(conn);
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let result = operation(&tx)?;
    tx.commit()?;
    Ok(result)
}

/// 中文按 2 字 bigram、英文/数字按词切分，用空格连接（与 memory/schema.mjs 的 segment 一致）。
pub fn segment(text: &str) -> Vec<String> {
    segment_spans(text)
        .into_iter()
        .map(|(token, _, _)| token)
        .collect()
}

/// `segment` with where each token came from: its start and end (exclusive) as character indexes
/// of `text`. A Han bigram covers its two characters, so neighbouring bigrams overlap.
fn segment_spans(text: &str) -> Vec<(String, usize, usize)> {
    let mut out: Vec<(String, usize, usize)> = Vec::new();
    let mut han = String::new();
    let mut han_start = 0usize;
    let mut word = String::new();
    let mut word_start = 0usize;

    fn flush_han(han: &mut String, start: usize, out: &mut Vec<(String, usize, usize)>) {
        let chars: Vec<char> = han.chars().collect();
        if chars.len() == 1 {
            out.push((chars[0].to_string(), start, start + 1));
        }
        for (i, w) in chars.windows(2).enumerate() {
            out.push((w.iter().collect(), start + i, start + i + 2));
        }
        han.clear();
    }
    fn flush_word(word: &mut String, start: usize, out: &mut Vec<(String, usize, usize)>) {
        if !word.is_empty() {
            out.push((word.to_lowercase(), start, start + word.chars().count()));
            word.clear();
        }
    }

    for (i, ch) in text.chars().enumerate() {
        let is_han = ('\u{4e00}'..='\u{9fff}').contains(&ch);
        let is_word = ch.is_ascii_alphanumeric() || ch == '_' || ch == '.';
        if is_han {
            flush_word(&mut word, word_start, &mut out);
            if han.is_empty() {
                han_start = i;
            }
            han.push(ch);
        } else if is_word {
            flush_han(&mut han, han_start, &mut out);
            if word.is_empty() {
                word_start = i;
            }
            word.push(ch);
        } else {
            flush_han(&mut han, han_start, &mut out);
            flush_word(&mut word, word_start, &mut out);
        }
    }
    flush_han(&mut han, han_start, &mut out);
    flush_word(&mut word, word_start, &mut out);
    out
}

/// 读者可见的 owner 范围。
#[derive(Debug, Clone)]
struct ReaderScope {
    allow_all: bool,
    ids: Vec<String>,
}

/// 解析读者能读哪些 owner_id（对应 JS 的 resolveReaderScope）。
fn resolve_reader_scope(
    conn: &Connection,
    reader_key: &str,
    is_owner: bool,
) -> rusqlite::Result<ReaderScope> {
    let mut ids: Vec<String> = vec![reader_key.to_string()];
    let person = reader_key
        .strip_prefix("private:")
        .filter(|rest| rest.chars().all(|c| c.is_ascii_digit()))
        .map(|qq| format!("person:{qq}"));
    if let Some(p) = &person {
        ids.push(p.clone());
    }

    let mut stmt = conn.prepare(
        "SELECT reader_type, read_type, read_id FROM memory_acl
         WHERE (reader_type='session' AND reader_id=?1)
            OR (reader_type='person'  AND reader_id=?2)
            OR (reader_type='team'    AND reader_id IN
                (SELECT team_id FROM team_member WHERE member_type='session' AND member_id=?1))",
    )?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map(
            params![reader_key, person.clone().unwrap_or_default()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut allow_all = is_owner;
    for (_rtype, read_type, read_id) in rows {
        match read_type.as_str() {
            "*" => allow_all = true,
            "session" if read_id == "*" => allow_all = true,
            "session" if !read_id.is_empty() => ids.push(read_id),
            "person" if !read_id.is_empty() && read_id != "*" => {
                ids.push(format!("person:{read_id}"))
            }
            "team" if !read_id.is_empty() => {
                let mut ms = conn
                    .prepare("SELECT member_type, member_id FROM team_member WHERE team_id=?1")?;
                let members: Vec<(String, String)> = ms
                    .query_map(params![read_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (mt, mid) in members {
                    ids.push(if mt == "person" {
                        format!("person:{mid}")
                    } else {
                        mid
                    });
                }
            }
            _ => {}
        }
    }
    ids.sort();
    ids.dedup();
    Ok(ReaderScope { allow_all, ids })
}

/// 数据库行（FTS 与回退查询共用同一形状）。
type Row = (
    i64,
    String,
    String,
    String,
    Option<String>,
    String,
    f64,
    i64,
    f64,
    i64,
    i64,
    Option<String>,
    String,
    Option<i64>,
    String,
    i64,
    Option<i64>,
);

/// Read one `Row` (the 17 `memory` columns in the order both queries select them).
fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get::<_, f64>(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
        r.get(11)?,
        r.get(12)?,
        r.get::<_, Option<i64>>(13)?,
        r.get(14)?,
        r.get(15)?,
        r.get::<_, Option<i64>>(16)?,
    ))
}

fn hit_from_row(row: Row, score: f64) -> Hit {
    let (
        id,
        owner_id,
        scope,
        kind,
        subject,
        content,
        _rank,
        pinned,
        importance,
        hits,
        created_at,
        tags,
        source,
        last_used_at,
        owner_type,
        archived,
        expires_at,
    ) = row;
    Hit {
        id,
        owner_id,
        scope,
        kind,
        subject,
        content,
        score,
        tags,
        source,
        importance,
        pinned,
        hits,
        created_at,
        last_used_at,
        owner_type,
        archived,
        expires_at,
        provenance: Provenance::Inference,
        message_id: None,
        attributed_to: None,
    }
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    Fact,
    #[default]
    Inference,
}

/// 一条检索结果。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Hit {
    pub id: i64,
    pub owner_id: String,
    pub scope: String,
    pub kind: String,
    pub subject: Option<String>,
    pub content: String,
    pub score: f64,
    // 下面这些是给 `/api/memory/search` 用的完整投影：JS 那边返回的是 `SELECT m.*`，
    // 少一列对拍就是硬差异；提示词那条链只用前面几个字段，多出来不影响。
    pub tags: Option<String>,
    pub source: String,
    pub importance: f64,
    pub pinned: i64,
    pub hits: i64,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub owner_type: String,
    pub archived: i64,
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub provenance: Provenance,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub attributed_to: Option<String>,
}

impl Hit {
    pub fn provenance_label(&self) -> &'static str {
        match self.provenance {
            Provenance::Fact => "事实",
            Provenance::Inference => "推断",
        }
    }
}

/// SQLite messages may contain query text, stored values, trigger messages or paths.
/// Public errors and recall diagnostics keep only the operation and numeric code.
pub fn search_error_message(error: &rusqlite::Error) -> String {
    match error {
        rusqlite::Error::SqliteFailure(code, _) => {
            format!("memory search failed (sqlite code {})", code.extended_code)
        }
        _ => "memory search failed (row or statement error)".to_string(),
    }
}

/// The rows a reader may see, as a WHERE fragment over `memory m` plus its bound arguments:
/// not archived, inside the reader's owner scope, of the requested kinds, and never another
/// session's private rows (no ACL bypasses that). Search and Recall share it.
fn visibility_filter(
    conn: &Connection,
    reader_key: &str,
    is_owner: bool,
    kinds: &[String],
) -> rusqlite::Result<(String, Vec<String>)> {
    let scope = resolve_reader_scope(conn, reader_key, is_owner)?;

    let mut where_parts: Vec<String> = vec!["m.archived = 0".to_string()];
    let mut args: Vec<String> = Vec::new();

    if !scope.allow_all {
        let placeholders = vec!["?"; scope.ids.len()].join(",");
        where_parts.push(format!("m.owner_id IN ({placeholders})"));
        for id in &scope.ids {
            args.push(id.clone());
        }
    }
    if !kinds.is_empty() {
        let placeholders = vec!["?"; kinds.len()].join(",");
        where_parts.push(format!("m.kind IN ({placeholders})"));
        for k in kinds {
            args.push(k.clone());
        }
    }
    // 硬约束：非 owner 非本会话私聊，看不到 private（任何 ACL 都绕不过）
    if !is_owner {
        where_parts.push("(m.scope <> 'private' OR m.owner_id = ?)".to_string());
        args.push(reader_key.to_string());
    }
    Ok((where_parts.join(" AND "), args))
}

/// 检索记忆（对应 JS 的 searchMemory）。命中会自增 `hits`。
fn search(
    conn: &Connection,
    reader_key: &str,
    is_owner: bool,
    query: &str,
    kinds: &[String],
    limit: usize,
) -> rusqlite::Result<Vec<Hit>> {
    search_with(conn, reader_key, is_owner, query, kinds, limit, true)
}

/// 同 `search`；`count_hits=false` 只读（唤醒提示词渲染用），不改 `hits` / `last_used_at`；
/// 内容真正送达后由 `record_hits` 按送达的 id 计数。
fn search_with(
    conn: &Connection,
    reader_key: &str,
    is_owner: bool,
    query: &str,
    kinds: &[String],
    limit: usize,
    count_hits: bool,
) -> rusqlite::Result<Vec<Hit>> {
    let (where_sql, args) = visibility_filter(conn, reader_key, is_owner, kinds)?;

    let seg = segment(query)
        .into_iter()
        // Mirror JS searchMemory; segment itself also feeds the stored index.
        .map(|token| {
            token
                .chars()
                .filter(|c| {
                    ('\u{4e00}'..='\u{9fff}').contains(c)
                        || c.is_ascii_lowercase()
                        || c.is_ascii_digit()
                        || *c == '_'
                })
                .collect::<String>()
        })
        .filter(|token| !token.is_empty())
        .take(24)
        .collect::<Vec<_>>()
        .join(" ");

    let mut rows: Vec<Row> = Vec::new();

    if !seg.is_empty() {
        let sql = format!(
            "SELECT m.id, m.owner_id, m.scope, m.kind, m.subject, m.content,
                    bm25(memory_fts) AS rank, m.pinned, m.importance, m.hits, m.created_at,
                    m.tags, m.source, m.last_used_at, m.owner_type, m.archived, m.expires_at
             FROM memory_fts f JOIN memory m ON m.id = f.rowid
             WHERE memory_fts MATCH ? AND {where_sql}
             ORDER BY rank LIMIT 60"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut refs: Vec<&dyn ToSql> = vec![&seg as &dyn ToSql];
        for a in &args {
            refs.push(a as &dyn ToSql);
        }
        let mapped = stmt.query_map(refs.as_slice(), read_row)?;
        for row in mapped {
            rows.push(row?);
        }
    }

    if rows.is_empty() {
        let sql = format!(
            "SELECT m.id, m.owner_id, m.scope, m.kind, m.subject, m.content, 0 AS rank,
                    m.pinned, m.importance, m.hits, m.created_at,
                    m.tags, m.source, m.last_used_at, m.owner_type, m.archived, m.expires_at
             FROM memory m WHERE {where_sql}
             ORDER BY m.pinned DESC, m.importance DESC, m.created_at DESC LIMIT ?"
        );
        let mut stmt = conn.prepare(&sql)?;
        let lim = (limit * 4).max(20) as i64;
        let mut refs: Vec<&dyn ToSql> = Vec::new();
        for a in &args {
            refs.push(a as &dyn ToSql);
        }
        refs.push(&lim as &dyn ToSql);
        let mapped = stmt.query_map(refs.as_slice(), read_row)?;
        for row in mapped {
            rows.push(row?);
        }
    }

    let now = now_ms();
    let mut scored: Vec<Hit> = rows
        .into_iter()
        .map(|row| {
            let (rank, pinned, importance, hits, created_at) = (row.6, row.7, row.8, row.9, row.10);
            let age_days = ((now - created_at) as f64 / 86_400_000.0).max(0.0);
            let recency = (-age_days / 30.0).exp();
            let bm = if rank != 0.0 { -rank } else { 0.0 };
            let score = (if pinned != 0 { 1.0 } else { 0.0 })
                + 0.5 * importance
                + 0.3 * recency
                + 0.2 * bm.tanh()
                + 0.05 * ((hits as f64) / 5.0).tanh();
            hit_from_row(row, score)
        })
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // JS 的 searchMemory 只返回 limit 条；不截断会让调用方拿到比问的多得多的结果
    scored.truncate(limit);

    // 命中计数（与 JS 一致：只对最终 top N 自增）
    if count_hits {
        record_hits(conn, &scored.iter().map(|h| h.id).collect::<Vec<_>>())?;
    }
    Ok(scored)
}

// ── Recall: the automatic memory recall a wake carries ─────────────────────────

/// At most this many distinct terms of the message enter the match (the message is cut at 200
/// characters upstream, so this never drops a term of a chat-length message).
const RECALL_MAX_TERMS: usize = 200;
const RECALL_MAX_ITEMS: usize = 5;
const FUSION_DEPTH: usize = 20;
/// Relevance bar: a memory qualifies when it shares two or more distinct terms totalling at least
/// this many characters with the message (two Chinese words), or exactly one term that is an
/// identifier or at least `RECALL_LONG_TERM_CHARS` characters long. One shared Chinese bigram or
/// short common word (`好的`, `https`, `2026`) is not evidence of relevance, however short the
/// message is.
const RECALL_MIN_SHARED_CHARS: usize = 4;
const RECALL_LONG_TERM_CHARS: usize = 6;
/// bm25-ranked candidates are read in pages of this size until `limit` of them pass the bar, at
/// most `RECALL_MAX_CANDIDATES` in all, so a run of near-misses cannot hide a qualifying row.
const RECALL_PAGE: usize = 60;
const RECALL_MAX_CANDIDATES: usize = 600;

/// A character that counts toward the message floor and the shared amount: Han or ASCII
/// letter or digit (not the dot or underscore that only join a word).
fn recall_counts(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c) || c.is_ascii_alphanumeric()
}

/// A single-term evidence: at least four letters or digits, with a dot or underscore or letters
/// mixed with digits (`zebra148kw`, `v0.2.1`, `foo_bar`; not `__a_`).
fn recall_is_identifier(term: &str) -> bool {
    term.chars().filter(|c| c.is_ascii_alphanumeric()).count() >= 4
        && (term.contains(['.', '_'])
            || (term.chars().any(|c| c.is_ascii_lowercase())
                && term.chars().any(|c| c.is_ascii_digit())))
}

/// `covered` is the number of distinct message characters the shared terms cover (知道 and 道了
/// cover three, not four). A single shared term is judged by its own length, never by how often
/// the message repeats it.
fn recall_qualifies(shared: &[&String], covered: usize) -> bool {
    match shared {
        [] => false,
        [one] => {
            recall_is_identifier(one)
                || one.chars().filter(|c| recall_counts(*c)).count() >= RECALL_LONG_TERM_CHARS
        }
        _ => covered >= RECALL_MIN_SHARED_CHARS,
    }
}

/// One index term as Recall compares it: Han, lowercase ASCII, digits, underscore and the dot
/// `segment` keeps inside a word; a term without a letter, digit or Han character is dropped.
fn recall_term(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| {
            ('\u{4e00}'..='\u{9fff}').contains(c)
                || c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || *c == '_'
                || *c == '.'
        })
        .collect();
    let cleaned = cleaned.trim_matches('.');
    let stopword = matches!(
        cleaned,
        "你好" | "谢谢" | "请问" | "那个" | "这个" | "什么" | "一下"
    );
    (!stopword && cleaned.chars().any(char::is_alphanumeric)).then(|| cleaned.to_string())
}

/// The distinct terms of a message in message order, each with the indexes of the message
/// characters (`recall_counts`) it covers.
fn recall_term_spans(text: &str) -> Vec<(String, Vec<usize>)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<(String, Vec<usize>)> = Vec::new();
    for (token, start, end) in segment_spans(text) {
        let Some(term) = recall_term(&token) else {
            continue;
        };
        let covered: Vec<usize> = (start..end).filter(|&i| recall_counts(chars[i])).collect();
        if let Some(seen) = out.iter().position(|(existing, _)| *existing == term) {
            out[seen].1.extend(covered);
        } else if out.len() < RECALL_MAX_TERMS {
            out.push((term, covered));
        }
    }
    out
}

/// The distinct terms of a message, in message order.
#[cfg(test)]
fn recall_terms(text: &str) -> Vec<String> {
    recall_term_spans(text)
        .into_iter()
        .map(|(term, _)| term)
        .collect()
}

/// An FTS5 expression that matches a row holding any of the terms. Each term is a quoted FTS5
/// string, so punctuation and operator words in the message are data, never syntax.
fn recall_match_expression(terms: &[String]) -> String {
    terms
        .iter()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Automatic recall fuses applicable local semantic and sparse ranks. Both channels
/// filter by reader scope and the ids already held by this session generation.
/// Hits are not counted here: the caller counts the items it actually delivered with `record_hits`.
pub fn recall(
    reader: &Reader<'_>,
    text: &str,
    semantic: Option<SemanticQuery<'_>>,
    limit: usize,
    exclude: &[i64],
) -> rusqlite::Result<Vec<Hit>> {
    let limit = limit.min(RECALL_MAX_ITEMS);
    // A message under four letters, digits or Han characters is too short to say what it is about.
    if limit == 0 || text.chars().filter(|c| recall_counts(*c)).count() < RECALL_MIN_SHARED_CHARS {
        return Ok(Vec::new());
    }
    let excluded = if exclude.is_empty() {
        String::new()
    } else {
        let ids: Vec<String> = exclude.iter().map(i64::to_string).collect();
        format!(" AND m.id NOT IN ({})", ids.join(","))
    };

    let dense = semantic
        .and_then(|query| dense::rank(query, reader, &excluded, FUSION_DEPTH))
        .unwrap_or_default();
    let sparse_depth = if dense.is_empty() {
        limit
    } else {
        FUSION_DEPTH
    };
    let sparse = match sparse_recall(reader, text, &excluded, sparse_depth) {
        Ok(hits) => hits,
        Err(error) if dense.is_empty() => return Err(error),
        Err(error) => {
            eprintln!(
                "memory recall sparse search failed: {}",
                search_error_message(&error)
            );
            Vec::new()
        }
    };
    let mut hits = if dense.is_empty() {
        sparse
    } else {
        fuse_recall(sparse, dense, limit)
    };
    reader.attach_provenance(&mut hits)?;
    Ok(hits)
}

fn fuse_recall(sparse: Vec<Hit>, dense: Vec<Hit>, limit: usize) -> Vec<Hit> {
    const RANK_OFFSET: f64 = 60.0;
    let mut fused = std::collections::HashMap::<i64, Hit>::new();
    for ranking in [sparse, dense] {
        for (position, mut hit) in ranking.into_iter().enumerate() {
            hit.score = 1.0 / (RANK_OFFSET + 1.0 + position as f64);
            fused
                .entry(hit.id)
                .and_modify(|existing| existing.score += hit.score)
                .or_insert(hit);
        }
    }
    let mut hits: Vec<_> = fused.into_values().collect();
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    hits.truncate(limit);
    hits
}

fn sparse_recall(
    reader: &Reader<'_>,
    text: &str,
    excluded: &str,
    limit: usize,
) -> rusqlite::Result<Vec<Hit>> {
    let (conn, reader_key, is_owner) = (reader.conn, reader.key.as_str(), reader.is_owner);
    let spans = recall_term_spans(text);
    let terms: Vec<String> = spans.iter().map(|(term, _)| term.clone()).collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let (where_sql, args) = visibility_filter(conn, reader_key, is_owner, &[])?;
    let sql = format!(
        "SELECT m.id, m.owner_id, m.scope, m.kind, m.subject, m.content,
                bm25(memory_fts) AS rank, m.pinned, m.importance, m.hits, m.created_at,
                m.tags, m.source, m.last_used_at, m.owner_type, m.archived, m.expires_at,
                f.seg
         FROM memory_fts f JOIN memory m ON m.id = f.rowid
         WHERE memory_fts MATCH ? AND {where_sql}{excluded}
         ORDER BY rank, m.id LIMIT {RECALL_PAGE} OFFSET ?"
    );
    let expression = recall_match_expression(&terms);
    let mut stmt = conn.prepare(&sql)?;

    let now = now_ms();
    let mut scored: Vec<Hit> = Vec::new();
    let mut offset = 0usize;
    while offset < RECALL_MAX_CANDIDATES && scored.len() < limit {
        let offset_arg = offset as i64;
        let mut refs: Vec<&dyn ToSql> = vec![&expression as &dyn ToSql];
        for a in &args {
            refs.push(a as &dyn ToSql);
        }
        refs.push(&offset_arg as &dyn ToSql);
        let page = stmt
            .query_map(refs.as_slice(), |r| {
                Ok((read_row(r)?, r.get::<_, String>(17)?))
            })?
            .collect::<rusqlite::Result<Vec<(Row, String)>>>()?;
        let last_page = page.len() < RECALL_PAGE;
        for (row, seg) in page {
            // An indexed dotted word (`example.test`) is also its parts, as the FTS tokenizer sees it.
            let mut indexed: std::collections::HashSet<String> = std::collections::HashSet::new();
            for token in seg.split_whitespace() {
                indexed.extend(token.split('.').filter_map(recall_term));
                indexed.extend(recall_term(token));
            }
            let shared_spans: Vec<&(String, Vec<usize>)> = spans
                .iter()
                .filter(|(term, _)| indexed.contains(term))
                .collect();
            let shared: Vec<&String> = shared_spans.iter().map(|(term, _)| term).collect();
            let covered: std::collections::HashSet<usize> = shared_spans
                .iter()
                .flat_map(|(_, positions)| positions.iter().copied())
                .collect();
            let shared_chars = covered.len();
            if !recall_qualifies(&shared, shared_chars) {
                continue;
            }
            let (rank, pinned, importance, hits, created_at) = (row.6, row.7, row.8, row.9, row.10);
            let age_days = ((now - created_at) as f64 / 86_400_000.0).max(0.0);
            // Relevance spans most of [0, 1); the boosts add at most 0.05 in total.
            let relevance =
                0.5 * (shared_chars as f64 / 12.0).tanh() + 0.5 * (-rank).max(0.0).tanh();
            let boost = 0.02 * if pinned != 0 { 1.0 } else { 0.0 }
                + 0.015 * importance.clamp(0.0, 1.0)
                + 0.01 * (-age_days / 30.0).exp()
                + 0.005 * ((hits as f64) / 5.0).tanh();
            scored.push(hit_from_row(row, relevance + boost));
        }
        if last_page {
            break;
        }
        offset += RECALL_PAGE;
    }
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.id.cmp(&b.id))
    });
    scored.truncate(limit);
    Ok(scored)
}

/// 每条记忆计一次命中（`hits` +1，`last_used_at` 更新）。
pub fn record_hits(conn: &Connection, ids: &[i64]) -> rusqlite::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let now = now_ms();
    let mut bump =
        conn.prepare("UPDATE memory SET hits = hits + 1, last_used_at = ?1 WHERE id = ?2")?;
    for id in ids {
        bump.execute(params![now, id])?;
    }
    Ok(())
}

// ── 写路径（与 memory-store.mjs / schema.mjs 对齐）────────────────────────────

/// 新增一条记忆并写入 FTS 索引（对应 schema.mjs 的 writeMemory）。
pub struct MemoryInput<'a> {
    pub owner_type: &'a str,
    pub owner_id: &'a str,
    pub scope: &'a str,
    pub kind: &'a str,
    pub subject: Option<&'a str>,
    pub content: &'a str,
    pub tags: Option<&'a str>,
    pub source: &'a str,
    pub importance: f64,
}

pub fn write_memory(conn: &Connection, m: &MemoryInput<'_>) -> rusqlite::Result<i64> {
    atomic(conn, |conn| write_memory_inner(conn, m))
}

fn write_memory_inner(conn: &Connection, m: &MemoryInput<'_>) -> rusqlite::Result<i64> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO memory(owner_type,owner_id,scope,kind,subject,content,tags,source,importance,created_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![m.owner_type, m.owner_id, m.scope, m.kind, m.subject, m.content, m.tags, m.source, m.importance, now],
    )?;
    let id = conn.last_insert_rowid();
    let seg = segment(&format!(
        "{} {} {}",
        m.subject.unwrap_or(""),
        m.content,
        m.tags.unwrap_or("")
    ))
    .join(" ");
    conn.execute(
        "INSERT INTO memory_fts(rowid, seg) VALUES(?1, ?2)",
        params![id, seg],
    )?;
    Ok(id)
}

/// 幂等写入：同 (owner, source, content) 视为同一条（对应 upsertMemory）。
/// 返回 (id, 是否去重命中, 命中行是否已归档)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpsertOutcome {
    pub id: i64,
    pub deduped: bool,
    pub archived: bool,
}

fn upsert_memory(conn: &Connection, m: &MemoryInput<'_>) -> rusqlite::Result<UpsertOutcome> {
    atomic(conn, |conn| upsert_memory_inner(conn, m))
}

fn upsert_memory_inner(conn: &Connection, m: &MemoryInput<'_>) -> rusqlite::Result<UpsertOutcome> {
    let existing: Option<(i64, bool)> = conn
        .query_row(
            "SELECT id, archived FROM memory
             WHERE owner_id = ?1 AND source = ?2 AND content = ?3 LIMIT 1",
            params![m.owner_id, m.source, m.content],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((id, archived)) = existing {
        if !archived {
            conn.execute(
                "UPDATE memory SET last_used_at = ?1, importance = MAX(importance, ?2)
                 WHERE id = ?3",
                params![now_ms(), m.importance, id],
            )?;
        }
        return Ok(UpsertOutcome {
            id,
            deduped: true,
            archived,
        });
    }
    let id = write_memory(conn, m)?;
    audit(conn, m.source, "write", Some(id), Some(m.kind))?;
    Ok(UpsertOutcome {
        id,
        deduped: false,
        archived: false,
    })
}

/// 删除一条记忆（连同 FTS 行），返回是否真的删掉了。
fn forget_memory(conn: &Connection, id: i64, actor: &str) -> rusqlite::Result<bool> {
    atomic(conn, |conn| {
        let changed = conn.execute("DELETE FROM memory WHERE id = ?1", params![id])?;
        conn.execute("DELETE FROM memory_fts WHERE rowid = ?1", params![id])?;
        audit(conn, actor, "forget", Some(id), None)?;
        Ok(changed > 0)
    })
}

/// Exact visibility check without a bounded search or hit-counter side effects.
fn can_read_id(conn: &Connection, key: &str, is_owner: bool, id: i64) -> rusqlite::Result<bool> {
    let row: Option<(String, String, i64)> = conn
        .query_row(
            "SELECT owner_id,scope,archived FROM memory WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((owner, scope, archived)) = row else {
        return Ok(false);
    };
    let allowed = resolve_reader_scope(conn, key, is_owner)?;
    Ok(archived == 0
        && (is_owner || scope != "private" || owner == key)
        && (allowed.allow_all || allowed.ids.contains(&owner)))
}

fn archive_memory(
    conn: &Connection,
    id: i64,
    archived: bool,
    actor: &str,
) -> rusqlite::Result<bool> {
    atomic(conn, |conn| {
        let changed = conn.execute(
            "UPDATE memory SET archived=?1 WHERE id=?2",
            params![archived as i64, id],
        )?;
        audit(
            conn,
            actor,
            if archived { "archive" } else { "unarchive" },
            Some(id),
            None,
        )?;
        Ok(changed > 0)
    })
}

/// 审计（对应 memory-store.mjs 的 audit）。
fn audit(
    conn: &Connection,
    actor: &str,
    action: &str,
    memory_id: Option<i64>,
    detail: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO memory_audit(at,actor,action,memory_id,detail) VALUES(?1,?2,?3,?4,?5)",
        params![now_ms(), actor, action, memory_id, detail],
    )?;
    Ok(())
}

/// ACL 列表的 JSON 投影（JS 那边是 `SELECT * FROM memory_acl`，键名就是列名）。
pub fn list_acl_json(conn: &Connection) -> rusqlite::Result<Vec<serde_json::Value>> {
    Ok(list_acl(conn)?
        .into_iter()
        .map(
            |(id, reader_type, reader_id, read_type, read_id, level, created_at)| {
                serde_json::json!({
                    "id": id,
                    "reader_type": reader_type,
                    "reader_id": reader_id,
                    "read_type": read_type,
                    "read_id": read_id,
                    "level": level,
                    "created_at": created_at,
                })
            },
        )
        .collect())
}
/// 授权（存在则只更新 level）。
#[allow(dead_code)] // P4 MCP 工具会用到
fn grant_acl(
    conn: &Connection,
    reader_type: &str,
    reader_id: &str,
    read_type: &str,
    read_id: &str,
    level: &str,
    actor: &str,
) -> rusqlite::Result<i64> {
    atomic(conn, |conn| {
        let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM memory_acl WHERE reader_type=?1 AND reader_id=?2 AND read_type=?3 AND read_id=?4",
            params![reader_type, reader_id, read_type, read_id],
            |r| r.get(0),
        )
        .optional()?;
        let detail = format!("{reader_type}:{reader_id} -> {read_type}:{read_id}={level}");
        if let Some(id) = existing {
            conn.execute(
                "UPDATE memory_acl SET level=?1 WHERE id=?2",
                params![level, id],
            )?;
            audit(conn, actor, "acl-update", None, Some(&detail))?;
            return Ok(id);
        }
        conn.execute(
        "INSERT INTO memory_acl(reader_type,reader_id,read_type,read_id,level,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
        params![reader_type, reader_id, read_type, read_id, level, now_ms()],
    )?;
        let id = conn.last_insert_rowid();
        audit(conn, actor, "acl-grant", None, Some(&detail))?;
        Ok(id)
    })
}

/// 撤销授权。
#[allow(dead_code)] // P4 MCP 工具会用到
fn revoke_acl(
    conn: &Connection,
    reader_type: &str,
    reader_id: &str,
    read_type: &str,
    read_id: &str,
    actor: &str,
) -> rusqlite::Result<bool> {
    atomic(conn, |conn| {
        let changed = conn.execute(
        "DELETE FROM memory_acl WHERE reader_type=?1 AND reader_id=?2 AND read_type=?3 AND read_id=?4",
        params![reader_type, reader_id, read_type, read_id],
    )?;
        let detail = format!("{reader_type}:{reader_id} -> {read_type}:{read_id}");
        audit(conn, actor, "acl-revoke", None, Some(&detail))?;
        Ok(changed > 0)
    })
}

/// ACL 行：(id, reader_type, reader_id, read_type, read_id, level)。
type AclRow = (i64, String, String, String, String, String, i64);

/// 列出现有授权。
#[allow(dead_code)] // P4 MCP 工具会用到
fn list_acl(conn: &Connection) -> rusqlite::Result<Vec<AclRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, reader_type, reader_id, read_type, read_id, level, created_at FROM memory_acl ORDER BY id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ── 索引提示（对应 memory-store.mjs 的 indexHint）─────

/// 索引提示：只返回「库里有什么」的词，不含正文。
pub fn index_hint(
    reader: &Reader<'_>,
    exclude: &[String],
    max_terms: usize,
) -> rusqlite::Result<(i64, Vec<String>)> {
    let (conn, reader_key, is_owner) = (reader.conn, reader.key.as_str(), reader.is_owner);
    let scope = resolve_reader_scope(conn, reader_key, is_owner)?;
    let mut where_parts: Vec<String> = vec!["archived = 0".to_string()];
    let mut args: Vec<String> = Vec::new();
    if !scope.allow_all {
        let ph = vec!["?"; scope.ids.len()].join(",");
        where_parts.push(format!("owner_id IN ({ph})"));
        args.extend(scope.ids.iter().cloned());
    }
    if !is_owner {
        where_parts.push("(scope <> 'private' OR owner_id = ?)".to_string());
        args.push(reader_key.to_string());
    }
    let where_sql = where_parts.join(" AND ");

    let sql = format!(
        "SELECT COALESCE(subject, kind) AS term, COUNT(*) AS n FROM memory
         WHERE {where_sql} GROUP BY term ORDER BY n DESC, MAX(created_at) DESC LIMIT ?"
    );
    let lim = max_terms as i64;
    let mut refs: Vec<&dyn ToSql> = Vec::new();
    for a in &args {
        refs.push(a as &dyn ToSql);
    }
    refs.push(&lim as &dyn ToSql);

    let mut stmt = conn.prepare(&sql)?;
    let raw_terms: Vec<Option<String>> = stmt
        .query_map(refs.as_slice(), |r| r.get::<_, Option<String>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let terms: Vec<String> = raw_terms
        .into_iter()
        .flatten()
        .filter(|t| !exclude.iter().any(|e| e == t))
        .collect();

    let count_sql = format!("SELECT COUNT(*) FROM memory WHERE {where_sql}");
    let mut crefs: Vec<&dyn ToSql> = Vec::new();
    for a in &args {
        crefs.push(a as &dyn ToSql);
    }
    let total: i64 = conn.query_row(&count_sql, crefs.as_slice(), |r| r.get(0))?;
    Ok((total, terms))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fts_fixture(content: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../spec/memory-schema.sql"))
            .unwrap();
        for (text, importance) in [(content, 0.0), ("unrelated decoy", 1.0)] {
            write_memory(
                &conn,
                &MemoryInput {
                    owner_type: "session",
                    owner_id: "group:101",
                    scope: "group",
                    kind: "fact",
                    subject: None,
                    content: text,
                    tags: None,
                    source: "fixture",
                    importance,
                },
            )
            .unwrap();
        }
        conn
    }

    // ── Recall (automatic recall) ───────────────────────────────────────────────

    fn reader_for<'a>(conn: &'a Connection, key: &str, is_owner: bool) -> Reader<'a> {
        Reader {
            conn,
            key: key.to_string(),
            is_owner,
            has_provenance: false,
        }
    }

    fn recall(
        reader: &Reader<'_>,
        text: &str,
        limit: usize,
        exclude: &[i64],
    ) -> rusqlite::Result<Vec<Hit>> {
        super::recall(reader, text, None, limit, exclude)
    }

    fn recall_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../spec/memory-schema.sql"))
            .unwrap();
        for (owner, scope, subject, content, importance, pinned) in [
            (
                "group:101",
                "group",
                "户外计划",
                "周末一起去爬山看日出，早上六点出发",
                0.3,
                0,
            ),
            (
                "group:101",
                "group",
                "备份规则",
                "数据库备份每天凌晨三点执行并保留七天",
                1.0,
                1,
            ),
            (
                "group:101",
                "group",
                "饮食习惯",
                "不吃香菜，喜欢喝无糖的热豆浆",
                0.9,
                0,
            ),
            (
                "group:202",
                "group",
                "别群计划",
                "周末一起去爬山的另一个群的约定",
                0.5,
                0,
            ),
        ] {
            let id = write_memory(
                &conn,
                &MemoryInput {
                    owner_type: "session",
                    owner_id: owner,
                    scope,
                    kind: "fact",
                    subject: Some(subject),
                    content,
                    tags: None,
                    source: "fixture",
                    importance,
                },
            )
            .unwrap();
            conn.execute(
                "UPDATE memory SET pinned=?1 WHERE id=?2",
                params![pinned, id],
            )
            .unwrap();
        }
        conn
    }

    fn recalled_subjects(hits: &[Hit]) -> Vec<String> {
        hits.iter()
            .map(|h| h.subject.clone().unwrap_or_default())
            .collect()
    }

    #[test]
    fn recall_finds_a_memory_that_shares_two_words_with_a_chat_length_message() {
        let conn = recall_fixture();
        let message = "我觉得这个周末可以一起去爬山吧，天气应该不错呀";
        assert!(message.chars().count() >= 20);
        let hits = recall(&reader_for(&conn, "group:101", false), message, 3, &[]).unwrap();
        assert_eq!(
            recalled_subjects(&hits),
            vec!["户外计划"],
            "only the memory that shares words, not the pinned or important rows"
        );
    }

    #[test]
    fn recall_finds_a_memory_by_one_shared_identifier() {
        let conn = recall_fixture();
        write_memory(
            &conn,
            &MemoryInput {
                owner_type: "session",
                owner_id: "group:101",
                scope: "group",
                kind: "fact",
                subject: Some("暗号"),
                content: "zebra148kw 是约好的暗号",
                tags: None,
                source: "fixture",
                importance: 0.1,
            },
        )
        .unwrap();
        let hits = recall(
            &reader_for(&conn, "group:101", false),
            "zebra148kw 你还记得吗",
            3,
            &[],
        )
        .unwrap();
        assert_eq!(recalled_subjects(&hits), vec!["暗号"]);
    }

    #[test]
    fn recall_of_an_unrelated_message_is_empty_even_with_pinned_and_recent_rows() {
        let conn = recall_fixture();
        for message in [
            "今天的天气真是太好了呀",
            "hello there general kenobi",
            "。。。！！！",
            "",
            "周末天气怎么样呢",
        ] {
            let hits = recall(&reader_for(&conn, "group:101", false), message, 3, &[]).unwrap();
            assert!(
                hits.is_empty(),
                "{message:?} recalled {:?}",
                recalled_subjects(&hits)
            );
        }
    }

    #[test]
    fn recall_never_repeats_an_item_the_session_generation_already_holds() {
        let conn = recall_fixture();
        let message = "周末一起去爬山看日出吧，早上出发";
        let first = recall(&reader_for(&conn, "group:101", false), message, 3, &[]).unwrap();
        assert_eq!(recalled_subjects(&first), vec!["户外计划"]);
        let held: Vec<i64> = first.iter().map(|h| h.id).collect();
        assert!(
            recall(&reader_for(&conn, "group:101", false), message, 3, &held)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn recall_stays_inside_the_readers_scope() {
        let conn = recall_fixture();
        let hits = recall(
            &reader_for(&conn, "group:101", false),
            "周末一起去爬山的约定",
            5,
            &[],
        )
        .unwrap();
        assert!(
            hits.iter().all(|h| h.owner_id == "group:101"),
            "{:?}",
            recalled_subjects(&hits)
        );
        let hits = recall(
            &reader_for(&conn, "group:101", true),
            "周末一起去爬山的约定",
            5,
            &[],
        )
        .unwrap();
        assert_eq!(hits.len(), 2, "the owner reads every group");
    }

    #[test]
    fn recall_query_punctuation_and_operators_cannot_break_the_match_expression() {
        let conn = recall_fixture();
        for message in [
            "周末 \"一起去爬山\" OR (NOT 备份) AND * : ^ - + NEAR(a b) 周末一起去爬山",
            "周末一起去爬山 \"",
            "' ; DROP TABLE memory; -- 周末一起去爬山",
        ] {
            let hits =
                recall(&reader_for(&conn, "group:101", false), message, 3, &[]).expect(message);
            assert_eq!(recalled_subjects(&hits)[..1], ["户外计划"], "{message}");
        }
        let long: String = "周末一起去爬山看日出".chars().cycle().take(200).collect();
        assert_eq!(
            recalled_subjects(
                &recall(&reader_for(&conn, "group:101", false), &long, 3, &[]).unwrap()
            )[..1],
            ["户外计划"]
        );
    }

    #[test]
    fn recall_ranks_by_relevance_before_pin_importance_and_recency() {
        let conn = recall_fixture();
        let message = "不吃香菜，喜欢喝热豆浆，数据库备份";
        let hits = recall(&reader_for(&conn, "group:101", false), message, 3, &[]).unwrap();
        // The pinned, importance 1.0 backup row shares one word; the diet row shares four.
        assert_eq!(
            recalled_subjects(&hits).first().map(String::as_str),
            Some("饮食习惯")
        );
    }

    fn add_row(
        conn: &Connection,
        owner: &str,
        scope: &str,
        subject: &str,
        content: &str,
        importance: f64,
        pinned: i64,
    ) -> i64 {
        let id = write_memory(
            conn,
            &MemoryInput {
                owner_type: "session",
                owner_id: owner,
                scope,
                kind: "fact",
                subject: Some(subject),
                content,
                tags: None,
                source: "fixture",
                importance,
            },
        )
        .unwrap();
        conn.execute(
            "UPDATE memory SET pinned=?1 WHERE id=?2",
            params![pinned, id],
        )
        .unwrap();
        id
    }

    #[test]
    fn recall_of_a_very_short_message_recalls_nothing_even_when_a_row_contains_it() {
        let conn = recall_fixture();
        add_row(
            &conn,
            "group:101",
            "group",
            "应答",
            "好的，哈哈，嗯，知道了",
            1.0,
            1,
        );
        for message in ["好的", "哈哈", "嗯", "好的。", "ok"] {
            let hits = recall(&reader_for(&conn, "group:101", false), message, 3, &[]).unwrap();
            assert!(
                hits.is_empty(),
                "{message:?} recalled {:?}",
                recalled_subjects(&hits)
            );
        }
    }

    #[test]
    fn recall_one_shared_common_token_is_not_enough() {
        let conn = recall_fixture();
        add_row(&conn, "group:101", "group", "链接", "这是一条专门用于验证检索分词规则的测试句子：参考 https://example.test/a 里的说明，2026 年计划，haha 好玩", 1.0, 1);
        for message in [
            "https",
            "2026",
            "haha",
            "你看这个 https 呀",
            "2026 怎么样",
            "haha 笑死我了",
        ] {
            let hits = recall(&reader_for(&conn, "group:101", false), message, 3, &[]).unwrap();
            assert!(
                hits.is_empty(),
                "{message:?} recalled {:?}",
                recalled_subjects(&hits)
            );
        }
        // A long word, or two distinct shared terms, is enough.
        let hits = recall(
            &reader_for(&conn, "group:101", false),
            "example 怎么样",
            3,
            &[],
        )
        .unwrap();
        assert_eq!(
            recalled_subjects(&hits),
            vec!["链接"],
            "a seven-character word alone"
        );
        let hits = recall(
            &reader_for(&conn, "group:101", false),
            "https 和 2026 都有",
            3,
            &[],
        )
        .unwrap();
        assert_eq!(
            recalled_subjects(&hits),
            vec!["链接"],
            "two distinct shared terms totalling four characters"
        );
    }

    #[test]
    fn recall_does_not_lose_a_qualifying_row_to_the_candidate_cut() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../spec/memory-schema.sql"))
            .unwrap();
        // 80 distinct Han characters: every neighbouring pair is a distinct message term.
        let chars: Vec<char> = (0..80u32)
            .map(|i| char::from_u32(0x4e00 + i * 13).unwrap())
            .collect();
        let message: String = chars.iter().collect();
        // 70 short rows each share exactly one term with the message and outrank the long row by
        // bm25; each alone is below the relevance bar.
        for i in 0..70 {
            add_row(
                &conn,
                "group:101",
                "group",
                &format!("短{i}"),
                &format!("{}{}，无关", chars[i], chars[i + 1]),
                0.0,
                0,
            );
        }
        let filler = "无关内容，".repeat(60);
        add_row(
            &conn,
            "group:101",
            "group",
            "长",
            &format!(
                "{}{}，{}{}，{filler}",
                chars[72], chars[73], chars[75], chars[76]
            ),
            0.0,
            0,
        );
        let hits = recall(&reader_for(&conn, "group:101", false), &message, 3, &[]).unwrap();
        assert_eq!(recalled_subjects(&hits), vec!["长"]);
    }

    #[test]
    fn recall_never_returns_another_sessions_private_row_or_an_archived_row() {
        let conn = recall_fixture();
        let private = add_row(
            &conn,
            "private:9",
            "private",
            "私聊",
            "周末一起去爬山看日出的私人安排",
            1.0,
            1,
        );
        let archived = add_row(
            &conn,
            "group:101",
            "group",
            "已归档",
            "周末一起去爬山看日出的旧安排",
            1.0,
            1,
        );
        archive_memory(&conn, archived, true, "fixture").unwrap();
        // Everything is readable to the group by a wildcard grant, except what the hard rules hide.
        grant_acl(&conn, "session", "group:101", "*", "", "read", "fixture").unwrap();
        let hits = recall(
            &reader_for(&conn, "group:101", false),
            "周末一起去爬山看日出",
            10,
            &[],
        )
        .unwrap();
        let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
        assert!(
            !ids.contains(&private),
            "another session's private row: {:?}",
            recalled_subjects(&hits)
        );
        assert!(
            !ids.contains(&archived),
            "an archived row: {:?}",
            recalled_subjects(&hits)
        );
        assert!(
            hits.iter()
                .any(|h| h.subject.as_deref() == Some("别群计划")),
            "the grant itself works: {:?}",
            recalled_subjects(&hits)
        );
    }

    #[test]
    fn recall_relevance_outweighs_pin_importance_and_recency() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../spec/memory-schema.sql"))
            .unwrap();
        add_row(
            &conn,
            "group:101",
            "group",
            "置顶少",
            "周末，一起，爬山，安排很满，计划另议",
            1.0,
            1,
        );
        add_row(
            &conn,
            "group:101",
            "group",
            "普通多",
            "周末一起去爬山看日出，然后吃早茶",
            0.0,
            0,
        );
        let hits = recall(
            &reader_for(&conn, "group:101", false),
            "周末一起去爬山看日出吧",
            3,
            &[],
        )
        .unwrap();
        assert_eq!(
            recalled_subjects(&hits),
            vec!["普通多", "置顶少"],
            "the pinned row shares fewer terms"
        );
    }

    #[test]
    fn recall_runs_the_full_width_expression_for_200_distinct_han_characters() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../spec/memory-schema.sql"))
            .unwrap();
        let chars: Vec<char> = (0..200u32)
            .map(|i| char::from_u32(0x4e00 + i * 7).unwrap())
            .collect();
        let message: String = chars.iter().collect();
        assert_eq!(recall_terms(&message).len(), 199);
        // The match (four distinct message characters) sits at the end, past any short term cap.
        add_row(
            &conn,
            "group:101",
            "group",
            "尾部",
            &format!(
                "{}{}{}{}，其余无关",
                chars[195], chars[196], chars[197], chars[198]
            ),
            0.0,
            0,
        );
        let hits = recall(&reader_for(&conn, "group:101", false), &message, 3, &[]).unwrap();
        assert_eq!(recalled_subjects(&hits), vec!["尾部"]);
    }

    #[test]
    fn recall_of_a_message_under_four_characters_recalls_nothing_even_through_overlapping_bigrams()
    {
        let conn = recall_fixture();
        add_row(
            &conn,
            "group:101",
            "group",
            "应答",
            "我早就知道了，没问题",
            1.0,
            1,
        );
        // 知道了 is three characters: its two bigrams overlap and must not count as four.
        for message in ["知道了", "知道了。", "我知道"] {
            let hits = recall(&reader_for(&conn, "group:101", false), message, 3, &[]).unwrap();
            assert!(
                hits.is_empty(),
                "{message:?} recalled {:?}",
                recalled_subjects(&hits)
            );
        }
    }

    #[test]
    fn recall_measures_what_is_shared_in_distinct_message_characters() {
        let conn = recall_fixture();
        add_row(
            &conn,
            "group:101",
            "group",
            "三字",
            "早就知道了，没问题",
            0.0,
            0,
        );
        add_row(
            &conn,
            "group:101",
            "group",
            "四字",
            "他知道了吗，我没问",
            0.0,
            0,
        );
        // 知道 + 道了 cover three characters; 知道 + 道了 + 了吗 cover four.
        let hits = recall(&reader_for(&conn, "group:101", false), "知道了吗", 3, &[]).unwrap();
        assert_eq!(
            recalled_subjects(&hits),
            vec!["四字"],
            "only the row sharing four distinct characters"
        );
        let hits = recall(
            &reader_for(&conn, "group:101", false),
            "你到底知道了没",
            3,
            &[],
        )
        .unwrap();
        assert!(
            hits.is_empty(),
            "three shared characters are not enough: {:?}",
            recalled_subjects(&hits)
        );
    }

    #[test]
    fn recall_identifier_counts_only_alphanumeric_characters() {
        assert!(!recall_is_identifier("__a_"));
        assert!(!recall_is_identifier("a.b"));
        assert!(!recall_is_identifier("v4.1"));
        assert!(recall_is_identifier("zebra148kw"));
        assert!(recall_is_identifier("v0.2.1"));
        assert!(recall_is_identifier("foo_bar"));
        let conn = recall_fixture();
        add_row(
            &conn,
            "group:101",
            "group",
            "符号",
            "代号 __a_ 的说明",
            1.0,
            1,
        );
        let hits = recall(
            &reader_for(&conn, "group:101", false),
            "请问 __a_ 怎么办",
            3,
            &[],
        )
        .unwrap();
        assert!(hits.is_empty(), "{:?}", recalled_subjects(&hits));
    }

    #[test]
    fn recall_a_repeated_short_term_does_not_pass_as_a_long_one() {
        let conn = recall_fixture();
        add_row(
            &conn,
            "group:101",
            "group",
            "链接",
            "参考 https 的说明，哈哈 很好玩",
            1.0,
            1,
        );
        add_row(
            &conn,
            "group:101",
            "group",
            "长词",
            "program 的写法很多",
            0.0,
            0,
        );
        for message in [
            "https https https",
            "哈哈 哈哈 哈哈",
            "哈哈哈哈哈哈",
            "https，https，https，https",
        ] {
            let hits = recall(&reader_for(&conn, "group:101", false), message, 3, &[]).unwrap();
            assert!(
                hits.is_empty(),
                "{message:?} recalled {:?}",
                recalled_subjects(&hits)
            );
        }
        // A word of six or more letters alone still recalls, repeated or not.
        for message in ["program", "program program"] {
            let hits = recall(&reader_for(&conn, "group:101", false), message, 3, &[]).unwrap();
            assert_eq!(recalled_subjects(&hits), vec!["长词"], "{message:?}");
        }
    }

    #[test]
    fn fts_dot_query_cleaning_matches_js_before_token_cap() {
        // The high-scoring decoy distinguishes MATCH from the recent-memory fallback.
        for (query, indexed) in [
            ("deepseek-v4.1-flash", "deepseek v41 flash"),
            ("v0.2.1", "v021"),
            ("管理员偏好", "管理员偏好"),
            (
                "管理员 DeepSeek-v4.1-flash v0.2.1",
                "管理员 deepseek v41 flash v021",
            ),
        ] {
            let conn = fts_fixture(indexed);
            let hits = search(&conn, "group:101", false, query, &[], 1).unwrap();
            assert_eq!(hits[0].content, indexed, "query: {query}");
        }
        let conn = fts_fixture("needle");
        let query = format!("{} needle", ". ".repeat(24));
        assert_eq!(
            search(&conn, "group:101", false, &query, &[], 1).unwrap()[0].content,
            "needle"
        );
        let query = format!("{} absent", "needle ".repeat(24));
        assert_eq!(
            search(&conn, "group:101", false, &query, &[], 1).unwrap()[0].content,
            "needle"
        );
    }

    #[test]
    fn fts_dot_punctuation_skips_match_and_dotted_storage_remains_searchable() {
        let conn = fts_fixture("deepseek-v4.1-flash v0.2.1");
        assert_eq!(
            search(&conn, "group:101", false, "deepseek", &[], 1).unwrap()[0].content,
            "deepseek-v4.1-flash v0.2.1",
            "existing dotted index tokens stay searchable"
        );
        conn.execute("DELETE FROM memory WHERE content='unrelated decoy'", [])
            .unwrap();
        for query in ["deepseek-v4.1-flash", "v0.2.1"] {
            assert_eq!(
                search(&conn, "group:101", false, query, &[], 1).unwrap()[0].content,
                "deepseek-v4.1-flash v0.2.1"
            );
        }
        // No FTS table: a punctuation-only query must take the normal recent fallback.
        conn.execute_batch("DROP TABLE memory_fts").unwrap();
        assert_eq!(
            search(&conn, "group:101", false, "...-!?", &[], 1)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn fts_dot_search_returns_row_and_hit_update_failures() {
        let conn = fts_fixture("needle");
        conn.execute("UPDATE memory SET content=x'FF' WHERE id=1", [])
            .unwrap();
        for query in ["needle", ""] {
            assert!(
                search(&conn, "group:101", false, query, &[], 5).is_err(),
                "must report row decoding failure for {query:?}"
            );
        }
        conn.execute("UPDATE memory SET content='needle' WHERE id=1", [])
            .unwrap();
        conn.execute_batch("CREATE TRIGGER fail_hits BEFORE UPDATE OF hits ON memory BEGIN SELECT RAISE(FAIL, 'private-query-token-path'); END;").unwrap();
        assert!(
            search(&conn, "group:101", false, "needle", &[], 5).is_err(),
            "must report hit-update failure as JS does"
        );
    }

    #[test]
    fn segment_matches_js() {
        // 与 JS 版对拍过的示例
        assert_eq!(segment("管理员偏好").join(" "), "管理 理员 员偏 偏好");
        assert_eq!(segment("你好").join(" "), "你好");
        assert_eq!(segment("A").join(" "), "a");
        assert_eq!(segment("DS 模型").join(" "), "ds 模型");
    }

    #[test]
    fn segment_single_han_kept() {
        assert_eq!(segment("鱼").join(" "), "鱼");
    }

    #[test]
    fn upsert_reports_archived_state_and_skips_touching_archived_row() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../spec/memory-schema.sql"))
            .unwrap();
        let input = |importance| MemoryInput {
            owner_type: "session",
            owner_id: "group:101",
            scope: "group",
            kind: "thought",
            subject: None,
            content: "已归档去重测试",
            tags: None,
            source: "agent",
            importance,
        };
        let created = upsert_memory(&conn, &input(0.3)).unwrap();
        assert_eq!(
            created,
            UpsertOutcome {
                id: 1,
                deduped: false,
                archived: false
            }
        );
        archive_memory(&conn, created.id, true, "console").unwrap();
        let deduped = upsert_memory(&conn, &input(0.9)).unwrap();
        assert_eq!(
            deduped,
            UpsertOutcome {
                id: created.id,
                deduped: true,
                archived: true
            }
        );
        let (last_used_at, importance): (Option<i64>, f64) = conn
            .query_row(
                "SELECT last_used_at, importance FROM memory WHERE id = ?1",
                [created.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(last_used_at, None);
        assert!((importance - 0.3).abs() < f64::EPSILON);

        archive_memory(&conn, created.id, false, "console").unwrap();
        let restored = upsert_memory(&conn, &input(0.9)).unwrap();
        assert_eq!(
            restored,
            UpsertOutcome {
                id: created.id,
                deduped: true,
                archived: false
            }
        );
    }

    #[test]
    fn recall_ignores_chinese_stopwords() {
        let conn = recall_fixture();
        add_row(
            &conn,
            "group:101",
            "group",
            "礼貌",
            "你好 谢谢 请问 那个 这个 什么 一下",
            1.0,
            1,
        );
        let hits = recall(
            &reader_for(&conn, "group:101", false),
            "你好请问一下那个是什么谢谢",
            3,
            &[],
        )
        .unwrap();
        assert!(
            hits.is_empty(),
            "stopwords alone must not trigger recall: {:?}",
            recalled_subjects(&hits)
        );
    }

    #[test]
    fn schema_v4_adds_embedding_cache_and_invalidates_on_update_or_delete() {
        let conn = prepare_connection(Connection::open_in_memory().unwrap()).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        let id1 = write_memory(
            &conn,
            &MemoryInput {
                owner_type: "session",
                owner_id: "group:101",
                scope: "group",
                kind: "fact",
                subject: Some("s"),
                content: "first",
                tags: None,
                source: "fixture",
                importance: 0.5,
            },
        )
        .unwrap();
        let id2 = write_memory(
            &conn,
            &MemoryInput {
                owner_type: "session",
                owner_id: "group:101",
                scope: "group",
                kind: "fact",
                subject: Some("s"),
                content: "second",
                tags: None,
                source: "fixture",
                importance: 0.5,
            },
        )
        .unwrap();
        for id in [id1, id2] {
            conn.execute("INSERT INTO memory_embedding_cache(memory_id, model, embedding) VALUES(?1, X'01', X'02')", [id]).unwrap();
        }
        conn.execute("UPDATE memory SET content='updated' WHERE id=?1", [id1])
            .unwrap();
        conn.execute("DELETE FROM memory WHERE id=?1", [id2])
            .unwrap();
        let remaining: i64 = conn
            .query_row("SELECT count(*) FROM memory_embedding_cache", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn hybrid_recall_attaches_provenance_to_dense_hits() {
        struct DummyResources(std::path::PathBuf);
        impl ModelResources for DummyResources {
            fn resources_dir(&self) -> &std::path::Path {
                &self.0
            }
            fn read_model_file(&self, _relative: &str) -> Option<Vec<u8>> {
                None
            }
            fn model_file_stamp(&self, _relative: &str) -> Option<FileStamp> {
                None
            }
        }
        let store = MemoryStore::open_prepared(
            Connection::open_in_memory().unwrap(),
            Some("private:7001".to_string()),
        )
        .unwrap();
        let reader = store.for_reader("private:7001");
        reader
            .write_message(
                VerifiedMessage {
                    id: "msg-1",
                    text: "幼童遇到风暴时需要拥抱",
                    author_id: "7001",
                    position: (10, 0),
                },
                None,
            )
            .unwrap()
            .unwrap();
        let model = DenseModel::with_loader(|_| {
            Some(Box::new(|texts: &[String]| {
                Some(
                    texts
                        .iter()
                        .map(|t| {
                            let mut v = vec![0.0f32; 512];
                            v[if t.contains("小孩") || t.contains("幼童") {
                                0
                            } else {
                                1
                            }] = 1.0;
                            v
                        })
                        .collect(),
                )
            }))
        });
        let resources = std::sync::Arc::new(DummyResources(std::path::PathBuf::from("dummy")));
        let hits = super::recall(
            &reader,
            "小孩害怕打雷该如何安抚",
            Some(SemanticQuery {
                query: "小孩害怕打雷该如何安抚",
                resources,
                model: Some(&model),
            }),
            5,
            &[],
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(matches!(hits[0].provenance, Provenance::Fact));
        assert_eq!(hits[0].message_id.as_deref(), Some("msg-1"));
    }
}
