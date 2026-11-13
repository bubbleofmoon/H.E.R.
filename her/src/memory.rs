//! P3 记忆库移植：直接读写 JS 建的同一个 `data/memory.db`（schema 不动、数据不搬）。
//!
//! 与 `qq-bridge/src/memory-store.mjs` 逐条对齐：
//!   resolveReaderScope  读者能读哪些 owner_id（ACL）
//!   searchMemory        FTS5(bigram) + 过滤 + 打分排序 + hits 自增
//!   **private 硬约束**  非 owner 且非本会话私聊时看不到 scope='private'
//!
//! 分词必须与 JS 的 `segment()` 完全一致，否则同一个库会出现不同的检索结果（且是静默的）。

use rusqlite::{params, Connection, OpenFlags, ToSql};
use std::path::PathBuf;

use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};

pub const DB_PATH: &str = "C:/qqbot/data/memory.db";

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
        open_at(&path).map_err(|e| format!("cannot open memory database: {e}"))
    }
}

pub fn open_at(path: &std::path::Path) -> rusqlite::Result<Connection> {
    Connection::open(path)
}

/// Complete the frozen schema atomically, including databases interrupted by an older initializer.
/// Existing rows, grants and FTS entries survive; only a missing FTS table is populated from rows.
pub fn open_ready_for_runtime(context: &RuntimeContext) -> Result<Connection, String> {
    let conn = open_for_runtime(context)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    // journal_mode cannot change in a transaction. Set it first; the schema's same-mode
    // PRAGMA is then harmless inside the transaction containing every DDL/seed operation.
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| e.to_string())?;
    atomic(&conn, |conn| {
        let exists = |name: &str| -> rusqlite::Result<bool> {
            conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)", [name], |r| r.get(0))
        };
        let had_fts = exists("memory_fts")?;
        let had_acl = exists("memory_acl")?;
        let mut complete = true;
        for name in ["memory", "memory_fts", "memory_acl", "team", "team_member", "role_group", "role_group_session", "memory_audit", "idx_memory_owner", "idx_memory_recent", "idx_acl_reader"] {
            complete &= exists(name)?;
        }
        if complete { return Ok(()); }
        conn.execute_batch(include_str!("../spec/memory-schema.sql"))?;
        if !had_fts {
            let mut stmt = conn.prepare("SELECT id, subject, content, tags FROM memory")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?)))?;
            for row in rows {
                let (id, subject, content, tags) = row?;
                let seg = segment(&format!("{} {} {}", subject.unwrap_or_default(), content, tags.unwrap_or_default())).join(" ");
                conn.execute("INSERT INTO memory_fts(rowid,seg) VALUES(?1,?2)", params![id, seg])?;
            }
        }
        if !had_acl {
            conn.execute("INSERT INTO memory_acl(reader_type,reader_id,read_type,read_id,level,created_at) VALUES('owner','main','*','*','read',?1)", [now_ms()])?;
        }
        Ok(())
    }).map_err(|e| e.to_string())?;
    Ok(conn)
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
    let mut out: Vec<String> = Vec::new();
    let mut han = String::new();
    let mut word = String::new();

    fn flush_han(han: &mut String, out: &mut Vec<String>) {
        let chars: Vec<char> = han.chars().collect();
        if chars.len() == 1 {
            out.push(chars[0].to_string());
        }
        for w in chars.windows(2) {
            out.push(w.iter().collect());
        }
        han.clear();
    }
    fn flush_word(word: &mut String, out: &mut Vec<String>) {
        if !word.is_empty() {
            out.push(word.to_lowercase());
            word.clear();
        }
    }

    for ch in text.chars() {
        let is_han = ('\u{4e00}'..='\u{9fff}').contains(&ch);
        let is_word = ch.is_ascii_alphanumeric() || ch == '_' || ch == '.';
        if is_han {
            flush_word(&mut word, &mut out);
            han.push(ch);
        } else if is_word {
            flush_han(&mut han, &mut out);
            word.push(ch);
        } else {
            flush_han(&mut han, &mut out);
            flush_word(&mut word, &mut out);
        }
    }
    flush_han(&mut han, &mut out);
    flush_word(&mut word, &mut out);
    out
}

/// 读者可见的 owner 范围。
#[derive(Debug, Clone)]
pub struct ReaderScope {
    pub allow_all: bool,
    pub ids: Vec<String>,
}

/// 解析读者能读哪些 owner_id（对应 JS 的 resolveReaderScope）。
pub fn resolve_reader_scope(
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
        .filter_map(Result::ok)
        .collect();

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
                    .filter_map(Result::ok)
                    .collect();
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
    let (id, owner_id, scope, kind, subject, content, _rank, pinned, importance, hits, created_at, tags, source, last_used_at, owner_type, archived, expires_at) = row;
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
    }
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
}

fn now_ms() -> i64 {
    crate::parity_clock::now_ms() as i64
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
pub fn search(
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
pub fn search_with(
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
            token.chars().filter(|c| {
                ('\u{4e00}'..='\u{9fff}').contains(c)
                    || c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_'
            }).collect::<String>()
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
/// A memory must share terms worth at least this many characters with the message (fewer when the
/// whole message has fewer). One shared Chinese bigram (a single two-character word) or a short
/// common word is not evidence of relevance; two such words, one longer word or one identifier is.
const RECALL_MIN_SHARED_CHARS: usize = 4;
/// bm25-ranked candidates read before the relevance bar and the exclusion are applied.
const RECALL_CANDIDATES: i64 = 60;

/// One index term as Recall compares it: Han, lowercase ASCII, digits, underscore and the dot
/// `segment` keeps inside a word; a term without a letter, digit or Han character is dropped.
fn recall_term(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| {
            ('\u{4e00}'..='\u{9fff}').contains(c)
                || c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '.'
        })
        .collect();
    let cleaned = cleaned.trim_matches('.');
    cleaned.chars().any(char::is_alphanumeric).then(|| cleaned.to_string())
}

/// The distinct terms of a message, in message order.
pub fn recall_terms(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    segment(text)
        .iter()
        .filter_map(|token| recall_term(token))
        .filter(|term| seen.insert(term.clone()))
        .take(RECALL_MAX_TERMS)
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

/// Automatic recall for `text` (the message being answered): at most `limit` memories the reader
/// may see that share enough words with it, best first, without the ids in `exclude` (what the session
/// generation already holds). A message nothing matches recalls nothing: there is no pinned or
/// recent padding. Relevance (bm25 and shared terms) leads the score; pin, importance, recency
/// and past use only add small boosts. Hits are not counted here: the caller counts the items
/// it actually delivered with `record_hits`.
pub fn recall(
    conn: &Connection,
    reader_key: &str,
    is_owner: bool,
    text: &str,
    limit: usize,
    exclude: &[i64],
) -> rusqlite::Result<Vec<Hit>> {
    let terms = recall_terms(text);
    if terms.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let (where_sql, args) = visibility_filter(conn, reader_key, is_owner, &[])?;
    let excluded = if exclude.is_empty() {
        String::new()
    } else {
        let ids: Vec<String> = exclude.iter().map(i64::to_string).collect();
        format!(" AND m.id NOT IN ({})", ids.join(","))
    };
    let sql = format!(
        "SELECT m.id, m.owner_id, m.scope, m.kind, m.subject, m.content,
                bm25(memory_fts) AS rank, m.pinned, m.importance, m.hits, m.created_at,
                m.tags, m.source, m.last_used_at, m.owner_type, m.archived, m.expires_at,
                f.seg
         FROM memory_fts f JOIN memory m ON m.id = f.rowid
         WHERE memory_fts MATCH ? AND {where_sql}{excluded}
         ORDER BY rank LIMIT {RECALL_CANDIDATES}"
    );
    let expression = recall_match_expression(&terms);
    let mut refs: Vec<&dyn ToSql> = vec![&expression as &dyn ToSql];
    for a in &args {
        refs.push(a as &dyn ToSql);
    }
    let mut stmt = conn.prepare(&sql)?;
    let candidates = stmt
        .query_map(refs.as_slice(), |r| Ok((read_row(r)?, r.get::<_, String>(17)?)))?
        .collect::<rusqlite::Result<Vec<(Row, String)>>>()?;

    let term_chars = |term: &str| term.chars().filter(|c| c.is_alphanumeric()).count();
    let bar = RECALL_MIN_SHARED_CHARS.min(terms.iter().map(|t| term_chars(t)).sum());
    let now = now_ms();
    let mut scored: Vec<Hit> = Vec::new();
    for (row, seg) in candidates {
        let indexed: std::collections::HashSet<String> =
            seg.split_whitespace().filter_map(recall_term).collect();
        let shared: usize = terms.iter().filter(|term| indexed.contains(*term)).map(|t| term_chars(t)).sum();
        if shared < bar {
            continue;
        }
        let (rank, pinned, importance, hits, created_at) = (row.6, row.7, row.8, row.9, row.10);
        let age_days = ((now - created_at) as f64 / 86_400_000.0).max(0.0);
        let relevance = 0.5 * (shared as f64 / 6.0).tanh() + 0.5 * (-rank).max(0.0).tanh();
        let boost = 0.04 * if pinned != 0 { 1.0 } else { 0.0 }
            + 0.04 * importance.clamp(0.0, 1.0)
            + 0.02 * (-age_days / 30.0).exp()
            + 0.01 * ((hits as f64) / 5.0).tanh();
        scored.push(hit_from_row(row, relevance + boost));
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
/// 返回 (id, 是否去重命中)。
pub struct UpsertOutcome {
    pub id: i64,
    pub deduped: bool,
}

pub fn upsert_memory(conn: &Connection, m: &MemoryInput<'_>) -> rusqlite::Result<UpsertOutcome> {
    atomic(conn, |conn| upsert_memory_inner(conn, m))
}

fn upsert_memory_inner(conn: &Connection, m: &MemoryInput<'_>) -> rusqlite::Result<UpsertOutcome> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM memory WHERE owner_id = ?1 AND source = ?2 AND content = ?3 LIMIT 1",
            params![m.owner_id, m.source, m.content],
            |r| r.get(0),
        )
        .ok();
    if let Some(id) = existing {
        conn.execute(
            "UPDATE memory SET last_used_at = ?1, importance = MAX(importance, ?2) WHERE id = ?3",
            params![now_ms(), m.importance, id],
        )?;
        return Ok(UpsertOutcome { id, deduped: true });
    }
    let id = write_memory(conn, m)?;
    audit(conn, m.source, "write", Some(id), Some(m.kind))?;
    Ok(UpsertOutcome { id, deduped: false })
}

/// 删除一条记忆（连同 FTS 行），返回是否真的删掉了。
#[allow(dead_code)] // P4 MCP 工具会用到
pub fn forget_memory(conn: &Connection, id: i64, actor: &str) -> rusqlite::Result<bool> {
    atomic(conn, |conn| {
        let changed = conn.execute("DELETE FROM memory WHERE id = ?1", params![id])?;
        conn.execute("DELETE FROM memory_fts WHERE rowid = ?1", params![id])?;
        audit(conn, actor, "forget", Some(id), None)?;
        Ok(changed > 0)
    })
}

/// Exact visibility check without a bounded search or hit-counter side effects.
pub fn can_read_id(
    conn: &Connection,
    key: &str,
    is_owner: bool,
    id: i64,
) -> rusqlite::Result<bool> {
    use rusqlite::OptionalExtension;
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

pub fn archive_memory(
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
pub fn audit(
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
pub fn grant_acl(
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
        .ok();
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
pub fn revoke_acl(
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
pub fn list_acl(conn: &Connection) -> rusqlite::Result<Vec<AclRow>> {
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
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

// ── 索引提示与热缓存（对应 memory-store.mjs 的 indexHint、hot-cache.mjs）─────

/// 索引提示：只返回「库里有什么」的词，不含正文。
pub fn index_hint(
    conn: &Connection,
    reader_key: &str,
    is_owner: bool,
    exclude: &[String],
    max_terms: usize,
) -> rusqlite::Result<(i64, Vec<String>)> {
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
    let terms: Vec<String> = stmt
        .query_map(refs.as_slice(), |r| r.get::<_, Option<String>>(0))?
        .filter_map(Result::ok)
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

/// 热缓存的一条。
#[derive(Debug, Clone, serde::Serialize)]
pub struct HotItem {
    pub id: i64,
    pub kind: String,
    pub subject: Option<String>,
    pub content: String,
    pub score: f64,
}

/// 只有这些字段参与评分。
type HotRow = (i64, String, Option<String>, String, i64, f64, i64, i64);

/// 按「置顶 > 重要度 > 新鲜度 > 命中」挑热缓存条目（与 hot-cache.mjs 的 score 一致）。
pub fn pick_hot(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<HotItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, subject, content, pinned, importance, hits, created_at
         FROM memory WHERE archived = 0",
    )?;
    let rows: Vec<HotRow> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .filter_map(Result::ok)
        .collect();

    let now = now_ms();
    let mut items: Vec<HotItem> = rows
        .into_iter()
        .map(
            |(id, kind, subject, content, pinned, importance, hits, created_at)| {
                let age_days = ((now - created_at) as f64 / 86_400_000.0).max(0.0);
                let score = (if pinned != 0 { 1.0 } else { 0.0 })
                    + 0.5 * importance
                    + 0.3 * (-age_days / 30.0).exp()
                    + 0.05 * ((hits as f64) / 5.0).tanh();
                HotItem {
                    id,
                    kind,
                    subject,
                    content,
                    score,
                }
            },
        )
        .collect();
    items.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    items.truncate(limit);
    Ok(items)
}

pub const HOT_BEGIN: &str = "<!-- memory:hot:begin 由桥接自动维护，勿手改 -->";
pub const HOT_END: &str = "<!-- memory:hot:end -->";
pub const HOT_BUDGET_CHARS: usize = 4096;

/// 渲染受管区块（与 hot-cache.mjs 的 render 逐字一致）。
pub fn render_hot(items: &[HotItem]) -> (String, usize, usize) {
    let mut lines: Vec<String> = vec![HOT_BEGIN.to_string()];
    let mut used = 0usize;
    let mut n = 0usize;
    for m in items {
        let tag = match &m.subject {
            Some(s) if !s.is_empty() => format!("{}·{}", m.kind, s),
            _ => m.kind.clone(),
        };
        let body: String = m.content.split_whitespace().collect::<Vec<_>>().join(" ");
        let body: String = body.chars().take(160).collect();
        let line = format!("- [{tag}] {body}");
        // 按字符数算预算（JS 用 .length，含代理对时略有差异；记忆内容基本是中文，等价）
        let line_len = line.chars().count();
        if used + line_len > HOT_BUDGET_CHARS - 120 {
            break;
        }
        lines.push(line);
        used += line_len + 1;
        n += 1;
    }
    lines.push(HOT_END.to_string());
    (lines.join("\n"), n, used)
}

/// 用新块替换 Markdown 里由 BEGIN/END 标记的区块；没有标记就追加到末尾。
/// 目前只被单测覆盖 —— 真正写 AGENTS.md 要等 P5 由 Rust 接管热缓存之后。
#[allow(dead_code)]
pub fn replace_hot_block(markdown: &str, block: &str) -> String {
    match (markdown.find(HOT_BEGIN), markdown.find(HOT_END)) {
        (Some(s), Some(e)) if e > s => {
            let mut out = String::with_capacity(markdown.len());
            out.push_str(&markdown[..s]);
            out.push_str(block);
            out.push_str(&markdown[e + HOT_END.len()..]);
            out
        }
        _ => format!("{markdown}\n\n{block}\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fts_fixture(content: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../spec/memory-schema.sql")).unwrap();
        for (text, importance) in [(content, 0.0), ("unrelated decoy", 1.0)] {
            write_memory(&conn, &MemoryInput {
                owner_type: "session", owner_id: "group:101", scope: "group", kind: "fact",
                subject: None, content: text, tags: None, source: "fixture", importance,
            }).unwrap();
        }
        conn
    }

    // ── Recall (automatic recall) ───────────────────────────────────────────────

    fn recall_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../spec/memory-schema.sql")).unwrap();
        for (owner, scope, subject, content, importance, pinned) in [
            ("group:101", "group", "户外计划", "周末一起去爬山看日出，早上六点出发", 0.3, 0),
            ("group:101", "group", "备份规则", "数据库备份每天凌晨三点执行并保留七天", 1.0, 1),
            ("group:101", "group", "饮食习惯", "不吃香菜，喜欢喝无糖的热豆浆", 0.9, 0),
            ("group:202", "group", "别群计划", "周末一起去爬山的另一个群的约定", 0.5, 0),
        ] {
            let id = write_memory(&conn, &MemoryInput {
                owner_type: "session", owner_id: owner, scope, kind: "fact",
                subject: Some(subject), content, tags: None, source: "fixture", importance,
            }).unwrap();
            conn.execute("UPDATE memory SET pinned=?1 WHERE id=?2", params![pinned, id]).unwrap();
        }
        conn
    }

    fn recalled_subjects(hits: &[Hit]) -> Vec<String> {
        hits.iter().map(|h| h.subject.clone().unwrap_or_default()).collect()
    }

    #[test]
    fn recall_finds_a_memory_that_shares_two_words_with_a_chat_length_message() {
        let conn = recall_fixture();
        let message = "我觉得这个周末可以一起去爬山吧，天气应该不错呀";
        assert!(message.chars().count() >= 20);
        let hits = recall(&conn, "group:101", false, message, 3, &[]).unwrap();
        assert_eq!(recalled_subjects(&hits), vec!["户外计划"], "only the memory that shares words, not the pinned or important rows");
    }

    #[test]
    fn recall_finds_a_memory_by_one_shared_identifier() {
        let conn = recall_fixture();
        write_memory(&conn, &MemoryInput {
            owner_type: "session", owner_id: "group:101", scope: "group", kind: "fact",
            subject: Some("暗号"), content: "zebra148kw 是约好的暗号", tags: None, source: "fixture", importance: 0.1,
        }).unwrap();
        let hits = recall(&conn, "group:101", false, "zebra148kw 你还记得吗", 3, &[]).unwrap();
        assert_eq!(recalled_subjects(&hits), vec!["暗号"]);
    }

    #[test]
    fn recall_of_an_unrelated_message_is_empty_even_with_pinned_and_recent_rows() {
        let conn = recall_fixture();
        for message in ["今天的天气真是太好了呀", "hello there general kenobi", "。。。！！！", "", "周末天气怎么样呢"] {
            let hits = recall(&conn, "group:101", false, message, 3, &[]).unwrap();
            assert!(hits.is_empty(), "{message:?} recalled {:?}", recalled_subjects(&hits));
        }
    }

    #[test]
    fn recall_never_repeats_an_item_the_session_generation_already_holds() {
        let conn = recall_fixture();
        let message = "周末一起去爬山看日出吧，早上出发";
        let first = recall(&conn, "group:101", false, message, 3, &[]).unwrap();
        assert_eq!(recalled_subjects(&first), vec!["户外计划"]);
        let held: Vec<i64> = first.iter().map(|h| h.id).collect();
        assert!(recall(&conn, "group:101", false, message, 3, &held).unwrap().is_empty());
    }

    #[test]
    fn recall_stays_inside_the_readers_scope() {
        let conn = recall_fixture();
        let hits = recall(&conn, "group:101", false, "周末一起去爬山的约定", 5, &[]).unwrap();
        assert!(hits.iter().all(|h| h.owner_id == "group:101"), "{:?}", recalled_subjects(&hits));
        let hits = recall(&conn, "group:101", true, "周末一起去爬山的约定", 5, &[]).unwrap();
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
            let hits = recall(&conn, "group:101", false, message, 3, &[]).expect(message);
            assert_eq!(recalled_subjects(&hits)[..1], ["户外计划"], "{message}");
        }
        let long: String = "周末一起去爬山看日出".chars().cycle().take(200).collect();
        assert_eq!(recalled_subjects(&recall(&conn, "group:101", false, &long, 3, &[]).unwrap())[..1], ["户外计划"]);
    }

    #[test]
    fn recall_ranks_by_relevance_before_pin_importance_and_recency() {
        let conn = recall_fixture();
        let message = "不吃香菜，喜欢喝热豆浆，数据库备份";
        let hits = recall(&conn, "group:101", false, message, 3, &[]).unwrap();
        // The pinned, importance 1.0 backup row shares one word; the diet row shares four.
        assert_eq!(recalled_subjects(&hits).first().map(String::as_str), Some("饮食习惯"));
    }

    #[test]
    fn fts_dot_query_cleaning_matches_js_before_token_cap() {
        // The high-scoring decoy distinguishes MATCH from the recent-memory fallback.
        for (query, indexed) in [
            ("deepseek-v4.1-flash", "deepseek v41 flash"),
            ("v0.2.1", "v021"),
            ("管理员偏好", "管理员偏好"),
            ("管理员 DeepSeek-v4.1-flash v0.2.1", "管理员 deepseek v41 flash v021"),
        ] {
            let conn = fts_fixture(indexed);
            let hits = search(&conn, "group:101", false, query, &[], 1).unwrap();
            assert_eq!(hits[0].content, indexed, "query: {query}");
        }
        let conn = fts_fixture("needle");
        let query = format!("{} needle", ". ".repeat(24));
        assert_eq!(search(&conn, "group:101", false, &query, &[], 1).unwrap()[0].content, "needle");
        let query = format!("{} absent", "needle ".repeat(24));
        assert_eq!(search(&conn, "group:101", false, &query, &[], 1).unwrap()[0].content, "needle");
    }

    #[test]
    fn fts_dot_punctuation_skips_match_and_dotted_storage_remains_searchable() {
        let conn = fts_fixture("deepseek-v4.1-flash v0.2.1");
        assert_eq!(search(&conn, "group:101", false, "deepseek", &[], 1).unwrap()[0].content,
                   "deepseek-v4.1-flash v0.2.1", "existing dotted index tokens stay searchable");
        conn.execute("DELETE FROM memory WHERE content='unrelated decoy'", []).unwrap();
        for query in ["deepseek-v4.1-flash", "v0.2.1"] {
            assert_eq!(search(&conn, "group:101", false, query, &[], 1).unwrap()[0].content,
                       "deepseek-v4.1-flash v0.2.1");
        }
        // No FTS table: a punctuation-only query must take the normal recent fallback.
        conn.execute_batch("DROP TABLE memory_fts").unwrap();
        assert_eq!(search(&conn, "group:101", false, "...-!?", &[], 1).unwrap().len(), 1);
    }

    #[test]
    fn fts_dot_search_returns_row_and_hit_update_failures() {
        let conn = fts_fixture("needle");
        conn.execute("UPDATE memory SET content=x'FF' WHERE id=1", []).unwrap();
        for query in ["needle", ""] {
            assert!(search(&conn, "group:101", false, query, &[], 5).is_err(),
                    "must report row decoding failure for {query:?}");
        }
        conn.execute("UPDATE memory SET content='needle' WHERE id=1", []).unwrap();
        conn.execute_batch("CREATE TRIGGER fail_hits BEFORE UPDATE OF hits ON memory BEGIN SELECT RAISE(FAIL, 'private-query-token-path'); END;").unwrap();
        assert!(search(&conn, "group:101", false, "needle", &[], 5).is_err(),
                "must report hit-update failure as JS does");
    }

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
        let schema = include_str!("../spec/memory-schema.sql");
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

    #[test]
    fn segment_matches_js() {
        // 与 JS 版对拍过的示例
        assert_eq!(segment("管理员偏好").join(" "), "管理 理员 员偏 偏好");
        assert_eq!(segment("你好").join(" "), "你好");
        assert_eq!(segment("A").join(" "), "a");
        assert_eq!(segment("DS 模型").join(" "), "ds 模型");
    }

    #[test]
    fn hot_block_replacement() {
        let md = format!("头部\n{HOT_BEGIN}\n旧内容\n{HOT_END}\n尾部");
        let out = replace_hot_block(&md, &format!("{HOT_BEGIN}\n新内容\n{HOT_END}"));
        assert!(out.contains("新内容"));
        assert!(!out.contains("旧内容"));
        assert!(out.starts_with("头部"));
        assert!(out.ends_with("尾部"));
    }

    #[test]
    fn hot_block_appends_when_missing() {
        let out = replace_hot_block("没有标记", &format!("{HOT_BEGIN}\nX\n{HOT_END}"));
        assert!(out.contains(HOT_BEGIN));
        assert!(out.starts_with("没有标记"));
    }

    #[test]
    fn segment_single_han_kept() {
        assert_eq!(segment("鱼").join(" "), "鱼");
    }
}
