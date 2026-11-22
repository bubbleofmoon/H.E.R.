import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  AlertTriangle,
  Check,
  ChevronDown,
  ChevronRight,
  Plus,
  RefreshCw,
  RotateCcw,
  Sparkles,
  Trash2,
  X,
} from 'lucide-react';
import {
  SlangConfig,
  SlangEntry,
  SlangStatus,
  addSlang,
  batchConfirmSlang,
  batchDeleteSlang,
  batchRejectSlang,
  clearSlang,
  confirmSlang,
  deleteSlang,
  extractSlang,
  fetchSlang,
  patchSlang,
  rejectSlang,
  researchSlang,
  saveSlangConfig,
} from '../../api/her';
import { EmptyState, SearchInput } from '../../components/ConsoleWidgets';
import { Badge, Button, Card, CardHeader, Switch, Tabs } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import { formatDateTimeMs, formatErrorMessage } from '../../utils/format';
import { INPUT_CLASS, unicodeScalarLength, utf16Length } from '../../utils/herHelpers';

const STATUS_LABELS: Record<SlangStatus, string> = {
  candidate: '候选词',
  confirmed: '已确认',
  rejected: '已拒绝',
};

function parseThresholds(raw: string): number[] {
  const nums = raw
    .split(/[\s,，]+/)
    .map((s) => Number(s.trim()))
    .filter((n) => Number.isFinite(n) && n >= 1)
    .map((n) => Math.round(n));
  return Array.from(new Set(nums)).sort((a, b) => a - b);
}

function normalizeThresholds(arr?: number[]): number[] {
  if (!Array.isArray(arr) || arr.length === 0) return [2, 4, 8];
  return Array.from(
    new Set(arr.filter((n) => Number.isFinite(n) && n >= 1).map((n) => Math.round(n))),
  ).sort((a, b) => a - b);
}

function arraysEqual(a: number[], b: number[]): boolean {
  return a.length === b.length && a.every((v, idx) => v === b[idx]);
}

function diffSlangConfig(
  saved: SlangConfig,
  draft: SlangConfig,
  thresholdsDraft: number[],
): Partial<SlangConfig> {
  const patch: Partial<SlangConfig> = {};
  if (draft.enabled !== saved.enabled) patch.enabled = draft.enabled;
  if (draft.autoResearch !== saved.autoResearch) patch.autoResearch = draft.autoResearch;
  if (draft.learnerFallbackDsh !== saved.learnerFallbackDsh) {
    patch.learnerFallbackDsh = draft.learnerFallbackDsh;
  }
  if (draft.extractMinMessages !== saved.extractMinMessages) {
    patch.extractMinMessages = draft.extractMinMessages;
  }
  if (draft.extractCooldownMs !== saved.extractCooldownMs) {
    patch.extractCooldownMs = draft.extractCooldownMs;
  }
  if (draft.injectMax !== saved.injectMax) patch.injectMax = draft.injectMax;
  if (draft.learnerBackend !== saved.learnerBackend) {
    patch.learnerBackend = draft.learnerBackend;
  }
  if (draft.learnerPreset !== saved.learnerPreset) patch.learnerPreset = draft.learnerPreset;
  if (draft.workspaceTitle !== saved.workspaceTitle) {
    patch.workspaceTitle = draft.workspaceTitle;
  }
  const savedThresholds = normalizeThresholds(saved.inferenceThresholds);
  const nextThresholds = thresholdsDraft.length > 0 ? thresholdsDraft : [2, 4, 8];
  if (!arraysEqual(savedThresholds, nextThresholds)) {
    patch.inferenceThresholds = nextThresholds;
  }
  return patch;
}

export const SlangTab: React.FC = () => {
  const { conversations, openConfirm, pushToast, setDirtyEntry } = useConsole();

  const [statusTab, setStatusTab] = useState<SlangStatus>('candidate');
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [entries, setEntries] = useState<SlangEntry[]>([]);
  const [writable, setWritable] = useState(true);
  const [savedConfig, setSavedConfig] = useState<SlangConfig | null>(null);
  const [configDraft, setConfigDraft] = useState<SlangConfig | null>(null);
  const [thresholdsInput, setThresholdsInput] = useState('2, 4, 8');

  const [search, setSearch] = useState('');
  const [selectedIds, setSelectedIds] = useState<string[]>([]);
  const [expandedEvidence, setExpandedEvidence] = useState<Record<string, boolean>>({});
  const [extractKey, setExtractKey] = useState('');

  // Manual add form
  const [addOpen, setAddOpen] = useState(false);
  const [newContent, setNewContent] = useState('');
  const [newMeaning, setNewMeaning] = useState('');
  const [newUsage, setNewUsage] = useState('');
  const [newExample, setNewExample] = useState('');
  const [newRisk, setNewRisk] = useState('');
  const [newStatus, setNewStatus] = useState<'candidate' | 'confirmed'>('confirmed');

  // Inline edit state (preserving sources array intact)
  const [editing, setEditing] = useState<{
    id: string;
    status: SlangStatus;
    content: string;
    meaning: string;
    usage: string;
    example: string;
    risk: string;
    sources: string[];
    sourceDraft: string;
  } | null>(null);

  const mountedRef = useRef(true);
  const loadGenRef = useRef(0);
  const savingConfigRef = useRef(false);
  const savedConfigRef = useRef<SlangConfig | null>(null);
  const configDraftRef = useRef<SlangConfig | null>(null);
  const thresholdsInputRef = useRef('2, 4, 8');

  useEffect(() => {
    savedConfigRef.current = savedConfig;
    configDraftRef.current = configDraft;
    thresholdsInputRef.current = thresholdsInput;
  }, [savedConfig, configDraft, thresholdsInput]);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      setDirtyEntry(null);
    };
  }, [setDirtyEntry]);

  const loadSlang = useCallback(async (preserveConfigDraft = false) => {
    const gen = ++loadGenRef.current;
    setLoading(true);
    setError(null);
    try {
      const res = await fetchSlang();
      if (!mountedRef.current || gen !== loadGenRef.current) return;
      const nextEntries = res.entries ?? [];
      setEntries(nextEntries);
      setWritable(res.writable !== false);
      setEditing((prev) => {
        if (!prev) return null;
        const matched = nextEntries.find((e) => e.id === prev.id);
        return matched ? { ...prev, status: matched.status } : null;
      });

      const prevSaved = savedConfigRef.current;
      const prevDraft = configDraftRef.current;
      setSavedConfig(res.config);

      if (!preserveConfigDraft || !prevDraft || !prevSaved) {
        setConfigDraft(res.config);
        setThresholdsInput(normalizeThresholds(res.config.inferenceThresholds).join(', '));
      } else {
        const curThresholds = parseThresholds(thresholdsInputRef.current);
        const dirtyPatch = diffSlangConfig(prevSaved, prevDraft, curThresholds);
        setConfigDraft({ ...res.config, ...dirtyPatch });
        if (!('inferenceThresholds' in dirtyPatch)) {
          setThresholdsInput(normalizeThresholds(res.config.inferenceThresholds).join(', '));
        }
      }

      setSelectedIds((prev) => prev.filter((id) => nextEntries.some((e) => e.id === id)));
    } catch (err) {
      if (!mountedRef.current || gen !== loadGenRef.current) return;
      setError(formatErrorMessage(err, '加载黑话词库失败'));
    } finally {
      if (mountedRef.current && gen === loadGenRef.current) {
        setLoading(false);
      }
    }
  }, []);

  useEffect(() => {
    void loadSlang(false);
  }, [loadSlang]);

  const parsedThresholds = useMemo(() => parseThresholds(thresholdsInput), [thresholdsInput]);

  const configPatch = useMemo(() => {
    if (!savedConfig || !configDraft) return {};
    return diffSlangConfig(savedConfig, configDraft, parsedThresholds);
  }, [savedConfig, configDraft, parsedThresholds]);

  const configChangedCount = Object.keys(configPatch).length;

  const handleSaveConfig = useCallback(async () => {
    if (savingConfigRef.current || !writable || configChangedCount === 0) return;
    savingConfigRef.current = true;
    const submittedPatch = { ...configPatch };
    setBusy(true);
    try {
      const res = await saveSlangConfig(submittedPatch);
      if (!mountedRef.current) return;
      setSavedConfig(res.config);
      setConfigDraft((prev) => {
        if (!prev) return res.config;
        const merged = { ...res.config };
        if (prev.enabled !== submittedPatch.enabled && 'enabled' in submittedPatch) merged.enabled = prev.enabled;
        if (prev.autoResearch !== submittedPatch.autoResearch && 'autoResearch' in submittedPatch) {
          merged.autoResearch = prev.autoResearch;
        }
        if (
          prev.learnerFallbackDsh !== submittedPatch.learnerFallbackDsh &&
          'learnerFallbackDsh' in submittedPatch
        ) {
          merged.learnerFallbackDsh = prev.learnerFallbackDsh;
        }
        if (
          prev.extractMinMessages !== submittedPatch.extractMinMessages &&
          'extractMinMessages' in submittedPatch
        ) {
          merged.extractMinMessages = prev.extractMinMessages;
        }
        if (
          prev.extractCooldownMs !== submittedPatch.extractCooldownMs &&
          'extractCooldownMs' in submittedPatch
        ) {
          merged.extractCooldownMs = prev.extractCooldownMs;
        }
        if (prev.injectMax !== submittedPatch.injectMax && 'injectMax' in submittedPatch) {
          merged.injectMax = prev.injectMax;
        }
        if (
          prev.learnerBackend !== submittedPatch.learnerBackend &&
          'learnerBackend' in submittedPatch
        ) {
          merged.learnerBackend = prev.learnerBackend;
        }
        if (
          prev.learnerPreset !== submittedPatch.learnerPreset &&
          'learnerPreset' in submittedPatch
        ) {
          merged.learnerPreset = prev.learnerPreset;
        }
        if (
          prev.workspaceTitle !== submittedPatch.workspaceTitle &&
          'workspaceTitle' in submittedPatch
        ) {
          merged.workspaceTitle = prev.workspaceTitle;
        }
        return merged;
      });
      if (
        !('inferenceThresholds' in submittedPatch) ||
        arraysEqual(
          parseThresholds(thresholdsInputRef.current),
          submittedPatch.inferenceThresholds ?? [],
        )
      ) {
        setThresholdsInput(normalizeThresholds(res.config.inferenceThresholds).join(', '));
      }
      pushToast('已保存黑话学习参数', 'success');
    } catch (err) {
      pushToast(formatErrorMessage(err, '保存黑话参数失败'), 'danger');
    } finally {
      savingConfigRef.current = false;
      if (mountedRef.current) setBusy(false);
    }
  }, [writable, configChangedCount, configPatch, pushToast]);

  const handleDiscardConfig = useCallback(() => {
    if (!savedConfig) return;
    setConfigDraft(savedConfig);
    setThresholdsInput(normalizeThresholds(savedConfig.inferenceThresholds).join(', '));
  }, [savedConfig]);

  useEffect(() => {
    if (configChangedCount > 0 && writable) {
      setDirtyEntry({
        count: configChangedCount,
        label: `黑话学习参数有 ${configChangedCount} 项未保存更改`,
        onSave: handleSaveConfig,
        onDiscard: handleDiscardConfig,
      });
    } else {
      setDirtyEntry(null);
    }
  }, [configChangedCount, writable, setDirtyEntry, handleSaveConfig, handleDiscardConfig]);

  const counts = useMemo(() => {
    let candidate = 0;
    let confirmed = 0;
    let rejected = 0;
    for (const e of entries) {
      if (e.status === 'candidate') candidate += 1;
      else if (e.status === 'confirmed') confirmed += 1;
      else if (e.status === 'rejected') rejected += 1;
    }
    return { candidate, confirmed, rejected };
  }, [entries]);

  const visibleEntries = useMemo(() => {
    const q = search.trim().toLowerCase();
    return entries.filter((e) => {
      if (e.status !== statusTab) return false;
      if (!q) return true;
      return (
        e.content.toLowerCase().includes(q) ||
        e.meaning.toLowerCase().includes(q) ||
        e.usage.toLowerCase().includes(q)
      );
    });
  }, [entries, statusTab, search]);

  const selectedVisibleIds = useMemo(
    () => selectedIds.filter((id) => visibleEntries.some((e) => e.id === id)),
    [selectedIds, visibleEntries],
  );

  const toggleSelect = (id: string) => {
    setSelectedIds((prev) => (prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id]));
  };

  const toggleSelectAllVisible = () => {
    const visibleSet = visibleEntries.map((e) => e.id);
    const allSelected = visibleSet.length > 0 && visibleSet.every((id) => selectedIds.includes(id));
    if (allSelected) {
      setSelectedIds((prev) => prev.filter((id) => !visibleSet.includes(id)));
    } else {
      setSelectedIds((prev) => Array.from(new Set([...prev, ...visibleSet])));
    }
  };

  const handleAdd = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!writable) return;
    if (!newContent.trim() || unicodeScalarLength(newContent.trim()) > 50) {
      pushToast('黑话词条不能为空且不超过 50 个字符', 'warn');
      return;
    }
    if (newStatus === 'confirmed' && !newMeaning.trim()) {
      pushToast('已确认黑话必须填写含义，否则不会注入 AI 上下文', 'warn');
      return;
    }
    if (
      utf16Length(newMeaning.trim()) > 300 ||
      utf16Length(newUsage.trim()) > 300 ||
      utf16Length(newExample.trim()) > 300 ||
      utf16Length(newRisk.trim()) > 300
    ) {
      pushToast('含义、用法、例句与风险字段均不能超过 300 字', 'warn');
      return;
    }
    setBusy(true);
    try {
      await addSlang({
        content: newContent.trim(),
        meaning: newMeaning.trim(),
        usage: newUsage.trim(),
        example: newExample.trim(),
        risk: newRisk.trim(),
        status: newStatus,
      });
      if (!mountedRef.current) return;
      pushToast(`已添加黑话「${newContent.trim()}」`, 'success');
      setNewContent('');
      setNewMeaning('');
      setNewUsage('');
      setNewExample('');
      setNewRisk('');
      setAddOpen(false);
      await loadSlang(true);
    } catch (err) {
      pushToast(formatErrorMessage(err, '添加黑话失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleAddEditSource = () => {
    if (!editing) return;
    const candidate = editing.sourceDraft.trim();
    if (!candidate) return;
    if (utf16Length(candidate) > 300) {
      pushToast('单条来源不能超过 300 字', 'warn');
      return;
    }
    if (editing.sources.length >= 10) {
      pushToast('来源最多 10 条', 'warn');
      return;
    }
    if (!editing.sources.includes(candidate)) {
      setEditing((prev) =>
        prev ? { ...prev, sources: [...prev.sources, candidate], sourceDraft: '' } : prev,
      );
    } else {
      setEditing((prev) => (prev ? { ...prev, sourceDraft: '' } : prev));
    }
  };

  const handleSaveEdit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!editing || !writable) return;
    const liveEntry = entries.find((item) => item.id === editing.id);
    const effectiveStatus = liveEntry?.status ?? editing.status;
    if (!editing.content.trim() || utf16Length(editing.content.trim()) > 50) {
      pushToast('词条不能为空且不超过 50 个字', 'warn');
      return;
    }
    if (effectiveStatus === 'confirmed' && !editing.meaning.trim()) {
      pushToast('该词条已确认，含义不能为空', 'warn');
      return;
    }
    if (editing.sources.length > 10 || editing.sources.some((s) => utf16Length(s) > 300)) {
      pushToast('来源最多 10 条，且每条不超过 300 字', 'warn');
      return;
    }
    if (
      utf16Length(editing.meaning.trim()) > 300 ||
      utf16Length(editing.usage.trim()) > 300 ||
      utf16Length(editing.example.trim()) > 300 ||
      utf16Length(editing.risk.trim()) > 300
    ) {
      pushToast('含义、用法、例句与风险字段均不能超过 300 字', 'warn');
      return;
    }
    setBusy(true);
    try {
      await patchSlang(editing.id, {
        content: editing.content.trim(),
        meaning: editing.meaning.trim(),
        usage: editing.usage.trim(),
        example: editing.example.trim(),
        risk: editing.risk.trim(),
        sources: editing.sources,
      });
      if (!mountedRef.current) return;
      pushToast('已更新黑话词条', 'success');
      setEditing(null);
      await loadSlang(true);
    } catch (err) {
      pushToast(formatErrorMessage(err, '更新黑话失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleConfirmEntry = async (entry: SlangEntry) => {
    if (!writable) return;
    if (!entry.meaning.trim()) {
      setEditing({
        id: entry.id,
        status: entry.status,
        content: entry.content,
        meaning: entry.meaning,
        usage: entry.usage,
        example: entry.example,
        risk: entry.risk,
        sources: [...entry.sources],
        sourceDraft: '',
      });
      pushToast('请先填写含义后再确认，否则不会注入 AI 上下文', 'warn');
      return;
    }
    setBusy(true);
    try {
      const res = await confirmSlang(entry.id);
      if (!mountedRef.current) return;
      setEditing((prev) => (prev?.id === entry.id ? { ...prev, status: res.entry.status } : prev));
      pushToast(`已确认黑话「${entry.content}」`, 'success');
      await loadSlang(true);
    } catch (err) {
      pushToast(formatErrorMessage(err, '确认黑话失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleRejectEntry = async (entry: SlangEntry) => {
    if (!writable) return;
    setBusy(true);
    try {
      const res = await rejectSlang(entry.id);
      if (!mountedRef.current) return;
      setEditing((prev) => (prev?.id === entry.id ? { ...prev, status: res.entry.status } : prev));
      pushToast(`已拒绝「${entry.content}」`, 'info');
      await loadSlang(true);
    } catch (err) {
      pushToast(formatErrorMessage(err, '拒绝黑话失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleRestoreEntry = async (entry: SlangEntry) => {
    if (!writable) return;
    setBusy(true);
    try {
      const res = await patchSlang(entry.id, { status: 'candidate' });
      if (!mountedRef.current) return;
      setEditing((prev) => (prev?.id === entry.id ? { ...prev, status: res.entry.status } : prev));
      pushToast(`已将「${entry.content}」恢复至候选词`, 'success');
      await loadSlang(true);
    } catch (err) {
      pushToast(formatErrorMessage(err, '恢复词条失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleDeleteEntry = (entry: SlangEntry) => {
    if (!writable) return;
    openConfirm({
      title: `确认删除黑话「${entry.content}」？`,
      description: '删除后该条目将从黑话词库中永久移除。',
      confirmLabel: '确认删除',
      danger: true,
      onConfirm: async () => {
        setBusy(true);
        try {
          await deleteSlang(entry.id);
          if (!mountedRef.current) return;
          setEditing((prev) => (prev?.id === entry.id ? null : prev));
          pushToast(`已删除「${entry.content}」`, 'success');
          await loadSlang(true);
        } catch (err) {
          pushToast(formatErrorMessage(err, '删除黑话失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleBatchResearch = async () => {
    if (!writable) return;
    setBusy(true);
    try {
      const res = await researchSlang(selectedVisibleIds.length > 0 ? selectedVisibleIds : undefined);
      if (!mountedRef.current) return;
      pushToast(`已入队研究 ${res.count} 条候选黑话`, 'success');
    } catch (err) {
      pushToast(formatErrorMessage(err, '触发黑话研究失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleBatchConfirm = async () => {
    if (!writable || selectedVisibleIds.length === 0) return;
    setBusy(true);
    try {
      const res = await batchConfirmSlang(selectedVisibleIds);
      if (!mountedRef.current) return;
      pushToast(
        `批量确认 ${res.confirmedCount} 条${res.skippedCount > 0 ? `，跳过 ${res.skippedCount} 条（需先填写含义）` : ''}`,
        res.skippedCount > 0 ? 'warn' : 'success',
      );
      await loadSlang(true);
    } catch (err) {
      pushToast(formatErrorMessage(err, '批量确认失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleBatchReject = async () => {
    if (!writable || selectedVisibleIds.length === 0) return;
    setBusy(true);
    try {
      const res = await batchRejectSlang(selectedVisibleIds);
      if (!mountedRef.current) return;
      pushToast(`已批量拒绝 ${res.rejectedCount} 条`, 'info');
      await loadSlang(true);
    } catch (err) {
      pushToast(formatErrorMessage(err, '批量拒绝失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleBatchDelete = () => {
    if (!writable || selectedVisibleIds.length === 0) return;
    openConfirm({
      title: `确认批量删除已选 ${selectedVisibleIds.length} 条黑话？`,
      description: '删除后不可撤销。',
      confirmLabel: '确认删除',
      danger: true,
      onConfirm: async () => {
        setBusy(true);
        try {
          const res = await batchDeleteSlang({ ids: selectedVisibleIds });
          if (!mountedRef.current) return;
          pushToast(`已删除 ${res.removedCount} 条黑话`, 'success');
          await loadSlang(true);
        } catch (err) {
          pushToast(formatErrorMessage(err, '批量删除失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleExtractNow = async () => {
    if (!writable) return;
    setBusy(true);
    try {
      const res = await extractSlang(extractKey || undefined);
      if (!mountedRef.current) return;
      pushToast(`已触发对 ${res.key} 的黑话提取任务`, 'success');
    } catch (err) {
      pushToast(formatErrorMessage(err, '立即提取黑话失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleClearStatus = (target?: SlangStatus) => {
    if (!writable) return;
    openConfirm({
      title: target ? `确认清空全部「${STATUS_LABELS[target]}」？` : '确认清空整个黑话词库？',
      description: target
        ? '仅删除当前状态标签下的词条，操作不可逆。'
        : '将清空全部候选/已确认/已拒绝黑话以及提取窗口与提交频控状态，不可恢复。',
      confirmLabel: '确认清空',
      danger: true,
      onConfirm: async () => {
        setBusy(true);
        try {
          const res = await clearSlang(target);
          if (!mountedRef.current) return;
          pushToast(`已清空 ${res.removedCount} 条黑话`, 'success');
          await loadSlang(true);
        } catch (err) {
          pushToast(formatErrorMessage(err, '清空失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader
          title="黑话词库"
          subtitle="审核群聊沉淀的候选词汇，确认释义后才会注入 AI 上下文。"
          actions={
            <div className="flex flex-wrap items-center gap-2">
              <select
                value={extractKey}
                aria-label="选择黑话提取目标会话"
                onChange={(e) => setExtractKey(e.target.value)}
                className="h-7 rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs"
              >
                <option value="">自动选取可学习会话</option>
                {conversations.map((c) => (
                  <option key={c.key} value={c.key}>
                    {c.name ? `${c.name} (${c.key})` : c.key}
                  </option>
                ))}
              </select>
              <Button size="sm" variant="secondary" disabled={busy || !writable} onClick={() => void handleExtractNow()}>
                <Sparkles className="h-3.5 w-3.5" />
                立即提取黑话
              </Button>
              <Button size="sm" variant="primary" disabled={!writable} onClick={() => setAddOpen((v) => !v)}>
                <Plus className="h-3.5 w-3.5" />
                新增黑话
              </Button>
              <Button size="sm" variant="ghost" disabled={loading} onClick={() => void loadSlang(true)}>
                <RefreshCw className={`h-3.5 w-3.5 ${loading ? 'animate-spin' : ''}`} />
                刷新
              </Button>
            </div>
          }
        />

        <div className="space-y-3 p-4 text-xs">
          {!writable && (
            <div className="rounded border border-amber-300 bg-[var(--warn-soft)] p-2.5 text-[var(--warn)]">
              当前黑话存储为只读状态，所有新增、编辑、审批、批量与清空操作已禁用。
            </div>
          )}

          <Tabs
            className="mb-2"
            tabs={[
              { id: 'candidate', label: '候选词', count: counts.candidate },
              { id: 'confirmed', label: '已确认', count: counts.confirmed },
              { id: 'rejected', label: '已拒绝', count: counts.rejected },
            ]}
            active={statusTab}
            onChange={(id) => {
              setStatusTab(id as SlangStatus);
              setSelectedIds([]);
            }}
          />

          {addOpen && (
            <Card className="bg-[var(--bg-subtle)]/40 p-3">
              <form className="space-y-2" onSubmit={(e) => void handleAdd(e)}>
                <div className="grid grid-cols-1 gap-2 sm:grid-cols-3">
                  <input
                    type="text"
                    maxLength={50}
                    disabled={!writable || busy}
                    value={newContent}
                    onChange={(e) => setNewContent(e.target.value)}
                    placeholder="词条 (≤ 50 字) *"
                    className={INPUT_CLASS}
                  />
                  <input
                    type="text"
                    maxLength={300}
                    disabled={!writable || busy}
                    value={newMeaning}
                    onChange={(e) => setNewMeaning(e.target.value)}
                    placeholder="含义 (≤ 300 字，已确认必填)"
                    className={INPUT_CLASS}
                  />
                  <select
                    value={newStatus}
                    disabled={!writable || busy}
                    aria-label="新增黑话初始状态"
                    onChange={(e) => setNewStatus(e.target.value as 'candidate' | 'confirmed')}
                    className={INPUT_CLASS}
                  >
                    <option value="confirmed">直接设为已确认</option>
                    <option value="candidate">放入候选词</option>
                  </select>
                </div>
                <div className="grid grid-cols-1 gap-2 sm:grid-cols-3">
                  <input
                    type="text"
                    maxLength={300}
                    disabled={!writable || busy}
                    value={newUsage}
                    onChange={(e) => setNewUsage(e.target.value)}
                    placeholder="用法说明（≤ 300 字，可选）"
                    className={INPUT_CLASS}
                  />
                  <input
                    type="text"
                    maxLength={300}
                    disabled={!writable || busy}
                    value={newExample}
                    onChange={(e) => setNewExample(e.target.value)}
                    placeholder="例句（≤ 300 字，可选）"
                    className={INPUT_CLASS}
                  />
                  <input
                    type="text"
                    maxLength={300}
                    disabled={!writable || busy}
                    value={newRisk}
                    onChange={(e) => setNewRisk(e.target.value)}
                    placeholder="语境风险提醒（≤ 300 字，可选）"
                    className={INPUT_CLASS}
                  />
                </div>
                <div className="flex justify-end gap-2">
                  <Button size="sm" variant="ghost" onClick={() => setAddOpen(false)}>
                    取消
                  </Button>
                  <Button size="sm" variant="primary" type="submit" disabled={busy || !writable}>
                    保存词条
                  </Button>
                </div>
              </form>
            </Card>
          )}

          {editing && (
            <Card className="border-[var(--accent)] bg-[var(--accent-soft)]/25 p-3">
              <form className="space-y-2" onSubmit={(e) => void handleSaveEdit(e)}>
                <div className="font-semibold text-[var(--text-primary)]">
                  编辑词条「{editing.content}」（当前状态：{STATUS_LABELS[editing.status]}）
                </div>
                <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                  <input
                    type="text"
                    maxLength={50}
                    disabled={!writable || busy}
                    value={editing.content}
                    onChange={(e) =>
                      setEditing((prev) => (prev ? { ...prev, content: e.target.value } : prev))
                    }
                    placeholder="词条 (≤ 50 字)"
                    className={INPUT_CLASS}
                  />
                  <input
                    type="text"
                    maxLength={300}
                    disabled={!writable || busy}
                    value={editing.meaning}
                    onChange={(e) =>
                      setEditing((prev) => (prev ? { ...prev, meaning: e.target.value } : prev))
                    }
                    placeholder="含义（≤ 300 字，已确认词条必填）"
                    className={INPUT_CLASS}
                  />
                  <input
                    type="text"
                    maxLength={300}
                    disabled={!writable || busy}
                    value={editing.usage}
                    onChange={(e) =>
                      setEditing((prev) => (prev ? { ...prev, usage: e.target.value } : prev))
                    }
                    placeholder="用法 (≤ 300 字)"
                    className={INPUT_CLASS}
                  />
                  <input
                    type="text"
                    maxLength={300}
                    disabled={!writable || busy}
                    value={editing.example}
                    onChange={(e) =>
                      setEditing((prev) => (prev ? { ...prev, example: e.target.value } : prev))
                    }
                    placeholder="例句 (≤ 300 字)"
                    className={INPUT_CLASS}
                  />
                  <input
                    type="text"
                    maxLength={300}
                    disabled={!writable || busy}
                    value={editing.risk}
                    onChange={(e) =>
                      setEditing((prev) => (prev ? { ...prev, risk: e.target.value } : prev))
                    }
                    placeholder="风险提示 (≤ 300 字)"
                    className={INPUT_CLASS}
                  />
                </div>

                <div className="space-y-1.5">
                  <div className="flex flex-wrap gap-1.5">
                    {editing.sources.map((src, idx) => (
                      <span
                        key={`${src}-${idx}`}
                        className="inline-flex items-center gap-1 rounded border border-[var(--border-hairline)] bg-[var(--bg-surface)] px-2 py-0.5 text-[11px]"
                      >
                        <span>{src}</span>
                        <button
                          type="button"
                          disabled={!writable || busy}
                          aria-label={`移除来源 ${src}`}
                          onClick={() =>
                            setEditing((prev) =>
                              prev
                                ? { ...prev, sources: prev.sources.filter((_, i) => i !== idx) }
                                : prev,
                            )
                          }
                          className="text-[var(--text-muted)] hover:text-[var(--danger)]"
                        >
                          <X className="h-3 w-3" />
                        </button>
                      </span>
                    ))}
                  </div>
                  <div className="flex gap-1.5">
                    <input
                      type="text"
                      maxLength={300}
                      disabled={!writable || busy || editing.sources.length >= 10}
                      value={editing.sourceDraft}
                      onChange={(e) =>
                        setEditing((prev) =>
                          prev ? { ...prev, sourceDraft: e.target.value } : prev,
                        )
                      }
                      onKeyDown={(e) => {
                        if (e.key === 'Enter') {
                          e.preventDefault();
                          handleAddEditSource();
                        }
                      }}
                      placeholder="添加来源说明（最多 10 条，支持逗号）"
                      aria-label="添加黑话来源"
                      className={INPUT_CLASS}
                    />
                    <Button
                      size="sm"
                      variant="secondary"
                      disabled={!writable || busy || editing.sources.length >= 10 || !editing.sourceDraft.trim()}
                      onClick={handleAddEditSource}
                    >
                      添加来源
                    </Button>
                  </div>
                </div>

                <div className="flex justify-end gap-2">
                  <Button size="sm" variant="ghost" onClick={() => setEditing(null)}>
                    取消
                  </Button>
                  <Button size="sm" variant="primary" type="submit" disabled={busy || !writable}>
                    保存修改
                  </Button>
                </div>
              </form>
            </Card>
          )}

          <div className="flex flex-wrap items-center justify-between gap-2">
            <div className="flex flex-wrap items-center gap-2">
              <div className="w-56">
                <SearchInput value={search} onChange={setSearch} placeholder="筛选词条或释义…" />
              </div>
              <Button size="sm" variant="ghost" onClick={toggleSelectAllVisible}>
                全选 / 取消 ({selectedVisibleIds.length}/{visibleEntries.length})
              </Button>
            </div>

            <div className="flex flex-wrap items-center gap-1.5">
              {statusTab === 'candidate' && (
                <>
                  <Button
                    size="sm"
                    variant="secondary"
                    disabled={busy || !writable}
                    onClick={() => void handleBatchResearch()}
                  >
                    <Sparkles className="h-3.5 w-3.5" />
                    {selectedVisibleIds.length > 0 ? `研究已选 (${selectedVisibleIds.length})` : '研究全部候选'}
                  </Button>
                  <Button
                    size="sm"
                    variant="primary"
                    disabled={busy || !writable || selectedVisibleIds.length === 0}
                    onClick={() => void handleBatchConfirm()}
                  >
                    <Check className="h-3.5 w-3.5" />
                    批量确认
                  </Button>
                  <Button
                    size="sm"
                    variant="secondary"
                    disabled={busy || !writable || selectedVisibleIds.length === 0}
                    onClick={() => void handleBatchReject()}
                  >
                    <X className="h-3.5 w-3.5" />
                    批量拒绝
                  </Button>
                </>
              )}
              <Button
                size="sm"
                variant="danger"
                disabled={busy || !writable || selectedVisibleIds.length === 0}
                onClick={handleBatchDelete}
              >
                <Trash2 className="h-3.5 w-3.5" />
                批量删除
              </Button>
            </div>
          </div>

          {error && (
            <div className="rounded border border-rose-300 bg-[var(--danger-soft)] p-3 text-[var(--danger)]">
              {error}
            </div>
          )}

          {!loading && visibleEntries.length === 0 ? (
            <EmptyState
              title="当前分类暂无黑话"
              description="可切换状态标签页、触发「立即提取黑话」或手动新增。"
            />
          ) : (
            <div className="space-y-2">
              {visibleEntries.map((entry) => {
                const checked = selectedIds.includes(entry.id);
                const evOpen = Boolean(expandedEvidence[entry.id]);
                return (
                  <div
                    key={entry.id}
                    className="rounded-md border border-[var(--border-hairline)] p-3"
                  >
                    <div className="flex flex-wrap items-start justify-between gap-2">
                      <div className="flex items-start gap-2.5">
                        <input
                          type="checkbox"
                          checked={checked}
                          aria-label={`选择黑话 ${entry.content}`}
                          onChange={() => toggleSelect(entry.id)}
                          className="mt-1"
                        />
                        <div className="space-y-1">
                          <div className="flex flex-wrap items-center gap-1.5">
                            <span className="text-[13px] font-semibold text-[var(--text-primary)]">
                              {entry.content}
                            </span>
                            <Badge tone="neutral">{entry.source}</Badge>
                            <Badge tone="accent">出现 {entry.count}</Badge>
                            {entry.risk && <Badge tone="warn">风险：{entry.risk}</Badge>}
                          </div>
                          <div className="text-xs text-[var(--text-secondary)]">
                            {entry.meaning || '（待补充含义）'}
                          </div>
                          {(entry.usage || entry.example) && (
                            <div className="text-[11px] text-[var(--text-muted)]">
                              {entry.usage && <span className="mr-3">用法：{entry.usage}</span>}
                              {entry.example && <span>例句：{entry.example}</span>}
                            </div>
                          )}
                        </div>
                      </div>

                      <div className="flex flex-wrap items-center gap-1">
                        <Button
                          size="sm"
                          variant="ghost"
                          disabled={busy || !writable}
                          onClick={() =>
                            setEditing({
                              id: entry.id,
                              status: entry.status,
                              content: entry.content,
                              meaning: entry.meaning,
                              usage: entry.usage,
                              example: entry.example,
                              risk: entry.risk,
                              sources: [...entry.sources],
                              sourceDraft: '',
                            })
                          }
                        >
                          编辑
                        </Button>
                        {entry.status === 'candidate' && (
                          <>
                            <Button
                              size="sm"
                              variant="primary"
                              disabled={busy || !writable}
                              onClick={() => void handleConfirmEntry(entry)}
                            >
                              确认
                            </Button>
                            <Button
                              size="sm"
                              variant="ghost"
                              disabled={busy || !writable}
                              onClick={() => void handleRejectEntry(entry)}
                            >
                              拒绝
                            </Button>
                          </>
                        )}
                        {entry.status === 'rejected' && (
                          <Button
                            size="sm"
                            variant="secondary"
                            disabled={busy || !writable}
                            onClick={() => void handleRestoreEntry(entry)}
                          >
                            <RotateCcw className="h-3 w-3" />
                            恢复为候选
                          </Button>
                        )}
                        <Button
                          size="sm"
                          variant="ghost"
                          aria-label={`删除黑话 ${entry.content}`}
                          disabled={busy || !writable}
                          onClick={() => handleDeleteEntry(entry)}
                        >
                          <Trash2 className="h-3 w-3 text-[var(--danger)]" />
                        </Button>
                      </div>
                    </div>

                    {entry.evidence.length > 0 && (
                      <div className="mt-2 border-t border-[var(--border-hairline)] pt-1.5">
                        <button
                          type="button"
                          aria-expanded={evOpen}
                          onClick={() =>
                            setExpandedEvidence((prev) => ({
                              ...prev,
                              [entry.id]: !prev[entry.id],
                            }))
                          }
                          className="inline-flex items-center gap-1 text-[11px] text-[var(--text-muted)] hover:text-[var(--text-primary)]"
                        >
                          {evOpen ? (
                            <ChevronDown className="h-3 w-3" />
                          ) : (
                            <ChevronRight className="h-3 w-3" />
                          )}
                          群聊语境证据 ({entry.evidence.length})
                        </button>
                        {evOpen && (
                          <div className="mt-1.5 space-y-1">
                            {entry.evidence.map((ev, idx) => (
                              <div
                                key={`${entry.id}-${idx}`}
                                className="rounded bg-[var(--bg-subtle)]/50 px-2.5 py-1 text-[11px]"
                              >
                                <span className="font-mono text-[var(--text-muted)]">
                                  {ev.key} · {ev.sender} · {formatDateTimeMs(ev.time)}：
                                </span>
                                <span className="text-[var(--text-primary)]">{ev.text}</span>
                              </div>
                            ))}
                          </div>
                        )}
                      </div>
                    )}
                  </div>
                );
              })}
            </div>
          )}
        </div>
      </Card>

      {configDraft && (
        <Card>
          <CardHeader
            title="黑话学习配置 (/api/slang/config)"
            subtitle="配置自动提取触发门槛、冷却时间、推理阈值与注入上限；仅提交改动字段。"
          />
          <div className="grid grid-cols-1 gap-3 p-4 text-xs sm:grid-cols-2 lg:grid-cols-3">
            <div className="flex items-center justify-between rounded border border-[var(--border-hairline)] p-2.5">
              <span>启用黑话学习</span>
              <Switch
                checked={configDraft.enabled}
                disabled={!writable}
                onChange={(next) =>
                  setConfigDraft((prev) => (prev ? { ...prev, enabled: next } : prev))
                }
                label="启用黑话学习"
              />
            </div>

            <div className="flex items-center justify-between rounded border border-[var(--border-hairline)] p-2.5">
              <span>自动发起候选研究</span>
              <Switch
                checked={configDraft.autoResearch}
                disabled={!writable}
                onChange={(next) =>
                  setConfigDraft((prev) => (prev ? { ...prev, autoResearch: next } : prev))
                }
                label="自动发起候选研究"
              />
            </div>

            <div className="flex items-center justify-between rounded border border-[var(--border-hairline)] p-2.5">
              <span>允许回退 DSH 后端</span>
              <Switch
                checked={configDraft.learnerFallbackDsh}
                disabled={!writable}
                onChange={(next) =>
                  setConfigDraft((prev) => (prev ? { ...prev, learnerFallbackDsh: next } : prev))
                }
                label="允许回退 DSH"
              />
            </div>

            <label className="space-y-1">
              <span className="text-[11px] text-[var(--text-secondary)]">最小提取消息数 (≥ 1)</span>
              <input
                type="number"
                min={1}
                disabled={!writable}
                value={configDraft.extractMinMessages}
                onChange={(e) =>
                  setConfigDraft((prev) =>
                    prev ? { ...prev, extractMinMessages: Number(e.target.value || 1) } : prev,
                  )
                }
                className={INPUT_CLASS}
              />
            </label>

            <label className="space-y-1">
              <span className="text-[11px] text-[var(--text-secondary)]">提取冷却毫秒 (≥ 0)</span>
              <input
                type="number"
                min={0}
                disabled={!writable}
                value={configDraft.extractCooldownMs}
                onChange={(e) =>
                  setConfigDraft((prev) =>
                    prev ? { ...prev, extractCooldownMs: Number(e.target.value || 0) } : prev,
                  )
                }
                className={INPUT_CLASS}
              />
            </label>

            <label className="space-y-1">
              <span className="text-[11px] text-[var(--text-secondary)]">注入提示词条数 (1–30)</span>
              <input
                type="number"
                min={1}
                max={30}
                disabled={!writable}
                value={configDraft.injectMax}
                onChange={(e) =>
                  setConfigDraft((prev) =>
                    prev ? { ...prev, injectMax: Number(e.target.value || 1) } : prev,
                  )
                }
                className={INPUT_CLASS}
              />
            </label>

            <label className="space-y-1">
              <span className="text-[11px] text-[var(--text-secondary)]">出现次数触发阈值</span>
              <input
                type="text"
                disabled={!writable}
                value={thresholdsInput}
                onChange={(e) => setThresholdsInput(e.target.value)}
                placeholder="2, 4, 8"
                className={INPUT_CLASS}
              />
            </label>

            <label className="space-y-1">
              <span className="text-[11px] text-[var(--text-secondary)]">学习后端 (learnerBackend)</span>
              <input
                type="text"
                disabled={!writable}
                value={configDraft.learnerBackend}
                onChange={(e) =>
                  setConfigDraft((prev) => (prev ? { ...prev, learnerBackend: e.target.value } : prev))
                }
                className={INPUT_CLASS}
              />
            </label>

            <label className="space-y-1">
              <span className="text-[11px] text-[var(--text-secondary)]">学习预设 (learnerPreset)</span>
              <input
                type="text"
                disabled={!writable}
                value={configDraft.learnerPreset}
                onChange={(e) =>
                  setConfigDraft((prev) => (prev ? { ...prev, learnerPreset: e.target.value } : prev))
                }
                className={INPUT_CLASS}
              />
            </label>

            <label className="space-y-1">
              <span className="text-[11px] text-[var(--text-secondary)]">学习工作区标题 (workspaceTitle)</span>
              <input
                type="text"
                disabled={!writable}
                value={configDraft.workspaceTitle}
                onChange={(e) =>
                  setConfigDraft((prev) => (prev ? { ...prev, workspaceTitle: e.target.value } : prev))
                }
                className={INPUT_CLASS}
              />
            </label>
          </div>
        </Card>
      )}

      <Card danger>
        <CardHeader
          title={
            <span className="flex items-center gap-2 text-[var(--danger)]">
              <AlertTriangle className="h-4 w-4" />
              <span>危险操作：清空黑话词库</span>
            </span>
          }
          subtitle="仅删除指定分类或彻底清空词库与提取冷却记录；每次操作均需二次确认。"
        />
        <div className="flex flex-wrap items-center gap-2 p-4">
          <Button
            size="sm"
            variant="secondary"
            disabled={busy || !writable}
            onClick={() => handleClearStatus(statusTab)}
          >
            清空当前「{STATUS_LABELS[statusTab]}」分类
          </Button>
          <Button
            size="sm"
            variant="danger"
            disabled={busy || !writable}
            onClick={() => handleClearStatus()}
          >
            清空全部黑话词库
          </Button>
        </div>
      </Card>
    </div>
  );
};
