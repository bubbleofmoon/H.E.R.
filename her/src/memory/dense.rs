//! Local semantic recall. The vector cache is derived from the authoritative memory rows.
use std::{path::PathBuf, sync::Mutex};

use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use rusqlite::{ffi, Connection, ToSql, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

use super::{atomic, hit_from_row, read_row, visibility_filter, Hit, Reader};
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};

const MODEL_DIR: &str = "recall/bge-small-zh-v1.5";
const DIMENSIONS: usize = 512;
const BACKFILL_BATCH: usize = 16;
// Provisional cosine distance ceiling. Calibration belongs to the public benchmark step.
const MAX_DISTANCE: f64 = 0.22;

const CACHE_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS memory_embedding_cache (
        memory_id INTEGER PRIMARY KEY,
        model BLOB NOT NULL,
        embedding BLOB NOT NULL
    );
    CREATE TRIGGER IF NOT EXISTS memory_embedding_delete AFTER DELETE ON memory BEGIN
        DELETE FROM memory_embedding_cache WHERE memory_id = OLD.id;
    END;
    CREATE TRIGGER IF NOT EXISTS memory_embedding_update
        AFTER UPDATE OF id, subject, content, archived ON memory BEGIN
        DELETE FROM memory_embedding_cache WHERE memory_id = OLD.id;
    END;";

struct LocalModel {
    path: PathBuf,
    fingerprint: [u8; 32],
    encoder: TextEmbedding,
}

static MODEL: Mutex<Option<LocalModel>> = Mutex::new(None);

impl LocalModel {
    fn load(context: &RuntimeContext, path: PathBuf) -> Option<Self> {
        let read = |file| {
            context
                .read_file(RuntimeArea::Resources, &format!("{MODEL_DIR}/{file}"))
                .ok()
        };
        let onnx = read("onnx/model.onnx")?;
        let tokenizer = TokenizerFiles {
            tokenizer_file: read("tokenizer.json")?,
            config_file: read("config.json")?,
            special_tokens_map_file: read("special_tokens_map.json")?,
            tokenizer_config_file: read("tokenizer_config.json")?,
        };
        let mut hash = Sha256::new();
        hash.update(b"bge-small-zh-v1.5-cls-v1");
        hash.update(1u32.to_ne_bytes());
        for file in [
            &onnx,
            &tokenizer.tokenizer_file,
            &tokenizer.config_file,
            &tokenizer.special_tokens_map_file,
            &tokenizer.tokenizer_config_file,
        ] {
            hash.update(Sha256::digest(file));
        }
        let model = UserDefinedEmbeddingModel::new(onnx, tokenizer).with_pooling(Pooling::Cls);
        let options = InitOptionsUserDefined::new().with_intra_threads(2);
        let encoder = TextEmbedding::try_new_from_user_defined(model, options).ok()?;
        Some(Self {
            path,
            fingerprint: hash.finalize().into(),
            encoder,
        })
    }
}

pub(super) fn rank(
    context: &RuntimeContext,
    reader: &Reader<'_>,
    query: &str,
    excluded: &str,
    limit: usize,
) -> Option<Vec<Hit>> {
    #[cfg(test)]
    if let Some(embed) = fixture::encoder(context) {
        return rank_with(reader, query, excluded, limit, &[0; 32], embed);
    }

    let path = context
        .resolve_path(
            RuntimeArea::Resources,
            &format!("{MODEL_DIR}/onnx/model.onnx"),
            PathIntent::ReadFile,
        )
        .ok()?;
    // Concurrent wakes can use sparse recall while another wake is doing CPU inference.
    let mut slot = MODEL.try_lock().ok()?;
    if slot.as_ref().is_none_or(|model| model.path != path) {
        *slot = LocalModel::load(context, path);
    }
    let LocalModel {
        fingerprint,
        encoder,
        ..
    } = slot.as_mut()?;
    rank_with(reader, query, excluded, limit, fingerprint, |texts| {
        encoder.embed(texts, Some(4)).ok()
    })
}

fn rank_with(
    reader: &Reader<'_>,
    query: &str,
    excluded: &str,
    limit: usize,
    model: &[u8],
    mut embed: impl FnMut(&[String]) -> Option<Vec<Vec<f32>>>,
) -> Option<Vec<Hit>> {
    let query = format!("为这个句子生成表示以用于检索相关文章：{query}");
    let vectors = encode(&mut embed, &[query])?;
    let query_vector = vectors.first()?;
    let conn = reader.conn;
    register_sqlite_vec(conn).ok()?;
    ensure_cache(conn).ok()?;
    backfill(reader, model, &mut embed).ok()??;

    // Resolve grants and read ranked rows in the same snapshot. A previously indexed memory
    // must still pass the current reader's ACL before it can compete for a dense rank.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred).ok()?;
    let (visibility, args) = scope(&tx, reader).ok()?;
    let sql = format!(
        "WITH scoped AS MATERIALIZED (
            SELECT m.id, vec_distance_cosine(e.embedding, ?1) AS distance
            FROM memory m JOIN memory_embedding_cache e ON e.memory_id = m.id
            WHERE e.model = ?2 AND {visibility}{excluded}
         )
         SELECT m.id, m.owner_id, m.scope, m.kind, m.subject, m.content,
                s.distance, m.pinned, m.importance, m.hits, m.created_at,
                m.tags, m.source, m.last_used_at, m.owner_type, m.archived, m.expires_at
         FROM scoped s JOIN memory m ON m.id = s.id
         WHERE s.distance <= {MAX_DISTANCE}
         ORDER BY s.distance, m.id LIMIT {limit}"
    );
    let mut refs: Vec<&dyn ToSql> = vec![query_vector, &model];
    refs.extend(args.iter().map(|arg| arg as &dyn ToSql));
    let mut stmt = tx.prepare(&sql).ok()?;
    let hits = stmt
        .query_map(refs.as_slice(), |row| {
            let row = read_row(row)?;
            let similarity = 1.0 - row.6;
            Ok(hit_from_row(row, similarity))
        })
        .ok()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ok()?;
    Some(hits)
}

fn scope(conn: &Connection, reader: &Reader<'_>) -> rusqlite::Result<(String, Vec<String>)> {
    visibility_filter(conn, &reader.key, true, &[]) // RED-SCOPE
}

fn ensure_cache(conn: &Connection) -> rusqlite::Result<()> {
    let exists = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'memory_embedding_cache')",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !exists {
        atomic(conn, |conn| conn.execute_batch(CACHE_SCHEMA))?;
    }
    Ok(())
}

fn backfill(
    reader: &Reader<'_>,
    model: &[u8],
    embed: &mut impl FnMut(&[String]) -> Option<Vec<Vec<f32>>>,
) -> rusqlite::Result<Option<()>> {
    let conn = reader.conn;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    let (visibility, args) = scope(&tx, reader)?;
    let sql = format!(
        "SELECT m.id, m.subject, m.content
         FROM memory m LEFT JOIN memory_embedding_cache e
           ON e.memory_id = m.id AND e.model = ?1
         WHERE e.memory_id IS NULL AND {visibility}
         ORDER BY m.id LIMIT {BACKFILL_BATCH}"
    );
    let mut refs: Vec<&dyn ToSql> = vec![&model];
    refs.extend(args.iter().map(|arg| arg as &dyn ToSql));
    let pending = {
        let mut stmt = tx.prepare(&sql)?;
        let rows = stmt.query_map(refs.as_slice(), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    tx.commit()?;

    if pending.is_empty() {
        return Ok(Some(()));
    }
    let texts: Vec<_> = pending
        .iter()
        .map(|(_, subject, content)| {
            let subject = subject.as_deref().unwrap_or("");
            // The tokenizer truncates to the model's input length. Bound input before tokenizing, too.
            format!("{subject}\n{content}")
                .chars()
                .take(2_000)
                .collect()
        })
        .collect();
    let Some(vectors) = encode(embed, &texts) else {
        return Ok(None);
    };

    // Inference holds no SQLite transaction. Revalidate each row and the scope when publishing
    // in case the memory was edited, forgotten, archived, or its grant was revoked meanwhile.
    atomic(conn, |conn| {
        let (visibility, args) = scope(conn, reader)?;
        let sql = format!(
            "INSERT INTO memory_embedding_cache(memory_id, model, embedding)
             SELECT m.id, ?1, ?2 FROM memory m
             WHERE m.id = ?3 AND m.subject IS ?4 AND m.content = ?5 AND {visibility}
             ON CONFLICT(memory_id) DO UPDATE SET model = excluded.model, embedding = excluded.embedding"
        );
        let mut stmt = conn.prepare(&sql)?;
        for ((id, subject, content), vector) in pending.iter().zip(vectors) {
            let mut refs: Vec<&dyn ToSql> = vec![&model, &vector, id, subject, content];
            refs.extend(args.iter().map(|arg| arg as &dyn ToSql));
            stmt.execute(refs.as_slice())?;
        }
        Ok(Some(()))
    })
}

fn encode(
    embed: &mut impl FnMut(&[String]) -> Option<Vec<Vec<f32>>>,
    texts: &[String],
) -> Option<Vec<Vec<u8>>> {
    let vectors = embed(texts)?;
    if vectors.len() != texts.len() {
        return None;
    }
    vectors
        .into_iter()
        .map(|vector| {
            if vector.len() != DIMENSIONS
                || vector.iter().any(|v| !v.is_finite())
                || vector.iter().all(|v| *v == 0.0)
            {
                return None;
            }
            Some(vector.into_iter().flat_map(f32::to_ne_bytes).collect())
        })
        .collect()
}

fn register_sqlite_vec(conn: &Connection) -> rusqlite::Result<()> {
    // sqlite-vec exposes an untyped entry point. Its C initializer has the same signature
    // as SQLite's extension callback and is built with SQLITE_CORE, so no API table is needed.
    let init: rusqlite::auto_extension::RawAutoExtension =
        unsafe { std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ()) };
    let mut message = std::ptr::null_mut();
    let code = unsafe { init(conn.handle(), &mut message, std::ptr::null()) };
    unsafe { ffi::sqlite3_free(message.cast()) };
    if code == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(rusqlite::Error::SqliteFailure(ffi::Error::new(code), None))
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    use super::*;
    use std::collections::HashMap;

    type Encoder = fn(&[String]) -> Option<Vec<Vec<f32>>>;
    static ENCODERS: std::sync::LazyLock<Mutex<HashMap<PathBuf, Encoder>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

    pub(crate) struct FixtureEncoder(PathBuf);

    impl Drop for FixtureEncoder {
        fn drop(&mut self) {
            ENCODERS.lock().unwrap().remove(&self.0);
        }
    }

    pub(crate) fn install(context: &RuntimeContext, embed: Encoder) -> FixtureEncoder {
        let path = context.runtime_root().unwrap().to_owned();
        assert!(ENCODERS
            .lock()
            .unwrap()
            .insert(path.clone(), embed)
            .is_none());
        FixtureEncoder(path)
    }

    pub(super) fn encoder(context: &RuntimeContext) -> Option<Encoder> {
        ENCODERS
            .lock()
            .unwrap()
            .get(context.runtime_root()?)
            .copied()
    }
}
