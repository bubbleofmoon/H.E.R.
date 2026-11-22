import React, { useCallback, useEffect, useRef, useState } from 'react';
import { AlertTriangle, Check, Edit3, Eye, History, RefreshCw, Trash2, X, Zap } from 'lucide-react';
import {
  ConversationStateDetail,
  InboundGap,
  LightMemorySnapshot,
  ModelRoutesResponse,
  PersonaListResponse,
  SendOutboxBatch,
  SettingsHistoryEntry,
  TurnRecord,
  appendLightMemory,
  assignConversationPersona,
  changeConversationScope,
  clearLightMemory,
  fetchConversationSettings,
  fetchConversationSettingsHistory,
  fetchConversationState,
  fetchInboundGaps,
  fetchInitialPrompt,
  fetchLightMemory,
  fetchModelRoutes,
  fetchPersonasList,
  fetchSendOutbox,
  fetchSessions,
  fetchTurns,
  persistLiveRestartBanner,
  removeLightMemory,
  resetConversationSetting,
  resetSession,
  resolveTurn,
  retryInboundGaps,
  saveInitialPrompt,
  setConversationSetting,
  switchConversationEngine,
  undoSetting,
  updateLightMemory,
  wakeConversation,
} from '../api/run';
import { Badge, Button, Card, Tabs } from './Primitives';
import { useConsole } from '../stores/consoleStore';

type MemoryCategory = 'activeTopic' | 'pendingThought' | 'memberImpression';

interface EditingMemoryState {
  category: MemoryCategory;
  keyOrOldContent: string;
  contentDraft: string;
  targetDraft: string;
  extraDraft: string;
}

export const ConversationInspector: React.FC<{ conversationKey: string }> = ({
  conversationKey,
}) => {
  const { setInspector, setInspectorDirty, openConfirm, pushToast, refreshLiveData } =
    useConsole();

  const [tab, setTab] = useState<'interact' | 'recovery' | 'settings'>('interact');
  const [loading, setLoading] = useState(true);
  const [loadNotice, setLoadNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Tab 1 state
  const [stateDetail, setStateDetail] = useState<ConversationStateDetail | null>(null);
  const [initialPromptSaved, setInitialPromptSaved] = useState('');
  const [initialPromptDraft, setInitialPromptDraft] = useState('');
  const [lightMemory, setLightMemory] = useState<LightMemorySnapshot | null>(null);
  const [memCategory, setMemCategory] = useState<MemoryCategory>('activeTopic');
  const [memTargetInput, setMemTargetInput] = useState('');
  const [memInput, setMemInput] = useState('');
  const [editingMemory, setEditingMemory] = useState<EditingMemoryState | null>(null);

  // Tab 2 state
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [turns, setTurns] = useState<TurnRecord[]>([]);
  const [gaps, setGaps] = useState<InboundGap[]>([]);
  const [outbox, setOutbox] = useState<SendOutboxBatch[]>([]);

  // Tab 3 state
  const [personas, setPersonas] = useState<PersonaListResponse['personas']>([]);
  const [savedPersona, setSavedPersona] = useState<string>('');
  const [draftPersona, setDraftPersona] = useState<string>('');
  const [routes, setRoutes] = useState<ModelRoutesResponse['routes']>([]);
  const [savedRoute, setSavedRoute] = useState<string>('');
  const [draftRoute, setDraftRoute] = useState<string>('');
  const [savedEngine, setSavedEngine] = useState<'dsh' | 'harness'>('dsh');
  const [draftEngine, setDraftEngine] = useState<'dsh' | 'harness'>('dsh');
  const [admitted, setAdmitted] = useState(false);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [historyEntries, setHistoryEntries] = useState<SettingsHistoryEntry[]>([]);
  const [historyLoading, setHistoryLoading] = useState(false);

  const loadSeqRef = useRef(0);
  const mountedRef = useRef(true);
  const editGenRef = useRef({ prompt: 0, persona: 0, route: 0, engine: 0 });
  const hasLoadedRef = useRef({ prompt: false, persona: false, route: false, engine: false });
  const savedRef = useRef({ prompt: '', persona: '', route: '', engine: 'dsh' as 'dsh' | 'harness' });
  const draftRef = useRef({ prompt: '', persona: '', route: '', engine: 'dsh' as 'dsh' | 'harness' });

  savedRef.current = {
    prompt: initialPromptSaved,
    persona: savedPersona,
    route: savedRoute,
    engine: savedEngine,
  };
  draftRef.current = {
    prompt: initialPromptDraft,
    persona: draftPersona,
    route: draftRoute,
    engine: draftEngine,
  };

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const loadHistory = useCallback(async () => {
    setHistoryLoading(true);
    try {
      const res = await fetchConversationSettingsHistory(conversationKey);
      if (mountedRef.current) {
        setHistoryEntries(Array.isArray(res.entries) ? res.entries : []);
      }
    } catch (err) {
      if (mountedRef.current) {
        pushToast(err instanceof Error ? err.message : '读取会话设置历史失败', 'warn');
      }
    } finally {
      if (mountedRef.current) {
        setHistoryLoading(false);
      }
    }
  }, [conversationKey, pushToast]);

  const loadAll = useCallback(
    async (opts?: {
      forceSync?: ('prompt' | 'persona' | 'route' | 'engine')[];
      expectedGen?: Partial<Record<'prompt' | 'persona' | 'route' | 'engine', number>>;
    }) => {
      const seq = ++loadSeqRef.current;
      const startGen = { ...editGenRef.current };
      const expectedGen = opts?.expectedGen;
      const force = new Set(opts?.forceSync || []);
      setLoading(true);
      const [
        stateRes,
        promptRes,
        memoryRes,
        sessionsRes,
        turnsRes,
        gapsRes,
        outboxRes,
        personasRes,
        convSettingsRes,
        modelsRes,
      ] = await Promise.allSettled([
        fetchConversationState(conversationKey),
        fetchInitialPrompt(conversationKey),
        fetchLightMemory(conversationKey),
        fetchSessions(),
        fetchTurns(),
        fetchInboundGaps(),
        fetchSendOutbox(conversationKey),
        fetchPersonasList(),
        fetchConversationSettings(),
        fetchModelRoutes(),
      ]);

      if (!mountedRef.current || seq !== loadSeqRef.current) return;

      const warnings: string[] = [];

      if (stateRes.status === 'fulfilled') {
        setStateDetail(stateRes.value);
      } else {
        warnings.push('唤醒状态');
      }

      if (promptRes.status === 'fulfilled') {
        const txt = promptRes.value.text || '';
        const prevSaved = savedRef.current.prompt;
        const wasLoaded = hasLoadedRef.current.prompt;
        hasLoadedRef.current.prompt = true;
        setInitialPromptSaved(txt);
        if (
          editGenRef.current.prompt === (expectedGen?.prompt ?? startGen.prompt) &&
          (force.has('prompt') || (!wasLoaded ? editGenRef.current.prompt === 0 : draftRef.current.prompt === prevSaved))
        ) {
          setInitialPromptDraft(txt);
        }
      } else {
        warnings.push('初始提示词');
      }

      if (memoryRes.status === 'fulfilled') {
        setLightMemory(memoryRes.value);
      } else {
        warnings.push('轻记忆');
      }

      if (sessionsRes.status === 'fulfilled') {
        const hit = sessionsRes.value.sessions.find((s) => s.key === conversationKey);
        setSessionId(hit?.sessionId || null);
      } else {
        warnings.push('会话映射');
      }

      if (turnsRes.status === 'fulfilled') {
        const recent = turnsRes.value.turns.filter((t) => t.key === conversationKey);
        const mergedTurns = [...recent];
        const seenIds = new Set(mergedTurns.map((t) => t.turnId));
        for (const p of turnsRes.value.paused || []) {
          if (p.key === conversationKey && !seenIds.has(p.turnId)) {
            mergedTurns.push({
              turnId: p.turnId,
              code: p.code,
              key: p.key,
              state: 'paused',
              pauseReason: p.reason,
              pauseDetail: p.detail ?? null,
            });
          }
        }
        mergedTurns.sort((a, b) => {
          const aPaused = a.pauseReason || a.state === 'paused' ? 1 : 0;
          const bPaused = b.pauseReason || b.state === 'paused' ? 1 : 0;
          if (aPaused !== bPaused) return bPaused - aPaused;
          return (b.beganAt ?? 0) - (a.beganAt ?? 0);
        });
        const pausedList = mergedTurns.filter((t) => t.pauseReason || t.state === 'paused');
        const otherList = mergedTurns
          .filter((t) => !t.pauseReason && t.state !== 'paused')
          .slice(0, 10);
        setTurns([...pausedList, ...otherList]);
      } else {
        warnings.push('轮次历史');
      }

      if (gapsRes.status === 'fulfilled') {
        const convGaps = gapsRes.value.gaps.filter((g) => g.conversation === conversationKey);
        convGaps.sort((a, b) => {
          const aUnres = a.status !== 'recovered' ? 1 : 0;
          const bUnres = b.status !== 'recovered' ? 1 : 0;
          if (aUnres !== bUnres) return bUnres - aUnres;
          return b.id - a.id;
        });
        const unresolved = convGaps.filter((g) => g.status !== 'recovered');
        const recovered = convGaps.filter((g) => g.status === 'recovered').slice(0, 5);
        setGaps([...unresolved, ...recovered]);
      } else {
        warnings.push('入站缺口');
      }

      if (outboxRes.status === 'fulfilled') {
        setOutbox(outboxRes.value.batches || []);
      } else {
        warnings.push('发件箱');
      }

      if (personasRes.status === 'fulfilled') {
        setPersonas(personasRes.value.personas || []);
        const cur = personasRes.value.overrides?.[conversationKey] || '';
        const prevSaved = savedRef.current.persona;
        const wasLoaded = hasLoadedRef.current.persona;
        hasLoadedRef.current.persona = true;
        setSavedPersona(cur);
        if (
          editGenRef.current.persona === (expectedGen?.persona ?? startGen.persona) &&
          (force.has('persona') || (!wasLoaded ? editGenRef.current.persona === 0 : draftRef.current.persona === prevSaved))
        ) {
          setDraftPersona(cur);
        }
      } else {
        warnings.push('人设列表');
      }

      if (modelsRes.status === 'fulfilled') {
        setRoutes(modelsRes.value.routes || []);
        const row = modelsRes.value.conversations.find((c) => c.conversation === conversationKey);
        const storedRoute = row?.stored || '';
        const prevSaved = savedRef.current.route;
        const wasLoaded = hasLoadedRef.current.route;
        hasLoadedRef.current.route = true;
        setSavedRoute(storedRoute);
        if (
          editGenRef.current.route === (expectedGen?.route ?? startGen.route) &&
          (force.has('route') || (!wasLoaded ? editGenRef.current.route === 0 : draftRef.current.route === prevSaved))
        ) {
          setDraftRoute(storedRoute);
        }
      } else {
        warnings.push('模型路由');
      }

      if (convSettingsRes.status === 'fulfilled') {
        const row = convSettingsRes.value.conversations.find(
          (c) => c.conversation === conversationKey,
        );
        setAdmitted(Boolean(row?.admitted));
        const engVal =
          typeof row?.settings?.['conversation.engine']?.value === 'string'
            ? row.settings['conversation.engine'].value
            : row?.engineToday || 'dsh';
        const normalized: 'dsh' | 'harness' = engVal === 'harness' ? 'harness' : 'dsh';
        const prevSaved = savedRef.current.engine;
        const wasLoaded = hasLoadedRef.current.engine;
        hasLoadedRef.current.engine = true;
        setSavedEngine(normalized);
        if (
          editGenRef.current.engine === (expectedGen?.engine ?? startGen.engine) &&
          (force.has('engine') || (!wasLoaded ? editGenRef.current.engine === 0 : draftRef.current.engine === prevSaved))
        ) {
          setDraftEngine(normalized);
        }
      } else {
        warnings.push('单会话设置');
      }

      setLoadNotice(warnings.length > 0 ? `以下详情暂不可用：${warnings.join('、')}` : null);
      setLoading(false);
    },
    [conversationKey],
  );

  useEffect(() => {
    void loadAll();
  }, [loadAll]);

  const promptDirty = initialPromptDraft !== initialPromptSaved;
  const personaDirty = draftPersona !== savedPersona;
  const routeDirty = draftRoute !== savedRoute;
  const engineDirty = draftEngine !== savedEngine;
  const hasAnyDirty = promptDirty || personaDirty || routeDirty || engineDirty;
  const hasMemoryDraft =
    memInput.trim().length > 0 || memTargetInput.trim().length > 0 || editingMemory !== null;

  useEffect(() => {
    setInspectorDirty(hasAnyDirty || hasMemoryDraft);
    return () => setInspectorDirty(false);
  }, [hasAnyDirty, hasMemoryDraft, setInspectorDirty]);

  const handleSaveAll = async () => {
    if (busy || !hasAnyDirty) return;
    const saveStartGen = { ...editGenRef.current };
    setBusy(true);
    let wroteAny = false;
    const forceSync: ('prompt' | 'persona' | 'route' | 'engine')[] = [];
    const errors: string[] = [];

    if (promptDirty) {
      try {
        await saveInitialPrompt(conversationKey, initialPromptDraft);
        wroteAny = true;
        if (editGenRef.current.prompt === saveStartGen.prompt) {
          forceSync.push('prompt');
          if (mountedRef.current) setInitialPromptSaved(initialPromptDraft);
        }
      } catch (err) {
        errors.push(
          `初始提示词：${err instanceof Error ? err.message : '保存失败'}`,
        );
      }
    }

    if (personaDirty) {
      try {
        const res = await assignConversationPersona(conversationKey, draftPersona || null);
        wroteAny = true;
        if (editGenRef.current.persona === saveStartGen.persona) {
          forceSync.push('persona');
          if (mountedRef.current) setSavedPersona(draftPersona);
        }
        if (Array.isArray(res.rebriefFailed) && res.rebriefFailed.length > 0) {
          pushToast(
            `人设已更新，但 ${res.rebriefFailed.length} 项会话重发引导未完成`,
            'warn',
          );
        }
      } catch (err) {
        errors.push(`人设：${err instanceof Error ? err.message : '保存失败'}`);
      }
    }

    if (routeDirty) {
      try {
        if (draftRoute) {
          await setConversationSetting(conversationKey, 'conversation.route', draftRoute);
        } else {
          await resetConversationSetting(conversationKey, 'conversation.route');
        }
        wroteAny = true;
        if (editGenRef.current.route === saveStartGen.route) {
          forceSync.push('route');
          if (mountedRef.current) setSavedRoute(draftRoute);
        }
      } catch (err) {
        errors.push(`模型路由：${err instanceof Error ? err.message : '保存失败'}`);
      }
    }

    if (engineDirty) {
      try {
        await setConversationSetting(conversationKey, 'conversation.engine', draftEngine);
        wroteAny = true;
        if (editGenRef.current.engine === saveStartGen.engine) {
          forceSync.push('engine');
          if (mountedRef.current) setSavedEngine(draftEngine);
        }
      } catch (err) {
        errors.push(`回合引擎：${err instanceof Error ? err.message : '保存失败'}`);
      }
    }

    if (errors.length > 0) {
      pushToast(`部分或全部更改保存失败：${errors.join('；')}`, 'danger');
    } else if (wroteAny) {
      pushToast('已保存单会话更改', 'success');
    }

    if (wroteAny) {
      const safeForceSync = forceSync.filter((f) => editGenRef.current[f] === saveStartGen[f]);
      window.dispatchEvent(new CustomEvent('her-scope-updated', { detail: {} }));
      await loadAll({ forceSync: safeForceSync, expectedGen: saveStartGen });
      if (historyOpen) {
        void loadHistory();
      }
    }

    if (mountedRef.current) setBusy(false);
  };

  const handleDiscardAll = () => {
    setInitialPromptDraft(initialPromptSaved);
    setDraftPersona(savedPersona);
    setDraftRoute(savedRoute);
    setDraftEngine(savedEngine);
  };

  const handleWakeNow = async () => {
    if (busy) return;
    setBusy(true);
    try {
      const res = await wakeConversation(conversationKey);
      pushToast(`已排队立即唤醒（reason=${res.reason}）`, 'success');
      const [sRes, oRes] = await Promise.allSettled([
        fetchConversationState(conversationKey),
        fetchSendOutbox(conversationKey),
      ]);
      if (mountedRef.current) {
        if (sRes.status === 'fulfilled') setStateDetail(sRes.value);
        if (oRes.status === 'fulfilled') setOutbox(oRes.value.batches || []);
      }
    } catch (err) {
      pushToast(err instanceof Error ? err.message : '触发唤醒失败', 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleAddMemory = async () => {
    const content = memInput.trim();
    const target = memTargetInput.trim();
    if (memCategory === 'memberImpression' && !target) {
      pushToast('请填写群友昵称或成员标识作为印象目标', 'warn');
      return;
    }
    if (!content || busy) return;
    setBusy(true);
    try {
      await appendLightMemory({
        key: conversationKey,
        category: memCategory,
        content,
        extra: memCategory === 'memberImpression' ? { target } : undefined,
      });
      setMemInput('');
      if (memCategory === 'memberImpression') setMemTargetInput('');
      pushToast('已写入轻记忆', 'success');
      const mRes = await fetchLightMemory(conversationKey);
      if (mountedRef.current) setLightMemory(mRes);
    } catch (err) {
      pushToast(err instanceof Error ? err.message : '写入轻记忆失败', 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleSaveEditMemory = async () => {
    if (!editingMemory || busy) return;
    const trimmedContent = editingMemory.contentDraft.trim();
    if (!trimmedContent) {
      pushToast('记忆内容不能为空', 'warn');
      return;
    }
    setBusy(true);
    try {
      if (editingMemory.category === 'activeTopic') {
        await updateLightMemory({
          key: conversationKey,
          category: 'activeTopic',
          oldContent: editingMemory.keyOrOldContent,
          newContent: trimmedContent,
          newExtra: { pendingQuestion: editingMemory.extraDraft.trim() },
        });
      } else if (editingMemory.category === 'pendingThought') {
        await updateLightMemory({
          key: conversationKey,
          category: 'pendingThought',
          oldContent: editingMemory.keyOrOldContent,
          newContent: trimmedContent,
          newExtra: { motivation: editingMemory.extraDraft.trim() },
        });
      } else {
        const nextTarget = editingMemory.targetDraft.trim();
        await updateLightMemory({
          key: conversationKey,
          category: 'memberImpression',
          target: editingMemory.keyOrOldContent,
          newContent: trimmedContent,
          newExtra: nextTarget ? { target: nextTarget } : undefined,
        });
      }
      if (mountedRef.current) setEditingMemory(null);
      pushToast('已更新轻记忆条目', 'success');
      const mRes = await fetchLightMemory(conversationKey);
      if (mountedRef.current) setLightMemory(mRes);
    } catch (err) {
      pushToast(err instanceof Error ? err.message : '更新轻记忆失败', 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleRemoveMemory = (category: MemoryCategory, value: string) => {
    const label =
      category === 'activeTopic' ? '活跃话题' : category === 'pendingThought' ? '待表达想法' : '成员印象';
    openConfirm({
      title: `删除该条${label}？`,
      description: `即将从 ${conversationKey} 移除「${value}」，删除后不可恢复。`,
      confirmLabel: '确认删除',
      danger: true,
      onConfirm: async () => {
        setBusy(true);
        try {
          await removeLightMemory(
            category === 'memberImpression'
              ? { key: conversationKey, category, target: value }
              : { key: conversationKey, category, content: value },
          );
          pushToast('已删除轻记忆条目', 'success');
          const mRes = await fetchLightMemory(conversationKey);
          if (mountedRef.current) setLightMemory(mRes);
        } catch (err) {
          pushToast(err instanceof Error ? err.message : '删除轻记忆失败', 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleClearCategoryMemory = () => {
    const label =
      memCategory === 'activeTopic'
        ? '活跃话题'
        : memCategory === 'pendingThought'
          ? '待表达想法'
          : '成员印象';
    openConfirm({
      title: `清空 ${conversationKey} 的「${label}」？`,
      description: `将仅清空该会话的「${label}」类别记录，其他类别保留且不可撤销。`,
      confirmLabel: '确认清空该类',
      danger: true,
      onConfirm: async () => {
        setBusy(true);
        try {
          await clearLightMemory(conversationKey, memCategory);
          pushToast(`已清空${label}`, 'success');
          const mRes = await fetchLightMemory(conversationKey);
          if (mountedRef.current) setLightMemory(mRes);
        } catch (err) {
          pushToast(err instanceof Error ? err.message : '清空该类轻记忆失败', 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleClearMemory = () => {
    openConfirm({
      title: `清空 ${conversationKey} 的全部轻记忆？`,
      description: '将清除该会话的活跃话题、待表达想法与成员印象记录，且不可撤销。',
      confirmLabel: '确认清空',
      danger: true,
      onConfirm: async () => {
        setBusy(true);
        try {
          await clearLightMemory(conversationKey);
          pushToast('已清空轻记忆', 'success');
          const mRes = await fetchLightMemory(conversationKey);
          if (mountedRef.current) setLightMemory(mRes);
        } catch (err) {
          pushToast(err instanceof Error ? err.message : '清空轻记忆失败', 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleResetSession = () => {
    openConfirm({
      title: `重置 ${conversationKey} 的会话映射？`,
      description:
        '此操作将解绑当前底层会话 ID、重置该会话的社交状态并取消当前可终止的轮次；下一条消息将建立全新会话上下文。',
      confirmLabel: '确认重置',
      danger: true,
      onConfirm: async () => {
        setBusy(true);
        try {
          await resetSession(conversationKey);
          pushToast('会话映射与状态已重置', 'success');
          window.dispatchEvent(new CustomEvent('her-scope-updated', { detail: {} }));
          await loadAll();
        } catch (err) {
          pushToast(err instanceof Error ? err.message : '重置会话映射失败', 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleResolveTurn = (turn: TurnRecord, action: 'abandon' | 'retry') => {
    const isSendUnresolved = turn.pauseReason === 'send-unresolved';
    openConfirm({
      title:
        action === 'abandon'
          ? '确认放弃暂停轮次？'
          : isSendUnresolved
            ? '确认带风险重试未决发送轮次？'
            : '确认重试该暂停轮次？',
      description:
        action === 'abandon'
          ? '放弃该轮次会关闭当前暂停状态并释放会话队列，无法恢复。'
          : isSendUnresolved
            ? '该轮次停在 send-unresolved 状态，外部发送结果可能已送达；确认重试可能导致重复发送。'
            : '确认后将把该暂停轮次的事件重新放回队列处理。',
      confirmLabel: action === 'abandon' ? '确认放弃' : '确认重试',
      danger: action === 'abandon' || isSendUnresolved,
      onConfirm: async () => {
        setBusy(true);
        try {
          await resolveTurn(turn.turnId, action, action === 'retry' && isSendUnresolved);
          pushToast(action === 'abandon' ? '已放弃该卡住轮次' : '已重试该轮次', 'success');
          window.dispatchEvent(new CustomEvent('her-scope-updated', { detail: {} }));
          await loadAll();
        } catch (err) {
          pushToast(err instanceof Error ? err.message : '处理轮次失败', 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleRetryGaps = async () => {
    if (busy) return;
    setBusy(true);
    try {
      const res = await retryInboundGaps(conversationKey);
      pushToast(`已重置 ${res.reset} 条入站消息缺口`, 'success');
      window.dispatchEvent(new CustomEvent('her-scope-updated', { detail: {} }));
      await loadAll();
    } catch (err) {
      pushToast(err instanceof Error ? err.message : '重试入站缺口失败', 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleImmediateEngineSwitch = () => {
    openConfirm({
      title: `立即将 ${conversationKey} 切换为 ${draftEngine}？`,
      description: '将等待当前回合结算并迁移会话代次；若当前有长时间排空回合可能返回 409。',
      confirmLabel: '立即切换',
      danger: false,
      onConfirm: async () => {
        const actionStartGen = { ...editGenRef.current };
        setBusy(true);
        try {
          await switchConversationEngine(conversationKey, draftEngine);
          pushToast(`已立即切换至 ${draftEngine}`, 'success');
          if (mountedRef.current && editGenRef.current.engine === actionStartGen.engine) {
            setSavedEngine(draftEngine);
          }
          const safeForceSync = (['engine'] as const).filter(
            (f) => editGenRef.current[f] === actionStartGen[f],
          );
          window.dispatchEvent(new CustomEvent('her-scope-updated', { detail: {} }));
          await loadAll({ forceSync: safeForceSync, expectedGen: actionStartGen });
        } catch (err) {
          pushToast(err instanceof Error ? err.message : '立即切换引擎失败', 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleResetEngine = () => {
    openConfirm({
      title: `重置 ${conversationKey} 的回合引擎为注册表默认？`,
      description: '将清除单会话引擎覆盖配置（注意：注册表默认值可能不同于当前全局模式配置）。',
      confirmLabel: '确认重置',
      danger: false,
      onConfirm: async () => {
        const actionStartGen = { ...editGenRef.current };
        setBusy(true);
        try {
          await resetConversationSetting(conversationKey, 'conversation.engine');
          pushToast('已重置单会话回合引擎设置', 'success');
          const safeForceSync = (['engine'] as const).filter(
            (f) => editGenRef.current[f] === actionStartGen[f],
          );
          window.dispatchEvent(new CustomEvent('her-scope-updated', { detail: {} }));
          await loadAll({ forceSync: safeForceSync, expectedGen: actionStartGen });
          if (historyOpen) void loadHistory();
        } catch (err) {
          pushToast(err instanceof Error ? err.message : '重置回合引擎失败', 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleUndoSettingEntry = (entry: SettingsHistoryEntry) => {
    openConfirm({
      title: `撤销配置变更 ${entry.setting}？`,
      description: `将回退至修改前的值（${JSON.stringify(entry.old)}）。`,
      confirmLabel: '确认撤销',
      danger: true,
      onConfirm: async () => {
        const actionStartGen = { ...editGenRef.current };
        setBusy(true);
        try {
          const res = await undoSetting(entry.entry);
          pushToast(
            res.mirrorError ? `已撤销（回写警告：${res.mirrorError}）` : '已撤销该配置变更',
            res.mirrorError ? 'warn' : 'success',
          );
          const candidateSync: ('route' | 'engine')[] =
            entry.setting === 'conversation.route'
              ? ['route']
              : entry.setting === 'conversation.engine'
                ? ['engine']
                : [];
          const safeForceSync = candidateSync.filter(
            (f) => editGenRef.current[f] === actionStartGen[f],
          );
          window.dispatchEvent(new CustomEvent('her-scope-updated', { detail: {} }));
          await loadAll({ forceSync: safeForceSync, expectedGen: actionStartGen });
          await loadHistory();
        } catch (err) {
          pushToast(err instanceof Error ? err.message : '撤销配置变更失败', 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleRemoveFromScope = () => {
    openConfirm({
      title: `从允许范围移出会话 ${conversationKey}？`,
      description:
        '移出会立即吊销其会话凭据、清除该会话作用域配置并停止响应消息；需再次手动接入才会恢复。',
      confirmLabel: '确认移出',
      danger: true,
      onConfirm: async () => {
        setBusy(true);
        try {
          const res = await changeConversationScope(conversationKey, 'remove', true);
          pushToast(`已移出会话 ${conversationKey}`, 'success');
          if (res.liveRestart?.state === 'required') {
            persistLiveRestartBanner(res.liveRestart, null);
          }
          setInspectorDirty(false);
          window.dispatchEvent(
            new CustomEvent('her-scope-updated', { detail: { liveRestart: res.liveRestart } }),
          );
          await refreshLiveData();
          setInspector(null);
        } catch (err) {
          pushToast(err instanceof Error ? err.message : '移出会话失败', 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const impressionEntries = Object.entries(lightMemory?.raw.memberImpressions || {});
  const totalMemoryCount =
    (lightMemory?.raw.activeTopics.length ?? 0) +
    (lightMemory?.raw.pendingThoughts.length ?? 0) +
    impressionEntries.length;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="border-b border-[var(--border-hairline)] px-4 pt-2">
        <Tabs
          tabs={[
            { id: 'interact', label: '互动与状态' },
            { id: 'recovery', label: '恢复与队列' },
            { id: 'settings', label: '单会话设置' },
          ]}
          active={tab}
          onChange={(id) => setTab(id as 'interact' | 'recovery' | 'settings')}
          className="mb-0 border-b-0"
        />
      </div>

      <div className="flex-1 space-y-4 overflow-y-auto p-4 text-xs">
        {loading && !stateDetail ? (
          <div className="py-6 text-center text-[var(--text-muted)]">读取会话详情中…</div>
        ) : null}

        {loadNotice && (
          <div className="rounded border border-amber-300 bg-[var(--warn-soft)]/50 px-3 py-2 text-[11px] text-[var(--warn)] dark:border-amber-900">
            {loadNotice}
          </div>
        )}

        {tab === 'interact' && (
          <>
            <Card className="p-3 space-y-2.5">
              <div className="flex items-center justify-between">
                <div className="font-semibold text-[var(--text-primary)]">唤醒状态与控制</div>
                <Button size="sm" variant="primary" disabled={busy} onClick={() => void handleWakeNow()}>
                  <Zap className="h-3.5 w-3.5" />
                  立即唤醒
                </Button>
              </div>
              <dl className="grid grid-cols-2 gap-2 text-[11px]">
                <div>
                  <dt className="text-[var(--text-muted)]">唤醒模式</dt>
                  <dd className="font-medium text-[var(--text-primary)]">
                    {stateDetail?.wakeConfig?.mode || '默认'}
                    {stateDetail?.wakeConfig?.infinite ? '（无限休眠）' : ''}
                  </dd>
                </div>
                <div>
                  <dt className="text-[var(--text-muted)]">未读 / 最近 / 未回应唤醒</dt>
                  <dd className="tabular-nums text-[var(--text-primary)]">
                    {stateDetail?.unreadCount ?? 0} / {stateDetail?.recentCount ?? 0} /{' '}
                    {stateDetail?.wakeConfig?.noActionCount ?? 0}
                  </dd>
                </div>
                <div>
                  <dt className="text-[var(--text-muted)]">上次唤醒原因</dt>
                  <dd className="truncate font-mono">{stateDetail?.lastWakeReason || '—'}</dd>
                </div>
                <div>
                  <dt className="text-[var(--text-muted)]">下次预计唤醒</dt>
                  <dd className="truncate font-mono">{stateDetail?.wakeConfig?.sleepUntil || '—'}</dd>
                </div>
              </dl>
            </Card>

            <div className="space-y-1.5">
              <div className="flex items-center justify-between">
                <span className="font-semibold text-[var(--text-primary)]">首轮初始提示词</span>
                {initialPromptDraft && (
                  <Button
                    size="sm"
                    variant="ghost"
                    onClick={() => {
                      editGenRef.current.prompt += 1;
                      setInitialPromptDraft('');
                    }}
                  >
                    清空草稿
                  </Button>
                )}
              </div>
              <textarea
                rows={4}
                value={initialPromptDraft}
                onChange={(e) => {
                  editGenRef.current.prompt += 1;
                  setInitialPromptDraft(e.target.value);
                }}
                placeholder="为该会话下一次新建立时注入一次性引导说明…"
                className="w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] p-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
              />
            </div>

            <Card className="p-3 space-y-2.5">
              <div className="flex items-center justify-between gap-2">
                <span className="font-semibold text-[var(--text-primary)]">
                  轻记忆（话题 / 想法 / 成员印象）
                </span>
                <div className="flex items-center gap-1">
                  <Button size="sm" variant="ghost" disabled={busy} onClick={handleClearCategoryMemory}>
                    清空当前类
                  </Button>
                  <Button size="sm" variant="ghost" disabled={busy} onClick={handleClearMemory}>
                    <Trash2 className="h-3.5 w-3.5" />
                    清空
                  </Button>
                </div>
              </div>
              <div className="space-y-1.5">
                <div className="flex gap-1.5">
                  <select
                    value={memCategory}
                    onChange={(e) => {
                      const val = e.target.value;
                      setMemCategory(
                        val === 'pendingThought' || val === 'memberImpression' ? val : 'activeTopic',
                      );
                    }}
                    className="h-8 rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs text-[var(--text-primary)] focus:outline-none"
                  >
                    <option value="activeTopic">活跃话题</option>
                    <option value="pendingThought">待表达想法</option>
                    <option value="memberImpression">成员印象</option>
                  </select>
                  {memCategory === 'memberImpression' && (
                    <input
                      type="text"
                      value={memTargetInput}
                      onChange={(e) => setMemTargetInput(e.target.value)}
                      placeholder="群友名…"
                      className="h-8 w-24 rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs text-[var(--text-primary)] focus:outline-none"
                    />
                  )}
                  <input
                    type="text"
                    value={memInput}
                    onChange={(e) => setMemInput(e.target.value)}
                    placeholder={
                      memCategory === 'memberImpression' ? '输入印象特征…' : '追加轻记忆内容…'
                    }
                    className="h-8 min-w-0 flex-1 rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs text-[var(--text-primary)] focus:outline-none"
                  />
                  <Button
                    size="sm"
                    variant="secondary"
                    disabled={busy}
                    onClick={() => void handleAddMemory()}
                  >
                    添加
                  </Button>
                </div>
              </div>

              {editingMemory && (
                <div className="space-y-2 rounded border border-[var(--accent)]/40 bg-[var(--accent-soft)]/30 p-2.5">
                  <div className="font-medium text-[var(--text-primary)]">
                    编辑{' '}
                    {editingMemory.category === 'activeTopic'
                      ? '活跃话题'
                      : editingMemory.category === 'pendingThought'
                        ? '待表达想法'
                        : `成员印象 (${editingMemory.keyOrOldContent})`}
                  </div>
                  {editingMemory.category === 'memberImpression' && (
                    <input
                      type="text"
                      value={editingMemory.targetDraft}
                      onChange={(e) =>
                        setEditingMemory({ ...editingMemory, targetDraft: e.target.value })
                      }
                      placeholder="群友名称"
                      className="h-7 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs text-[var(--text-primary)]"
                    />
                  )}
                  <input
                    type="text"
                    value={editingMemory.contentDraft}
                    onChange={(e) =>
                      setEditingMemory({ ...editingMemory, contentDraft: e.target.value })
                    }
                    placeholder={
                      editingMemory.category === 'memberImpression'
                        ? '特征标签（逗号/顿号分隔）'
                        : '正文内容'
                    }
                    className="h-7 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs text-[var(--text-primary)]"
                  />
                  {editingMemory.category !== 'memberImpression' && (
                    <input
                      type="text"
                      value={editingMemory.extraDraft}
                      onChange={(e) =>
                        setEditingMemory({ ...editingMemory, extraDraft: e.target.value })
                      }
                      placeholder={
                        editingMemory.category === 'activeTopic' ? '待跟进问题（可选）' : '触发动机（可选）'
                      }
                      className="h-7 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs text-[var(--text-primary)]"
                    />
                  )}
                  <div className="flex justify-end gap-1.5">
                    <Button size="sm" variant="ghost" onClick={() => setEditingMemory(null)}>
                      <X className="h-3 w-3" />
                      取消
                    </Button>
                    <Button
                      size="sm"
                      variant="primary"
                      disabled={busy}
                      onClick={() => void handleSaveEditMemory()}
                    >
                      <Check className="h-3 w-3" />
                      保存修改
                    </Button>
                  </div>
                </div>
              )}

              <div className="space-y-1.5">
                {lightMemory?.raw.activeTopics.map((topic, i) => (
                  <div
                    key={`topic-${i}`}
                    className="flex items-start justify-between gap-2 rounded bg-[var(--bg-subtle)]/60 px-2.5 py-1.5"
                  >
                    <div className="min-w-0">
                      <Badge tone="accent" className="mr-1.5">
                        话题
                      </Badge>
                      <span className="break-all">{topic.text}</span>
                      {topic.pendingQuestion && (
                        <div className="mt-0.5 text-[11px] text-[var(--text-muted)]">
                          待问：{topic.pendingQuestion}
                        </div>
                      )}
                    </div>
                    <div className="flex shrink-0 items-center gap-2">
                      <button
                        type="button"
                        disabled={busy}
                        onClick={() =>
                          setEditingMemory({
                            category: 'activeTopic',
                            keyOrOldContent: topic.text,
                            contentDraft: topic.text,
                            targetDraft: '',
                            extraDraft: topic.pendingQuestion || '',
                          })
                        }
                        className="text-[var(--text-muted)] hover:text-[var(--accent)] disabled:opacity-45"
                        aria-label="编辑话题"
                      >
                        <Edit3 className="h-3 w-3" />
                      </button>
                      <button
                        type="button"
                        disabled={busy}
                        onClick={() => handleRemoveMemory('activeTopic', topic.text)}
                        className="text-[var(--text-muted)] hover:text-[var(--danger)] disabled:opacity-45"
                      >
                        移除
                      </button>
                    </div>
                  </div>
                ))}
                {lightMemory?.raw.pendingThoughts.map((thought, i) => (
                  <div
                    key={`thought-${i}`}
                    className="flex items-start justify-between gap-2 rounded bg-[var(--bg-subtle)]/60 px-2.5 py-1.5"
                  >
                    <div className="min-w-0">
                      <Badge tone="neutral" className="mr-1.5">
                        想法
                      </Badge>
                      <span className="break-all">{thought.text}</span>
                      {thought.motivation && (
                        <div className="mt-0.5 text-[11px] text-[var(--text-muted)]">
                          动机：{thought.motivation}
                        </div>
                      )}
                    </div>
                    <div className="flex shrink-0 items-center gap-2">
                      <button
                        type="button"
                        disabled={busy}
                        onClick={() =>
                          setEditingMemory({
                            category: 'pendingThought',
                            keyOrOldContent: thought.text,
                            contentDraft: thought.text,
                            targetDraft: '',
                            extraDraft: thought.motivation || '',
                          })
                        }
                        className="text-[var(--text-muted)] hover:text-[var(--accent)] disabled:opacity-45"
                        aria-label="编辑想法"
                      >
                        <Edit3 className="h-3 w-3" />
                      </button>
                      <button
                        type="button"
                        disabled={busy}
                        onClick={() => handleRemoveMemory('pendingThought', thought.text)}
                        className="text-[var(--text-muted)] hover:text-[var(--danger)] disabled:opacity-45"
                      >
                        移除
                      </button>
                    </div>
                  </div>
                ))}
                {impressionEntries.map(([memberName, info]) => (
                  <div
                    key={`impression-${memberName}`}
                    className="flex items-start justify-between gap-2 rounded bg-[var(--bg-subtle)]/60 px-2.5 py-1.5"
                  >
                    <div className="min-w-0">
                      <Badge tone="warn" className="mr-1.5">
                        印象
                      </Badge>
                      <span className="font-medium text-[var(--text-primary)]">{memberName}</span>
                      <span className="ml-1.5 text-[var(--text-secondary)]">
                        {info.traits && info.traits.length > 0 ? info.traits.join('、') : '暂无标签'}
                      </span>
                      {typeof info.interactionCount === 'number' && (
                        <span className="ml-1.5 text-[11px] text-[var(--text-muted)]">
                          ({info.interactionCount} 次互动)
                        </span>
                      )}
                    </div>
                    <div className="flex shrink-0 items-center gap-2">
                      <button
                        type="button"
                        disabled={busy}
                        onClick={() =>
                          setEditingMemory({
                            category: 'memberImpression',
                            keyOrOldContent: memberName,
                            contentDraft: (info.traits || []).join('，'),
                            targetDraft: memberName,
                            extraDraft: '',
                          })
                        }
                        className="text-[var(--text-muted)] hover:text-[var(--accent)] disabled:opacity-45"
                        aria-label="编辑印象"
                      >
                        <Edit3 className="h-3 w-3" />
                      </button>
                      <button
                        type="button"
                        disabled={busy}
                        onClick={() => handleRemoveMemory('memberImpression', memberName)}
                        className="text-[var(--text-muted)] hover:text-[var(--danger)] disabled:opacity-45"
                      >
                        移除
                      </button>
                    </div>
                  </div>
                ))}
                {totalMemoryCount === 0 && (
                  <div className="text-[11px] text-[var(--text-muted)]">暂无轻记忆条目</div>
                )}
              </div>
            </Card>
          </>
        )}

        {tab === 'recovery' && (
          <>
            <Card className="p-3 space-y-2">
              <div className="flex items-center justify-between">
                <div className="font-semibold text-[var(--text-primary)]">底层会话映射</div>
                <Button size="sm" variant="secondary" disabled={busy} onClick={handleResetSession}>
                  <RefreshCw className="h-3.5 w-3.5" />
                  重置会话映射
                </Button>
              </div>
              <div className="font-mono text-[11px] text-[var(--text-secondary)] break-all">
                {sessionId || '当前无 DSH 映射或由 Harness 托管'}
              </div>
            </Card>

            <Card className="p-3 space-y-2">
              <div className="font-semibold text-[var(--text-primary)]">近期与暂停轮次</div>
              {turns.length === 0 ? (
                <div className="text-[11px] text-[var(--text-muted)]">暂无轮次记录</div>
              ) : (
                <div className="space-y-2">
                  {turns.map((turn) => (
                    <div
                      key={turn.turnId}
                      className="rounded border border-[var(--border-hairline)] p-2.5 space-y-1.5"
                    >
                      <div className="flex items-center justify-between gap-2">
                        <span className="font-mono font-medium">
                          {turn.code || turn.turnId.slice(0, 10)}
                        </span>
                        <Badge tone={turn.pauseReason ? 'danger' : 'neutral'}>
                          {turn.pauseReason || turn.state}
                        </Badge>
                      </div>
                      {turn.pauseDetail && (
                        <div className="text-[11px] text-[var(--warn)] break-all">
                          {turn.pauseDetail}
                        </div>
                      )}
                      {turn.sends && turn.sends.length > 0 && (
                        <div className="text-[11px] text-[var(--text-muted)] tabular-nums">
                          {turn.sends.map((s) => (
                            <span key={s.batchId} className="mr-2">
                              批次 {s.batchId.slice(0, 8)} 已发 {s.sent}/{s.total}
                              {Array.isArray(s.unknown) && s.unknown.length > 0
                                ? ` · 未知 ${s.unknown.length}`
                                : ''}
                              {Array.isArray(s.pending) && s.pending.length > 0
                                ? ` · 待发 ${s.pending.length}`
                                : ''}
                            </span>
                          ))}
                        </div>
                      )}
                      {(turn.pauseReason || turn.state === 'paused') && (
                        <div className="flex items-center gap-2 pt-1">
                          <Button
                            size="sm"
                            variant="primary"
                            disabled={busy}
                            onClick={() => handleResolveTurn(turn, 'retry')}
                          >
                            重试
                          </Button>
                          <Button
                            size="sm"
                            variant="danger"
                            disabled={busy}
                            onClick={() => handleResolveTurn(turn, 'abandon')}
                          >
                            放弃
                          </Button>
                        </div>
                      )}
                    </div>
                  ))}
                </div>
              )}
            </Card>

            <Card className="p-3 space-y-2">
              <div className="flex items-center justify-between">
                <span className="font-semibold text-[var(--text-primary)]">入站消息缺口</span>
                {gaps.some((g) => g.status !== 'recovered') && (
                  <Button size="sm" variant="secondary" disabled={busy} onClick={() => void handleRetryGaps()}>
                    立即重试恢复
                  </Button>
                )}
              </div>
              {gaps.length === 0 ? (
                <div className="text-[11px] text-[var(--text-muted)]">暂无入站缺口记录</div>
              ) : (
                gaps.map((gap) => (
                  <div
                    key={gap.id}
                    className="flex items-center justify-between rounded bg-[var(--bg-subtle)]/60 px-2.5 py-1.5 text-[11px]"
                  >
                    <span className="truncate">{gap.reason || `#${gap.id}`}</span>
                    <Badge tone={gap.status === 'recovered' ? 'success' : 'warn'}>
                      {gap.status}
                    </Badge>
                  </div>
                ))
              )}
            </Card>

            <Card className="p-3 space-y-2">
              <div className="font-semibold text-[var(--text-primary)]">近期发送发件箱</div>
              {outbox.length === 0 ? (
                <div className="text-[11px] text-[var(--text-muted)]">暂无发送批次记录</div>
              ) : (
                outbox.slice(0, 5).map((batch) => (
                  <div
                    key={batch.batchId}
                    className="flex items-center justify-between rounded bg-[var(--bg-subtle)]/60 px-2.5 py-1.5 text-[11px]"
                  >
                    <span className="font-mono">{batch.batchId.slice(0, 12)}</span>
                    <span className="tabular-nums">
                      已发 {batch.sent}/{batch.total}
                      {Array.isArray(batch.unknown) && batch.unknown.length > 0
                        ? ` · 未知 ${batch.unknown.length}`
                        : ''}
                    </span>
                  </div>
                ))
              )}
            </Card>

            <Card className="border-dashed p-3">
              <div className="flex items-center gap-2 font-medium text-[var(--text-primary)]">
                <Eye className="h-3.5 w-3.5 text-[var(--accent)]" />
                <span>会话检视 (#9 预留槽位)</span>
              </div>
              <p className="mt-1 text-[11px] text-[var(--text-muted)]">
                单回合上下文裁剪快照与工具思考链检视面板将在 #9 接入此位置。
              </p>
            </Card>
          </>
        )}

        {tab === 'settings' && (
          <>
            <div className="space-y-3">
              <label className="block space-y-1">
                <span className="font-medium text-[var(--text-primary)]">单会话人设覆盖</span>
                <select
                  value={draftPersona}
                  onChange={(e) => {
                    editGenRef.current.persona += 1;
                    setDraftPersona(e.target.value);
                  }}
                  className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
                >
                  <option value="">（跟随全局默认人设）</option>
                  {personas
                    .filter((p) => p.activeVersion !== null)
                    .map((p) => (
                      <option key={p.id} value={p.id}>
                        {p.name} (v{p.activeVersion})
                      </option>
                    ))}
                </select>
              </label>

              <label className="block space-y-1">
                <span className="font-medium text-[var(--text-primary)]">单会话模型路由</span>
                <select
                  value={draftRoute}
                  onChange={(e) => {
                    editGenRef.current.route += 1;
                    setDraftRoute(e.target.value);
                  }}
                  disabled={!admitted}
                  className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 font-mono text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none disabled:opacity-50"
                >
                  <option value="">（使用全局配置顺序）</option>
                  {routes.map((r) => (
                    <option key={r.id} value={r.id}>
                      {r.id} — {r.model} ({r.health.state})
                    </option>
                  ))}
                </select>
              </label>

              <div className="space-y-1.5">
                <label className="block space-y-1">
                  <span className="font-medium text-[var(--text-primary)]">单会话回合引擎</span>
                  <select
                    value={draftEngine}
                    onChange={(e) => {
                      editGenRef.current.engine += 1;
                      setDraftEngine(e.target.value === 'harness' ? 'harness' : 'dsh');
                    }}
                    disabled={!admitted}
                    className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 font-mono text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none disabled:opacity-50"
                  >
                    <option value="dsh">dsh</option>
                    <option value="harness">harness</option>
                  </select>
                </label>
                <div className="flex flex-wrap gap-2">
                  <Button
                    size="sm"
                    variant="secondary"
                    disabled={!admitted || busy}
                    onClick={handleImmediateEngineSwitch}
                  >
                    立即切换回合引擎（等待排空）
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    disabled={!admitted || busy}
                    onClick={handleResetEngine}
                  >
                    重置为默认
                  </Button>
                </div>
              </div>
            </div>

            <Card className="p-3 space-y-2">
              <div className="flex items-center justify-between">
                <span className="inline-flex items-center gap-1.5 font-medium text-[var(--text-primary)]">
                  <History className="h-3.5 w-3.5 text-[var(--text-secondary)]" />
                  本会话设置历史与撤销
                </span>
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => {
                    const next = !historyOpen;
                    setHistoryOpen(next);
                    if (next) void loadHistory();
                  }}
                >
                  {historyOpen ? '收起' : '查看历史'}
                </Button>
              </div>
              {historyOpen && (
                <div className="space-y-1.5">
                  {historyLoading ? (
                    <div className="text-[11px] text-[var(--text-muted)]">读取变更历史中…</div>
                  ) : historyEntries.length === 0 ? (
                    <div className="text-[11px] text-[var(--text-muted)]">暂无单会话设置历史</div>
                  ) : (
                    historyEntries.slice(0, 10).map((entry) => (
                      <div
                        key={String(entry.entry)}
                        className="flex items-center justify-between gap-2 rounded bg-[var(--bg-subtle)]/60 px-2.5 py-1.5 text-[11px]"
                      >
                        <div className="min-w-0">
                          <div className="font-mono text-[var(--text-primary)]">{entry.setting}</div>
                          <div className="truncate text-[var(--text-muted)]">
                            {JSON.stringify(entry.old)} → {JSON.stringify(entry.new)}
                          </div>
                        </div>
                        <Button
                          size="sm"
                          variant="ghost"
                          disabled={busy}
                          onClick={() => handleUndoSettingEntry(entry)}
                        >
                          撤销
                        </Button>
                      </div>
                    ))
                  )}
                </div>
              )}
            </Card>

            <Card danger className="p-3 space-y-2">
              <div className="flex items-center gap-1.5 font-semibold text-[var(--danger)]">
                <AlertTriangle className="h-3.5 w-3.5" />
                <span>危险操作</span>
              </div>
              <p className="text-[11px] text-[var(--text-secondary)]">
                移出会话会将其从允许列表与实时服务列表移除并立即吊销凭据。
              </p>
              <Button size="sm" variant="danger" disabled={busy} onClick={handleRemoveFromScope}>
                移出会话
              </Button>
            </Card>
          </>
        )}
      </div>

      <footer className="flex shrink-0 items-center justify-between gap-2 border-t border-[var(--border-hairline)] bg-[var(--bg-subtle)]/60 px-4 py-2.5 text-xs">
        {hasAnyDirty ? (
          <>
            <span className="text-[11px] font-medium text-[var(--accent)]">有未保存的更改</span>
            <div className="flex items-center gap-2">
              <Button size="sm" variant="ghost" disabled={busy} onClick={handleDiscardAll}>
                放弃
              </Button>
              <Button size="sm" variant="primary" disabled={busy} onClick={() => void handleSaveAll()}>
                保存
              </Button>
            </div>
          </>
        ) : (
          <div className="ml-auto">
            <Button size="sm" variant="secondary" onClick={() => setInspector(null)}>
              关闭
            </Button>
          </div>
        )}
      </footer>
    </div>
  );
};
