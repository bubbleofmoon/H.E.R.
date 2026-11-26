//! The DSH turn adapter (`turn_engine::Adapter::Dsh`): DSH sessions and their policy, model
//! selection and failover, prompt submission and the follow-stream collector, DSH cancellation,
//! usage ledger rows and the CLI fallback after an exhausted DSH chain. `pipeline.rs` keeps the
//! code every engine shares and reaches this module only through its registration points: the
//! `Adapter::Dsh` arm of `run_admitted_turn`, chat availability, login and wake parking checks,
//! and the public DSH entry points other modules call (`ensure_session`, `cancel*`, `has_dsh`,
//! `ask_with_failover`, `handle_with_failover`).

use super::*;
use crate::dependency::{Clock, Idempotency, RetryDecision, RetryPolicy, SystemClock};
use crate::dsh::{FollowStream, TurnUsage};

const ACCESS_REFUSAL_PREFIX: &str = "回合已取消：DSH 会话准入：";

pub(crate) fn is_dsh_access_refusal(error: &str) -> bool {
    error.starts_with(ACCESS_REFUSAL_PREFIX)
}

/// A local loss of session authority must bypass backend reporting and failover.
#[derive(Debug)]
pub(super) enum DshTurnError {
    AccessRefused(String),
    Failure(String),
    Exhausted { reason: String, session: String },
}

impl DshTurnError {
    fn after_exhaustion(reason: String, session: &str, policy_refused: bool) -> Self {
        if policy_refused {
            Self::Failure(reason)
        } else {
            Self::Exhausted { reason, session: session.to_owned() }
        }
    }
}

impl From<String> for DshTurnError {
    fn from(error: String) -> Self {
        Self::Failure(error)
    }
}

impl From<&str> for DshTurnError {
    fn from(error: &str) -> Self {
        Self::Failure(error.into())
    }
}

impl From<DshTurnError> for String {
    fn from(error: DshTurnError) -> Self {
        match error {
            DshTurnError::AccessRefused(reason) => format!("{ACCESS_REFUSAL_PREFIX}{reason}"),
            DshTurnError::Failure(error) | DshTurnError::Exhausted { reason: error, .. } => error,
        }
    }
}

impl Pipeline {
    fn dsh_switch_summary(&self, key: &str, sid: &str, cfg: &serde_json::Value) -> Result<Option<String>, String> {
        if !self.context.is_isolated() { return Ok(None); }
        let generation = self.turn_epochs.first().unwrap_or(&self.turn_epoch);
        let Some(summary) = crate::engine_switch::summary(&self.context, key, generation, crate::turn_engine::Engine::Dsh)? else {
            return Ok(None);
        };
        let (turns, _) = crate::turn_ledger::fold(&crate::turn_ledger::read_records(&self.context)?);
        if turns.iter().any(|turn| turn.key == key && turn.epochs.contains(generation)
            && turn.attempts.iter().any(|attempt| attempt.session_id == sid && attempt.accepted)) {
            return Ok(None);
        }
        Ok(crate::engine_switch::safe_text(&self.context, cfg,
            &format!("{}\n{summary}", crate::harness::handover::SUMMARY_LABEL), 2_100))
    }

    /// The DSH adapter's admitted turn. Reserved2 acquires its login here, after any pinned CLI.
    pub(super) async fn run_dsh_turn(
        &mut self,
        key: &str,
        prompt: &str,
        timeout: std::time::Duration,
        cfg: &serde_json::Value,
    ) -> Result<String, String> {
        if !self.has_dsh() {
            if self.reserved2_budget.is_none() {
                return Err(crate::dsh_credential::UNBOUND.to_string());
            }
            if crate::dsh_health::is_down(&self.context) {
                return Err("DSH is unavailable for the wake".into());
            }
            self.rpc = crate::dsh::rpc_for(&self.context, cfg)?;
        }
        // A pinned CLI backend was the caller's to run: the DSH adapter's configuration
        // copy drops `backends.sessions`, so its failover never runs that CLI again.
        let mut cfg_dsh = cfg.clone();
        if let Some(b) = cfg_dsh.get_mut("backends").and_then(|v| v.as_object_mut()) {
            b.remove("sessions");
        }
        let result = self.ask_with_failover_with_deadline(key, prompt, timeout, &cfg_dsh).await;
        if let Err(DshTurnError::Exhausted { session, .. }) = &result {
            self.send_persona_failure_line(key, Some(session));
        }
        result.map_err(String::from)
    }

    fn preset_for(&self, key: &str) -> String {
        if self.context.is_isolated()
            && crate::social_state::mode(&self.context) == "closed-agent"
        {
            return self.preset_closed.clone();
        }
        // Chat mode has one preset for every conversation (JS `modePreset`).
        if self.context.is_isolated()
            && matches!(
                crate::social_state::mode(&self.context).as_str(),
                "chat" | "reserved"
            )
        {
            return self.preset_chat.clone();
        }
        let owner_key = format!("private:{}", self.owner_qq);
        if !self.owner_qq.is_empty() && key == owner_key {
            self.preset_owner.clone()
        } else {
            self.preset_group.clone()
        }
    }

    fn ensure_workspace(&mut self) -> Result<String, String> {
        if self.context.is_isolated() {
            if let Some(id) = crate::session_map::workspace_id(&self.context)? {
                self.store.workspace_id = id.clone();
                return Ok(id);
            }
        } else if !self.store.workspace_id.is_empty() {
            return Ok(self.store.workspace_id.clone());
        }
        if self.context.is_isolated() {
            let selected = self
                .context
                .resolve_path(RuntimeArea::Data, "agents", PathIntent::CreateDirectory)
                .map_err(str::to_string)?;
            if selected.to_string_lossy() != self.workspace_dir {
                return Err("候选 workspace path changed after initialization".to_string());
            }
        } else {
            let _ = std::fs::create_dir_all(&self.workspace_dir);
        }
        let ws = self
            .rpc
            .call("workspace/create", serde_json::json!({ "path": self.workspace_dir }))?;
        let id = ws
            .get("workspace")
            .and_then(|w| w.get("workspaceId"))
            .and_then(|i| i.as_str())
            .ok_or_else(|| format!("workspace/create 没返回 workspaceId：{ws}"))?
            .to_string();
        self.store.workspace_id = id.clone();
        if self.context.is_isolated() {
            crate::session_map::set_workspace_id(&self.context, &id)?;
        } else {
            self.store.save_for_runtime(&self.context)?;
        }
        Ok(id)
    }

    /// 取（或新建）某个 QQ 会话对应的 DSH sessionId。
    pub(super) fn ensure_session(&mut self, key: &str) -> Result<String, DshTurnError> {
        if key.starts_with("ab:") {
            return Err("shadow isolation refused: stored shadow sessions cannot use the live session path".into());
        }
        // JS `ensureSession`: the access epoch seen before DSH creates a session must still hold when
        // it is bound; a revocation, reset or mode change in between archives the new session.
        let epoch = if self.context.is_isolated() { Some(self.access_epoch(key)) } else { None };
        if epoch.as_ref().is_some_and(|e| !matches!(e.allowed, Ok(true))) {
            return Err(DshTurnError::AccessRefused(format!("当前运行模式未放行会话 {key}")));
        }
        if self.context.is_isolated() {
            // Fresh read: the console may have reset or replaced this mapping since the last turn.
            match crate::session_map::lookup(&self.context, key)? {
                Some(sid) => {
                    // A session made under another mode must not carry this mode's messages
                    // (JS `isCurrentSession` policy check): retire it and start a new one.
                    if let Some(reason) = self.stale_policy(key)? {
                        return self.replace_session(key, &sid, reason);
                    }
                    self.store.sessions.insert(
                        key.to_string(),
                        SessionRef { session_id: sid.clone(), workspace_id: self.store.workspace_id.clone() },
                    );
                    return Ok(sid);
                }
                None => {
                    self.store.sessions.remove(key);
                }
            }
        } else if let Some(s) = self.store.sessions.get(key) {
            if !s.session_id.is_empty() {
                return Ok(s.session_id.clone());
            }
        }
        let ws_id = self.ensure_workspace()?;
        let preset = self.preset_for(key);
        // A conversation of a QQ group must never fall back to DSH's default preset (it carries local
        // tools): chat refuses to create a session unless DSH offers the configured preset.
        if self.context.is_isolated()
            && matches!(
                crate::social_state::mode(&self.context).as_str(),
                "chat" | "reserved"
            )
        {
            self.require_preset(&preset)?;
        }
        let access_changed = |pipe: &Self| epoch.as_ref().is_some_and(|before| !pipe.access_epoch(key).still(before));
        let refused = || DshTurnError::AccessRefused(format!("创建对话过程中 {key} 发生重置或权限变动"));
        if access_changed(self) {
            return Err(refused());
        }
        let create = |rpc: &crate::dsh::Rpc| {
            let mut request = serde_json::json!({ "workspaceId": ws_id });
            if !preset.is_empty() { request["agentPreset"] = serde_json::json!(preset); }
            rpc.call(
                "session/create",
                request,
            )
        };
        // 401 = DSH 换了签名密钥（重启）。重读密钥 + 重新铸 cookie 再试一次 ——
        // 不这么做的话，DSH 每重启一次，Rust 就得人工重启才能恢复。
        let created = match create(&self.rpc) {
            Ok(v) => v,
            Err(_) if access_changed(self) => return Err(refused()),
            Err(e) if e.contains("401") => {
                println!("[dsh] 建联接口遇 401，重载本地凭证后再重投一次");
                self.rpc.relogin().map_err(|re| format!("{e}；重新登录也失败：{re}"))?;
                if access_changed(self) {
                    return Err(refused());
                }
                match create(&self.rpc) {
                    Ok(created) => created,
                    Err(_) if access_changed(self) => return Err(refused()),
                    Err(error) => return Err(error.into()),
                }
            }
            Err(e) => return Err(e.into()),
        };
        let sid = created
            .get("sessionId")
            .and_then(|s| s.as_str())
            .ok_or_else(|| format!("session/create 没返回 sessionId：{created}"))?
            .to_string();

        if access_changed(self) {
            return Err(DshTurnError::AccessRefused(self.discard_created_session(key, &sid, false)));
        }
        if self.context.is_isolated()
            && matches!(
                crate::social_state::mode(&self.context).as_str(),
                "chat" | "reserved"
            )
        {
            // Chat: the JS title (`renameSessionForUi`) is resolved after the model is selected and
            // set once the first prompt is in DSH's hands.
            self.title_for = Some((sid.clone(), key.to_string()));
        } else {
            // 起个好认的名字，方便在 DSH 里区分是 Rust 建的
            let _ = self.rpc.call(
                "workspace/rename",
                serde_json::json!({ "sessionId": sid, "title": format!("Rust · {key}") }),
            );
        }

        if access_changed(self) {
            return Err(DshTurnError::AccessRefused(self.discard_created_session(key, &sid, false)));
        }
        self.store.sessions.insert(
            key.to_string(),
            SessionRef { session_id: sid.clone(), workspace_id: ws_id },
        );
        if self.context.is_isolated() {
            let policy = self.session_policy(key);
            if let Err(error) = crate::session_map::bind(&self.context, key, &sid, Some(&policy)) {
                // Never leave a created session running unbound in DSH.
                let discarded = self.discard_created_session(key, &sid, false);
                return Err(format!("绑定新会话失败：{error}；{discarded}").into());
            }
        } else {
            self.store.save_for_runtime(&self.context)?;
        }
        // A change between the check and the binding retires the mapping again (JS re-checks after
        // `saveState`).
        if access_changed(self) {
            return Err(DshTurnError::AccessRefused(self.discard_created_session(key, &sid, true)));
        }
        Ok(sid)
    }

    /// What a session's authority depends on: mode file, current allowance, the ledger's reset
    /// count, the conversation token and the session policy.
    fn access_epoch(&self, key: &str) -> AccessEpoch {
        let mode = crate::social_state::mode(&self.context);
        AccessEpoch {
            mode_stamp: crate::reserved::mode_stamp(&self.context),
            allowed: self
                .context
                .load_config()
                .map(|cfg| {
                    if mode == "reserved2" {
                        crate::social_state::inbound_admitted(&cfg, key)
                    } else {
                        crate::social_state::access_allowed(&cfg, &mode, key)
                    }
                })
                .map_err(str::to_string),
            resets: crate::turn_ledger::reset_count(&self.context, key),
            token: crate::social_state::token(&self.context, key).map_err(String::from),
            policy: self.session_policy(key),
        }
    }

    /// A session created across an access change is never used: unbind it if it was bound, archive
    /// it in DSH and refuse the turn.
    fn discard_created_session(&mut self, key: &str, sid: &str, bound: bool) -> String {
        let mut failures = Vec::new();
        if bound {
            if let Err(error) = crate::session_map::retire_if(&self.context, key, sid) {
                failures.push(format!("解除映射失败：{error}"));
            }
        }
        self.store.sessions.remove(key);
        if let Err(error) = crate::dsh::archive_session(&self.rpc, sid) {
            failures.push(format!("归档新会话 {sid} 失败：{error}"));
        }
        println!("[session] 创建对话过程中 {key} 发生重置或权限变动，放弃新建会话 {sid}");
        if failures.is_empty() {
            "创建对话过程中发生重置或权限变动，放弃新建会话".to_string()
        } else {
            println!("[session] ⚠️ 丢弃新会话 {sid} 未完成：{}", failures.join("；"));
            format!("创建对话过程中发生重置或权限变动，放弃新建会话；{}", failures.join("；"))
        }
    }

    /// JS `resolvePresetName(strict)`: the preset must be in DSH's list; an unknown list refuses too.
    fn require_preset(&self, preset: &str) -> Result<(), String> {
        let listed = self.rpc.call("agentPresets/list", serde_json::json!({}))?;
        let ids: Vec<String> = listed
            .get("presets")
            .and_then(|p| p.as_array())
            .map(|items| items.iter().filter_map(|p| p.get("id").and_then(|i| i.as_str()).map(str::to_string)).collect())
            .unwrap_or_default();
        if !preset.is_empty() && ids.iter().any(|id| id == preset) {
            return Ok(());
        }
        Err(format!(
            "拒建会话：chat 模式要求预设 \"{preset}\"，未在 DSH 列表（{}）中查到；若退回默认预设会将本机工具暴露到 QQ 群",
            ids.join(", ")
        ))
    }

    pub(super) fn closed_preset(&self) -> Result<String, String> {
        let local = self.context.read_optional_text(RuntimeArea::Data, "mode.json")?
            .map(|s| serde_json::from_str::<serde_json::Value>(&s).map_err(|e| e.to_string()))
            .transpose()?.unwrap_or_default();
        let wanted = local.get("closedAgentPreset").and_then(|v| v.as_str()).unwrap_or("").trim();
        let settings = self.rpc.call("settings/describe", serde_json::json!({}))?;
        let default = settings.get("namespaces").and_then(|v| v.as_array()).and_then(|ns| ns.iter().find(|n| n["ns"] == "agent-presets"))
            .and_then(|n| n.get("user").and_then(|v| v.get("default")).or_else(|| n.get("value").and_then(|v| v.get("default"))))
            .and_then(|v| v.as_str()).unwrap_or("").to_string();
        let listed = self.rpc.call("agentPresets/list", serde_json::json!({}))?;
        let presets = listed.get("presets").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let default = if default.is_empty() { presets.iter().find(|p| p["isDefault"] == true).and_then(|p| p["id"].as_str()).unwrap_or("").to_string() } else { default };
        if wanted.is_empty() { return Ok(default); }
        if presets.is_empty() || presets.iter().any(|p| p["id"].as_str() == Some(wanted)) { Ok(wanted.to_string()) } else { Ok(default) }
    }

    /// Resolve the title of a freshly created session (group name through OneBot).
    fn prepare_title(&mut self) {
        let Some((sid, key)) = self.title_for.take() else { return };
        let group_name = key
            .strip_prefix("group:")
            .map(|id| {
                self.ob
                    .request("get_group_list", serde_json::json!({}))
                    .and_then(|v| v.get("data").cloned())
                    .and_then(|list| {
                        list.as_array().and_then(|items| {
                            items
                                .iter()
                                .find(|g| g.get("group_id").map(|v| v.to_string().trim_matches('"').to_string()).as_deref() == Some(id))
                                .and_then(|g| g.get("group_name").and_then(|n| n.as_str()).map(str::to_string))
                        })
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        self.pending_title = Some((sid, crate::chat::session_title(&key, &group_name)));
    }

    /// Send the pending session title (failure never matters).
    fn flush_title(&mut self) {
        if let Some((sid, title)) = self.pending_title.take() {
            let _ = self.rpc.call("session/rename", serde_json::json!({ "sessionId": sid, "title": title }));
        }
    }

    /// Cancel the turn of one known session (a scheduler cleanup after a reset or mode change):
    /// stop its DSH work; only while it is still the conversation's mapping does the conversation
    /// lose its token and every open turn (as `cancel`). A session that a newer one replaced only
    /// has its own open turns ended: the replacement keeps running.
    pub fn cancel_session(&mut self, key: &str, sid: &str) -> Result<(), String> {
        let stopped = crate::dsh::stop_session_work(&self.rpc, sid);
        if crate::session_map::lookup(&self.context, key)?.as_deref() == Some(sid) {
            crate::social_state::ensure_or_rotate(&self.context, key, true)?;
        }
        let current = crate::session_map::lookup(&self.context, key)?;
        if let Ok(records) = crate::turn_ledger::read_records(&self.context) {
            let (turns, _) = crate::turn_ledger::fold(&records);
            for turn in turns.iter().filter(|t| {
                t.key == key && t.state.is_open() && (t.session_id == sid || (t.session_id.is_empty() && current.is_none()))
            }) {
                let _ = crate::turn_ledger::Recorder::attach(&self.context, &turn.id, key).without_retry().end("cancelled", "superseded");
            }
        }
        stopped.map(|_| ()).map_err(|e| format!("取消回合失败：{e}"))
    }

    /// 取消这个会话正在跑的回合（对应 DSH 的 `session/cancel`）。
    ///
    /// 用途：新来的是确定性触发（@ / 关键词 / 提问），而当前回合还没说出口、
    /// 又已经跑够久 —— 与其让 @ 排队 3 分钟（JS 那边实测 10:58→11:01），
    /// 不如把这一轮作废，用新消息重开。已经说过话的回合不取消（见 wake.rs 的判定）。
    pub fn cancel(&mut self, key: &str) -> Result<(), String> {
        match self.cancel_with_reason(key, "superseded")?.stop_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Report exactly the epoch minted by this cancellation so the scheduler can retain
    /// buffered work without adopting a later reset or revocation's token. A failed DSH stop is
    /// reported beside the minted epoch, never instead of it.
    pub fn cancel_with_reason(&mut self, key: &str, reason: &str) -> Result<Cancelled, String> {
        self.cancel_with_dispatch(key, reason, None)
    }

    pub(crate) fn cancel_reserved2_cap(
        context: &RuntimeContext, cfg: &serde_json::Value, key: &str, dispatch: Option<(&str, &MintedEpoch)>,
    ) -> Result<Cancelled, String> {
        let mut control = Self::new_without_dsh(context, cfg, true)?;
        control.cancel_with_dispatch(key, "turnTimeout", dispatch)
    }

    fn cancel_with_dispatch(
        &mut self, key: &str, reason: &str, dispatch: Option<(&str, &MintedEpoch)>,
    ) -> Result<Cancelled, String> {
        let sid = if self.context.is_isolated() {
            crate::session_map::lookup(&self.context, key)?
        } else {
            self.store.sessions.get(key).map(|r| r.session_id.clone())
        };
        let Some(sid) = sid else {
            if let Some((expected, publication)) = dispatch {
                // An unmapped first wake can still be waiting for the turn gate.
                let mut minted = publication.lock().unwrap_or_else(|p| p.into_inner());
                let current = minted.as_deref().unwrap_or(expected);
                let token = crate::social_state::revoke_if_current(&self.context, key, current)?;
                if let Some(replacement) = &token {
                    *minted = Some(replacement.clone());
                }
                return Ok(Cancelled { token, stop_error: None });
            }
            return Ok(Cancelled::default());
        };
        if reason == "turnTimeout" && self.context.is_isolated() && !self.has_dsh() {
            let cfg = self.context.load_config().map_err(str::to_string)?;
            self.rpc = crate::dsh::rpc_for(&self.context, &cfg)?;
        }
        if self.context.is_isolated() {
            // 被取消/被替代的回合不能再有迟到的结果：先清 inbox（DSH 的 cancel 保留队列）并取消，
            // 再换令牌，之后它的工具调用与发送都因令牌不符被拒；账本里把它记为已取消。
            // A diagnostic intent preserves the reason if DSH ends before cancel returns.
            // It does not finalize the turn: crash recovery must still reconcile live work.
            let open = crate::turn_ledger::read_records(&self.context).ok()
                .map(|records| crate::turn_ledger::fold(&records).0).unwrap_or_default();
            let pending_wait = crate::social_state::observation_wait_turn(&self.context, key)
                .unwrap_or(None);
            let sleeping = reason == "turnTimeout" && open.iter().any(|t|t.key == key && t.state.is_open()
                && pending_wait.as_deref() == Some(t.id.as_str()));
            let terminal_reason = if sleeping { "observationSleep" } else { reason };
            for turn in open.iter().filter(|t| t.key == key && t.state.is_open()) {
                if crate::turn_ledger::Recorder::attach(&self.context, &turn.id, key).without_retry()
                    .verified("cancel-requested", terminal_reason).is_err() {
                    eprintln!("[turn] cancellation intent write failed; enforcing stop and revocation; recovery required");
                }
            }
            let stopped = crate::dsh::stop_session_work(&self.rpc, &sid);
            let token = crate::social_state::ensure_or_rotate(&self.context, key, true)?;
            for turn in open.iter().filter(|t| t.key == key && t.state.is_open()) {
                let recorder = crate::turn_ledger::Recorder::attach(&self.context, &turn.id, key).without_retry();
                let result = if sleeping && stopped.is_ok() {
                    recorder.end_cancellation("completed", terminal_reason)
                } else if sleeping {
                    recorder.unknown("dsh-unreachable", "observation stop unconfirmed")
                } else { recorder.end_cancellation("cancelled", terminal_reason) };
                if result.is_err() { eprintln!("[turn] terminal ledger write failed; recovery required"); }
            }
            return Ok(Cancelled { token: Some(token), stop_error: stopped.err().map(|e| format!("取消回合失败：{e}")) });
        }
        match self.rpc.call(
            "session/cancel",
            serde_json::json!({ "sessionId": sid }),
        ) {
            Ok(_) => Ok(Cancelled::default()),
            Err(e) => Err(format!("取消回合失败：{e}")),
        }
    }

    /// Mode retirement already revoked the old epoch. Stale driver cleanup must
    /// never resolve the new mapping or rotate a replacement turn's token.
    pub fn cancel_reserved_epoch(&self, key: &str, token: &str) -> Result<(), String> {
        let epoch = crate::send_outbox::token_epoch(token);
        let (turns, _) =
            crate::turn_ledger::fold(&crate::turn_ledger::read_records(&self.context)?);
        let mut sessions = std::collections::BTreeSet::new();
        for turn in turns
            .iter()
            .filter(|t| t.key == key && t.epochs.contains(&epoch) && t.state.is_open())
        {
            if !turn.session_id.is_empty() {
                sessions.insert(turn.session_id.clone());
            }
            crate::turn_ledger::Recorder::attach(&self.context, &turn.id, key).without_retry()
                .end("cancelled", "reserved mode retired")?;
        }
        for sid in sessions {
            crate::dsh::stop_session_work(&self.rpc, &sid)?;
        }
        Ok(())
    }

    fn note_session(&self, sid: &str) {
        if let Some(turn) = &self.cur_turn {
            let _ = turn.session(sid);
        }
        if let Some(slot) = &self.session_slot {
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(sid.to_string());
        }
    }
    /// Whether the pipeline holds a DSH login.
    pub fn has_dsh(&self) -> bool { self.rpc.credential().is_bound() }

    pub(super) fn prepare_dsh_chat(&mut self, cfg: &serde_json::Value, mode: &str) -> Result<bool, String> {
        if crate::dsh_health::is_down(&self.context) {
            return Ok(false);
        }
        if !self.has_dsh() {
            self.rpc = crate::dsh::rpc_for(&self.context, cfg)?;
        }
        let _ = self.rpc.relogin();
        if mode == "closed-agent" { self.preset_closed = self.closed_preset()?; }
        Ok(true)
    }

    pub(super) fn unsubmitted_dsh_deferred(&self) -> bool {
        self.dispatch_definitely_unsubmitted() && crate::dsh_health::is_down(&self.context)
    }

    pub(super) fn defer_unsubmitted_dsh_chat(&self, key: &str, cfg: &serde_json::Value) -> bool {
        let down = self
            .chat_failure_defers
            .unwrap_or_else(|| crate::dsh_health::is_down(&self.context));
        if !self.dispatch_definitely_unsubmitted() || !down {
            return false;
        }
        crate::admin_alert::notify_notice(
            &self.context, cfg,
            &format!("[{key}] DSH 不可用，消息已排队，恢复后会重新核对权限再补投"), "turn-failure",
        );
        true
    }

    pub(super) fn reserved2_wake_parks(&self, cfg: &serde_json::Value, key: &str) -> bool {
        crate::dsh_health::is_down(&self.context)
            && crate::wake_needs_dsh(&self.context, cfg, key)
    }

    /// 作废当前这次尝试：先清 inbox 并取消 DSH 里的回合，再换令牌。
    ///
    /// Returns the reason when DSH's work could not be stopped: then nothing proves the abandoned attempt is dead.
    fn abandon_attempt(&mut self, key: &str, sid: &str, why: &str) -> Result<(), String> {
        let stopped = crate::dsh::stop_session_work(&self.rpc, sid).map(|_| ());
        if let Err(error) = &stopped {
            println!("[turn] ⚠️ 停止 DSH 回合失败 {key}：{error}");
        }
        if let Err(error) = self.rotate_turn_token(key) {
            println!("[turn] ⚠️ 换令牌失败 {key}：{error}");
        }
        if stopped.is_ok() {
            if let Some(turn) = &self.cur_turn {
                let _ = turn.verified("attempt-abandoned", why);
            }
        }
        stopped
    }

    #[cfg(test)]
    pub(crate) fn abandon_attempt_for_test(&mut self, key: &str, sid: &str, after_rotation: impl FnOnce() + Send + 'static) -> Result<(), String> {
        self.after_abandon_rotation = Some(Box::new(after_rotation));
        self.abandon_attempt(key, sid, "fixture collector timeout")
    }

    /// JS 的会话策略串：`JSON.stringify([mode, preset, closed-agent ? ownerQQ : null])`。
    /// 一并写入映射，回滚到 JS 后它认得这些会话而不是整体退役。未解析的 preset 与 JS 一样序列化为 `""`。
    fn session_policy(&self, key: &str) -> String {
        let mode = crate::social_state::mode(&self.context);
        let owner = if mode == "closed-agent" {
            match self.owner_qq.parse::<i64>() {
                Ok(n) => serde_json::json!(n),
                Err(_) => serde_json::json!(self.owner_qq),
            }
        } else {
            serde_json::Value::Null
        };
        let preset = self.preset_for(key);
        serde_json::json!([mode, preset, owner]).to_string()
    }

    /// The stored session policy names another mode than the current one (or, in chat mode, another
    /// preset): the session was made for different rules and must be replaced. `None` when there is
    /// no stored policy (older records) or it still fits. Reserved2 compares the mode only, because
    /// its preset depends on DSH's preset list.
    fn stale_policy(&self, key: &str) -> Result<Option<&'static str>, String> {
        let doc = crate::session_map::read(&self.context)?;
        let Some(stored) = doc.get("sessionPolicies").and_then(|p| p.get(key)).and_then(|p| p.as_str()) else {
            return Ok(None);
        };
        let mode = crate::social_state::mode(&self.context);
        let stored_mode = serde_json::from_str::<serde_json::Value>(stored)
            .ok()
            .and_then(|v| v.get(0).and_then(|m| m.as_str()).map(str::to_string));
        if stored_mode.as_deref() != Some(mode.as_str()) {
            return Ok(Some("mode-changed"));
        }
        if matches!(mode.as_str(), "chat" | "reserved" | "closed-agent") && stored != self.session_policy(key) {
            return Ok(Some("policy-changed"));
        }
        Ok(None)
    }

    /// 会话明确失效（DSH 不认它了）：撤销映射、停掉/归档旧会话、作废旧令牌、换新会话。
    fn replace_session(&mut self, key: &str, old: &str, reason: &str) -> Result<String, DshTurnError> {
        crate::session_map::retire_if(&self.context, key, old)?;
        let _ = crate::dsh::stop_session_work(&self.rpc, old);
        let _ = crate::dsh::archive_session(&self.rpc, old);
        self.rotate_turn_token(key)?;
        let _ = crate::social_state::mark_unbootstrapped(&self.context, key);
        self.store.sessions.remove(key);
        self.failover_state().forget_session(old);
        let new = self.ensure_session(key)?;
        crate::turn_ledger::record_replacement(&self.context, key, old, &new, reason)?;
        self.note_session(&new);
        println!("[session] {key} 的 DSH 会话 {old} 已失效（{reason}），换用 {new}");
        Ok(new)
    }

    pub(super) fn failover_state(&self) -> std::sync::MutexGuard<'_, FailoverState> {
        self.failover.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Give the session its model, once per session (JS `ensureChatModel`/`modelAppliedSessions`).
    ///
    /// The selection is applied again only when this session has none yet, after a failover step, or when the configured
    /// chain changed. It starts at the sticky candidate (the one a failed turn fell back to), tries each candidate under
    /// `RetryPolicy::SELECT_MODEL` (two attempts, one second apart, as JS) and moves on to the next; a 401 re-reads the
    /// credentials once per call without using up an attempt. Returns the candidate DSH now has selected.
    pub(super) fn apply_model(
        &mut self,
        cfg: &serde_json::Value,
        key: &str,
        sid: &str,
        chain: &[failover::Candidate],
    ) -> Result<usize, String> {
        let signature = chain
            .iter()
            .map(|c| format!("{}/{}/{}", c.provider, c.model, c.effort))
            .collect::<Vec<_>>()
            .join("|");
        let start = {
            let mut state = self.failover_state();
            state.note_chain(sid, &signature);
            if !state.needs_apply(sid) {
                return Ok(state.selected_index(sid, chain.len()));
            }
            state.start_index(sid, chain.len())
        };
        let clock = SystemClock;
        let mut last_err = String::new();
        let mut relogged = false;
        for idx in start..chain.len() {
            let cand = &chain[idx];
            self.active_candidate = Some(cand.clone());
            let mut retry = RetryPolicy::SELECT_MODEL.start(clock.now_ms());
            loop {
                let attempt = retry.failures() + 1;
                match self.rpc.call(
                    "session/selectModel",
                    serde_json::json!({
                        "sessionId": sid,
                        "provider": cand.provider,
                        "model": cand.model,
                        "reasoningEffort": cand.effort
                    }),
                ) {
                    Ok(_) => {
                        self.failover_state().mark_applied(sid, idx);
                        if idx > 0 {
                            println!("[failover] 已降级到候选 {}/{}：{}", idx + 1, chain.len(), cand.label());
                            recovery_log::record(
                                &self.context,
                                Trigger::Turn,
                                Component::ModelChain,
                                "degraded",
                                Outcome::Ok,
                                &format!("{key} runs on candidate {}/{}: {}", idx + 1, chain.len(), cand.label()),
                            );
                        }
                        return Ok(idx);
                    }
                    Err(e) => {
                        last_err = format!("{}：{e}", cand.label());
                        println!(
                            "[failover] 设置模型失败 {}（第 {}/{} 次）：{}",
                            cand.label(),
                            attempt,
                            RetryPolicy::SELECT_MODEL.max_attempts,
                            crate::activity::redact(&self.context, &e)
                        );
                        recovery_log::record(
                            &self.context,
                            Trigger::Turn,
                            Component::ModelChain,
                            "select-model-failed",
                            Outcome::Failed,
                            &format!("{}: {e}", cand.label()),
                        );
                        crate::dsh_health::note_call_failure(&self.context, cfg, &e);
                        // 401 = DSH 换了签名密钥（重启）：重读密钥后再试同一个候选，一次调用只做一次，不占尝试次数。
                        if e.contains("401") && !relogged {
                            relogged = true;
                            println!("[failover] 命中 401，重读密钥后重试");
                            match self.rpc.relogin() {
                                Ok(()) => continue,
                                Err(re) => last_err = format!("{}：{e}；重新登录也失败：{re}", cand.label()),
                            }
                        }
                        let now = clock.now_ms();
                        match retry.on_failure(now, Idempotency::Safe, crate::dependency::jitter_unit(now, idx as u64)) {
                            RetryDecision::Retry { after_ms, .. } => {
                                std::thread::sleep(std::time::Duration::from_millis(after_ms));
                            }
                            RetryDecision::GiveUp(_) => break,
                        }
                    }
                }
            }
            println!("[failover] {} 暂不可达，切换下一备选路由", cand.label());
        }
        recovery_log::record(
            &self.context,
            Trigger::Turn,
            Component::ModelChain,
            "chain-exhausted-at-select",
            Outcome::Failed,
            &format!("{key}: no candidate accepted the selection: {last_err}"),
        );
        Err(format!("所有候选模型都设置失败：{last_err}"))
    }

    /// 带 failover 的一轮：选模型 → 投 prompt → 失败换候选重投一次。
    ///
    /// **只在还没发出任何字节时重投**：这里发送发生在回合成功之后，
    /// 所以重投不会重复发言；一旦将来改成流式边说边发，这个前提就得重新审。
    #[cfg(test)]
    pub async fn ask_with_failover(
        &mut self,
        key: &str,
        prompt: &str,
        timeout_secs: u64,
        cfg: &serde_json::Value,
    ) -> Result<String, String> {
        self.ask_with_failover_with_deadline(key, prompt, std::time::Duration::from_secs(timeout_secs), cfg)
            .await.map_err(String::from)
    }

    async fn ask_with_failover_with_deadline(
        &mut self,
        key: &str,
        prompt: &str,
        timeout: std::time::Duration,
        cfg: &serde_json::Value,
    ) -> Result<String, DshTurnError> {
        if !self.has_dsh() {
            return Err(crate::dsh_credential::UNBOUND.into());
        }
        self.failure_reported = false;
        self.refusals_recorded.clear();
        self.active_candidate = None;
        let result = self.ask_with_failover_inner(key, prompt, timeout, cfg).await;
        if let Err(DshTurnError::Failure(error) | DshTurnError::Exhausted { reason: error, .. }) = &result {
            if !self.failure_reported && !is_cancellation_error(error)
                && error.as_str() != crate::prompt::WAKE_BRIEFING_CHANGED
                && !error.contains("preset") && !error.contains("预设")
                // #179: a guard refusal (here a session creation it stopped) is the guard's own alert, not a model failure.
                && !error.contains(crate::dsh_profile::REFUSAL_PREFIX) {
                let candidate = self.active_candidate.clone().unwrap_or_else(|| failover::model_chain_for(cfg, key)[0].clone());
                self.report_backend_failure(cfg, key, "dsh", &candidate.provider, &candidate.model, "", error);
            }
        }
        // A failure to reach DSH marks it down (queued work waits for recovery); a call that went through marks it up.
        match &result {
            Ok(_) => crate::dsh_health::note_call_success(&self.context, cfg),
            Err(DshTurnError::Failure(error) | DshTurnError::Exhausted { reason: error, .. }) =>
                crate::dsh_health::note_call_failure(&self.context, cfg, error),
            Err(DshTurnError::AccessRefused(_)) => {}
        }
        result
    }

    pub(super) fn cli_fallback(&mut self, cfg: &serde_json::Value, key: &str, prompt: &str, timeout_secs: u64) -> Result<(String, String), (String, bool)> {
        let context = self.context.clone();
        let mut policy_refused = false;
        let result = crate::backend::cli_fallback_with_runtime_reporting(&context, cfg, key, prompt, timeout_secs, |id, error| {
            policy_refused |= crate::model_failure::Failure::classify("", error).class == "refusal";
            let (provider, model) = crate::model_failure::cli_identity(cfg, id);
            self.report_backend_failure(cfg, key, id, &provider, &model, "", error);
        });
        // The backend took the compatibility render: its recall was delivered.
        if result.is_ok() {
            crate::prompt::record_recall_hits(&self.context, &self.compat_recall);
        }
        result.map_err(|error| (error, policy_refused))
    }

    /// JS `shouldAuditKey`: every conversation is audited except the owner's private chat in closed-agent mode.
    fn should_audit_key(&self, cfg: &serde_json::Value, key: &str) -> bool {
        !(crate::social_state::mode(&self.context) == "closed-agent"
            && crate::chat::is_owner_key(cfg, key))
    }

    /// Redact outbound sensitive text when the conversation requires audit masking.
    fn hide_if_sensitive(&self, cfg: &serde_json::Value, key: &str, text: &str) -> String {
        let guard_cfg = self.context.load_config().unwrap_or_else(|_| cfg.clone());
        if self.should_audit_key(cfg, key) && configured_outbound_violation(&guard_cfg, text, &[]).is_some() {
            "（涉及敏感内容，已遮蔽）".to_string()
        } else {
            text.to_string()
        }
    }

    /// Retain the local activity/recovery diagnostics; the shared failure reporter owns the private alert.
    fn report_turn_error(&mut self, cfg: &serde_json::Value, key: &str, message: &str) {
        let safe = self.hide_if_sensitive(cfg, key, &crate::activity::slice(message, 500));
        println!("[turn] ⚠️ 处理回合发生故障 ({key})：{}", crate::activity::redact(&self.context, &safe));
        crate::activity::append(&self.context, &format!("{key} agent 处理出错：{}", crate::activity::slice(&safe, 80)));
        recovery_log::record(
            &self.context,
            Trigger::Turn,
            Component::ModelChain,
            "turn-error",
            Outcome::Failed,
            &format!("{key}: {safe}"),
        );
    }

    async fn ask_with_failover_inner(
        &mut self,
        key: &str,
        prompt: &str,
        timeout: std::time::Duration,
        cfg: &serde_json::Value,
    ) -> Result<String, DshTurnError> {
        // CLI adapters accept seconds; the DSH collector retains the exact millisecond budget.
        let timeout_secs = timeout.as_secs().saturating_add(u64::from(timeout.subsec_nanos() > 0));
        if key.starts_with("ab:") {
            return Err("shadow isolation refused: shadow cannot use live or CLI failover".into());
        }
        self.last_report = None;
        take_model_error(&self.context, key);
        // ── 分段计时 ──
        //
        // 为什么加：用户反馈"刚唤醒也要反应半天"，但原来的日志只有裸 println、没有时间戳，
        // 算不出时间花在哪。这里把一轮拆成 6 段落盘（logs/turn-timing.jsonl）：
        //   cli    试 CLI 后端（opencode 免费档）
        //   sess   建/复用 DSH 会话
        //   model  session/selectModel（候选链逐个设）
        //   open   FollowStream 的 WS 握手
        //   accept session/prompt 被接受
        //   turn   模型真正干活到回合结束（含工具调用）
        // 不加这个就只能猜，而"猜"今天已经错过好几次了。
        let t0 = std::time::Instant::now();
        let mut seg_cli: i64 = 0;
        let mut seg_sess: i64 = 0;
        let mut seg_model: i64 = 0;
        let mut seg_open: i64 = 0;
        let mut seg_accept: i64 = 0;
        let mut seg_turn: i64 = 0;
        let mut attempts: usize = 0;
        let mut used_model: Option<String> = None;
        let mut prompt_chars = prompt.chars().count();
        // 会话级**后端**覆盖：`backends.sessions["group:xxx"] = "opencode-free"` 时，
        // 这个会话直接走 CLI 后端，不经过 DSH。
        //
        // 为什么需要它：opencode 的免费档只允许在它自己的客户端里用（直连 API 返回
        // FreeTierError），所以想白嫖就必须让 opencode CLI 当后端 —— 而 CLI 后端
        // 原本只在"DSH 全挂"时兜底，正常路径永远走不到。
        if let Some(id) = pinned_cli_backend(cfg, key) {
            // 灰度观测：CLI 后端的调用**不进 DSH 账本**（不经过 session/prompt），
            // 所以要单独记一行，否则"这个群花了多少、多快"完全看不见。
            //
            // 超时**单独收紧**：CLI 后端的热态是 2.7s，但偶尔会顶上 180s（实测一次端到端
            // 就是 180s 超时后回落 DSH，用户等了 296 秒）。CLI 是"能快就快、慢就让路"的
            // 角色，不该占满整个回合预算 —— 这里最多等 25s，超了立刻回落 DSH。
            // CLI 后端（opencode 免费档）在这个环境里的实测：
            //   · 简单问答 4.5~6.6s ✅
            //   · 需要调工具的提示词 25s 都不够（直接跑要 51s，且只烧 137 token
            //     —— 时间全在 agent 运行时开销上）
            // 所以给它 **10 秒**：够跑简单问答，跑不完就立刻回落 DSH，
            // 别让用户白等 30 秒。注意 backend.rs 里的 clamp 下限已改成 5 秒，
            // 否则这里设的数字会被抬回 30。
            let cli_timeout = timeout_secs.min(10);
            let started = std::time::Instant::now();
            let result = crate::backend::cli_fallback_by_id_with_runtime(
                &self.context, cfg, id, key, prompt, cli_timeout,
            );
            let elapsed_ms = started.elapsed().as_millis() as i64;
            let record = serde_json::json!({
                "ts": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
                "key": key,
                "backend": id,
                "elapsedMs": elapsed_ms,
                "ok": result.is_ok(),
                "chars": result.as_ref().map(|(_, t)| t.chars().count()).unwrap_or(0),
                "err": result.as_ref().err().map(|error| crate::model_failure::Failure::classify("", error).summary()).unwrap_or_default(),
            })
            .to_string();
            let _ = Self::append_runtime_log(&self.context, "logs/opencode-trial.jsonl", &record);
            match result {
                Ok((used, text)) => {
                    println!("[backend] {key} 指定走 CLI 后端 {used}（来自 backends.sessions，{elapsed_ms}ms）");
                    return Ok(text);
                }
                Err(note) => {
                    let (provider, model) = crate::model_failure::cli_identity(cfg, id);
                    self.report_backend_failure(cfg, key, id, &provider, &model, "", &note);
                    return Err(note.into());
                }
            }
        }
        // A pinned CLI may run independently, but its fallback must not contact a DSH already
        // known to be down. Unpinned callers can safely park this unsubmitted turn for recovery.
        if crate::dsh_health::is_down(&self.context) {
            return Err("DSH 不可用，未提交到 DSH".into());
        }
        // #179: while a guarded DSH profile row is missing the turn is refused before any DSH call; the guard alerts
        // the owner itself, so no model-failure alert follows.
        if let Err(error) = crate::dsh_profile::admit(&self.context) {
            self.failure_reported = true;
            return Err(error.into());
        }
        // 按会话取候选链：这个群配了独立模型就用它的，否则回落全局。
        seg_cli = t0.elapsed().as_millis() as i64;
        let chain = failover::model_chain_for(cfg, key);
        let mut sid = self.ensure_session(key)?;
        self.note_session(&sid);
        seg_sess = t0.elapsed().as_millis() as i64 - seg_cli;
        let mut prompt_text = prompt.to_string();
        // The memories the text in `prompt_text` carries as recall, counted when a submission of it is accepted.
        let mut delivered_recall = self.compat_recall.clone();
        // 连模型都设不上（DSH 不可达 / provider 全坏）：同样给 CLI 后端一次机会，
        // 否则这条最常见的"DSH 挂了"路径根本走不到下面的 failover 分支。
        //
        // DSH 明确说会话已不存在（重启丢失、归档）是「明确失效」：撤销映射、换新会话，
        // 旧会话的令牌随之作废；这与「状态不明」不同，后者绝不重建（ticket 10）。
        // The follow stream is opened right after the session exists and before the model is
        // selected (as the JS bridge does), and reused by the first attempt below.
        let mut pre_follow = match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            FollowStream::open(&self.rpc.base, &self.rpc.cookie(), &sid),
        )
        .await
        {
            Ok(Ok(f)) => Some(f),
            _ => None,
        };
        let applied = match self.apply_model(cfg, key, &sid, &chain) {
            Err(e) if self.cur_turn.is_some() && crate::dsh::is_session_gone(&e) => {
                sid = self.replace_session(key, &sid, "dsh-not-found")?;
                pre_follow = None;
                prompt_text = patch_token_line(&prompt_text, &self.turn_token);
                self.apply_model(cfg, key, &sid, &chain)
            }
            other => other,
        };
        let mut idx = match applied {
            Ok(i) => i,
            Err(e) => {
                let candidate = self.active_candidate.clone().unwrap_or_else(|| chain[0].clone());
                self.report_backend_failure(cfg, key, "dsh", &candidate.provider, &candidate.model, "", &e);
                match self.cli_fallback(cfg, key, prompt, timeout_secs) {
                    Ok((backend_id, text)) => {
                        println!("[failover] 模型全设不上（{e}），改用 CLI 后端 {backend_id} 拿到回复");
                        return Ok(text);
                    }
                    Err((note, cli_refused)) => {
                        let refused = crate::model_failure::Failure::classify("", &e).class == "refusal";
                        return Err(DshTurnError::after_exhaustion(
                            format!("{e}；CLI 后端也没成：{note}"), &sid, refused || cli_refused,
                        ));
                    }
                }
            },
        };
        self.prepare_title();
        seg_model = t0.elapsed().as_millis() as i64 - seg_cli - seg_sess;

        loop {
            attempts += 1;
            // A model error noted by an earlier attempt belongs to that attempt: this attempt's own outcome decides what the
            // caller is told, so a stale note can never hide a different failure.
            take_model_error(&self.context, key);
            used_model = Some(format!("{}/{}", chain[idx].provider, chain[idx].model));
            self.active_candidate = Some(chain[idx].clone());
            // WS 握手也要有超时：`FollowStream::open` 里没有超时，
            // DSH 换密钥/重启的那一瞬间握手会一直挂着 —— 外面套的兜底超时也救不了，
            // 那层只能作用于 await 点，握手中卡住就一直不返回。
            let mut follow = match pre_follow.take() {
                Some(f) => f,
                None => match tokio::time::timeout(
                    std::time::Duration::from_secs(20),
                    FollowStream::open(&self.rpc.base, &self.rpc.cookie(), &sid),
                )
                .await
                {
                    Ok(Ok(f)) => f,
                    Ok(Err(e)) => return Err(e.into()),
                    Err(_) => return Err("FollowStream 握手超时（20s）".into()),
                },
            };
            seg_open += t0.elapsed().as_millis() as i64 - seg_cli - seg_sess - seg_model;
            // Persist the tracked prompt id before submission. Rpc defaults requestId to it;
            // DSH echoes requestId in data.source.rpcId for exact recovery. A failed write refuses submission.
            let rpc_id = crate::dsh::mint_request_id();
            self.turn_validity(key, &sid)?;
            let mut layers: Option<serde_json::Value> = None;
            if let Some(turn) = self.cur_turn.as_ref().filter(|_| self.context.is_isolated() && self.chat.is_none()) {
                follow.track_briefing(&self.context, key, &sid, &self.turn_token)?;
                if self.current_reason != "wakeReminder" {
                    // A persona switch or compaction can start a new briefing generation while this
                    // wake is prepared; nothing was submitted, so prepare once more.
                    let wake = match crate::prompt::prepare_dsh_wake_layered(&self.context, cfg, key, &sid, &self.turn_token, &self.current_reason, &rpc_id, prompt, &turn.id) {
                        Err(error) if error == crate::prompt::WAKE_BRIEFING_CHANGED => crate::prompt::prepare_dsh_wake_layered(&self.context, cfg, key, &sid, &self.turn_token, &self.current_reason, &rpc_id, prompt, &turn.id),
                        other => other,
                    }?;
                    layers = Some(serde_json::json!({"injected": wake.layers.to_json(), "cut": wake.cut.to_json()}));
                    (prompt_text, delivered_recall) = (wake.text, wake.recalled);
                    // Refusal provenance follows the single packet actually delivered, not the
                    // compatibility full render that was built before session resolution.
                    self.carried_messages.clear();
                    crate::model_failure::merge_refs(&mut self.carried_messages,
                        crate::model_failure::snapshot_refs(&prompt_text).unwrap_or_default());
                    prompt_chars = prompt_text.chars().count();
                }
            }
            let handed_over = self.dsh_switch_summary(key, &sid, cfg)?.map(|summary| {
                format!("{summary}\n\n{prompt_text}")
            });
            if let Some(turn) = &self.cur_turn {
                turn.attempt_with_layers(&rpc_id, &sid, layers.as_ref())
                    .map_err(|e| format!("回合账本写入失败，不提交 prompt：{e}"))?;
            }
            let submitted_text = handed_over.as_deref().unwrap_or(&prompt_text);
            let mut content = vec![serde_json::json!({"type": "text", "text": submitted_text})];
            if let Some(chat) = &self.chat { content.extend(chat.media_parts.iter().cloned()); }
            let sent = self.rpc.call_with_rpc_id(
                "session/prompt",
                serde_json::json!({
                    "sessionId": sid,
                    "mode": "queue",
                    "content": content
                }),
                &rpc_id,
            );
            // Chat mode: a slash command DSH runs itself answers directly and starts no model turn.
            if self.chat.is_some() {
                if let Some(command) = sent.as_ref().ok().and_then(|v| v.get("command")).filter(|c| c.is_object()) {
                    let text = command.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                    follow.close().await;
                    if let Some(turn) = &self.cur_turn {
                        let _ = turn.accepted(&rpc_id);
                    }
                    self.last_report = Some(TurnReport { end_kind: "command".to_string(), send_tool_ok: false, command: Some(text) });
                    self.finish_turn("completed", "dsh-command");
                    return Ok(String::new());
                }
            }
            // 提交结果三种：被接受；明确没提交；不知道（连接断了、响应读不到）。
            // 第三种先去 DSH 核对，核对不了就暂停，绝不整轮重投。
            let submitted: Result<(), Submit> = match sent {
                Ok(v) if v.get("accepted").and_then(|a| a.as_bool()) == Some(true) => Ok(()),
                Ok(v) => Err(Submit::Refused(format!("prompt 未被接受：{v}"))),
                Err(e) if e.starts_with(crate::dsh_profile::REFUSAL_PREFIX) => {
                    // #179: the guard refused the prompt before it left (a profile row went missing since the check
                    // above); nothing was submitted, and no other candidate may try.
                    follow.close().await;
                    self.failure_reported = true;
                    return Err(e.into());
                }
                Err(e) if !prompt_error_needs_probe(self.cur_turn.is_some(), &e) => {
                    Err(Submit::Refused(format!("prompt 调用失败：{e}")))
                }
                Err(e) => match crate::dsh::probe_prompt(&self.rpc, &sid, std::slice::from_ref(&rpc_id)).await {
                    crate::dsh::PromptProbe::Turn(_) | crate::dsh::PromptProbe::Queued => Ok(()),
                    crate::dsh::PromptProbe::NotFound { proved: true }
                    | crate::dsh::PromptProbe::SessionGone
                    | crate::dsh::PromptProbe::RemovedFromQueue => {
                        Err(Submit::Refused(format!("prompt 调用失败（已向 DSH 核实未提交）：{e}")))
                    }
                    crate::dsh::PromptProbe::NotFound { proved: false } => Err(Submit::Unverified(format!(
                        "prompt 调用失败（{e}），DSH 日志不足以证明它没有被接受"
                    ))),
                    crate::dsh::PromptProbe::Unreachable(why) => Err(Submit::Unverified(format!(
                        "prompt 调用失败（{e}），核对 DSH 也失败：{why}"
                    ))),
                },
            };
            // 明确标注类型：所有分支都只产出 Err（成功分支直接 return），
            // 不标注的话编译器会推断出 `Result<!, String>`，后面的 unwrap_or_default 就炸。
            if !matches!(&submitted, Err(Submit::Refused(_))) { self.dispatch_unsubmitted = false; }
            let mut may_still_run = false;
            let mut had_send = false;
            // An attempt that ended in `turn/end` error: (code, detail, the model's message).
            let mut model_failure: Option<(String, String, String)> = None;
            let mut refusal_sources = crate::dsh::RefusalSources::default();
            let mut replayable = false;
            let failed: Result<(), String> = match submitted {
                Err(Submit::Unverified(why)) => {
                    follow.close().await;
                    return self.pause_unknown(key, "prompt-unverified", &why).map_err(DshTurnError::from);
                }
                Err(Submit::Refused(why)) => {
                    follow.close().await;
                    // The shared failure reporter below owns the owner-private notification.
                    Err(why)
                }
                Ok(()) => {
                    if let Some(turn) = &self.cur_turn {
                        let _ = turn.accepted(&rpc_id);
                    }
                    // DSH holds the prompt: the memories it recalled were used, once per accepted submission.
                    crate::prompt::record_recall_hits(&self.context, &delivered_recall);
                    // Chat mode: acknowledge once the prompt is in DSH's hands (not again on a retry).
                    if let Some(ack) = self.chat.as_mut().and_then(|c| c.ack.take()) {
                        self.send_chat_text(key, &sid, "ack", &ack);
                    }
                    // JS starts the group-title lookup without awaiting it. Preserve the live
                    // group-turn order: acknowledgement, pending UI rename, final reply.
                    self.flush_title();
                    let start_context = self.context.clone();
                    let start_turn = self.cur_turn.as_ref().map(|turn|turn.id.clone());
                    let start_key = key.to_string();
                    let on_start = move || {
                        if let Some(id)=start_turn.as_deref() {
                            crate::turn_ledger::Recorder::attach(&start_context,id,&start_key).started()?;
                        }
                        Ok(())
                    };
                    let completed_turn = self.cur_turn.clone();
                    let completed_rpc = rpc_id.clone();
                    let on_end = move |ended: &serde_json::Value| {
                        if ended.pointer("/reason/kind").and_then(|v| v.as_str()) == Some("completed") {
                            if let Some(turn) = &completed_turn {
                                turn.observe_completion(&completed_rpc)?;
                            }
                        }
                        Ok(())
                    };
                    let mut retired = None;
                    let reply = if self.chat.as_ref().is_some_and(|c| c.mode == "closed-agent") {
                        // A reset or mode change can retire the session even if DSH never sends
                        // turn/end. Do not hold the conversation queue until the turn timeout.
                        let completion = follow.next_turn_with_lifecycle(timeout, on_start, on_end);
                        tokio::pin!(completion);
                        let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
                        loop {
                            tokio::select! {
                                result = &mut completion => break result,
                                _ = tick.tick() => {
                                    // A mode write or reset can hold the gate across DSH I/O.
                                    // Retry on the next tick so the collector yields between checks.
                                    if let Ok(Some(_gate)) = crate::closed_agent::try_lock(&self.context) {
                                        if let Err(reason) = self.turn_validity(key, &sid) {
                                            let already_ended = self.cur_turn.as_ref().and_then(|turn| {
                                                crate::turn_ledger::read_records(&self.context).ok().map(|records| {
                                                    let (turns, _) = crate::turn_ledger::fold(&records);
                                                    turns.iter().any(|entry| entry.id == turn.id && !entry.state.is_open())
                                                })
                                            }).unwrap_or(false);
                                            if already_ended { self.turn_final = true; }
                                            else { self.finish_turn_with_retry("cancelled", &reason, false); }
                                            retired = Some(reason);
                                            break Err("closed-agent turn retired".to_string());
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        follow.next_turn_with_lifecycle(timeout, on_start, on_end).await
                    };
                    #[cfg(test)]
                    if let Some(hook) = self.completion_observed_hook.as_mut() { hook(&rpc_id); }
                    seg_turn += t0.elapsed().as_millis() as i64
                        - seg_cli
                        - seg_sess
                        - seg_model
                        - seg_open
                        - seg_accept;
                    let ended = follow.last_ended.clone();
                    // Deterministic race seam: hold bookkeeping after a genuine end,
                    // while the independent hard-cap handler can still run.
                    #[cfg(feature = "parity-clock")]
                    if self.context.is_isolated()
                        && ended.pointer("/reason/kind").and_then(|v| v.as_str()) == Some("completed")
                    {
                        let bookkeeping = async {
                            if self.context.read_optional_text(RuntimeArea::Data, "fault-hold-collector-bookkeeping")?.is_some() {
                                self.context.write_atomic(RuntimeArea::Data, "collector-bookkeeping-ready", b"completed")?;
                                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
                                while self.context.read_optional_text(RuntimeArea::Data, "fault-hold-collector-bookkeeping")?.is_some() {
                                    if tokio::time::Instant::now() >= deadline {
                                        return Err("collector bookkeeping test gate timed out".into());
                                    }
                                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                                }
                            }
                            Ok::<(), String>(())
                        }.await;
                        // Releasing the fixture gate can race its optional-file read. A
                        // bookkeeping path failure cannot replace the observed DSH result;
                        // still classify backend refusals/errors through the normal path.
                        if bookkeeping.is_err() {
                            eprintln!("[collector] bookkeeping barrier unavailable; continuing completion classification");
                        }
                    }
                    // 失败回合也要落账：token 已经花了（JS 同样在事件到达时就记）。
                    self.record_turn_usage(
                        key,
                        &sid,
                        &follow,
                        Some(&chain[idx].model),
                        Some(&chain[idx].provider),
                    );
                    had_send = follow.tools.iter().any(|t| is_send_tool(&t.name));
                    let send_tool_ok = follow.tools.iter().any(|t| is_send_tool(&t.name) && t.done && t.ok);
                    if let Some(turn) = &self.cur_turn {
                        if let Some(dsh_turn) = follow.turn_id() {
                            replayable = turn.dsh_turn(&rpc_id, dsh_turn).is_ok();
                        }
                        let _ = turn.tools(follow.tools.iter().map(|t| t.to_json()).collect());
                    }
                    refusal_sources = std::mem::take(&mut follow.refusal);
                    follow.close().await;
                    let end_kind = ended.pointer("/reason/kind").and_then(|k| k.as_str());
                    let classified_failure = failover::classify_turn_end(&ended);
                    if reply.is_err() || classified_failure.is_some() {
                        if let Some(turn) = &self.cur_turn {
                            if let Err(error) = turn.release_completion(&rpc_id) {
                                return self.pause_unknown(key, "completion-release-failed", &error).map_err(DshTurnError::from);
                            }
                        }
                    }
                    let ledger_cancel = self.cur_turn.as_ref().and_then(|turn| {
                        // Fold only this turn's records since its begin (offset captured in begin_ledger_turn).
                        // A tail without the turn's `begun` (a replaced ledger) falls back to the whole ledger.
                        let folded = self.turn_ledger_offset
                            .and_then(|offset| crate::turn_ledger::fold_turn_since(&self.context, offset, &turn.id).ok().flatten())
                            .or_else(|| crate::turn_ledger::read_records(&self.context).ok().and_then(|records| {
                                crate::turn_ledger::fold(&records).0.into_iter().find(|t| t.id == turn.id)
                            }));
                        folded.and_then(|t| {
                            if t.state == crate::turn_ledger::State::Cancelled { Some(t.outcome_reason) }
                            else { t.finalization_claim().filter(|(claim, _)| *claim == "cancel-requested")
                                .map(|(_, reason)| reason.to_string()) }
                        })
                    });
                    let observation_sleep = ledger_cancel.as_deref() == Some("observationSleep");
                    let cancel_reason = if observation_sleep { None } else if let Some(reason) = ledger_cancel.as_deref() {
                        Some(match reason {
                            "turnTimeout" => "turnTimeout",
                            "superseded" => "superseded",
                            _ => "turnCancelled",
                        })
                    } else if retired.is_some() { Some("turnInvalidated") }
                    else if matches!(end_kind, Some("aborted" | "cancelled")) { Some("dshAborted") }
                    else if reply.as_ref().err().is_some_and(|e| e.starts_with("等回复超时")) { Some("collectorTimeout") }
                    else if reply.is_err() { Some("collectorFailure") }
                    else { None };
                    // The DSH adapter's reported facts become the turn timing record.
                    self.report_facts(key, &crate::turn_engine::Facts {
                        ok: reply.is_ok() && cancel_reason.is_none() && classified_failure.is_none(),
                        end_kind: end_kind.map(str::to_string),
                        cancel_reason,
                        attempt: rpc_id.clone(),
                        model: used_model.clone(),
                        attempts,
                        prompt_chars,
                        segments: vec![("cliMs", seg_cli), ("sessMs", seg_sess), ("modelMs", seg_model),
                            ("openMs", seg_open), ("acceptMs", seg_accept)],
                        turn_ms: seg_turn,
                        total_ms: t0.elapsed().as_millis() as i64,
                    });
                    if let Some(reason) = retired {
                        return Err(format!("回合已取消：{reason}").into());
                    }
                    if let Some(reason) = ledger_cancel {
                        // The independent cap/preemption handler owns terminal settlement after
                        // revocation (Recorder rejects our competing completion/failure while its
                        // claim stands). DSH's aborted end is still a successful transport result.
                        self.turn_final = true;
                        if observation_sleep { return Ok(String::new()); }
                        return Err(format!("回合已取消：{reason}").into());
                    }
                    match reply {
                        Ok(text) => match classified_failure {
                            Some((kind, code, detail)) => {
                                let message = ended
                                    .pointer("/reason/error/message")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("未知错误")
                                    .to_string();
                                note_model_error(&self.context, key, &message);
                                // The model's words reach logs, the ledger and the console output through this text: redacted here.
                                let failure = format!(
                                    "回合失败（{} {} {}）",
                                    kind.as_str(),
                                    crate::activity::redact(&self.context, &code),
                                    crate::activity::redact(&self.context, &detail)
                                );
                                model_failure = Some((code, detail, message));
                                Err(failure)
                            }
                            None => {
                                self.last_report = Some(TurnReport {
                                    end_kind: ended
                                        .pointer("/reason/kind")
                                        .and_then(|k| k.as_str())
                                        .unwrap_or("completed")
                                        .to_string(),
                                    send_tool_ok,
                                    command: None,
                                });
                                self.finish_attempt("completed", "ok", Some(&rpc_id), true);
                                if ended.pointer("/reason/kind").and_then(|k| k.as_str()) == Some("completed") {
                                    self.failover_state().on_completed(key, &sid);
                                }
                                return Ok(text);
                            }
                        },
                        Err(e) => {
                            // 等回复超时 / 流断开：DSH 里这一轮可能还在跑。
                            may_still_run = true;
                            Err(e)
                        }
                    }
                }
            };

            let why = match failed {
                Err(e) => e,
                Ok(()) => unreachable!("成功分支已经提前 return"),
            };
            let (failure_code, failure_detail) = model_failure.as_ref()
                .map(|(code, detail, _)| (code.as_str(), detail.as_str())).unwrap_or(("", why.as_str()));
            let failure = crate::model_failure::Failure::classify(failure_code, failure_detail);
            note_model_error(&self.context, key, &failure.summary());
            let turn_id = self.cur_turn.as_ref().filter(|_| replayable).map(|turn| turn.id.clone());
            let call = turn_id.as_deref().map(|turn_id| crate::model_failure::RefusalCall {
                turn_id,
                index: refusal_sources.model_call(),
                rpc_id: Some(&rpc_id),
            });
            self.alert_backend_failure(cfg, key, "dsh", &chain[idx].provider, &chain[idx].model, &failure,
                &refusal_sources.messages, call);
            // 等回复超时 / 流断开：DSH 里这一轮可能还在跑，结果未知。绝不把整条 prompt 交给下一个候选再投一遍
            // （ticket 25 决定 Q3，ticket 10 的未知结果合同）：先清队列并取消 DSH 里的回合、再换令牌，让它之后迟到的
            // 工具调用和发送全部因令牌不符被拒；取消核实不了就暂停交管理员，否则按失败收尾。只有明确的
            // `turn/end` error 或 selectModel/prompt 被拒才换候选。
            if may_still_run {
                if self.cur_turn.is_some() {
                    if let Err(stop) = self.abandon_attempt(key, &sid, &why) {
                        recovery_log::record(
                            &self.context,
                            Trigger::Turn,
                            Component::ModelChain,
                            "turn-unverified",
                            Outcome::Unknown,
                            &format!("{key}: {why}; stopping the DSH turn failed: {stop}"),
                        );
                        return self.pause_unknown(key, "turn-unverified", &format!("{why}；停止 DSH 回合失败：{stop}")).map_err(DshTurnError::from);
                    }
                }
                recovery_log::record(
                    &self.context,
                    Trigger::Turn,
                    Component::ModelChain,
                    "turn-abandoned",
                    Outcome::Unknown,
                    &format!("{key}: {why}; the prompt is not delivered again"),
                );
                return Err(format!("{why}（回合状态未知，已取消该回合，不换候选重投）").into());
            }
            // 重投之前核对这一轮是否仍然有效：会话被重置/替换、令牌被换过都意味着它已被取消，
            // 不能靠重投把它救回来。
            if let Err(invalid) = self.turn_validity(key, &sid) {
                if let Some((_, _, message)) = &model_failure {
                    self.report_turn_error(cfg, key, message);
                }
                self.finish_turn("cancelled", &invalid);
                return Err(format!("回合已取消：{invalid}（此前失败：{why}）").into());
            }
            let kind = failover::classify_message(&why);
            // 这一轮里模型已经发过消息就不再换候选重投：重投会让它再发一遍。
            let already_sent = had_send || self.turn_has_send_batch(key);
            // Preserve local failover diagnostics. Private alerts contain metadata only.
            let (code, detail) = match &model_failure {
                Some((code, detail, _)) => (
                    crate::activity::redact(&self.context, code),
                    crate::activity::redacted_preview(&self.context, &detail, 200),
                ),
                None => ("prompt-refused".to_string(), crate::activity::redacted_preview(&self.context, &why, 200)),
            };
            let policy_refused = crate::model_failure::Failure::classify(failure_code, failure_detail).class == "refusal";
            let action = self.failover_state().plan(key, &sid, chain.len(), already_sent);
            let outcome = match action {
                failover::FailoverAction::Retry { idx: next } => {
                    println!(
                        "[failover] {key} 线路遇挫（{} {}），换接 {} 重试",
                        kind.as_str(),
                        crate::activity::redacted_preview(&self.context, &why, 80),
                        chain[next].label()
                    );
                    recovery_log::record(
                        &self.context,
                        Trigger::Turn,
                        Component::ModelChain,
                        "failover-step",
                        Outcome::Ok,
                        &format!("{key}: {code} {detail}; next candidate {}", chain[next].label()),
                    );
                    self.failover_state().on_failover(key, &sid, next);
                    Ok(next)
                }
                failover::FailoverAction::GiveUp(g) => {
                    if g == failover::GiveUp::LastCandidate {
                            recovery_log::record(
                            &self.context,
                            Trigger::Turn,
                            Component::ModelChain,
                            "candidates-exhausted",
                            Outcome::Failed,
                            &format!("{key}: {code} {detail}"),
                        );
                    }
                    Err(g)
                }
            };
            if let Some((_, _, message)) = &model_failure {
                self.report_turn_error(cfg, key, message);
            }
            match outcome {
                Ok(_) => match self.apply_model(cfg, key, &sid, &chain) {
                    Ok(next) => {
                        idx = next;
                        continue;
                    }
                    Err(select_error) => {
                        // Selection failed: retain the local diagnostic and allow the configured CLI fallback.
                        let safe = crate::activity::redacted_preview(&self.context, &select_error, 200);
                            recovery_log::record(
                            &self.context,
                            Trigger::Turn,
                            Component::ModelChain,
                            "candidates-exhausted",
                            Outcome::Failed,
                            &format!("{key}: the next candidate could not be selected: {safe}"),
                        );
                        return self.exhausted_chain(cfg, key, prompt, timeout_secs, format!("{why}；切换候选后设置模型失败：{select_error}"), "select-failed")
                            .map_err(|(reason, cli_refused)| DshTurnError::after_exhaustion(reason, &sid, policy_refused || cli_refused));
                    }
                },
                Err(failover::GiveUp::AlreadySent) => {
                    return Err(format!("{why}（模型已发送过消息，不重投、不回落）").into());
                }
                Err(g) => {
                    // DSH 的候选链全挂了 —— 再试 `backends` 里配的 CLI 后端（R4，仅 `backends.failoverFallback` 开启时才会跑）。
                    // 只有整条 DSH 链都失败才走到这里，正常回合不会因此多花时间。
                    let why_full = format!("{}（{}）", why, g.as_str());
                    return self.exhausted_chain(cfg, key, prompt, timeout_secs, why_full, g.as_str())
                        .map_err(|(reason, cli_refused)| DshTurnError::after_exhaustion(reason, &sid, policy_refused || cli_refused));
                }
            }
        }
    }

    /// The DSH candidate chain failed: the opt-in CLI backend fallback gets its chance (`backends.failoverFallback`), else
    /// the turn fails with `why_full`.
    fn exhausted_chain(
        &mut self,
        cfg: &serde_json::Value,
        key: &str,
        prompt: &str,
        timeout_secs: u64,
        why_full: String,
        reason: &str,
    ) -> Result<String, (String, bool)> {
        match self.cli_fallback(cfg, key, prompt, timeout_secs) {
            Ok((backend_id, text)) => {
                println!("[failover] DSH 全挂，改用 CLI 后端 {backend_id} 拿到回复");
                recovery_log::record(
                    &self.context,
                    Trigger::Turn,
                    Component::ModelChain,
                    "cli-fallback",
                    Outcome::Ok,
                    &format!("{key}: DSH chain failed ({reason}); answered by CLI backend {backend_id}"),
                );
                Ok(text)
            }
            Err((note, policy_refused)) => Err((format!("{why_full}；CLI 后端也没成：{note}"), policy_refused)),
        }
    }
    /// 投一句 prompt 并等到这一轮结束，返回回复正文。
    ///
    /// **先订阅再投递**：反过来的话回合可能在订阅建立前就结束，实时流不重放。
    pub async fn ask(&mut self, key: &str, prompt: &str, timeout_secs: u64) -> Result<String, String> {
        if !self.has_dsh() {
            return Err(crate::dsh_credential::UNBOUND.to_string());
        }
        let sid = self.ensure_session(key)?;
        self.prepare_title();
        let mut follow = FollowStream::open(&self.rpc.base, &self.rpc.cookie(), &sid).await?;
        let accepted = self.rpc.call(
            "session/prompt",
            serde_json::json!({
                "sessionId": sid,
                "mode": "queue",
                "content": [{ "type": "text", "text": prompt }]
            }),
        )?;
        if accepted.get("accepted").and_then(|a| a.as_bool()) != Some(true) {
            return Err(format!("prompt 未被接受：{accepted}"));
        }
        self.flush_title();
        let reply = follow
            .next_turn(std::time::Duration::from_secs(timeout_secs))
            .await;
        // turn/end 之后 usage 已收集完：先落账，再关流。
        self.record_turn_usage(&key, &sid, &follow, None, None);
        follow.close().await;
        reply
    }

    /// 回合结束后把 usage 落账（Rust 接管写账本的唯一入口）。
    ///
    /// 写失败只打印一行，绝不影响回复 —— JS 那边也是 try/catch 吞掉，
    /// 账本坏了不能连带把 QQ 回复卡住。没有 usage 明细时写 `b:null`，不编造数字。
    fn record_turn_usage(
        &self,
        key: &str,
        sid: &str,
        follow: &FollowStream,
        fallback_model: Option<&str>,
        fallback_provider: Option<&str>,
    ) {
        // 事件里的 model/selection 不一定先于 usage 到达；用刚生效的候选兜底，
        // 避免账本出现 model:null 把单价算成兜底价。
        let model = follow
            .model
            .clone()
            .or_else(|| fallback_model.map(|s| s.to_string()));
        let provider = follow
            .provider
            .clone()
            .or_else(|| fallback_provider.map(|s| s.to_string()));
        let records = turn_records(
            key,
            sid,
            &follow.turn_usage,
            follow.last_seq,
            follow.turn_id(),
            &model,
            &provider,
            now_ms(),
        );
        // 开场快照里的历史采样与 tokenUsage 基线也一并核对：已在账本里的按序号跳过，缺的补记。
        let mut rows = records;
        rows.extend(crate::ledger::harvest_candidates(Some(key), sid, &follow.snapshot.harvest, now_ms()));
        if let Err(e) = crate::ledger::append_planned(&self.context, &rows) {
            eprintln!("[ledger] 写账本失败：{e}");
        }
    }

    /// 把回复发回 QQ（先看静默标记，再过哨兵）。
    ///
    /// `[SILENT]` 是模型"我选择不说"的合法出口（JS `isSilentMarker`）。
    /// 没有这一步的话，整段 `[SILENT]` 会被原样发到群里 —— 那是最尴尬的一种 bug：
    /// 它只会出现在"AI 决定闭嘴"的时刻，平时测不到。
    #[allow(dead_code)]
    pub fn reply(&mut self, target: &Target, text: &str) -> Result<Vec<serde_json::Value>, String> {
        if is_silent_marker(text) {
            println!("[pipeline] {} 选择静默（[SILENT]），不发送", target.key());
            return Ok(Vec::new());
        }
        let cfg = self.context.load_config().map_err(str::to_string)?;
        if let Some(mute) = crate::group_mute::active(&self.context, &target.key()) {
            return Err(mute.refusal_text(crate::parity_clock::now_ms(), &cfg));
        }
        if let Some(why) = configured_outbound_violation(&cfg, text, &self.secrets) {
            return Err(format!("被出站哨兵拦下：{why}"));
        }
        self.tune = SendTuning::from_config(&cfg);
        self.limiter.max_per_minute = SendTuning::max_per_minute(&cfg);
        self.limiter.max_per_hour = SendTuning::max_per_hour(&cfg);
        let sent = self.ob.send_text(target, text, &self.tune, &mut self.limiter);
        if let Err(error) = &sent {
            if crate::onebot::is_mute_refusal(error) {
                // #46: QQ refused with result=120; later sends and wakes for this group stay local.
                crate::group_mute::learn_from_refusal(&self.context, &target.key());
            }
        }
        sent
    }

    /// 完整一轮：投 prompt → 等回复 → 发回 QQ。
    ///
    /// `live=false` 时只走前半段并打印，不发送（影子模式）。
    #[allow(dead_code)]
    pub async fn handle(&mut self, target: &Target, prompt: &str) -> Result<Option<String>, String> {
        let key = target.key();
        let key: &str = &key;
        let reply = self.ask(&key, prompt, 180).await?;
        let trimmed = reply.trim().to_string();
        if trimmed.is_empty() {
            return Err("agent 返回了空回复".to_string());
        }
        if !self.live {
            println!("[pipeline] dry-run {key}：回复 {} 字，未发送", trimmed.chars().count());
            return Ok(Some(trimmed));
        }
        let sent = self.reply(target, &trimmed)?;
        println!("[pipeline] {key}：已发送 {} 条", sent.len());
        Ok(Some(trimmed))
    }
}

/// 把一轮的 usage 采样转成账本行（字段与 JS `token-ledger.js` 的 appendLine 一致）。
///
/// 为什么一条 usage 一行、而不是整轮合并成一行：JS 也是按 `(turn, step, retry)` 落账。
/// 保持同样的粒度，两边同时写时才能靠 `seq` 幂等折叠；合并成一行会和多步的 JS 记录
/// 叠在一起，把同一笔 token 算两遍。
pub(super) fn turn_records(
    key: &str,
    sid: &str,
    samples: &[TurnUsage],
    seq: i64,
    turn: Option<i64>,
    model: &Option<String>,
    provider: &Option<String>,
    at: i64,
) -> Vec<serde_json::Value> {
    if let Some(turn) = turn {
        if samples.is_empty() {
            // 回合确实跑过，但事件里没有 token 明细：写 `b:null` 留证据，账本按 0 计。
            // **不编造数字**；`step:0` 表示「无明细」，不会和正常 step 撞键。
            return vec![serde_json::json!({
                "k": "turn", "t": at, "sid": sid, "key": key, "seq": seq,
                "turn": turn, "step": 0, "retry": 0,
                "model": model, "provider": provider, "b": serde_json::Value::Null,
            })];
        }
    }
    samples
        .iter()
        .map(|s| {
            serde_json::json!({
                "k": "turn", "t": s.t, "sid": sid, "key": key, "seq": s.seq,
                "turn": s.turn, "step": s.step, "retry": s.retry,
                "model": s.model.clone().or_else(|| model.clone()),
                "provider": s.provider.clone().or_else(|| provider.clone()),
                "b": s.usage,
            })
        })
        .collect()
}

/// Outcome of a `session/prompt` submission that did not end in acceptance.
enum Submit {
    /// DSH refused it or it never reached DSH: safe to try another candidate.
    Refused(String),
    /// The request may have been accepted and nothing proves otherwise: never resubmit.
    Unverified(String),
}

/// Whether an error of `session/prompt` leaves the outcome open (check with DSH) rather than being a definite refusal.
/// Only DSH's own refusal (or a 401, refused before processing) is definite, with or without a ledger turn: a timeout or a
/// dropped connection may still have been accepted, so the prompt is never assumed unsent and never fails over blind.
pub(super) fn prompt_error_needs_probe(_has_turn: bool, error: &str) -> bool {
    !definite_refusal(error)
}

/// DSH answered with a refusal (or HTTP 401): the prompt was definitely not accepted.
pub(crate) fn definite_refusal(error: &str) -> bool {
    error.contains(" 被拒：") || error.starts_with("401")
}

/// The access state a new session was created under (see `Pipeline::ensure_session`).
struct AccessEpoch {
    mode_stamp: Result<String, String>,
    allowed: Result<bool, String>,
    resets: Result<usize, String>,
    token: Result<Option<String>, String>,
    policy: String,
}

impl AccessEpoch {
    /// Unchanged since `before`; anything unreadable counts as changed (fail closed). A token that
    /// appears where there was none is not a change (a first contact mints it); one that changes or
    /// disappears is.
    fn still(&self, before: &AccessEpoch) -> bool {
        fn same<T: PartialEq>(a: &Result<T, String>, b: &Result<T, String>) -> bool {
            matches!((a, b), (Ok(x), Ok(y)) if x == y)
        }
        let token = match (&before.token, &self.token) {
            (Ok(None), Ok(_)) => true,
            (Ok(Some(old)), Ok(Some(now))) => old == now,
            _ => false,
        };
        same(&before.mode_stamp, &self.mode_stamp)
            && matches!(self.allowed, Ok(true))
            && same(&before.allowed, &self.allowed)
            && same(&before.resets, &self.resets)
            && token
            && before.policy == self.policy
    }
}
