//! Local semantic recall. The vector cache is derived from the authoritative memory rows.
use std::{
    collections::HashMap,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, LazyLock, Mutex, PoisonError, TryLockError},
    thread,
    time::{Duration, Instant, SystemTime},
};

use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use rusqlite::{ffi, Connection, ToSql, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

use super::{atomic, hit_from_row, read_row, visibility_filter, Hit, Reader};

#[cfg(test)]
#[path = "calibration.rs"]
mod calibration;

const MODEL_DIR: &str = "recall/bge-small-zh-v1.5";
const MODEL_FILES: [&str; 5] = [
    "onnx/model.onnx",
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];
const DIMENSIONS: usize = 512;
const VECTOR_BYTES: usize = DIMENSIONS * std::mem::size_of::<f32>();
const BACKFILL_BATCH: usize = 16;
const INFERENCE_BUDGET: Duration = Duration::from_millis(250);
// Provisional cosine distance ceiling. Calibration belongs to the public benchmark step.
const MAX_DISTANCE: f64 = 0.22;

pub(super) const CACHE_SCHEMA_V4: &str = "
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

pub type EmbeddingFn = Box<dyn FnMut(&[String]) -> Option<Vec<Vec<f32>>> + Send>;
type LoadModel = dyn Fn(&dyn ModelResources) -> Option<LocalModel> + Send + Sync;

#[derive(Clone, PartialEq, Eq)]
pub struct FileStamp {
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
}

impl FileStamp {
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            size: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
        }
    }
}

/// Host-provided access to the local embedding model directory.
pub trait ModelResources: Send + Sync {
    fn resources_dir(&self) -> &Path;
    fn read_model_file(&self, relative: &str) -> Option<Vec<u8>>;
    fn model_file_stamp(&self, relative: &str) -> Option<FileStamp>;
}

#[derive(Clone)]
pub struct DenseModel {
    runner: Arc<ModelRunner>,
}

impl DenseModel {
    pub fn shared() -> &'static Self {
        static SHARED: LazyLock<DenseModel> = LazyLock::new(|| DenseModel {
            runner: Arc::new(ModelRunner::new(LocalModel::load)),
        });
        &SHARED
    }

    pub fn with_loader(
        load: impl Fn(&Path) -> Option<EmbeddingFn> + Send + Sync + 'static,
    ) -> Self {
        Self {
            runner: Arc::new(ModelRunner::new(move |resources| {
                Some(LocalModel {
                    fingerprint: [0; 32],
                    embed: load(resources.resources_dir())?,
                })
            })),
        }
    }
}

pub struct SemanticQuery<'a> {
    pub query: &'a str,
    pub resources: Arc<dyn ModelResources>,
    pub model: Option<&'a DenseModel>,
}

struct LocalModel {
    fingerprint: [u8; 32],
    embed: EmbeddingFn,
}

impl LocalModel {
    fn load(resources: &dyn ModelResources) -> Option<Self> {
        // ort otherwise falls back to a DLL name resolved through the Windows search path.
        #[cfg(windows)]
        if !std::env::var("ORT_DYLIB_PATH")
            .is_ok_and(|path| std::path::Path::new(&path).is_absolute())
        {
            return None;
        }

        let read = |file| resources.read_model_file(&format!("{MODEL_DIR}/{file}"));
        let [onnx, tokenizer_file, config_file, special_tokens_map_file, tokenizer_config_file] =
            MODEL_FILES;
        let onnx = read(onnx)?;
        let tokenizer = TokenizerFiles {
            tokenizer_file: read(tokenizer_file)?,
            config_file: read(config_file)?,
            special_tokens_map_file: read(special_tokens_map_file)?,
            tokenizer_config_file: read(tokenizer_config_file)?,
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
        let mut encoder = TextEmbedding::try_new_from_user_defined(model, options).ok()?;
        Some(Self {
            fingerprint: hash.finalize().into(),
            embed: Box::new(move |texts| encoder.embed(texts, Some(4)).ok()),
        })
    }
}

type ModelFiles = [Option<FileStamp>; MODEL_FILES.len()];

fn model_files(resources: &dyn ModelResources) -> ModelFiles {
    MODEL_FILES.map(|file| resources.model_file_stamp(&format!("{MODEL_DIR}/{file}")))
}

fn guarded<T>(attempt: impl FnOnce() -> Option<T>) -> Option<T> {
    catch_unwind(AssertUnwindSafe(attempt)).ok().flatten()
}

#[derive(Default)]
struct ModelState {
    active: Option<(PathBuf, LocalModel)>,
    failed: HashMap<PathBuf, ModelFiles>,
}

impl ModelState {
    fn model(&mut self, resources: &dyn ModelResources, load: &LoadModel) -> Option<&mut LocalModel> {
        let path = resources.resources_dir();
        if self.active.as_ref().is_some_and(|(root, _)| root == path) {
            return self.active.as_mut().map(|(_, model)| model);
        }
        let files = model_files(resources);
        if self.failed.get(path) == Some(&files) {
            return None;
        }
        let Some(model) = guarded(|| load(resources)) else {
            self.failed.insert(path.to_owned(), files);
            return None;
        };
        self.failed.remove(path);
        let (_, model) = self.active.insert((path.to_owned(), model));
        Some(model)
    }
}

struct ModelRunner {
    // This lock only transfers ownership of the model state. It is released before spawning,
    // reading files, touching SQLite or waiting on a channel. None means one worker owns it.
    state: Mutex<Option<ModelState>>,
    load: Box<LoadModel>,
}

struct ModelLease {
    runner: Arc<ModelRunner>,
    state: Option<ModelState>,
}

impl Drop for ModelLease {
    fn drop(&mut self) {
        *self
            .runner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = self.state.take();
    }
}

struct Inference {
    query: mpsc::Receiver<([u8; 32], Vec<u8>)>,
    rows: mpsc::Sender<Vec<String>>,
    vectors: mpsc::Receiver<Vec<u8>>,
}

fn receive_before<T>(receiver: &mpsc::Receiver<T>, deadline: Instant) -> Option<T> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    receiver.recv_timeout(remaining).ok()
}

impl ModelRunner {
    fn new(load: impl Fn(&dyn ModelResources) -> Option<LocalModel> + Send + Sync + 'static) -> Self {
        Self {
            state: Mutex::new(Some(ModelState::default())),
            load: Box::new(load),
        }
    }

    fn start(
        self: &Arc<Self>,
        resources: Arc<dyn ModelResources>,
        query: String,
        deadline: Instant,
    ) -> Option<Inference> {
        let state = match self.state.try_lock() {
            Ok(mut state) => state.take(),
            Err(TryLockError::Poisoned(error)) => error.into_inner().take(),
            Err(TryLockError::WouldBlock) => None,
        }?;
        let mut lease = ModelLease {
            runner: Arc::clone(self),
            state: Some(state),
        };
        let (send_query, receive_query) = mpsc::channel();
        let (send_rows, receive_rows) = mpsc::channel::<Vec<String>>();
        let (send_vector, receive_vector) = mpsc::channel();
        thread::Builder::new()
            .name("recall-inference".into())
            .spawn(move || {
                let _ = (|| {
                    let model = lease
                        .state
                        .as_mut()?
                        .model(&*resources, &*lease.runner.load)?;
                    if Instant::now() >= deadline {
                        return None;
                    }
                    let query_vector = encode_query(&mut model.embed, &query)?;
                    send_query.send((model.fingerprint, query_vector)).ok()?;
                    for text in receive_before(&receive_rows, deadline)? {
                        if Instant::now() >= deadline {
                            break;
                        }
                        // Empty blobs record failed rows for this model without entering ranking.
                        let vector = encode_one(&mut model.embed, text).unwrap_or_default();
                        if Instant::now() >= deadline || send_vector.send(vector).is_err() {
                            break;
                        }
                    }
                    Some(())
                })();
                // A completed wake can start another job as soon as either channel disconnects.
                drop(lease);
                drop((send_query, send_vector));
            })
            .ok()?;
        Some(Inference {
            query: receive_query,
            rows: send_rows,
            vectors: receive_vector,
        })
    }
}

pub(super) fn rank(
    semantic: SemanticQuery<'_>,
    reader: &Reader<'_>,
    excluded: &str,
    limit: usize,
) -> Option<Vec<Hit>> {
    if !reader.conn.is_autocommit() {
        return None;
    }
    let runner = &semantic.model.unwrap_or_else(DenseModel::shared).runner;
    let deadline = Instant::now() + INFERENCE_BUDGET;
    let Inference {
        query: result,
        rows,
        vectors,
    } = runner.start(semantic.resources, semantic.query.to_owned(), deadline)?;
    let (model, query_vector) = receive_before(&result, deadline)?;
    register_sqlite_vec(reader.conn).ok()?;

    let pending = pending_rows(reader, &model).unwrap_or_default();
    let _ = rows.send(pending.iter().map(PendingRow::text).collect());
    let mut completed = Vec::new();
    while let Some(vector) = receive_before(&vectors, deadline) {
        completed.push(vector);
    }
    if !completed.is_empty() {
        let _ = publish(reader, &model, &pending, completed);
    }
    rank_cached(reader, &query_vector, excluded, limit, &model, MAX_DISTANCE)
}

fn encode_query(
    embed: &mut impl FnMut(&[String]) -> Option<Vec<Vec<f32>>>,
    query: &str,
) -> Option<Vec<u8>> {
    let query = format!("为这个句子生成表示以用于检索相关文章：{query}");
    encode_one(embed, query)
}

fn rank_cached(
    reader: &Reader<'_>,
    query_vector: &[u8],
    excluded: &str,
    limit: usize,
    model: &[u8],
    ceiling: f64,
) -> Option<Vec<Hit>> {
    // Resolve grants and read ranked rows in the same snapshot. A previously indexed memory
    // must still pass the current reader's ACL before it can compete for a dense rank.
    let tx = Transaction::new_unchecked(reader.conn, TransactionBehavior::Deferred).ok()?;
    let (visibility, args) = scope(&tx, reader).ok()?;
    let sql = format!(
        "WITH scoped AS MATERIALIZED (
            SELECT m.id, vec_distance_cosine(e.embedding, ?1) AS distance
            FROM memory m JOIN memory_embedding_cache e ON e.memory_id = m.id
            WHERE e.model = ?2 AND length(e.embedding) = {VECTOR_BYTES} AND {visibility}{excluded}
         )
         SELECT m.id, m.owner_id, m.scope, m.kind, m.subject, m.content,
                s.distance, m.pinned, m.importance, m.hits, m.created_at,
                m.tags, m.source, m.last_used_at, m.owner_type, m.archived, m.expires_at
         FROM scoped s JOIN memory m ON m.id = s.id
         WHERE s.distance <= ?
         ORDER BY s.distance, m.id LIMIT {limit}"
    );
    let mut refs: Vec<&dyn ToSql> = vec![&query_vector, &model];
    refs.extend(args.iter().map(|arg| arg as &dyn ToSql));
    refs.push(&ceiling);
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
    visibility_filter(conn, &reader.key, reader.is_owner, &[])
}

struct PendingRow {
    id: i64,
    subject: Option<String>,
    content: String,
}

impl PendingRow {
    fn text(&self) -> String {
        let subject = self.subject.as_deref().unwrap_or("");
        // The tokenizer also truncates to the model's input length.
        subject
            .chars()
            .chain(['\n'])
            .chain(self.content.chars())
            .take(2_000)
            .collect()
    }
}

fn pending_rows(reader: &Reader<'_>, model: &[u8]) -> rusqlite::Result<Vec<PendingRow>> {
    let tx = Transaction::new_unchecked(reader.conn, TransactionBehavior::Deferred)?;
    let (visibility, args) = scope(&tx, reader)?;
    let sql = format!(
        "SELECT m.id, m.subject, m.content
         FROM memory m LEFT JOIN memory_embedding_cache e
           ON e.memory_id = m.id AND e.model = ?1
         WHERE e.memory_id IS NULL AND {visibility}
         ORDER BY m.created_at DESC, m.id DESC LIMIT {BACKFILL_BATCH}"
    );
    let mut refs: Vec<&dyn ToSql> = vec![&model];
    refs.extend(args.iter().map(|arg| arg as &dyn ToSql));
    let pending = {
        let mut stmt = tx.prepare(&sql)?;
        let rows = stmt.query_map(refs.as_slice(), |row| {
            Ok(PendingRow {
                id: row.get(0)?,
                subject: row.get(1)?,
                content: row.get(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    tx.commit()?;
    Ok(pending)
}

fn publish(
    reader: &Reader<'_>,
    model: &[u8],
    pending: &[PendingRow],
    vectors: Vec<Vec<u8>>,
) -> rusqlite::Result<()> {
    // Inference holds no SQLite transaction. Revalidate each row and the scope when publishing
    // in case the memory was edited, forgotten, archived, or its grant was revoked meanwhile.
    atomic(reader.conn, |conn| {
        let (visibility, args) = scope(conn, reader)?;
        let sql = format!(
            "INSERT INTO memory_embedding_cache(memory_id, model, embedding)
             SELECT m.id, ?1, ?2 FROM memory m
             WHERE m.id = ?3 AND m.subject IS ?4 AND m.content = ?5 AND {visibility}
             ON CONFLICT(memory_id) DO UPDATE SET model = excluded.model, embedding = excluded.embedding
             WHERE memory_embedding_cache.model <> excluded.model
                OR length(memory_embedding_cache.embedding) = 0"
        );
        let mut stmt = conn.prepare(&sql)?;
        for (row, vector) in pending.iter().zip(vectors) {
            let mut refs: Vec<&dyn ToSql> =
                vec![&model, &vector, &row.id, &row.subject, &row.content];
            refs.extend(args.iter().map(|arg| arg as &dyn ToSql));
            stmt.execute(refs.as_slice())?;
        }
        Ok(())
    })
}

fn encode_one(
    embed: &mut impl FnMut(&[String]) -> Option<Vec<Vec<f32>>>,
    text: String,
) -> Option<Vec<u8>> {
    let mut vectors = guarded(|| embed(&[text]))?;
    if vectors.len() != 1 {
        return None;
    }
    encode_vector(vectors.pop()?)
}

fn encode_vector(vector: Vec<f32>) -> Option<Vec<u8>> {
    if vector.len() != DIMENSIONS
        || vector.iter().any(|v| !v.is_finite())
        || vector.iter().all(|v| *v == 0.0)
    {
        return None;
    }
    Some(vector.into_iter().flat_map(f32::to_ne_bytes).collect())
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
