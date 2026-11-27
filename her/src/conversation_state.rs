//! Storage and transaction boundary for persisted conversation state (`social-v2.json` and
//! `social-v1.json`) behind the social gate (`locks/social-v2.lock`).

use crate::console_auth::{credential_digest, credential_eq, decode_digest, digests_equal};
use crate::runtime::{runtime_io_error, PathIntent, RuntimeArea, RuntimeContext};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, Instant};

pub(crate) const FILE: &str = "social-v2.json";
pub(crate) const RESERVED_FILE: &str = "social-v1.json";

const CONVERSATIONS: &str = "conversations";
const AGENT_TOKEN: &str = "agentToken";
const UNREAD: &str = "unread";
const IMPRESSIONS: &str = "memberImpressions";
const MODEL_SEEN_SEQS: &str = "modelSeenSeqs";

pub(crate) const SOCIAL_STATE_GATE: &str = "locks/social-v2.lock";
/// Upper bound on how long a writer queues for `locks/social-v2.lock` before returning 503.
///
/// Lock order:
/// 1. `closed_agent::lock` (outermost, when held by access/owner handlers)
/// 2. Per-request unique `locks/observation-wait-*.lock` (`WaitLease::live` in `console.rs`,
///    acquired before `WaitLease::publish` enters `update_document`; unique per wait generation
///    so no writer blocks on it, and `observation_wait_turn` only performs a non-blocking probe)
/// 3. `SOCIAL_STATE_GATE` (`locks/social-v2.lock`)
/// 4. Selected store/coordination locks and in-process leaf mutexes: `locks/authority.lock`,
///    `locks/settings-store.lock`, `locks/sessions.lock`, `locks/personas.lock`,
///    `locks/social-v1.lock`, `locks/schedule.lock`, `wait_leases()` (inspected only as a non-I/O
///    leaf check via `WaitLease::still_current()` inside `WaitLease::publish`; `WaitLease::drop`
///    drops both `live` and the social gate before taking `wait_leases()`), and in-process agent
///    receipt/grant mutexes (`receipt_store`, `snapshot_grants`, `urgent_delivery_store`).
///
/// Work performed under `SOCIAL_STATE_GATE`: `acquire_social_state_lock` reads runtime config
/// (`load_config`) and rechecks `console_authority`; document updates perform `social-v2.json`
/// read/atomic-replace and lower-ranked selected store reads/writes (such as session/turn-ledger
/// cleanup on reset). Nothing acquires an outer lock, sleeps, waits on channels, or performs
/// network/gateway I/O while holding `SOCIAL_STATE_GATE`.
pub(crate) const SOCIAL_STATE_GATE_WAIT: Duration = Duration::from_secs(5);

fn is_reserved_typed_field(name: &str) -> bool {
    matches!(name, AGENT_TOKEN | UNREAD | IMPRESSIONS | MODEL_SEEN_SEQS)
}

fn reorder_map(order: &[String], mut entries: Map<String, Value>) -> Map<String, Value> {
    if order.is_empty() {
        return entries;
    }
    let mut out = Map::with_capacity(entries.len());
    for key in order {
        if let Some(value) = entries.remove(key) {
            out.insert(key.clone(), value);
        }
    }
    for (key, value) in entries {
        out.insert(key, value);
    }
    out
}

fn strip_receipts_in_map(conversations: &mut Map<String, Value>) {
    for conversation in conversations.values_mut().filter_map(Value::as_object_mut) {
        conversation.remove(MODEL_SEEN_SEQS);
    }
}

pub(crate) type Failure = (u16, String);

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// Parse a JSON number as a JS safe integer (`-(2^53 - 1)..=(2^53 - 1)`), accepting whole floats
/// such as `1.0`.
pub(crate) fn js_safe_integer(value: &Value) -> Option<i64> {
    if let Some(integer) = value.as_i64() {
        return (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER)
            .contains(&integer)
            .then_some(integer);
    }
    if let Some(integer) = value.as_u64() {
        return (integer <= MAX_SAFE_INTEGER as u64).then_some(integer as i64);
    }
    value.as_f64().and_then(|number| {
        (number.is_finite()
            && number.fract() == 0.0
            && number >= -(MAX_SAFE_INTEGER as f64)
            && number <= MAX_SAFE_INTEGER as f64)
            .then_some(number as i64)
    })
}

pub(crate) fn str_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

pub(crate) fn num_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok().map(|f| f as i64).unwrap_or(0),
        _ => 0,
    }
}

/// Non-reversible identity stored in turn and send records.
pub(crate) fn token_epoch(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn token_epoch_matches(token: &str, epoch: &str) -> bool {
    !token.is_empty()
        && decode_digest::<8>(epoch).is_some_and(|expected| {
            let digest = credential_digest(token);
            digests_equal(&expected, digest.first_chunk::<8>().unwrap())
        })
}

/// A fresh agent token from two OS-seeded ids.
pub(crate) fn fresh_agent_token() -> String {
    format!(
        "{}{}",
        crate::turn_ledger::mint_id(""),
        crate::turn_ledger::mint_id("")
    )
}

/// Typed unread message entry inside a conversation's `unread` array, preserving all stored and
/// unknown fields (including numeric spellings such as `1.0` or unsafe seqs) across round-trips.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct UnreadMessage {
    fields: Map<String, Value>,
}

impl UnreadMessage {
    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        value.as_object().cloned().map(|fields| Self { fields })
    }

    pub(crate) fn to_value(&self) -> Value {
        Value::Object(self.fields.clone())
    }

    pub(crate) fn safe_seq(&self) -> Option<i64> {
        self.fields.get("seq").and_then(js_safe_integer)
    }

    pub(crate) fn raw_i64_seq(&self) -> Option<i64> {
        self.fields
            .get("seq")
            .and_then(|seq| seq.as_i64().or_else(|| js_safe_integer(seq)))
    }

    pub(crate) fn sender(&self) -> Option<&str> {
        self.fields.get("sender").and_then(Value::as_str)
    }

    pub(crate) fn user_id_scalar(&self) -> Option<String> {
        self.fields
            .get("userId")
            .filter(|v| !v.is_null())
            .map(|v| str_of(Some(v)))
            .filter(|s| !s.is_empty())
    }

    pub(crate) fn text(&self) -> Option<&str> {
        self.fields.get("text").and_then(Value::as_str)
    }

    pub(crate) fn time(&self) -> Option<i64> {
        self.fields.get("time").map(|v| num_of(Some(v)))
    }

    pub(crate) fn is_self(&self) -> bool {
        self.fields.get("isSelf").and_then(Value::as_bool) == Some(true)
    }

    pub(crate) fn at_self(&self) -> bool {
        self.fields.get("atSelf").and_then(Value::as_bool) == Some(true)
    }

    pub(crate) fn quote_target_is_self(&self) -> bool {
        self.fields
            .get("quoteTargetIsSelf")
            .and_then(Value::as_bool)
            == Some(true)
    }

    pub(crate) fn message_id(&self) -> Option<&Value> {
        self.fields.get("messageId")
    }

    pub(crate) fn media(&self) -> Option<&[Value]> {
        self.fields
            .get("media")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
    }

    pub(crate) fn forward_ids(&self) -> Option<&[Value]> {
        self.fields
            .get("forwardIds")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
    }

    pub(crate) fn field(&self, name: &str) -> Option<&Value> {
        self.fields.get(name)
    }

    pub(crate) fn remove_full_text(&mut self) {
        self.fields.remove("fullText");
    }
}

/// Typed impression entry inside a conversation's `memberImpressions` map, tolerating legacy
/// scalar spellings, keeping missing `lastSeenAt` distinct from `0`, and preserving unknown fields
/// and insertion order on serialization.
#[derive(Debug, Clone, Default)]
pub(crate) struct MemberImpression {
    pub traits: Vec<String>,
    pub interaction_count: i64,
    pub last_seen_at: Option<i64>,
    pub extra: Map<String, Value>,
    key_order: Vec<String>,
}

impl PartialEq for MemberImpression {
    fn eq(&self, other: &Self) -> bool {
        self.traits == other.traits
            && self.interaction_count == other.interaction_count
            && self.last_seen_at == other.last_seen_at
            && self.extra == other.extra
    }
}

impl MemberImpression {
    pub(crate) fn new(traits: Vec<String>, interaction_count: i64, last_seen_at: Option<i64>) -> Self {
        Self {
            traits,
            interaction_count,
            last_seen_at,
            extra: Map::new(),
            key_order: Vec::new(),
        }
    }

    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        let key_order: Vec<String> = obj.keys().cloned().collect();
        let mut extra = obj.clone();
        let traits = extra
            .remove("traits")
            .and_then(|v| match v {
                Value::Array(items) => {
                    Some(items.iter().map(|x| str_of(Some(x))).collect::<Vec<_>>())
                }
                _ => None,
            })
            .unwrap_or_default();
        let interaction_count = num_of(extra.remove("interactionCount").as_ref());
        let last_seen_at = extra
            .remove("lastSeenAt")
            .filter(|v| !v.is_null())
            .map(|v| num_of(Some(&v)));
        Some(Self {
            traits,
            interaction_count,
            last_seen_at,
            extra,
            key_order,
        })
    }

    pub(crate) fn to_value(&self) -> Value {
        let mut out = Map::new();
        out.insert(
            "traits".to_string(),
            Value::Array(self.traits.iter().cloned().map(Value::String).collect()),
        );
        out.insert("interactionCount".to_string(), json!(self.interaction_count));
        if let Some(last_seen_at) = self.last_seen_at {
            out.insert("lastSeenAt".to_string(), json!(last_seen_at));
        }
        for (k, v) in &self.extra {
            out.insert(k.clone(), v.clone());
        }
        Value::Object(reorder_map(&self.key_order, out))
    }
}

/// Typed conversation record from `social-v2.json` owning `token` (`agentToken`), `generation`
/// (SHA-256 token epoch), `unread` (`unread`), and `impressions` (`memberImpressions`), while
/// preserving every other field and insertion order on atomic write.
#[derive(Debug, Clone, Default)]
pub(crate) struct Conversation {
    token_raw: Option<Value>,
    unread: Vec<UnreadMessage>,
    unread_dirty: bool,
    unread_raw: Option<Value>,
    impressions: IndexMap<String, MemberImpression>,
    impressions_dirty: bool,
    impressions_raw: Option<Value>,
    extra: Map<String, Value>,
    key_order: Vec<String>,
}

impl PartialEq for Conversation {
    fn eq(&self, other: &Self) -> bool {
        self.token_raw == other.token_raw
            && self.unread == other.unread
            && self.unread_dirty == other.unread_dirty
            && self.unread_raw == other.unread_raw
            && self.impressions == other.impressions
            && self.impressions_dirty == other.impressions_dirty
            && self.impressions_raw == other.impressions_raw
            && self.extra == other.extra
    }
}

impl Conversation {
    /// Parse a stored conversation JSON object tolerantly. Non-object values return `None`.
    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        let obj_ref = value.as_object()?;
        let key_order: Vec<String> = obj_ref
            .keys()
            .filter(|k| k.as_str() != MODEL_SEEN_SEQS)
            .cloned()
            .collect();
        let mut obj = obj_ref.clone();
        obj.remove(MODEL_SEEN_SEQS);
        let token_raw = obj.remove(AGENT_TOKEN);
        let unread_raw = obj.remove(UNREAD);
        let unread = unread_raw
            .as_ref()
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(UnreadMessage::from_value)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let impressions_raw = obj.remove(IMPRESSIONS);
        let mut impressions = IndexMap::new();
        if let Some(map) = impressions_raw.as_ref().and_then(Value::as_object) {
            for (name, entry) in map {
                if let Some(imp) = MemberImpression::from_value(entry) {
                    impressions.insert(name.clone(), imp);
                }
            }
        }
        Some(Self {
            token_raw,
            unread,
            unread_dirty: false,
            unread_raw,
            impressions,
            impressions_dirty: false,
            impressions_raw,
            extra: obj,
            key_order,
        })
    }

    pub(crate) fn to_value(&self) -> Value {
        let mut out = self.extra.clone();
        out.remove(MODEL_SEEN_SEQS);
        if let Some(token) = &self.token_raw {
            out.insert(AGENT_TOKEN.to_string(), token.clone());
        }
        if self.unread_dirty {
            out.insert(
                UNREAD.to_string(),
                Value::Array(self.unread.iter().map(UnreadMessage::to_value).collect()),
            );
        } else if let Some(raw) = &self.unread_raw {
            out.insert(UNREAD.to_string(), raw.clone());
        }
        if self.impressions_dirty {
            let mut map = Map::new();
            for (name, imp) in &self.impressions {
                map.insert(name.clone(), imp.to_value());
            }
            out.insert(IMPRESSIONS.to_string(), Value::Object(map));
        } else if let Some(raw) = &self.impressions_raw {
            out.insert(IMPRESSIONS.to_string(), raw.clone());
        }
        Value::Object(reorder_map(&self.key_order, out))
    }

    /// A fresh v2 conversation record with default wake configuration, empty message windows and a
    /// new agent token.
    pub(crate) fn new_default(context: &RuntimeContext) -> Self {
        let cfg = context.load_config().unwrap_or_else(|_| json!({}));
        let random = u64::from_str_radix(&crate::turn_ledger::mint_id("")[..8], 16).unwrap_or(0)
            as f64
            / 4_294_967_296.0;
        let mut extra = Map::new();
        extra.insert(
            "wakeConfig".to_string(),
            crate::wake::default_wake_config(&cfg, crate::turn_ledger::now_ms(), random),
        );
        extra.insert("recentMessages".to_string(), json!([]));
        extra.insert("lastWakeReason".to_string(), json!(""));
        extra.insert("lastAiReplyAt".to_string(), json!(0));
        extra.insert("lastActionAt".to_string(), json!(0));
        extra.insert("bootstrapSent".to_string(), json!(false));
        extra.insert("wakeTimes".to_string(), json!([]));
        extra.insert("sendTimes".to_string(), json!([]));
        Self {
            token_raw: Some(Value::String(fresh_agent_token())),
            unread: Vec::new(),
            unread_dirty: true,
            unread_raw: None,
            impressions: IndexMap::new(),
            impressions_dirty: false,
            impressions_raw: None,
            extra,
            key_order: Vec::new(),
        }
    }

    /// Active nonempty agent token, treating missing, `null`, non-string, and `""` uniformly as
    /// `None`.
    pub(crate) fn token(&self) -> Option<&str> {
        self.token_raw
            .as_ref()
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
    }

    pub(crate) fn token_is_null(&self) -> bool {
        self.token_raw.as_ref().is_none_or(Value::is_null)
    }

    /// The token's SHA-256 generation epoch, if an active token is present.
    pub(crate) fn generation(&self) -> Option<String> {
        self.token().map(token_epoch)
    }

    pub(crate) fn matches_token(&self, presented: &str) -> bool {
        self.token()
            .is_some_and(|expected| credential_eq(expected, presented))
    }

    pub(crate) fn matches_generation(&self, epoch: &str) -> bool {
        epoch.is_empty()
            || self
                .token()
                .is_some_and(|token| token_epoch_matches(token, epoch))
    }

    pub(crate) fn rotate_token(&mut self) -> String {
        let token = fresh_agent_token();
        self.token_raw = Some(Value::String(token.clone()));
        token
    }

    #[cfg(test)]
    pub(crate) fn set_token(&mut self, token: impl Into<String>) {
        self.token_raw = Some(Value::String(token.into()));
    }

    pub(crate) fn ensure_token(&mut self) -> String {
        match self.token() {
            Some(token) => token.to_string(),
            None => self.rotate_token(),
        }
    }

    pub(crate) fn unread(&self) -> &[UnreadMessage] {
        &self.unread
    }

    pub(crate) fn unread_count(&self) -> usize {
        self.unread.len()
    }

    pub(crate) fn unread_values(&self) -> Vec<Value> {
        self.unread.iter().map(UnreadMessage::to_value).collect()
    }

    pub(crate) fn unread_mut(&mut self) -> &mut Vec<UnreadMessage> {
        self.unread_dirty = true;
        &mut self.unread
    }

    pub(crate) fn push_unread(&mut self, mut message: UnreadMessage, limit: usize) {
        message.remove_full_text();
        let list = self.unread_mut();
        list.push(message);
        if list.len() > limit {
            let excess = list.len() - limit;
            list.drain(..excess);
        }
    }

    pub(crate) fn clear_unread(&mut self) {
        self.unread_dirty = true;
        self.unread.clear();
    }

    /// Drop unread items with `safe_seq <= through` (or all items when `through` is `None`).
    /// Items without a valid safe-integer `seq` are kept when `through` is `Some`, matching JS.
    pub(crate) fn acknowledge_unread(&mut self, through: Option<i64>) -> usize {
        let before = self.unread.len();
        let last_unread = self
            .extra
            .get("lastUnreadSeq")
            .and_then(js_safe_integer)
            .unwrap_or(0);
        match through {
            None => {
                self.clear_unread();
                self.extra
                    .insert("lastReadThroughSeq".to_string(), json!(last_unread));
            }
            Some(seq) => {
                self.unread_mut()
                    .retain(|m| m.safe_seq().map(|s| s > seq).unwrap_or(true));
                let previous = self
                    .extra
                    .get("lastReadThroughSeq")
                    .and_then(js_safe_integer)
                    .unwrap_or(0);
                self.extra
                    .insert("lastReadThroughSeq".to_string(), json!(previous.max(seq)));
            }
        }
        before.saturating_sub(self.unread.len())
    }

    pub(crate) fn impressions(&self) -> &IndexMap<String, MemberImpression> {
        &self.impressions
    }

    pub(crate) fn impressions_mut(&mut self) -> &mut IndexMap<String, MemberImpression> {
        self.impressions_dirty = true;
        &mut self.impressions
    }

    pub(crate) fn clear_impressions(&mut self) {
        self.impressions_dirty = true;
        self.impressions.clear();
    }

    pub(crate) fn impressions_to_value(&self) -> Value {
        let mut map = Map::new();
        for (name, imp) in &self.impressions {
            map.insert(name.clone(), imp.to_value());
        }
        Value::Object(map)
    }

    /// Record an observed interaction by an existing impression member (matched by QQ id or name).
    pub(crate) fn note_member_seen(
        &mut self,
        user_id: Option<&str>,
        sender: Option<&str>,
        now: i64,
    ) {
        if self.impressions.is_empty() {
            return;
        }
        let mut matched = false;
        for (name, entry) in self.impressions.iter_mut() {
            if [user_id, sender]
                .into_iter()
                .flatten()
                .any(|id| !id.is_empty() && id == name)
            {
                entry.interaction_count += 1;
                entry.last_seen_at = Some(now);
                matched = true;
            }
        }
        if matched {
            self.impressions_dirty = true;
        }
    }

    pub(crate) fn field(&self, name: &str) -> Option<&Value> {
        if is_reserved_typed_field(name) {
            return None;
        }
        self.extra.get(name)
    }

    #[cfg(test)]
    pub(crate) fn try_set_field(&mut self, name: impl Into<String>, value: Value) -> Result<(), Failure> {
        let name = name.into();
        if is_reserved_typed_field(&name) {
            return Err((
                500,
                format!("reserved conversation field `{name}` must use typed accessors"),
            ));
        }
        self.extra.insert(name, value);
        Ok(())
    }

    pub(crate) fn set_field(&mut self, name: impl Into<String>, value: Value) {
        let name = name.into();
        if is_reserved_typed_field(&name) {
            return;
        }
        self.extra.insert(name, value);
    }

    pub(crate) fn remove_field(&mut self, name: &str) -> Option<Value> {
        if is_reserved_typed_field(name) {
            return None;
        }
        self.extra.remove(name)
    }
}

/// Whole `social-v2.json` document preserving top-level unknown fields, key insertion order, and
/// non-migrated keys.
#[derive(Debug, Clone)]
pub(crate) struct SocialDocument {
    paused_raw: Option<Value>,
    reset_epoch_raw: Option<Value>,
    conversations: Map<String, Value>,
    extra: Map<String, Value>,
    key_order: Vec<String>,
}

impl SocialDocument {
    pub(crate) fn empty() -> Self {
        Self {
            paused_raw: None,
            reset_epoch_raw: None,
            conversations: Map::new(),
            extra: Map::new(),
            key_order: Vec::new(),
        }
    }

    pub(crate) fn from_value(value: Value) -> Result<Self, Failure> {
        let Value::Object(mut obj) = value else {
            return Err((500, "runtime data is invalid".to_string()));
        };
        let key_order: Vec<String> = obj.keys().cloned().collect();
        let paused_raw = obj.remove("paused");
        let reset_epoch_raw = obj.remove("resetEpoch");
        let conversations = match obj.remove(CONVERSATIONS) {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(mut map)) => {
                strip_receipts_in_map(&mut map);
                map
            }
            Some(_) => return Err((500, "runtime data is invalid".to_string())),
        };
        Ok(Self {
            paused_raw,
            reset_epoch_raw,
            conversations,
            extra: obj,
            key_order,
        })
    }

    pub(crate) fn into_value(self) -> Value {
        let mut out = self.extra;
        if let Some(paused) = self.paused_raw {
            out.insert("paused".to_string(), paused);
        }
        if let Some(reset_epoch) = self.reset_epoch_raw {
            out.insert("resetEpoch".to_string(), reset_epoch);
        }
        out.insert(CONVERSATIONS.to_string(), Value::Object(self.conversations));
        Value::Object(reorder_map(&self.key_order, out))
    }

    pub(crate) fn paused(&self) -> bool {
        self.paused_raw.as_ref().and_then(Value::as_bool) == Some(true)
    }

    pub(crate) fn set_paused(&mut self, paused: bool) {
        self.paused_raw = Some(Value::Bool(paused));
    }

    pub(crate) fn reset_epoch(&self) -> Value {
        self.reset_epoch_raw.clone().unwrap_or(Value::Null)
    }

    pub(crate) fn has_conversation(&self, key: &str) -> bool {
        self.conversations.contains_key(key)
    }

    pub(crate) fn conversation(&self, key: &str) -> Option<Conversation> {
        self.conversations
            .get(key)
            .and_then(Conversation::from_value)
    }

    pub(crate) fn conversations(&self) -> impl Iterator<Item = (&str, Conversation)> {
        self.conversations
            .iter()
            .filter_map(|(k, v)| Conversation::from_value(v).map(|c| (k.as_str(), c)))
    }

    pub(crate) fn conversation_keys(&self) -> Vec<String> {
        self.conversations.keys().cloned().collect()
    }

    pub(crate) fn is_empty_conversations(&self) -> bool {
        self.conversations.is_empty()
    }

    pub(crate) fn set_conversation(&mut self, key: impl Into<String>, conv: Conversation) {
        self.conversations.insert(key.into(), conv.to_value());
    }

    pub(crate) fn remove_conversation(&mut self, key: &str) -> bool {
        self.conversations.remove(key).is_some()
    }

    pub(crate) fn active_tokens(&self) -> Vec<String> {
        self.conversations()
            .filter_map(|(_, c)| c.token().map(str::to_string))
            .collect()
    }

    pub(crate) fn rotate_all_tokens(&mut self) {
        let keys: Vec<String> = self.conversations.keys().cloned().collect();
        for key in keys {
            if let Some(mut conv) = self.conversation(&key) {
                conv.rotate_token();
                self.set_conversation(key, conv);
            }
        }
    }

    pub(crate) fn replace_all_with_reset(
        &mut self,
        kept: Map<String, Value>,
        reset_epoch: String,
    ) {
        self.paused_raw = None;
        self.reset_epoch_raw = Some(Value::String(reset_epoch));
        self.conversations = kept;
        self.extra.clear();
    }
}

/// Typed interface to a runtime's persisted conversation state (`social-v2.json`), serialized
/// under the social gate (`locks/social-v2.lock`).
pub(crate) struct ConversationState<'a> {
    context: &'a RuntimeContext,
}

impl<'a> ConversationState<'a> {
    pub(crate) fn for_runtime(context: &'a RuntimeContext) -> Self {
        Self { context }
    }

    pub(crate) fn read(&self, key: &str) -> Result<Option<Conversation>, Failure> {
        let doc = self.read_document()?;
        Ok(doc.conversation(key))
    }

    pub(crate) fn read_document(&self) -> Result<SocialDocument, Failure> {
        let value = read_console_document(self.context)?;
        SocialDocument::from_value(value)
    }

    /// Read `social-v2.json` tolerantly, returning an empty `SocialDocument` when the file is absent.
    pub(crate) fn read_optional_document(&self) -> Result<SocialDocument, Failure> {
        let value = read_optional_document(self.context).map_err(|e| (500, e))?;
        SocialDocument::from_value(value)
    }

    /// Update `key` (creating a minimal empty record if absent) atomically under the social gate.
    pub(crate) fn update<T>(
        &self,
        key: &str,
        f: impl FnOnce(&mut Conversation) -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        self.update_inner(key, false, f)
    }

    /// Update `key` (initializing with `Conversation::new_default` if absent) atomically under the
    /// social gate.
    pub(crate) fn update_or_init<T>(
        &self,
        key: &str,
        f: impl FnOnce(&mut Conversation) -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        self.update_inner(key, true, f)
    }

    fn update_inner<T>(
        &self,
        key: &str,
        init_full: bool,
        f: impl FnOnce(&mut Conversation) -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        self.update_document(|doc| {
            let mut conv = match doc.conversations.get(key) {
                None | Some(Value::Null) => {
                    if init_full {
                        Conversation::new_default(self.context)
                    } else {
                        Conversation::default()
                    }
                }
                Some(existing) => Conversation::from_value(existing)
                    .ok_or_else(|| (500, "runtime data is invalid".to_string()))?,
            };
            let result = f(&mut conv)?;
            doc.set_conversation(key, conv);
            Ok(result)
        })
    }

    /// Update `key` only if it already exists as a conversation object, atomically under the
    /// social gate. Returns `Ok(None)` without writing when absent.
    pub(crate) fn update_existing<T>(
        &self,
        key: &str,
        f: impl FnOnce(&mut Conversation) -> Result<T, Failure>,
    ) -> Result<Option<T>, Failure> {
        let _gate = acquire(self.context)?;
        let value = read_console_document(self.context)?;
        let mut doc = SocialDocument::from_value(value)?;
        let Some(mut conv) = doc.conversation(key) else {
            return Ok(None);
        };
        let result = f(&mut conv)?;
        doc.set_conversation(key, conv);
        write_console_document(self.context, &doc.into_value())
            .map_err(|error| (500, format!("social-state/write: {error}")))?;
        Ok(Some(result))
    }

    /// Update the whole `social-v2.json` document atomically under the social gate.
    pub(crate) fn update_document<T>(
        &self,
        f: impl FnOnce(&mut SocialDocument) -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        let _gate = acquire(self.context)?;
        let value = read_console_document(self.context)?;
        let mut doc = SocialDocument::from_value(value)?;
        let result = f(&mut doc)?;
        write_console_document(self.context, &doc.into_value())
            .map_err(|error| (500, format!("social-state/write: {error}")))?;
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Token/generation helpers backed by ConversationState
// ---------------------------------------------------------------------------

/// Read the active token using the console's read policy.
pub(crate) fn read_agent_token(
    context: &RuntimeContext,
    key: &str,
) -> Result<Option<String>, String> {
    let conv = ConversationState::for_runtime(context)
        .read(key)
        .map_err(|(_, message)| message)?;
    Ok(conv.and_then(|c| c.token().map(str::to_string)))
}

/// Compare and rotate under the same gate. Return the replacement only when the token matched.
pub(crate) fn revoke_agent_token_if_current(
    context: &RuntimeContext,
    key: &str,
    expected: &str,
) -> Result<Option<String>, String> {
    ConversationState::for_runtime(context)
        .update_existing(key, |conv| {
            if !conv.matches_token(expected) {
                return Ok(None);
            }
            Ok(Some(conv.rotate_token()))
        })
        .map(Option::flatten)
        .map_err(|(_, message)| message)
}

/// Return the token now in force, initializing a missing record or rotating when requested.
pub(crate) fn ensure_or_rotate_agent_token(
    context: &RuntimeContext,
    key: &str,
    rotate: bool,
) -> Result<String, String> {
    ConversationState::for_runtime(context)
        .update_or_init(key, |conv| {
            if rotate {
                Ok(conv.rotate_token())
            } else {
                Ok(conv.ensure_token())
            }
        })
        .map_err(|(_, message)| message)
}

pub(crate) fn read_active_tokens(context: &RuntimeContext) -> Result<Vec<String>, Failure> {
    Ok(ConversationState::for_runtime(context)
        .read_document()?
        .active_tokens())
}

// ---------------------------------------------------------------------------
// First-generation simulation (`social-v1.json`) state & generation
// ---------------------------------------------------------------------------

/// First-generation simulation persists its own generation alongside the participation record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ReservedGeneration(String);

impl ReservedGeneration {
    pub(crate) fn new() -> Self {
        Self(crate::turn_ledger::mint_id("v1-"))
    }

    pub(crate) fn retire(&mut self) {
        self.0 = crate::turn_ledger::mint_id("exit-");
    }
}

impl Default for ReservedGeneration {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ReservedMessage {
    pub sender: String,
    pub text: String,
    pub plain: String,
    pub quote_target_is_self: bool,
    pub is_owner: bool,
    pub media: Vec<Value>,
    pub message_id: String,
    pub event_key: String,
    pub time: i64,
}

impl ReservedMessage {
    pub(crate) fn line(&self, owner: bool) -> String {
        format!(
            "{}{}：{}{}",
            if owner && self.is_owner {
                "【管理员】"
            } else {
                ""
            },
            self.sender,
            self.text,
            crate::chat::media_hint(&self.message_id, self.media.len())
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ReservedConversation {
    pub phase: String,
    pub last_check_at: i64,
    pub next_check_at: i64,
    pub last_active_message_at: i64,
    pub active_entered_at: i64,
    pub active_deadline_at: i64,
    pub active_exit_at: i64,
    pub last_ai_reply_at: i64,
    pub probe_deadline: i64,
    pub proactive_next_check_at: i64,
    pub generation: ReservedGeneration,
    pub recent: Vec<ReservedMessage>,
    pub summary: Vec<ReservedMessage>,
    pub silent: Vec<ReservedMessage>,
}

impl Default for ReservedConversation {
    fn default() -> Self {
        Self {
            phase: "idle".into(),
            last_check_at: 0,
            next_check_at: 0,
            last_active_message_at: 0,
            active_entered_at: 0,
            active_deadline_at: 0,
            active_exit_at: 0,
            last_ai_reply_at: 0,
            probe_deadline: 0,
            proactive_next_check_at: 0,
            generation: ReservedGeneration::new(),
            recent: vec![],
            summary: vec![],
            silent: vec![],
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ReservedState {
    pub owner: String,
    pub seed: u32,
    pub console_config_stamp: String,
    pub console_social_config: Option<Value>,
    pub conversations: BTreeMap<String, ReservedConversation>,
    pub flush: BTreeMap<String, ReservedFlush>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ReservedFlush {
    pub key: String,
    pub status: String,
    pub remaining: usize,
}

pub(crate) fn reserved_document_exists(context: &RuntimeContext) -> Result<bool, String> {
    Ok(context
        .read_optional_text(RuntimeArea::Data, RESERVED_FILE)?
        .is_some())
}

pub(crate) fn read_reserved_document(context: &RuntimeContext) -> Result<ReservedState, String> {
    match context.read_optional_text(RuntimeArea::Data, RESERVED_FILE)? {
        Some(text) => serde_json::from_str(&text).map_err(|_| "reserved state is invalid".into()),
        None => Ok(ReservedState::default()),
    }
}

/// Acquire a bounded runtime data lock with retries until `SOCIAL_STATE_GATE_WAIT`.
fn acquire_file_lock<F>(
    context: &RuntimeContext,
    relative: &str,
    sleep_step: Duration,
    mut on_busy: F,
    busy_message: &str,
    error_prefix: &str,
) -> Result<File, Failure>
where
    F: FnMut(),
{
    let deadline = Instant::now() + SOCIAL_STATE_GATE_WAIT;
    loop {
        match context.open_locked_file(RuntimeArea::Data, relative) {
            Ok(file) => return Ok(file),
            Err(error) if error == "runtime file is already locked" => {
                on_busy();
                if Instant::now() >= deadline {
                    return Err((503, busy_message.to_string()));
                }
                std::thread::sleep(sleep_step);
            }
            Err(error) => return Err((503, format!("{error_prefix}: {error}"))),
        }
    }
}

/// The reserved gate follows the v2 social gate when a reset holds both.
pub(crate) fn update_reserved_document<T>(
    context: &RuntimeContext,
    update: impl FnOnce(&mut ReservedState) -> Result<T, String>,
) -> Result<T, String> {
    let _gate = acquire_file_lock(
        context,
        "locks/social-v1.lock",
        Duration::from_millis(10),
        || {},
        "reserved state gate: runtime file is already locked",
        "reserved state gate",
    )
    .map_err(|(_, msg)| msg)?;
    let mut state = read_reserved_document(context)?;
    let result = update(&mut state)?;
    let bytes = serde_json::to_vec_pretty(&state).map_err(|error| error.to_string())?;
    context.write_atomic(RuntimeArea::Data, RESERVED_FILE, &bytes)?;
    Ok(result)
}

// ---------------------------------------------------------------------------
// Document I/O, startup validation, and gate
// ---------------------------------------------------------------------------

fn read_text(context: &RuntimeContext) -> Result<String, String> {
    let path = context.resolve_path_detailed(RuntimeArea::Data, FILE, PathIntent::ReadFile)?;
    if context.resolve_path_detailed(RuntimeArea::Data, FILE, PathIntent::ReadFile)? != path {
        return Err("path changed class=leaf".into());
    }
    let bytes = fs::read(path).map_err(|e| runtime_io_error("leaf/read", &e))?;
    String::from_utf8(bytes).map_err(|_| "kind=InvalidData class=encoding".to_string())
}

pub(crate) fn validate_startup_document(path: &Path) -> Result<(), &'static str> {
    let text = fs::read_to_string(path).map_err(|_| "runtime data is unavailable")?;
    let state: Value = serde_json::from_str(&text).map_err(|_| "runtime data is invalid")?;
    if !state.is_object() {
        return Err("runtime data is invalid");
    }
    Ok(())
}

pub(crate) fn read_document(context: &RuntimeContext) -> Result<Value, String> {
    let text = read_text(context)?;
    serde_json::from_str(&text).map_err(|e| {
        format!(
            "social-state/parse kind=InvalidData line={} column={}",
            e.line(),
            e.column()
        )
    })
}

pub(crate) fn read_optional_document(context: &RuntimeContext) -> Result<Value, String> {
    match context.read_optional_text(RuntimeArea::Data, FILE)? {
        Some(text) => serde_json::from_str(&text).map_err(|e| e.to_string()),
        None => Ok(empty_document()),
    }
}

#[cfg(test)]
pub(crate) fn new_conversation(context: &RuntimeContext) -> Value {
    Conversation::new_default(context).to_value()
}

fn empty_document() -> Value {
    json!({ CONVERSATIONS: {} })
}

pub(crate) fn read_console_document(context: &RuntimeContext) -> Result<Value, Failure> {
    let mut state = read_document(context).map_err(|e| (500, format!("social-state/read: {e}")))?;
    if !state.is_object() {
        return Err((500, "runtime data is invalid".to_string()));
    }
    strip_receipts(&mut state);
    Ok(state)
}

pub(crate) fn strip_receipts(state: &mut Value) {
    if let Some(conversations) = state
        .get_mut(CONVERSATIONS)
        .and_then(Value::as_object_mut)
    {
        strip_receipts_in_map(conversations);
    }
}

pub(crate) fn write_console_document(
    context: &RuntimeContext,
    state: &Value,
) -> Result<(), String> {
    let mut state = state.clone();
    strip_receipts(&mut state);
    let text = serde_json::to_string_pretty(&state).map_err(|e| format!("序列化失败：{e}"))?;
    context.write_atomic(RuntimeArea::Data, FILE, text.as_bytes())
}

/// Social gate guard for one runtime root (`locks/social-v2.lock`).
pub(crate) struct Gate {
    _file: File,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Gate").finish_non_exhaustive()
    }
}

#[cfg(test)]
thread_local! {
    static ON_GATE_CONTENTION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Acquire `locks/social-v2.lock` with bounded retry. If `context.console_authority` is present,
/// recheck it under the gate so a request whose credential rotated while waiting commits nothing.
fn acquire_social_state_lock(context: &RuntimeContext) -> Result<File, Failure> {
    let file = acquire_file_lock(
        context,
        SOCIAL_STATE_GATE,
        Duration::from_millis(5),
        || {
            #[cfg(test)]
            ON_GATE_CONTENTION.with(|hook| {
                if let Some(notify) = hook.take() {
                    notify();
                }
            });
        },
        "selected runtime social state is busy",
        "social-state/gate",
    )?;
    if let Some(proof) = &context.console_authority {
        let cfg = context
            .load_config()
            .map_err(|_| (503, "runtime config is unavailable".to_string()))?;
        proof.recheck(context, &cfg)?;
    }
    Ok(file)
}

/// Acquire the selected runtime's social gate (`locks/social-v2.lock`).
pub(crate) fn acquire(context: &RuntimeContext) -> Result<Gate, Failure> {
    let file = acquire_social_state_lock(context)?;
    Ok(Gate { _file: file })
}

/// Acquire the social gate for the selected runtime.
pub(crate) fn acquire_config_gate(context: &RuntimeContext) -> Result<Option<Gate>, Failure> {
    acquire(context).map(Some)
}

/// Raw document read-modify-write under the social gate for callers migrating remaining
/// non-conversation-scoped or unmigrated fields.
#[cfg(test)]
pub(crate) fn update_raw<T>(
    context: &RuntimeContext,
    update: impl FnOnce(&mut Value) -> Result<T, Failure>,
) -> Result<T, Failure> {
    let _gate = acquire(context)?;
    let mut state = read_console_document(context)?;
    let result = update(&mut state)?;
    write_console_document(context, &state)
        .map_err(|error| (500, format!("social-state/write: {error}")))?;
    Ok(result)
}

#[cfg(test)]
pub(crate) fn update_selected_raw<T>(
    context: &RuntimeContext,
    update: impl FnOnce(&mut Value) -> Result<T, Failure>,
) -> Result<(Value, T), Failure> {
    let _gate = acquire(context)?;
    let mut state = read_console_document(context)?;
    let result = update(&mut state)?;
    write_console_document(context, &state)
        .map_err(|error| (500, format!("social-state/write: {error}")))?;
    Ok((state, result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::tests::runtime_fixture;
    use std::sync::mpsc;

    fn state_fixture() -> crate::runtime::tests::FixtureRoot {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../.test-fixtures")
            .join(crate::turn_ledger::mint_id("conversation-state-"));
        runtime_fixture(
            root,
            &json!({"consoleToken": "fixture-token"}),
            &json!({
                "dsh": "http://127.0.0.1:1",
                "model": "http://127.0.0.1:2",
                "onebot": "http://127.0.0.1:3"
            }),
        )
    }

    #[test]
    fn token_reads_tolerate_missing_and_malformed_fields() {
        let fixture = state_fixture();
        let context = RuntimeContext::from_root(&fixture.0).unwrap();
        update_raw(&context, |state| {
            state["conversations"] = json!({
                "group:1": {"agentToken": {"old": true}},
                "group:2": {"agentToken": ""},
                "group:3": {"agentToken": "fixture-live-token"},
                "group:4": {"custom": true},
                "group:5": {"agentToken": null},
                "group:6": false
            });
            Ok(())
        })
        .unwrap();
        let state = ConversationState::for_runtime(&context);
        for key in [
            "group:1", "group:2", "group:4", "group:5", "group:6", "group:7",
        ] {
            assert!(
                read_agent_token(&context, key).unwrap().is_none(),
                "only a nonempty string is an active token"
            );
            assert!(
                state
                    .read(key)
                    .unwrap()
                    .and_then(|c| c.generation())
                    .is_none(),
                "inactive or missing tokens must have no generation"
            );
        }
        assert_eq!(
            read_agent_token(&context, "group:3").unwrap().as_deref(),
            Some("fixture-live-token"),
            "a stored string token remains readable"
        );
        assert_eq!(
            state.read("group:3").unwrap().unwrap().generation(),
            Some(token_epoch("fixture-live-token"))
        );
    }

    #[test]
    fn token_rotation_preserves_unrelated_state() {
        let fixture = state_fixture();
        let context = RuntimeContext::from_root(&fixture.0).unwrap();
        let key = "group:1";
        update_raw(&context, |state| {
            state["custom"] = json!({"keep": true});
            state["conversations"] = json!({
                "group:1": {"agentToken": false, "custom": {"keep": [1, 2]}},
                "group:2": {"custom": "other record"}
            });
            Ok(())
        })
        .unwrap();
        let token = ensure_or_rotate_agent_token(&context, key, false).unwrap();
        assert_eq!(
            ensure_or_rotate_agent_token(&context, key, false).unwrap(),
            token,
            "an existing usable token is kept unless rotation is requested"
        );
        let replacement = revoke_agent_token_if_current(&context, key, &token)
            .unwrap()
            .unwrap();
        assert_ne!(replacement, token);
        assert_eq!(
            read_console_document(&context).unwrap(),
            json!({
                "custom": {"keep": true},
                "conversations": {
                    "group:1": {"agentToken": replacement, "custom": {"keep": [1, 2]}},
                    "group:2": {"custom": "other record"}
                }
            }),
            "token rotation must preserve all unrelated state"
        );
    }

    #[test]
    fn stale_token_cannot_revoke_replacement() {
        let fixture = state_fixture();
        let context = RuntimeContext::from_root(&fixture.0).unwrap();
        let key = "group:1";
        let token = ensure_or_rotate_agent_token(&context, key, false).unwrap();
        let replacement = ensure_or_rotate_agent_token(&context, key, true).unwrap();
        assert_ne!(replacement, token);
        assert!(
            revoke_agent_token_if_current(&context, key, &token)
                .unwrap()
                .is_none(),
            "an obsolete token must not rotate its replacement"
        );
        assert_eq!(read_agent_token(&context, key).unwrap(), Some(replacement));
    }

    #[test]
    fn token_epoch_keeps_wire_identity() {
        assert_eq!(
            token_epoch("abc"),
            "ba7816bf8f01cfea",
            "the token epoch must keep its stored SHA-256 prefix"
        );
    }

    #[test]
    fn tolerant_parsing_and_unknown_field_preservation_round_trip() {
        let fixture = state_fixture();
        let context = RuntimeContext::from_root(&fixture.0).unwrap();
        let unsafe_seq: i64 = 9_007_199_254_740_992;
        update_raw(&context, |state| {
            *state = json!({
                "topUnknown": {"nested": [1, "x"]},
                "conversations": {
                    "group:1": {
                        "agentToken": "tok-1",
                        "wakeConfig": {"mode": "diving", "infinite": true},
                        "unread": [
                            null,
                            "bad-entry",
                            {"seq": 1.0, "text": "hello", "fullText": "hello full", "customMsgField": 7},
                            {"seq": unsafe_seq, "text": "keep-unsafe"}
                        ],
                        "memberImpressions": {
                            "badScalar": "ignored-on-read",
                            "alice": {
                                "traits": [42, "calm"],
                                "interactionCount": "3",
                                "lastSeenAt": "500",
                                "customImpField": "keep-me"
                            }
                        },
                        "extraConvField": {"keep": true}
                    },
                    "group:bad": false
                }
            });
            Ok(())
        })
        .unwrap();

        let state = ConversationState::for_runtime(&context);
        assert!(state.read("group:bad").unwrap().is_none());
        let conv = state.read("group:1").unwrap().unwrap();
        assert_eq!(conv.token(), Some("tok-1"));
        assert_eq!(conv.generation(), Some(token_epoch("tok-1")));
        assert_eq!(conv.unread_count(), 2, "non-object unread entries are skipped tolerantly");
        assert_eq!(conv.unread()[0].safe_seq(), Some(1));
        assert_eq!(conv.unread()[1].safe_seq(), None);
        assert_eq!(conv.unread()[1].raw_i64_seq(), Some(unsafe_seq));
        let alice = conv.impressions().get("alice").unwrap();
        assert_eq!(alice.traits, vec!["42".to_string(), "calm".to_string()]);
        assert_eq!(alice.interaction_count, 3);
        assert_eq!(alice.last_seen_at, Some(500));
        assert!(!conv.impressions().contains_key("badScalar"));

        // Update only token: unread, impressions, and unknown fields stay untouched.
        state
            .update("group:1", |c| {
                c.set_token("tok-2");
                Ok(())
            })
            .unwrap();
        let after_token = read_console_document(&context).unwrap();
        assert_eq!(after_token["topUnknown"], json!({"nested": [1, "x"]}));
        assert_eq!(after_token["conversations"]["group:bad"], json!(false));
        assert_eq!(
            after_token["conversations"]["group:1"]["extraConvField"],
            json!({"keep": true})
        );
        assert_eq!(
            after_token["conversations"]["group:1"]["wakeConfig"],
            json!({"mode": "diving", "infinite": true})
        );
        assert_eq!(
            after_token["conversations"]["group:1"]["memberImpressions"]["alice"]["customImpField"],
            json!("keep-me")
        );

        // Update unread and impressions via typed mutators.
        state
            .update("group:1", |c| {
                c.set_field("lastUnreadSeq", json!(1));
                let acknowledged = c.acknowledge_unread(Some(1));
                assert_eq!(acknowledged, 1);
                c.note_member_seen(None, Some("alice"), 999);
                Ok(())
            })
            .unwrap();
        let after_mut = state.read("group:1").unwrap().unwrap();
        assert_eq!(after_mut.token(), Some("tok-2"));
        assert_eq!(after_mut.unread_count(), 1, "unsafe seq entry is retained");
        assert_eq!(after_mut.unread()[0].raw_i64_seq(), Some(unsafe_seq));
        let alice = after_mut.impressions().get("alice").unwrap();
        assert_eq!(alice.interaction_count, 4);
        assert_eq!(alice.last_seen_at, Some(999));
        assert_eq!(alice.extra.get("customImpField"), Some(&json!("keep-me")));
    }

    #[test]
    fn unread_and_impression_transitions_on_typed_interface() {
        let fixture = state_fixture();
        let context = RuntimeContext::from_root(&fixture.0).unwrap();
        let state = ConversationState::for_runtime(&context);
        let key = "group:10";

        let tok1 = state
            .update_or_init(key, |c| {
                let tok = c.ensure_token();
                c.push_unread(
                    UnreadMessage::from_value(&json!({
                        "seq": 1,
                        "text": "preview",
                        "fullText": "should-be-stripped-from-unread",
                        "custom": 11
                    }))
                    .unwrap(),
                    2,
                );
                c.push_unread(
                    UnreadMessage::from_value(&json!({"seq": 2, "text": "two"})).unwrap(),
                    2,
                );
                c.push_unread(
                    UnreadMessage::from_value(&json!({"seq": 3, "text": "three"})).unwrap(),
                    2,
                );
                c.impressions_mut().insert(
                    "bob".to_string(),
                    MemberImpression {
                        traits: vec!["funny".to_string()],
                        interaction_count: 1,
                        last_seen_at: Some(200),
                        extra: Map::new(),
                        ..Default::default()
                    },
                );
                Ok(tok)
            })
            .unwrap();

        let conv = state.read(key).unwrap().unwrap();
        assert_eq!(conv.token(), Some(tok1.as_str()));
        assert_eq!(conv.generation(), Some(token_epoch(&tok1)));
        assert_eq!(conv.unread_count(), 2);
        assert_eq!(conv.unread()[0].safe_seq(), Some(2));
        assert_eq!(conv.unread()[1].safe_seq(), Some(3));
        assert_eq!(conv.impressions().get("bob").unwrap().traits, vec!["funny"]);

        // Verify fullText is stripped when pushed to unread.
        state
            .update(key, |c| {
                c.push_unread(
                    UnreadMessage::from_value(&json!({
                        "seq": 4,
                        "text": "short",
                        "fullText": "long",
                        "keepTag": "yes"
                    }))
                    .unwrap(),
                    5,
                );
                c.clear_impressions();
                Ok(())
            })
            .unwrap();
        let conv = state.read(key).unwrap().unwrap();
        let last = conv.unread().last().unwrap();
        assert!(last.field("fullText").is_none());
        assert_eq!(last.field("keepTag"), Some(&json!("yes")));
        assert!(conv.impressions().is_empty());

        state
            .update(key, |c| {
                c.clear_unread();
                Ok(())
            })
            .unwrap();
        assert_eq!(state.read(key).unwrap().unwrap().unread_count(), 0);
    }

    #[test]
    fn concurrent_updates_preserve_both_changes() {
        let fixture = state_fixture();
        let context = RuntimeContext::from_root(&fixture.0).unwrap();
        let state = ConversationState::for_runtime(&context);
        let key = "group:1";
        state
            .update(key, |c| {
                c.set_field("untouched", json!(true));
                Ok(())
            })
            .unwrap();

        let (first_open, wait_for_first) = mpsc::channel();
        let (second_waiting, wait_for_second) = mpsc::channel();
        let (release_first, wait_for_release) = mpsc::channel();
        let timeout = Duration::from_secs(4);
        let (waited, first_result, second_result) = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                state.update(key, move |c| {
                    c.set_token("token-from-first");
                    c.push_unread(
                        UnreadMessage::from_value(&json!({"seq": 1, "text": "from-first"}))
                            .unwrap(),
                        10,
                    );
                    first_open.send(()).unwrap();
                    wait_for_release.recv_timeout(timeout).unwrap();
                    Ok(())
                })
            });
            wait_for_first.recv_timeout(timeout).unwrap();
            let second = scope.spawn(|| {
                ON_GATE_CONTENTION.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || second_waiting.send(()).unwrap()));
                });
                state.update(key, |c| {
                    c.impressions_mut().insert(
                        "alice".to_string(),
                        MemberImpression {
                            traits: vec!["kind".to_string()],
                            interaction_count: 1,
                            last_seen_at: Some(42),
                            extra: Map::new(),
                            ..Default::default()
                        },
                    );
                    Ok(())
                })
            });
            let waited = wait_for_second.recv_timeout(timeout);
            let _ = release_first.send(());
            (waited, first.join(), second.join())
        });
        waited.expect("the second updater reached the held social gate");
        first_result.unwrap().unwrap();
        second_result.unwrap().unwrap();

        let final_conv = state.read(key).unwrap().unwrap();
        assert_eq!(final_conv.field("untouched"), Some(&json!(true)));
        assert_eq!(final_conv.token(), Some("token-from-first"));
        assert_eq!(final_conv.unread_count(), 1);
        assert_eq!(
            final_conv.impressions().get("alice").map(|i| &i.traits),
            Some(&vec!["kind".to_string()])
        );
    }

    #[test]
    fn round_trip_preserves_insertion_order_and_omits_missing_last_seen() {
        let fixture = state_fixture();
        let context = RuntimeContext::from_root(&fixture.0).unwrap();
        let raw_json = r#"{
  "topFirst": 1,
  "paused": false,
  "conversations": {
    "group:order": {
      "firstField": "a",
      "agentToken": "tok-old",
      "middleField": "b",
      "unread": [{ "seq": 1, "text": "hi" }],
      "memberImpressions": {
        "alice": {
          "impFirst": true,
          "traits": ["calm"],
          "impMid": 2,
          "lastSeenAt": 0,
          "interactionCount": 3,
          "impLast": "end"
        },
        "bob": {
          "traits": ["quiet"],
          "interactionCount": 1
        }
      },
      "lastField": "c"
    }
  },
  "resetEpoch": 7,
  "topLast": "z"
}"#;
        let path = context.resolve_path(RuntimeArea::Data, FILE, PathIntent::WriteFile).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, raw_json).unwrap();

        let state = ConversationState::for_runtime(&context);
        state
            .update("group:order", |c| {
                c.set_token("tok-new");
                c.push_unread(
                    UnreadMessage::from_value(&json!({ "seq": 2, "text": "next" })).unwrap(),
                    10,
                );
                let alice = c.impressions().get("alice").unwrap();
                assert_eq!(alice.last_seen_at, Some(0));
                let bob = c.impressions().get("bob").unwrap();
                assert_eq!(bob.last_seen_at, None);
                c.note_member_seen(None, Some("alice"), 555);
                Ok(())
            })
            .unwrap();

        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let top_keys: Vec<&str> = written.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            top_keys,
            vec!["topFirst", "paused", "conversations", "resetEpoch", "topLast"]
        );
        let conv_obj = written["conversations"]["group:order"].as_object().unwrap();
        let conv_keys: Vec<&str> = conv_obj.keys().map(String::as_str).collect();
        assert_eq!(
            conv_keys,
            vec![
                "firstField",
                "agentToken",
                "middleField",
                "unread",
                "memberImpressions",
                "lastField",
            ]
        );
        let alice_obj = conv_obj["memberImpressions"]["alice"].as_object().unwrap();
        let alice_keys: Vec<&str> = alice_obj.keys().map(String::as_str).collect();
        assert_eq!(
            alice_keys,
            vec![
                "impFirst",
                "traits",
                "impMid",
                "lastSeenAt",
                "interactionCount",
                "impLast",
            ]
        );
        let bob_obj = conv_obj["memberImpressions"]["bob"].as_object().unwrap();
        assert!(
            !bob_obj.contains_key("lastSeenAt"),
            "missing lastSeenAt must not be synthesized as 0"
        );
    }

    #[test]
    fn reserved_field_accessors_refuse_typed_keys_without_mutating_extra() {
        let mut conv = Conversation::default();
        conv.set_token("live-token");
        conv.push_unread(UnreadMessage::from_value(&json!({ "seq": 1 })).unwrap(), 5);
        for key in [AGENT_TOKEN, UNREAD, IMPRESSIONS, MODEL_SEEN_SEQS] {
            assert!(conv.try_set_field(key, json!("evil")).is_err());
            conv.set_field(key, json!("evil"));
            assert!(conv.field(key).is_none());
            assert!(conv.remove_field(key).is_none());
        }
        assert_eq!(conv.token(), Some("live-token"));
        assert_eq!(conv.unread_count(), 1);

        let serialized = conv.to_value();
        assert_eq!(serialized[AGENT_TOKEN], json!("live-token"));
        assert_eq!(serialized[UNREAD].as_array().unwrap().len(), 1);
    }

    #[test]
    fn empty_null_and_missing_tokens_normalize_uniformly_to_none() {
        let missing = Conversation::from_value(&json!({})).unwrap();
        let null = Conversation::from_value(&json!({ "agentToken": null })).unwrap();
        let empty = Conversation::from_value(&json!({ "agentToken": "" })).unwrap();
        let wrong_type = Conversation::from_value(&json!({ "agentToken": 42 })).unwrap();

        for conv in [&missing, &null, &empty, &wrong_type] {
            assert_eq!(conv.token(), None);
            assert_eq!(conv.generation(), None);
            assert!(!conv.matches_token(""));
            assert!(!conv.matches_token("live-token"));
        }
        assert_eq!(empty.to_value()["agentToken"], json!(""));
        assert_eq!(null.to_value()["agentToken"], json!(null));
    }
}
