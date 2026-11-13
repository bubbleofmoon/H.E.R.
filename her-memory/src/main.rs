// M5 前置验证：Rust 直接操作 JS 建的记忆库。
//
// 验证四件事（全部只读或导出，不改数据）：
//   1. 能打开同一个 memory.db（schema 完全兼容，不迁移、不重建）
//   2. 能用**同一套 bigram 分词**做 FTS5 检索，且命中结果与 JS 侧一致
//   3. 能按 ACL 语义筛出「群会话读不到私聊」
//   4. 能做 VACUUM INTO 一致性快照（备份链路在 Rust 侧也能跑）
use rusqlite::{params, Connection};
use serde_json::json;
use std::time::Instant;

const DB: &str = "C:/qqbot/data/memory.db";

/// 与 memory/schema.mjs 的 segment() 逐字对齐：中文 2 字 bigram + 英文/数字按词。
fn segment(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut han = String::new();
    let mut word = String::new();
    let flush_han = |han: &mut String, out: &mut Vec<String>| {
        let chars: Vec<char> = han.chars().collect();
        if chars.len() == 1 {
            out.push(chars[0].to_string());
        }
        for w in chars.windows(2) {
            out.push(w.iter().collect());
        }
        han.clear();
    };
    let flush_word = |word: &mut String, out: &mut Vec<String>| {
        if !word.is_empty() {
            out.push(word.to_lowercase());
            word.clear();
        }
    };
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
    out.join(" ")
}

fn diagnostic_hit(hit: &(i64, String, String, String, String, f64)) -> String {
    format!("    #{} rank {:.3}", hit.0, hit.5)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let t0 = Instant::now();
    let conn = Connection::open(DB)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
    println!("[1] 打开记忆库 OK（{}ms）", t0.elapsed().as_millis());

    // ① 表结构
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?
        .query_map([], |r| r.get(0))?
        .filter_map(Result::ok)
        .collect();
    println!("[1] 表 {} 张：{}", tables.len(), tables.join(", "));

    let total: i64 = conn.query_row("SELECT COUNT(*) FROM memory", [], |r| r.get(0))?;
    let acl: i64 = conn.query_row("SELECT COUNT(*) FROM memory_acl", [], |r| r.get(0))?;
    println!("[1] 记忆 {total} 条，ACL {acl} 条");

    // ② FTS 检索
    let q = segment("管理员偏好");
    let t1 = Instant::now();
    let mut stmt = conn.prepare(
        "SELECT m.id, m.owner_id, m.scope, m.kind, substr(m.content,1,40), bm25(memory_fts) AS rank
         FROM memory_fts f JOIN memory m ON m.id = f.rowid
         WHERE memory_fts MATCH ?1 ORDER BY rank LIMIT 5",
    )?;
    let hits: Vec<(i64, String, String, String, String, f64)> = stmt
        .query_map(params![q], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?
        .filter_map(Result::ok)
        .collect();
    println!("[2] 分词 {:?} -> 命中 {} 条（{}ms）", q, hits.len(), t1.elapsed().as_millis());
    for h in hits.iter().take(3) {
        println!("{}", diagnostic_hit(h));
    }

    // ③ ACL 语义：群会话读到的 private 必须为 0
    let group_key = "group:7002";
    let leak: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory WHERE owner_id = ?1 AND scope = 'private'",
        params![group_key],
        |r| r.get(0),
    )?;
    let all_private: i64 = conn.query_row("SELECT COUNT(*) FROM memory WHERE scope='private'", [], |r| r.get(0))?;
    println!("[3] private 记忆共 {all_private} 条；挂在群会话名下的 {leak} 条（应为 0）");

    // ④ 一致性快照
    let snap = "C:/qqbot/logs/her-memory-probe.db";
    let _ = std::fs::remove_file(snap);
    conn.execute(&format!("VACUUM INTO '{}'", snap), [])?;
    let size = std::fs::metadata(snap)?.len();
    println!("[4] VACUUM INTO 快照 OK：{} KB", size / 1024);

    // 汇总成 JSON，方便与 JS 侧对账
    let summary = json!({
        "tables": tables.len(),
        "memories": total,
        "acl": acl,
        "querySeg": q,
        "hits": hits.len(),
        "privateTotal": all_private,
        "privateUnderGroup": leak,
        "snapshotKB": size / 1024
    });
    std::fs::write("C:/qqbot/logs/her-memory-probe.json", serde_json::to_string_pretty(&summary)?)?;
    println!("\n汇总已写 logs/her-memory-probe.json");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn hotfix128_memory_probe_diagnostics_exclude_model_text() {
        let hit = (42, "group:1".into(), "group".into(), "note".into(),
            "K128session\nVALUExyz 0123456789abcdef0123456789abcdef0123456789abcdef".into(), 0.5);
        let line = super::diagnostic_hit(&hit);
        assert_eq!(line, "    #42 rank 0.500");
    }
}
