import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  Copy,
  Download,
  Plus,
  RefreshCw,
  RotateCcw,
  Sparkles,
  Star,
  Trash2,
  Users,
  X,
} from 'lucide-react';
import {
  LegacyRolesSummaryResponse,
  PERSONA_UPDATED_EVENT,
  PersonaListResponse,
  WakeWordsResponse,
  assignPersonaDefault,
  createPersona,
  deletePersona,
  duplicatePersona,
  emitPersonaUpdated,
  fetchPersonasList,
  fetchRoles,
  fetchWakeWords,
  importRoleCards,
  undoWakeWords,
  updateRoleInjectLimit,
} from '../../api/her';
import { EmptyState, SearchInput } from '../../components/ConsoleWidgets';
import { Badge, Button, Card, CardHeader } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import { formatDateTimeMs, formatErrorMessage } from '../../utils/format';
import { INPUT_CLASS, isSingleLineWithin, utf16Length } from '../../utils/herHelpers';

export const PersonaLibraryTab: React.FC = () => {
  const { conversations, setInspector, openConfirm, pushToast } = useConsole();

  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [rolesError, setRolesError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [personaData, setPersonaData] = useState<PersonaListResponse | null>(null);
  const [rolesData, setRolesData] = useState<LegacyRolesSummaryResponse | null>(null);
  const [injectLimitInput, setInjectLimitInput] = useState('');
  const [search, setSearch] = useState('');

  // Create form
  const [createOpen, setCreateOpen] = useState(false);
  const [newName, setNewName] = useState('');
  const [newAliases, setNewAliases] = useState<string[]>([]);
  const [newAliasDraft, setNewAliasDraft] = useState('');
  const [newCore, setNewCore] = useState('');
  const [newCompact, setNewCompact] = useState('');
  const [newNotes, setNewNotes] = useState('');
  const [newFailureLine, setNewFailureLine] = useState('');
  const [newNeediness, setNewNeediness] = useState(1);

  // Duplicate inline target
  const [dupTarget, setDupTarget] = useState<{ id: string; sourceName: string } | null>(null);
  const [dupName, setDupName] = useState('');

  // Wake words
  const [wakeConv, setWakeConv] = useState('');
  const [wakeLoading, setWakeLoading] = useState(false);
  const [wakeError, setWakeError] = useState<string | null>(null);
  const [wakeData, setWakeData] = useState<WakeWordsResponse | null>(null);

  const mountedRef = useRef(true);
  const listGenRef = useRef(0);
  const rolesGenRef = useRef(0);
  const wakeGenRef = useRef(0);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const loadRolesSummary = useCallback(async () => {
    const gen = ++rolesGenRef.current;
    setRolesError(null);
    try {
      const rRes = await fetchRoles();
      if (!mountedRef.current || gen !== rolesGenRef.current) return;
      setRolesData(rRes);
      setInjectLimitInput(String(rRes.limits?.injectMaxChars ?? 6000));
    } catch (err) {
      if (!mountedRef.current || gen !== rolesGenRef.current) return;
      setRolesData(null);
      setRolesError(formatErrorMessage(err, '读取遗留角色注入上限失败'));
    }
  }, []);

  const loadAll = useCallback(async () => {
    const gen = ++listGenRef.current;
    setLoading(true);
    setError(null);
    try {
      const pRes = await fetchPersonasList();
      if (!mountedRef.current || gen !== listGenRef.current) return;
      setPersonaData(pRes);
    } catch (err) {
      if (!mountedRef.current || gen !== listGenRef.current) return;
      setError(formatErrorMessage(err, '加载人设库失败'));
    } finally {
      if (mountedRef.current && gen === listGenRef.current) {
        setLoading(false);
      }
    }
    void loadRolesSummary();
  }, [loadRolesSummary]);

  useEffect(() => {
    void loadAll();
  }, [loadAll]);

  useEffect(() => {
    const onUpdated = () => {
      void loadAll();
    };
    window.addEventListener(PERSONA_UPDATED_EVENT, onUpdated);
    return () => window.removeEventListener(PERSONA_UPDATED_EVENT, onUpdated);
  }, [loadAll]);

  const conversationOptions = useMemo(() => {
    const keys = new Set<string>();
    for (const c of conversations) {
      if (c.key) keys.add(c.key);
    }
    if (personaData?.overrides) {
      for (const k of Object.keys(personaData.overrides)) {
        keys.add(k);
      }
    }
    return Array.from(keys);
  }, [conversations, personaData]);

  useEffect(() => {
    if (!wakeConv && conversationOptions.length > 0) {
      setWakeConv(conversationOptions[0]);
    }
  }, [wakeConv, conversationOptions]);

  const loadWakeWords = useCallback(async (conv: string) => {
    const target = conv.trim();
    const gen = ++wakeGenRef.current;
    setWakeData(null);
    setWakeError(null);
    if (!target) {
      setWakeLoading(false);
      return;
    }
    setWakeLoading(true);
    try {
      const res = await fetchWakeWords(target);
      if (!mountedRef.current || gen !== wakeGenRef.current) return;
      setWakeData(res);
    } catch (err) {
      if (!mountedRef.current || gen !== wakeGenRef.current) return;
      setWakeData(null);
      setWakeError(formatErrorMessage(err, '读取唤醒词失败'));
    } finally {
      if (mountedRef.current && gen === wakeGenRef.current) {
        setWakeLoading(false);
      }
    }
  }, []);

  useEffect(() => {
    if (wakeConv) {
      void loadWakeWords(wakeConv);
    }
  }, [wakeConv, loadWakeWords]);

  const overrideCounts = useMemo(() => {
    const counts: Record<string, number> = {};
    if (personaData?.overrides) {
      for (const pid of Object.values(personaData.overrides)) {
        counts[pid] = (counts[pid] ?? 0) + 1;
      }
    }
    return counts;
  }, [personaData]);

  const filteredPersonas = useMemo(() => {
    const items = personaData?.personas ?? [];
    const q = search.trim().toLowerCase();
    if (!q) return items;
    return items.filter(
      (item) => item.name.toLowerCase().includes(q) || item.id.toLowerCase().includes(q),
    );
  }, [personaData, search]);

  const handleAddCreateAlias = () => {
    const candidate = newAliasDraft.trim();
    if (!candidate) return;
    if (/[\r\n]/.test(candidate) || utf16Length(candidate) > 40) {
      pushToast('每个唤醒别名需为单行且不超过 40 个 UTF-16 字符', 'warn');
      return;
    }
    if (newAliases.length >= 10) {
      pushToast('唤醒别名最多 10 个', 'warn');
      return;
    }
    if (!newAliases.includes(candidate)) {
      setNewAliases((prev) => [...prev, candidate]);
    }
    setNewAliasDraft('');
  };

  const handleCreate = async (e: React.FormEvent) => {
    e.preventDefault();
    const trimmedName = newName.trim();
    if (!trimmedName || /[\r\n]/.test(trimmedName) || utf16Length(trimmedName) > 40) {
      pushToast('人设名称不能为空，需为单行且不超过 40 个 UTF-16 字符', 'warn');
      return;
    }
    if (!newCore.trim() || utf16Length(newCore) > 20000) {
      pushToast('人设核心 (core) 不能为空且不超过 20000 个 UTF-16 字符', 'warn');
      return;
    }
    if (utf16Length(newCompact) > 20000 || utf16Length(newNotes) > 20000) {
      pushToast('精简人设与人设资料分别不能超过 20000 个 UTF-16 字符', 'warn');
      return;
    }
    if (newAliases.length > 10) {
      pushToast('唤醒别名最多 10 个', 'warn');
      return;
    }
    if (newFailureLine.trim() && !isSingleLineWithin(newFailureLine, 160)) {
      pushToast('终端失败提示语需为单行且不超过 160 个 UTF-16 字符', 'warn');
      return;
    }
    setBusy(true);
    try {
      const res = await createPersona({
        name: trimmedName,
        core: newCore,
        compact: newCompact,
        notes: newNotes,
        failureLine: newFailureLine.trim() ? newFailureLine : null,
        names: newAliases.length > 0 ? newAliases : [trimmedName],
        traits: { neediness: newNeediness },
      });
      if (!mountedRef.current) return;
      pushToast(`已创建人设「${trimmedName}」(待在右侧抽屉批准激活)`, 'success');
      setCreateOpen(false);
      setNewName('');
      setNewAliases([]);
      setNewAliasDraft('');
      setNewCore('');
      setNewCompact('');
      setNewNotes('');
      setNewFailureLine('');
      emitPersonaUpdated();
      setInspector({ kind: 'persona', id: res.id });
    } catch (err) {
      pushToast(formatErrorMessage(err, '创建人设失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleSetDefault = async (personaId: string | null, label = '更新默认人设') => {
    setBusy(true);
    try {
      const res = await assignPersonaDefault(personaId);
      if (!mountedRef.current) return;
      const failedList = Array.isArray(res.rebriefFailed) ? res.rebriefFailed : [];
      setPersonaData((prev) =>
        prev
          ? {
              ...prev,
              default: res.default ?? null,
              overrides: res.overrides ?? prev.overrides,
              rebriefDue: failedList.map((f) => f.conversation),
            }
          : prev,
      );
      if (failedList.length > 0) {
        const failedNames = failedList.map((f) => f.conversation).join('、');
        pushToast(`${label}已保存，但以下会话重新简报失败：${failedNames}`, 'warn');
      } else {
        pushToast(
          personaId === personaData?.default && label === '重试重新简报'
            ? `已重试重新简报（完成 ${res.rebriefed?.length ?? 0} 个会话）`
            : personaId
              ? '已设为全局默认人设'
              : '已清除全局默认人设',
          'success',
        );
      }
      emitPersonaUpdated();
    } catch (err) {
      pushToast(formatErrorMessage(err, `${label}失败`), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleDuplicate = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!dupTarget) return;
    const targetName = dupName.trim();
    if (!targetName || /[\r\n]/.test(targetName) || utf16Length(targetName) > 40) {
      pushToast('副本名称不能为空，需为单行且不超过 40 个 UTF-16 字符', 'warn');
      return;
    }
    setBusy(true);
    try {
      const res = await duplicatePersona(dupTarget.id, targetName);
      if (!mountedRef.current) return;
      pushToast(`已复制人设「${targetName}」`, 'success');
      setDupTarget(null);
      setDupName('');
      emitPersonaUpdated();
      setInspector({ kind: 'persona', id: res.id });
    } catch (err) {
      pushToast(formatErrorMessage(err, '复制人设失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleDelete = (id: string, name: string) => {
    openConfirm({
      title: `确认删除人设「${name}」？`,
      description: '删除后不可恢复；若该人设仍被设为全局默认或会话覆盖，请先清除分配。',
      confirmLabel: '确认删除',
      danger: true,
      onConfirm: async () => {
        setBusy(true);
        try {
          await deletePersona(id);
          if (!mountedRef.current) return;
          pushToast(`已删除人设「${name}」`, 'success');
          emitPersonaUpdated();
        } catch (err) {
          pushToast(formatErrorMessage(err, '删除人设失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleImportRoles = () => {
    openConfirm({
      title: '确认从角色卡导入人设？',
      description:
        '将一次性扫描并导入 resources/roles/*.md 为已批准人设。导入后该操作不可重复触发。',
      confirmLabel: '开始导入',
      onConfirm: async () => {
        setBusy(true);
        try {
          const res = await importRoleCards();
          if (!mountedRef.current) return;
          pushToast(
            `已导入 ${res.imported.length} 个人设${res.skipped.length > 0 ? `，跳过 ${res.skipped.length} 个` : ''}`,
            'success',
          );
          emitPersonaUpdated();
        } catch (err) {
          pushToast(formatErrorMessage(err, '导入角色卡失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleSaveInjectLimit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!rolesData || rolesError) return;
    const num = Number(injectLimitInput);
    if (!Number.isFinite(num) || num < 1000 || num > 20000) {
      pushToast('注入上限需在 1000 到 20000 字符之间', 'warn');
      return;
    }
    setBusy(true);
    try {
      const res = await updateRoleInjectLimit(Math.round(num));
      if (!mountedRef.current) return;
      setInjectLimitInput(String(res.maxInjectChars));
      pushToast(`已更新角色注入上限为 ${res.maxInjectChars} 字符`, 'success');
      await loadRolesSummary();
    } catch (err) {
      pushToast(formatErrorMessage(err, '更新注入上限失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleUndoWakeLog = (targetConv: string, logId: number) => {
    if (!targetConv || targetConv !== wakeConv) return;
    openConfirm({
      title: `确认撤销 ${targetConv} 的唤醒词变更 #${logId}？`,
      description: '将回滚该次唤醒词新增或移除操作；若后续已有冲突改动，后端会拒绝撤销。',
      confirmLabel: '撤销变更',
      onConfirm: async () => {
        setBusy(true);
        try {
          await undoWakeWords(targetConv, logId);
          if (!mountedRef.current) return;
          pushToast(`已撤销 ${targetConv} 唤醒词记录 #${logId}`, 'success');
          await loadWakeWords(targetConv);
        } catch (err) {
          pushToast(formatErrorMessage(err, '撤销唤醒词变更失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const wakeMatched = wakeData !== null && wakeData.conversation === wakeConv;

  return (
    <div className="space-y-4">
      {personaData?.rebriefDue && personaData.rebriefDue.length > 0 && (
        <div className="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-amber-300 bg-[var(--warn-soft)] px-3.5 py-2 text-xs text-[var(--warn)] dark:border-amber-900">
          <span>
            以下会话人设或摘要重新简报尚待完成：{personaData.rebriefDue.join('、')}
          </span>
          <Button
            size="sm"
            variant="secondary"
            disabled={busy || loading}
            onClick={() => void handleSetDefault(personaData.default ?? null, '重试重新简报')}
          >
            <RefreshCw className="h-3 w-3" />
            重试重新简报
          </Button>
        </div>
      )}

      <Card>
        <CardHeader
          title={
            <span className="flex items-center gap-2">
              <Users className="h-4 w-4 text-[var(--accent)]" />
              <span>人设库</span>
              {personaData && <Badge tone="neutral">{personaData.personas.length}</Badge>}
            </span>
          }
          subtitle="点击任意人设行在右侧抽屉编辑 core / compact / notes、审批版本或调整粘人度。"
          actions={
            <div className="flex flex-wrap items-center gap-2">
              <Button
                size="sm"
                variant="secondary"
                disabled={busy || personaData?.importedRolesAt != null}
                onClick={handleImportRoles}
              >
                <Download className="h-3.5 w-3.5" />
                {personaData?.importedRolesAt ? '已导入角色卡' : '从角色卡导入'}
              </Button>
              <Button size="sm" variant="primary" onClick={() => setCreateOpen((v) => !v)}>
                <Plus className="h-3.5 w-3.5" />
                新建人设
              </Button>
              <Button size="sm" variant="ghost" disabled={loading} onClick={() => void loadAll()}>
                <RefreshCw className={`h-3.5 w-3.5 ${loading ? 'animate-spin' : ''}`} />
                刷新
              </Button>
            </div>
          }
        />

        <div className="space-y-3 p-4">
          {createOpen && (
            <Card className="bg-[var(--bg-subtle)]/40 p-3.5">
              <form className="space-y-2.5" onSubmit={(e) => void handleCreate(e)}>
                <div className="flex items-center justify-between">
                  <span className="text-xs font-semibold text-[var(--text-primary)]">新建人设</span>
                  <Button size="sm" variant="ghost" onClick={() => setCreateOpen(false)}>
                    收起
                  </Button>
                </div>
                <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                  <input
                    type="text"
                    maxLength={40}
                    disabled={busy}
                    value={newName}
                    onChange={(e) => setNewName(e.target.value)}
                    placeholder="人设名称（单行，≤ 40 字符）*"
                    aria-label="新建人设名称"
                    className={INPUT_CLASS}
                  />
                  <select
                    disabled={busy}
                    value={newNeediness}
                    onChange={(e) => setNewNeediness(Number(e.target.value))}
                    aria-label="新建人设粘人度"
                    className={INPUT_CLASS}
                  >
                    <option value={0}>粘人度 0 · 独立克制</option>
                    <option value={1}>粘人度 1 · 适度回应</option>
                    <option value={2}>粘人度 2 · 较常惦记</option>
                    <option value={3}>粘人度 3 · 高度依赖</option>
                  </select>
                </div>

                <div className="space-y-1.5">
                  <div className="flex flex-wrap gap-1.5">
                    {newAliases.map((alias, idx) => (
                      <span
                        key={`${alias}-${idx}`}
                        className="inline-flex items-center gap-1 rounded border border-[var(--border-hairline)] bg-[var(--bg-surface)] px-2 py-0.5 text-[11px]"
                      >
                        <span>{alias}</span>
                        <button
                          type="button"
                          disabled={busy}
                          aria-label={`移除新建别名 ${alias}`}
                          onClick={() =>
                            setNewAliases((prev) => prev.filter((_, i) => i !== idx))
                          }
                          className="text-[var(--text-muted)] hover:text-[var(--danger)] disabled:opacity-50"
                        >
                          <X className="h-3 w-3" />
                        </button>
                      </span>
                    ))}
                  </div>
                  <div className="flex gap-1.5">
                    <input
                      type="text"
                      maxLength={40}
                      disabled={busy || newAliases.length >= 10}
                      value={newAliasDraft}
                      onChange={(e) => setNewAliasDraft(e.target.value)}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter') {
                          e.preventDefault();
                          handleAddCreateAlias();
                        }
                      }}
                      placeholder="唤醒别名（最多 10 个，可逐个添加；留空默认使用人设名）"
                      aria-label="添加新建人设别名"
                      className={INPUT_CLASS}
                    />
                    <Button
                      size="sm"
                      variant="secondary"
                      disabled={busy || newAliases.length >= 10 || !newAliasDraft.trim()}
                      onClick={handleAddCreateAlias}
                    >
                      添加别名
                    </Button>
                  </div>
                </div>

                <textarea
                  rows={4}
                  maxLength={20000}
                  disabled={busy}
                  value={newCore}
                  onChange={(e) => setNewCore(e.target.value)}
                  placeholder="人设核心 (core, ≤ 20000 字符) *"
                  className={`${INPUT_CLASS} font-mono`}
                />
                <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                  <textarea
                    rows={3}
                    maxLength={20000}
                    disabled={busy}
                    value={newCompact}
                    onChange={(e) => setNewCompact(e.target.value)}
                    placeholder="精简人设 (compact, ≤ 20000 字符，可选)"
                    className={`${INPUT_CLASS} font-mono`}
                  />
                  <textarea
                    rows={3}
                    maxLength={20000}
                    disabled={busy}
                    value={newNotes}
                    onChange={(e) => setNewNotes(e.target.value)}
                    placeholder="人设资料 (notes, ≤ 20000 字符，可选)"
                    className={`${INPUT_CLASS} font-mono`}
                  />
                </div>
                <input
                  type="text"
                  maxLength={160}
                  disabled={busy}
                  value={newFailureLine}
                  onChange={(e) => setNewFailureLine(e.target.value)}
                  placeholder="终端失败提示语 (failureLine, 单行 ≤ 160 字符，可选)"
                  aria-label="新建人设终端失败提示语"
                  className={INPUT_CLASS}
                />
                <div className="flex justify-end gap-2">
                  <Button size="sm" variant="primary" type="submit" disabled={busy}>
                    创建人设
                  </Button>
                </div>
              </form>
            </Card>
          )}

          {dupTarget && (
            <Card className="bg-[var(--bg-subtle)]/40 p-3">
              <form className="flex flex-wrap items-center gap-2" onSubmit={(e) => void handleDuplicate(e)}>
                <span className="text-xs font-medium text-[var(--text-primary)]">
                  复制「{dupTarget.sourceName}」为：
                </span>
                <input
                  type="text"
                  maxLength={40}
                  value={dupName}
                  onChange={(e) => setDupName(e.target.value)}
                  placeholder="输入新人设名称…"
                  aria-label="输入副本名称"
                  className={`${INPUT_CLASS} max-w-xs`}
                />
                <Button size="sm" variant="primary" type="submit" disabled={busy}>
                  确认复制
                </Button>
                <Button size="sm" variant="ghost" onClick={() => setDupTarget(null)}>
                  取消
                </Button>
              </form>
            </Card>
          )}

          <div className="flex flex-wrap items-center justify-between gap-2">
            <div className="w-full max-w-xs">
              <SearchInput value={search} onChange={setSearch} placeholder="筛选人设名称或 ID…" />
            </div>
            {personaData?.importedRolesAt && (
              <span className="text-[11px] text-[var(--text-muted)]">
                角色卡已导入于 {formatDateTimeMs(personaData.importedRolesAt)}
              </span>
            )}
          </div>

          {error && (
            <div className="rounded-md border border-rose-300 bg-[var(--danger-soft)] p-3 text-xs text-[var(--danger)]">
              {error}
            </div>
          )}

          {!loading && filteredPersonas.length === 0 ? (
            <EmptyState
              title="暂无人设"
              description="可点击右上角「新建人设」或从已有角色卡一次性导入。"
            />
          ) : (
            <div className="overflow-x-auto rounded-md border border-[var(--border-hairline)]">
              <table className="w-full border-collapse text-left text-xs">
                <thead className="bg-[var(--bg-subtle)] text-[11px] text-[var(--text-muted)]">
                  <tr>
                    <th className="px-3 py-2 font-medium">名称</th>
                    <th className="px-3 py-2 font-medium">ID</th>
                    <th className="px-3 py-2 font-medium">激活版本</th>
                    <th className="px-3 py-2 font-medium">待审批</th>
                    <th className="px-3 py-2 font-medium">分配情况</th>
                    <th className="px-3 py-2 text-right font-medium">操作</th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-[var(--border-hairline)]">
                  {filteredPersonas.map((p) => {
                    const isDefault = personaData?.default === p.id;
                    const overrideCount = overrideCounts[p.id] ?? 0;
                    return (
                      <tr
                        key={p.id}
                        tabIndex={0}
                        onClick={() => setInspector({ kind: 'persona', id: p.id })}
                        onKeyDown={(e) => {
                          if (e.key === 'Enter' || e.key === ' ') {
                            e.preventDefault();
                            setInspector({ kind: 'persona', id: p.id });
                          }
                        }}
                        className="cursor-pointer hover:bg-[var(--bg-hover)]/60 focus:bg-[var(--bg-hover)]/60 focus:outline-none"
                      >
                        <td className="px-3 py-2 font-medium text-[var(--text-primary)]">{p.name}</td>
                        <td className="px-3 py-2 font-mono text-[11px] text-[var(--text-muted)]">
                          {p.id}
                        </td>
                        <td className="px-3 py-2">
                          {p.activeVersion != null ? (
                            <Badge tone="success">v{p.activeVersion}</Badge>
                          ) : (
                            <Badge tone="warn">未批准</Badge>
                          )}
                          {p.latestVersion != null && p.latestVersion !== p.activeVersion && (
                            <span className="ml-1.5 text-[11px] text-[var(--text-muted)]">
                              最新 v{p.latestVersion}
                            </span>
                          )}
                        </td>
                        <td className="px-3 py-2">
                          {p.pendingVersions.length > 0 ? (
                            <Badge tone="warn">v{p.pendingVersions.join(', v')}</Badge>
                          ) : (
                            <span className="text-[var(--text-muted)]">—</span>
                          )}
                        </td>
                        <td className="px-3 py-2">
                          <div className="flex flex-wrap items-center gap-1">
                            {isDefault && <Badge tone="accent">全局默认</Badge>}
                            {overrideCount > 0 && (
                              <Badge tone="neutral">{overrideCount} 个会话</Badge>
                            )}
                            {!isDefault && overrideCount === 0 && (
                              <span className="text-[var(--text-muted)]">未分配</span>
                            )}
                          </div>
                        </td>
                        <td
                          className="px-3 py-2 text-right"
                          onClick={(e) => e.stopPropagation()}
                          onKeyDown={(e) => e.stopPropagation()}
                        >
                          <div className="inline-flex items-center gap-1">
                            <Button
                              size="sm"
                              variant="ghost"
                              disabled={busy || p.activeVersion == null}
                              onClick={() => void handleSetDefault(isDefault ? null : p.id)}
                            >
                              <Star className="h-3 w-3" />
                              {isDefault ? '取消默认' : '设默认'}
                            </Button>
                            <Button
                              size="sm"
                              variant="ghost"
                              disabled={busy}
                              onClick={() => {
                                setDupTarget({ id: p.id, sourceName: p.name });
                                setDupName(`${p.name} 副本`);
                              }}
                            >
                              <Copy className="h-3 w-3" />
                              复制
                            </Button>
                            <Button
                              size="sm"
                              variant="ghost"
                              aria-label={`删除人设 ${p.name}`}
                              disabled={busy}
                              onClick={() => handleDelete(p.id, p.name)}
                            >
                              <Trash2 className="h-3 w-3 text-[var(--danger)]" />
                            </Button>
                          </div>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </Card>

      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader
            title="唤醒词与撤销记录"
            subtitle="查看每个会话生效中的唤醒词（人设别名 + 关键字 - 屏蔽名）及最近 20 次变更撤销。"
            actions={
              <Button
                size="sm"
                variant="ghost"
                aria-label="刷新唤醒词记录"
                disabled={wakeLoading || !wakeConv}
                onClick={() => void loadWakeWords(wakeConv)}
              >
                <RefreshCw className={`h-3.5 w-3.5 ${wakeLoading ? 'animate-spin' : ''}`} />
                刷新
              </Button>
            }
          />
          <div className="space-y-3 p-4 text-xs">
            <label className="block space-y-1">
              <span className="text-[11px] font-medium text-[var(--text-secondary)]">目标会话</span>
              <select
                value={wakeConv}
                aria-label="选择查看唤醒词的会话"
                onChange={(e) => {
                  setWakeData(null);
                  setWakeConv(e.target.value);
                }}
                className={INPUT_CLASS}
              >
                {conversationOptions.length === 0 && <option value="">暂无可选会话</option>}
                {conversationOptions.map((key) => {
                  const found = conversations.find((c) => c.key === key);
                  return (
                    <option key={key} value={key}>
                      {found?.name ? `${found.name} (${key})` : key}
                    </option>
                  );
                })}
              </select>
            </label>

            {wakeError && (
              <div className="rounded border border-rose-300 bg-[var(--danger-soft)] p-2.5 text-[11px] text-[var(--danger)]">
                {wakeError}
              </div>
            )}

            {wakeLoading && !wakeMatched && (
              <div className="text-[11px] text-[var(--text-muted)]">正在加载 {wakeConv} 的唤醒词…</div>
            )}

            {wakeMatched && wakeData && (
              <>
                <div className="grid grid-cols-1 gap-2 rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/40 p-2.5">
                  <div>
                    <span className="text-[11px] text-[var(--text-muted)]">生效唤醒词：</span>
                    <span className="font-medium text-[var(--text-primary)]">
                      {wakeData.effective.length > 0 ? wakeData.effective.join('、') : '（无）'}
                    </span>
                  </div>
                  <div className="flex flex-wrap gap-3 text-[11px] text-[var(--text-secondary)]">
                    <span>人设名：{wakeData.personaNames.join('、') || '—'}</span>
                    <span>自定义词：{wakeData.keywords.join('、') || '—'}</span>
                    <span>已排除：{wakeData.dropNames.join('、') || '—'}</span>
                  </div>
                </div>

                <div className="space-y-1.5">
                  <div className="text-[11px] font-medium text-[var(--text-secondary)]">变更历史</div>
                  {wakeData.log.length === 0 ? (
                    <div className="text-[11px] text-[var(--text-muted)]">暂无唤醒词变更记录</div>
                  ) : (
                    wakeData.log
                      .slice()
                      .reverse()
                      .map((entry) => (
                        <div
                          key={entry.id}
                          className="flex flex-wrap items-center justify-between gap-2 rounded border border-[var(--border-hairline)] px-2.5 py-1.5"
                        >
                          <div className="min-w-0 space-y-0.5 text-[11px]">
                            <div className="flex items-center gap-1.5">
                              <span className="font-mono font-semibold">#{entry.id}</span>
                              <Badge tone="neutral">{entry.by}</Badge>
                              <span className="text-[var(--text-muted)]">
                                {formatDateTimeMs(entry.at)}
                              </span>
                            </div>
                            <div className="text-[var(--text-secondary)]">
                              {entry.added.keywords.length > 0 && (
                                <span className="mr-2">+词: {entry.added.keywords.join(', ')}</span>
                              )}
                              {entry.removed.keywords.length > 0 && (
                                <span className="mr-2">-词: {entry.removed.keywords.join(', ')}</span>
                              )}
                              {entry.added.dropNames.length > 0 && (
                                <span className="mr-2">
                                  +屏蔽: {entry.added.dropNames.join(', ')}
                                </span>
                              )}
                              {entry.removed.dropNames.length > 0 && (
                                <span>-屏蔽: {entry.removed.dropNames.join(', ')}</span>
                              )}
                            </div>
                          </div>
                          {entry.by !== 'undo' && (
                            <Button
                              size="sm"
                              variant="ghost"
                              disabled={busy || !wakeMatched}
                              onClick={() => handleUndoWakeLog(wakeData.conversation, entry.id)}
                            >
                              <RotateCcw className="h-3 w-3" />
                              撤销
                            </Button>
                          )}
                        </div>
                      ))
                  )}
                </div>
              </>
            )}
          </div>
        </Card>

        <div className="space-y-4">
          <Card>
            <CardHeader
              title="角色卡注入上限（管理员代理 / 兼容层）"
              subtitle="人设库已统一承担所有版本化编辑与分配；此处仅保留角色文本注入长度限制。"
            />
            <div className="space-y-3 p-4 text-xs">
              {rolesError && (
                <div className="flex items-center justify-between gap-2 rounded border border-rose-300 bg-[var(--danger-soft)] p-2.5 text-[11px] text-[var(--danger)]">
                  <span>{rolesError}</span>
                  <Button size="sm" variant="secondary" onClick={() => void loadRolesSummary()}>
                    重试
                  </Button>
                </div>
              )}

              <form className="flex flex-wrap items-end gap-2" onSubmit={(e) => void handleSaveInjectLimit(e)}>
                <label className="flex-1 space-y-1">
                  <span className="text-[11px] text-[var(--text-secondary)]">
                    最大注入字符数 (1000–20000)
                  </span>
                  <input
                    type="number"
                    min={1000}
                    max={20000}
                    disabled={!rolesData || Boolean(rolesError)}
                    value={injectLimitInput}
                    onChange={(e) => setInjectLimitInput(e.target.value)}
                    className={INPUT_CLASS}
                  />
                </label>
                <Button
                  size="sm"
                  variant="secondary"
                  type="submit"
                  disabled={busy || !rolesData || Boolean(rolesError)}
                >
                  保存上限
                </Button>
              </form>
              {rolesData && (
                <div className="text-[11px] text-[var(--text-muted)]">
                  当前共有 {rolesData.roles.length} 张遗留角色卡文件
                  {rolesData.current ? `，管理员代理模式当前选用「${rolesData.current}」` : ''}。
                </div>
              )}
            </div>
          </Card>

          <Card className="border-dashed">
            <CardHeader
              title={
                <span className="flex items-center gap-2">
                  <Sparkles className="h-4 w-4 text-[var(--accent)]" />
                  <span>人设建议 (#31 预留槽位)</span>
                </span>
              }
              subtitle="自动化/手动人设微调提案审阅列表将在 #31 接入此处。"
            />
            <div className="p-4 text-xs text-[var(--text-muted)]">
              当前已为每个人设维护好 editMode（auto / manual）与每周修改频率、可编辑段落等自演化策略；待 #31 后端提案队列完成后，将在此卡片展示待审人设建议。
            </div>
          </Card>
        </div>
      </div>
    </div>
  );
};
