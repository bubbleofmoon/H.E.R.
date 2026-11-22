import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { CheckCircle2, Copy, History, Plus, RefreshCw, Star, Trash2, X } from 'lucide-react';
import {
  PERSONA_UPDATED_EVENT,
  PersonaAutoPart,
  PersonaItemResponse,
  PersonaListResponse,
  PersonaVersionDetail,
  approvePersonaVersion,
  assignPersonaDefault,
  deletePersona,
  duplicatePersona,
  emitPersonaUpdated,
  fetchPersonaItem,
  fetchPersonasList,
  updatePersona,
} from '../api/her';
import { useConsole } from '../stores/consoleStore';
import { formatDateTimeMs, formatErrorMessage } from '../utils/format';
import { INPUT_CLASS, utf16Length } from '../utils/herHelpers';
import { Badge, Button, Card, Switch, Tabs } from './Primitives';

const AUTO_PARTS: { id: PersonaAutoPart; label: string }[] = [
  { id: 'core', label: '人设核心 (core)' },
  { id: 'notes', label: '人设资料 (notes)' },
  { id: 'compact', label: '精简人设 (compact)' },
  { id: 'names', label: '别名/唤醒名 (names)' },
  { id: 'traits', label: '性格参数 (traits)' },
];

const NEEDINESS_LABELS: Record<number, string> = {
  0: '0 · 独立克制',
  1: '1 · 适度回应',
  2: '2 · 较常惦记',
  3: '3 · 高度依赖',
};

interface PersonaDraft {
  name: string;
  core: string;
  notes: string;
  compact: string;
  names: string[];
  neediness: number;
  editMode: 'auto' | 'manual';
  parts: PersonaAutoPart[];
  perWeek: number;
  monthlyDriftPause: boolean;
  notify: boolean;
}

function versionToDraft(v: PersonaVersionDetail): PersonaDraft {
  return {
    name: v.name ?? '',
    core: v.core ?? '',
    notes: v.notes ?? '',
    compact: v.compact ?? '',
    names: Array.isArray(v.names) ? [...v.names] : [],
    neediness: typeof v.traits?.neediness === 'number' ? v.traits.neediness : 1,
    editMode: v.editMode === 'manual' ? 'manual' : 'auto',
    parts: Array.isArray(v.autoRules?.parts) ? [...v.autoRules.parts] : ['notes'],
    perWeek: typeof v.autoRules?.perWeek === 'number' ? v.autoRules.perWeek : 3,
    monthlyDriftPause: v.autoRules?.monthlyDriftPause !== false,
    notify: v.autoRules?.notify !== false,
  };
}

function normalizeDraftForCompare(d: PersonaDraft): string {
  return JSON.stringify({
    name: d.name,
    core: d.core,
    notes: d.notes,
    compact: d.compact,
    names: d.names,
    neediness: d.neediness,
    editMode: d.editMode,
    parts: [...d.parts].sort(),
    perWeek: d.perWeek,
    monthlyDriftPause: d.monthlyDriftPause,
    notify: d.notify,
  });
}

function isSingleLineWithin(value: string, maxUnits: number): boolean {
  return !/[\r\n]/.test(value) && utf16Length(value) <= maxUnits;
}

export const PersonaInspector: React.FC<{ personaId: string }> = ({ personaId }) => {
  const { setInspector, setInspectorDirty, openConfirm, pushToast } = useConsole();

  const [tab, setTab] = useState<'edit' | 'versions'>('edit');
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [detail, setDetail] = useState<PersonaItemResponse | null>(null);
  const [listInfo, setListInfo] = useState<PersonaListResponse | null>(null);
  const [savedDraft, setSavedDraft] = useState<PersonaDraft | null>(null);
  const [draft, setDraft] = useState<PersonaDraft | null>(null);
  const [aliasInput, setAliasInput] = useState('');
  const [dupName, setDupName] = useState('');
  const [dupOpen, setDupOpen] = useState(false);

  const mountedRef = useRef(true);
  const loadGenRef = useRef(0);
  const draftGenRef = useRef(0);
  const dirtyRef = useRef(false);
  const deletedRef = useRef(false);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      setInspectorDirty(false);
    };
  }, [setInspectorDirty]);

  const loadPersona = useCallback(
    async (preserveDraftIfDirty = false) => {
      const gen = ++loadGenRef.current;
      setLoading(true);
      setError(null);
      try {
        const [itemRes, listRes] = await Promise.all([
          fetchPersonaItem(personaId),
          fetchPersonasList().catch(() => null),
        ]);
        if (!mountedRef.current || gen !== loadGenRef.current) return;
        setDetail(itemRes);
        if (listRes) setListInfo(listRes);

        const versions = itemRes.persona.versions ?? [];
        const latest = versions[versions.length - 1] ?? null;
        if (latest) {
          const nextSaved = versionToDraft(latest);
          setSavedDraft(nextSaved);
          if (!preserveDraftIfDirty || !dirtyRef.current) {
            setDraft(nextSaved);
          }
          setDupName(`${latest.name} 副本`);
        }
      } catch (err) {
        if (!mountedRef.current || gen !== loadGenRef.current) return;
        setError(formatErrorMessage(err, '读取人设详情失败'));
      } finally {
        if (mountedRef.current && gen === loadGenRef.current) {
          setLoading(false);
        }
      }
    },
    [personaId],
  );

  useEffect(() => {
    deletedRef.current = false;
    void loadPersona(false);
  }, [loadPersona]);

  useEffect(() => {
    const onUpdated = () => {
      if (deletedRef.current) return;
      void loadPersona(true);
    };
    window.addEventListener(PERSONA_UPDATED_EVENT, onUpdated);
    return () => window.removeEventListener(PERSONA_UPDATED_EVENT, onUpdated);
  }, [loadPersona]);

  const isDirty = useMemo(() => {
    if (!savedDraft || !draft) return false;
    return normalizeDraftForCompare(savedDraft) !== normalizeDraftForCompare(draft);
  }, [savedDraft, draft]);

  useEffect(() => {
    dirtyRef.current = isDirty;
    setInspectorDirty(isDirty);
  }, [isDirty, setInspectorDirty]);

  const updateDraft = (updater: (prev: PersonaDraft) => PersonaDraft) => {
    draftGenRef.current += 1;
    setDraft((prev) => (prev ? updater(prev) : prev));
  };

  const versions = useMemo(() => detail?.persona.versions ?? [], [detail]);
  const activeVersionObj = useMemo(() => {
    const activeNum = detail?.persona.activeVersion;
    if (activeNum == null) return null;
    return versions.find((v) => v.version === activeNum) ?? null;
  }, [detail, versions]);
  const latestVersionObj = versions.length > 0 ? versions[versions.length - 1] : null;
  const isDefault = listInfo?.default === personaId;
  const assignedConversations = useMemo(() => {
    if (!listInfo?.overrides) return [];
    return Object.entries(listInfo.overrides)
      .filter(([, pid]) => pid === personaId)
      .map(([conv]) => conv);
  }, [listInfo, personaId]);

  const handleAddAlias = () => {
    if (!draft) return;
    const candidate = aliasInput.trim();
    if (!candidate) return;
    if (!isSingleLineWithin(candidate, 40)) {
      pushToast('每个唤醒别名需为单行且不超过 40 个 UTF-16 字符', 'warn');
      return;
    }
    if (draft.names.length >= 10) {
      pushToast('唤醒别名最多 10 个', 'warn');
      return;
    }
    if (draft.names.includes(candidate)) {
      pushToast('该别名已存在', 'info');
      return;
    }
    updateDraft((prev) => ({ ...prev, names: [...prev.names, candidate] }));
    setAliasInput('');
  };

  const handleSave = async () => {
    if (!draft || busy) return;
    const trimmedName = draft.name.trim();
    if (!trimmedName || !isSingleLineWithin(trimmedName, 40)) {
      pushToast('人设名称不能为空，需为单行且不超过 40 个 UTF-16 字符', 'warn');
      return;
    }
    if (!draft.core.trim() || utf16Length(draft.core) > 20000) {
      pushToast('人设核心 (core) 不能为空且不超过 20000 个 UTF-16 字符', 'warn');
      return;
    }
    if (utf16Length(draft.compact) > 20000 || utf16Length(draft.notes) > 20000) {
      pushToast('精简人设与人设资料分别不能超过 20000 个 UTF-16 字符', 'warn');
      return;
    }
    if (draft.names.length > 10 || draft.names.some((n) => !n.trim() || !isSingleLineWithin(n, 40))) {
      pushToast('唤醒别名最多 10 个，且每项为单行不超过 40 个 UTF-16 字符', 'warn');
      return;
    }

    const saveStartDraftGen = draftGenRef.current;
    const submittedDraft = draft;
    setBusy(true);
    try {
      const res = await updatePersona({
        id: personaId,
        name: trimmedName,
        core: submittedDraft.core,
        notes: submittedDraft.notes,
        compact: submittedDraft.compact,
        names: submittedDraft.names,
        traits: { neediness: submittedDraft.neediness },
        editMode: submittedDraft.editMode,
        autoRules: {
          parts: submittedDraft.parts,
          perWeek: Math.max(0, Math.min(20, Math.round(submittedDraft.perWeek))),
          monthlyDriftPause: submittedDraft.monthlyDriftPause,
          notify: submittedDraft.notify,
        },
      });
      if (!mountedRef.current) return;
      if (res.unchanged) {
        pushToast('内容与最新版本一致，未生成新版本', 'info');
      } else if (res.activeVersion === res.version) {
        pushToast(`已保存为 v${res.version} 并自动激活`, 'success');
      } else {
        pushToast(`已保存为待审核版本 v${res.version}（请在版本历史中批准激活）`, 'info');
      }
      if (draftGenRef.current === saveStartDraftGen) {
        setSavedDraft(submittedDraft);
        dirtyRef.current = false;
        setInspectorDirty(false);
      }
      emitPersonaUpdated();
    } catch (err) {
      pushToast(formatErrorMessage(err, '保存人设失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleApprove = (version: number, alreadyApproved: boolean) => {
    openConfirm({
      title: alreadyApproved ? `确认切换并激活 v${version}？` : `确认批准并激活版本 v${version}？`,
      description: alreadyApproved
        ? '将当前激活人设版本回滚/切换到该已批准版本。'
        : '首次批准将检查 core 与 compact 令牌预算，通过后立即设为当前激活版本。',
      confirmLabel: alreadyApproved ? '切换激活' : '批准激活',
      onConfirm: async () => {
        setBusy(true);
        try {
          await approvePersonaVersion(personaId, version);
          if (!mountedRef.current) return;
          pushToast(`人设版本 v${version} 已激活`, 'success');
          emitPersonaUpdated();
        } catch (err) {
          pushToast(formatErrorMessage(err, '批准版本失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleToggleDefault = async () => {
    setBusy(true);
    try {
      const res = await assignPersonaDefault(isDefault ? null : personaId);
      if (!mountedRef.current) return;
      if (Array.isArray(res.rebriefFailed) && res.rebriefFailed.length > 0) {
        const failedNames = res.rebriefFailed.map((f) => f.conversation).join('、');
        pushToast(`默认人设已更新，但以下会话重新简报失败：${failedNames}`, 'warn');
      } else {
        pushToast(isDefault ? '已清除全局默认人设' : '已设为全局默认人设', 'success');
      }
      emitPersonaUpdated();
    } catch (err) {
      pushToast(formatErrorMessage(err, '更新默认人设失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const executeDuplicate = async (targetName: string) => {
    setBusy(true);
    try {
      const res = await duplicatePersona(personaId, targetName);
      if (!mountedRef.current) return;
      pushToast(`已复制人设「${targetName}」`, 'success');
      setDupOpen(false);
      dirtyRef.current = false;
      setInspectorDirty(false);
      emitPersonaUpdated();
      setInspector({ kind: 'persona', id: res.id });
    } catch (err) {
      pushToast(formatErrorMessage(err, '复制人设失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleDuplicate = async (e: React.FormEvent) => {
    e.preventDefault();
    const targetName = dupName.trim();
    if (!targetName || !isSingleLineWithin(targetName, 40)) {
      pushToast('副本名称不能为空，需为单行且不超过 40 个 UTF-16 字符', 'warn');
      return;
    }
    if (isDirty) {
      openConfirm({
        title: '丢弃未保存修改并切换到副本？',
        description: '复制人设仅基于已保存版本创建；继续将丢弃当前抽屉未保存的修改。',
        confirmLabel: '复制并切换',
        danger: true,
        onConfirm: () => executeDuplicate(targetName),
      });
      return;
    }
    await executeDuplicate(targetName);
  };

  const handleLoadVersionDraft = (v: PersonaVersionDetail) => {
    const applyLoad = () => {
      draftGenRef.current += 1;
      setDraft(versionToDraft(v));
      setTab('edit');
      pushToast(`已将 v${v.version} 载入编辑草稿（尚未保存）`, 'info');
    };
    if (isDirty) {
      openConfirm({
        title: `丢弃未保存修改并载入 v${v.version}？`,
        description: '当前编辑草稿存在尚未保存的修改，载入历史版本将会覆盖它们。',
        confirmLabel: '覆盖草稿',
        danger: true,
        onConfirm: applyLoad,
      });
      return;
    }
    applyLoad();
  };

  const handleDelete = () => {
    openConfirm({
      title: `确认删除人设「${latestVersionObj?.name || personaId}」？`,
      description:
        isDefault || assignedConversations.length > 0
          ? '该人设当前仍有全局默认或会话分配，后端会拒绝删除，请先清除分配。'
          : '删除后不可恢复该人设的所有历史版本。',
      confirmLabel: '确认删除',
      danger: true,
      onConfirm: async () => {
        setBusy(true);
        try {
          await deletePersona(personaId);
          deletedRef.current = true;
          if (!mountedRef.current) return;
          pushToast('已删除人设', 'success');
          dirtyRef.current = false;
          setInspectorDirty(false);
          setInspector(null);
          emitPersonaUpdated();
        } catch (err) {
          pushToast(formatErrorMessage(err, '删除人设失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const togglePart = (part: PersonaAutoPart) => {
    updateDraft((prev) => {
      const exists = prev.parts.includes(part);
      return {
        ...prev,
        parts: exists ? prev.parts.filter((p) => p !== part) : [...prev.parts, part],
      };
    });
  };

  if (loading && !detail) {
    return <div className="p-4 text-xs text-[var(--text-muted)]">正在加载人设详情…</div>;
  }

  if (error || !detail || !draft) {
    return (
      <div className="space-y-3 p-4 text-xs">
        <div className="rounded-md border border-rose-300 bg-[var(--danger-soft)] p-3 text-[var(--danger)]">
          {error || '未能加载人设数据'}
        </div>
        <Button size="sm" variant="secondary" onClick={() => void loadPersona(true)}>
          <RefreshCw className="h-3.5 w-3.5" />
          重试
        </Button>
      </div>
    );
  }

  const budgets = detail.budgets;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="border-b border-[var(--border-hairline)] px-4 pt-2">
        <div className="mb-2 flex flex-wrap items-center gap-1.5">
          {detail.persona.activeVersion != null ? (
            <Badge tone="success">已激活 v{detail.persona.activeVersion}</Badge>
          ) : (
            <Badge tone="warn">尚无已批准版本</Badge>
          )}
          {isDefault && <Badge tone="accent">全局默认</Badge>}
          {assignedConversations.length > 0 && (
            <Badge tone="neutral">{assignedConversations.length} 个会话覆盖</Badge>
          )}
        </div>
        <Tabs
          className="mb-0 border-b-0"
          tabs={[
            { id: 'edit', label: '属性与设定' },
            { id: 'versions', label: '版本历史', count: versions.length },
          ]}
          active={tab}
          onChange={(id) => setTab(id as 'edit' | 'versions')}
        />
      </div>

      <div className="flex-1 space-y-3 overflow-y-auto p-4 text-xs">
        {tab === 'edit' ? (
          <>
            <div className="flex flex-wrap items-center gap-2">
              <Button
                size="sm"
                variant="secondary"
                disabled={busy || detail.persona.activeVersion == null}
                onClick={() => void handleToggleDefault()}
              >
                <Star className="h-3.5 w-3.5" />
                {isDefault ? '取消全局默认' : '设为全局默认'}
              </Button>
              <Button size="sm" variant="secondary" disabled={busy} onClick={() => setDupOpen((v) => !v)}>
                <Copy className="h-3.5 w-3.5" />
                复制人设
              </Button>
              <Button size="sm" variant="danger" disabled={busy} onClick={handleDelete}>
                <Trash2 className="h-3.5 w-3.5" />
                删除
              </Button>
            </div>

            {dupOpen && (
              <Card className="p-3">
                <form className="flex items-center gap-2" onSubmit={(e) => void handleDuplicate(e)}>
                  <input
                    type="text"
                    maxLength={40}
                    value={dupName}
                    onChange={(e) => setDupName(e.target.value)}
                    placeholder="输入新人设名称…"
                    aria-label="副本名称"
                    className={INPUT_CLASS}
                  />
                  <Button size="sm" variant="primary" type="submit" disabled={busy}>
                    确定复制
                  </Button>
                </form>
              </Card>
            )}

            <label className="block space-y-1">
              <span className="font-medium text-[var(--text-primary)]">
                人设名称（单行，≤ 40 字符）
              </span>
              <input
                type="text"
                maxLength={40}
                disabled={busy}
                value={draft.name}
                onChange={(e) => {
                  const val = e.target.value;
                  updateDraft((prev) => ({ ...prev, name: val }));
                }}
                className={INPUT_CLASS}
              />
            </label>

            <div className="space-y-1.5">
              <div className="flex items-center justify-between">
                <span className="font-medium text-[var(--text-primary)]">
                  唤醒别名（独立条目，最多 10 个，支持含逗号别名）
                </span>
                <span className="text-[11px] text-[var(--text-muted)]">{draft.names.length}/10</span>
              </div>
              <div className="flex flex-wrap gap-1.5">
                {draft.names.map((alias, idx) => (
                  <span
                    key={`${alias}-${idx}`}
                    className="inline-flex items-center gap-1 rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)] px-2 py-0.5 text-[11px]"
                  >
                    <span>{alias}</span>
                    <button
                      type="button"
                      disabled={busy}
                      aria-label={`移除别名 ${alias}`}
                      onClick={() =>
                        updateDraft((prev) => ({
                          ...prev,
                          names: prev.names.filter((_, i) => i !== idx),
                        }))
                      }
                      className="rounded p-0.5 text-[var(--text-muted)] hover:text-[var(--danger)]"
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
                  disabled={busy || draft.names.length >= 10}
                  value={aliasInput}
                  onChange={(e) => setAliasInput(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      e.preventDefault();
                      handleAddAlias();
                    }
                  }}
                  placeholder="输入别名后点击添加…"
                  aria-label="新增唤醒别名"
                  className={INPUT_CLASS}
                />
                <Button
                  size="sm"
                  variant="secondary"
                  disabled={busy || draft.names.length >= 10 || !aliasInput.trim()}
                  onClick={handleAddAlias}
                >
                  <Plus className="h-3.5 w-3.5" />
                  添加
                </Button>
              </div>
            </div>

            <label className="block space-y-1">
              <div className="flex items-center justify-between gap-2">
                <span className="font-medium text-[var(--text-primary)]">
                  人设核心 (core · {utf16Length(draft.core)}/20000)
                </span>
                {latestVersionObj && (
                  <span
                    className={`font-mono text-[11px] ${
                      latestVersionObj.coreTokens > budgets.coreTokens
                        ? 'text-[var(--danger)]'
                        : 'text-[var(--text-muted)]'
                    }`}
                  >
                    上版 {latestVersionObj.coreTokens} / {budgets.coreTokens} tok
                  </span>
                )}
              </div>
              <textarea
                rows={7}
                maxLength={20000}
                disabled={busy}
                value={draft.core}
                onChange={(e) => {
                  const val = e.target.value;
                  updateDraft((prev) => ({ ...prev, core: val }));
                }}
                className={`${INPUT_CLASS} font-mono`}
              />
            </label>

            <label className="block space-y-1">
              <div className="flex items-center justify-between gap-2">
                <span className="font-medium text-[var(--text-primary)]">
                  精简人设 (compact · {utf16Length(draft.compact)}/20000)
                </span>
                {latestVersionObj && (
                  <span
                    className={`font-mono text-[11px] ${
                      latestVersionObj.compactTokens > budgets.compactTokens
                        ? 'text-[var(--danger)]'
                        : 'text-[var(--text-muted)]'
                    }`}
                  >
                    上版 {latestVersionObj.compactTokens} / {budgets.compactTokens} tok
                  </span>
                )}
              </div>
              <textarea
                rows={4}
                maxLength={20000}
                disabled={busy}
                value={draft.compact}
                onChange={(e) => {
                  const val = e.target.value;
                  updateDraft((prev) => ({ ...prev, compact: val }));
                }}
                placeholder="用于上下文压缩后的精简人设提示…"
                className={`${INPUT_CLASS} font-mono`}
              />
            </label>

            <label className="block space-y-1">
              <span className="font-medium text-[var(--text-primary)]">
                人设资料 (notes · {utf16Length(draft.notes)}/20000)
              </span>
              <textarea
                rows={4}
                maxLength={20000}
                disabled={busy}
                value={draft.notes}
                onChange={(e) => {
                  const val = e.target.value;
                  updateDraft((prev) => ({ ...prev, notes: val }));
                }}
                placeholder="关系细节、近期口癖、人设资料（仅改动此项可自动批准激活）…"
                className={`${INPUT_CLASS} font-mono`}
              />
            </label>

            <Card className="space-y-2.5 p-3">
              <div className="font-medium text-[var(--text-primary)]">性格参数与自演化规则</div>
              <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                <label className="block space-y-1">
                  <span className="text-[11px] text-[var(--text-secondary)]">粘人度 (traits.neediness)</span>
                  <select
                    disabled={busy}
                    value={draft.neediness}
                    onChange={(e) => {
                      const val = Number(e.target.value);
                      updateDraft((prev) => ({ ...prev, neediness: val }));
                    }}
                    className={INPUT_CLASS}
                  >
                    {[0, 1, 2, 3].map((n) => (
                      <option key={n} value={n}>
                        {NEEDINESS_LABELS[n]}
                      </option>
                    ))}
                  </select>
                </label>

                <label className="block space-y-1">
                  <span className="text-[11px] text-[var(--text-secondary)]">编辑模式 (editMode)</span>
                  <select
                    disabled={busy}
                    value={draft.editMode}
                    onChange={(e) => {
                      const val = e.target.value as 'auto' | 'manual';
                      updateDraft((prev) => ({ ...prev, editMode: val }));
                    }}
                    className={INPUT_CLASS}
                  >
                    <option value="auto">auto · 允许提案建议</option>
                    <option value="manual">manual · 仅手动维护</option>
                  </select>
                </label>
              </div>

              <div className="space-y-1">
                <span className="text-[11px] text-[var(--text-secondary)]">允许自动建议修改的部分</span>
                <div className="flex flex-wrap gap-1.5">
                  {AUTO_PARTS.map((part) => {
                    const checked = draft.parts.includes(part.id);
                    return (
                      <button
                        key={part.id}
                        type="button"
                        disabled={busy}
                        aria-pressed={checked}
                        onClick={() => togglePart(part.id)}
                        className={`rounded border px-2 py-1 text-[11px] transition-colors ${
                          checked
                            ? 'border-[var(--accent)] bg-[var(--accent-soft)] text-[var(--accent)]'
                            : 'border-[var(--border-hairline)] bg-[var(--bg-subtle)] text-[var(--text-secondary)]'
                        }`}
                      >
                        {part.label}
                      </button>
                    );
                  })}
                </div>
              </div>

              <div className="grid grid-cols-1 gap-2 sm:grid-cols-3">
                <label className="block space-y-1">
                  <span className="text-[11px] text-[var(--text-secondary)]">每周上限 (0–20)</span>
                  <input
                    type="number"
                    min={0}
                    max={20}
                    disabled={busy}
                    value={draft.perWeek}
                    onChange={(e) => {
                      const val = Number(e.target.value || 0);
                      updateDraft((prev) => ({ ...prev, perWeek: val }));
                    }}
                    className={INPUT_CLASS}
                  />
                </label>

                <div className="flex items-center justify-between rounded border border-[var(--border-hairline)] px-2.5 py-1.5">
                  <span className="text-[11px] text-[var(--text-secondary)]">漂移暂停</span>
                  <Switch
                    checked={draft.monthlyDriftPause}
                    disabled={busy}
                    onChange={(next) => updateDraft((prev) => ({ ...prev, monthlyDriftPause: next }))}
                    label="月度漂移检测暂停"
                  />
                </div>

                <div className="flex items-center justify-between rounded border border-[var(--border-hairline)] px-2.5 py-1.5">
                  <span className="text-[11px] text-[var(--text-secondary)]">通知管理员</span>
                  <Switch
                    checked={draft.notify}
                    disabled={busy}
                    onChange={(next) => updateDraft((prev) => ({ ...prev, notify: next }))}
                    label="提案通知管理员"
                  />
                </div>
              </div>
            </Card>

            {activeVersionObj && (
              <div className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/50 px-3 py-2 text-[11px] text-[var(--text-muted)]">
                提示：若 core 与 compact 与当前激活版 v{activeVersionObj.version} 完全一致，保存将自动批准激活；否则将生成待批准版本，需在「版本历史」中点击批准。
              </div>
            )}
          </>
        ) : (
          <div className="space-y-2.5">
            {versions
              .slice()
              .reverse()
              .map((v) => {
                const isActive = detail.persona.activeVersion === v.version;
                return (
                  <Card key={v.version} className="space-y-2 p-3">
                    <div className="flex flex-wrap items-center justify-between gap-2">
                      <div className="flex items-center gap-1.5">
                        <span className="font-mono font-semibold text-[var(--text-primary)]">
                          v{v.version}
                        </span>
                        <Badge tone={isActive ? 'success' : v.approved ? 'accent' : 'warn'}>
                          {isActive ? '当前激活' : v.approved ? '已批准' : '待批准'}
                        </Badge>
                        <Badge tone="neutral">{v.source}</Badge>
                      </div>
                      <span className="text-[11px] text-[var(--text-muted)]">
                        {formatDateTimeMs(v.createdAt)}
                      </span>
                    </div>

                    <div className="flex flex-wrap gap-3 text-[11px] text-[var(--text-secondary)]">
                      <span>名称：{v.name}</span>
                      <span className="font-mono">
                        core {v.coreTokens}/{budgets.coreTokens}
                      </span>
                      <span className="font-mono">
                        compact {v.compactTokens}/{budgets.compactTokens}
                      </span>
                      <span>粘人度 {v.traits?.neediness ?? 1}</span>
                    </div>

                    <div className="flex flex-wrap items-center gap-2">
                      {!isActive && (
                        <Button
                          size="sm"
                          variant={v.approved ? 'secondary' : 'primary'}
                          disabled={busy}
                          onClick={() => handleApprove(v.version, v.approved)}
                        >
                          <CheckCircle2 className="h-3.5 w-3.5" />
                          {v.approved ? '切换到此版本' : '批准并激活'}
                        </Button>
                      )}
                      <Button
                        size="sm"
                        variant="ghost"
                        disabled={busy}
                        onClick={() => handleLoadVersionDraft(v)}
                      >
                        <History className="h-3.5 w-3.5" />
                        载入草稿
                      </Button>
                    </div>
                  </Card>
                );
              })}
          </div>
        )}
      </div>

      <footer className="flex shrink-0 items-center justify-between gap-2 border-t border-[var(--border-hairline)] bg-[var(--bg-subtle)]/60 px-4 py-2.5">
        <span className="text-[11px] text-[var(--text-muted)]">
          {isDirty ? '有未保存的人设修改' : `共 ${versions.length} 个版本`}
        </span>
        <div className="flex items-center gap-2">
          {isDirty && (
            <Button
              size="sm"
              variant="ghost"
              disabled={busy}
              onClick={() => {
                if (savedDraft) {
                  draftGenRef.current += 1;
                  setDraft(savedDraft);
                }
              }}
            >
              放弃
            </Button>
          )}
          <Button size="sm" variant="primary" disabled={!isDirty || busy} onClick={() => void handleSave()}>
            保存为新版本
          </Button>
        </div>
      </footer>
    </div>
  );
};
