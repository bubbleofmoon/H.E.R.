import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  AlertTriangle,
  Archive,
  ArrowUpRight,
  BookOpen,
  Brain,
  Database,
  Eye,
  Plus,
  RefreshCw,
  Search,
  Sparkles,
  Trash2,
} from 'lucide-react';
import { ApiRequestError } from '../../api/client';
import {
  LightMemorySnapshot,
  LongTermMemoryHit,
  archiveMemory,
  fetchLightMemory,
  fetchMemoryIndex,
  forgetMemory,
  previewMemory,
  searchMemories,
  writeMemory,
} from '../../api/her';
import { EmptyState, SearchInput } from '../../components/ConsoleWidgets';
import { Badge, Button, Card, CardHeader, SettingField } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import { formatDateTimeMs, formatErrorMessage } from '../../utils/format';
import { INPUT_CLASS, useSettingsDrafts } from '../../utils/herHelpers';

export const MemoryLibraryTab: React.FC = () => {
  const { settings, conversations, setInspector, openConfirm, pushToast } = useConsole();

  const [selectedKey, setSelectedKey] = useState('');
  const [searchQuery, setSearchQuery] = useState('');
  const [provenanceFilter, setProvenanceFilter] = useState<'all' | 'fact' | 'inference'>('all');
  const [scopeFilter, setScopeFilter] = useState<string>('all');
  const [kindFilter, setKindFilter] = useState('');
  const [hits, setHits] = useState<LongTermMemoryHit[] | null>(null);
  const [longTermNotice, setLongTermNotice] = useState<string | null>(null);
  const [longTermBusy, setLongTermBusy] = useState(false);

  const [previewBlock, setPreviewBlock] = useState<{ chars: number; block: string } | null>(null);
  const [indexInfo, setIndexInfo] = useState<{ total: number; terms: string[] } | null>(null);

  // Manual write form (reset when selectedKey changes)
  const [writeOpen, setWriteOpen] = useState(false);
  const [writeContent, setWriteContent] = useState('');
  const [writeKind, setWriteKind] = useState('fact');
  const [writeScope, setWriteScope] = useState('');
  const [writeSubject, setWriteSubject] = useState('');
  const [writeTags, setWriteTags] = useState('');

  // Archive by ID & danger-zone forget by ID
  const [archiveIdInput, setArchiveIdInput] = useState('');
  const [forgetIdInput, setForgetIdInput] = useState('');

  // Read-only light memory snapshot (mutations live in ConversationInspector per spec)
  const [lightLoading, setLightLoading] = useState(false);
  const [lightError, setLightError] = useState<string | null>(null);
  const [lightMemory, setLightMemory] = useState<LightMemorySnapshot | null>(null);

  const mountedRef = useRef(true);
  const lightGenRef = useRef(0);
  const searchGenRef = useRef(0);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  useEffect(() => {
    if (!selectedKey && conversations.length > 0) {
      setSelectedKey(conversations[0].key);
    }
  }, [selectedKey, conversations]);

  const memorySettings = useMemo(
    () =>
      settings.filter(
        (s) =>
          s.group === 'memory and digest' ||
          s.key === 'memory.digest.model' ||
          s.key === 'memory.digest.timeoutSecs' ||
          s.key === 'memory.digest.time' ||
          s.key === 'memory.digest.checkMinute',
      ),
    [settings],
  );

  const {
    drafts: settingDrafts,
    setDraft: setSettingDraft,
    handleReset: handleResetMemorySetting,
  } = useSettingsDrafts(memorySettings, '记忆与摘要参数');

  const handleSelectKey = (nextKey: string) => {
    if (nextKey === selectedKey) return;
    searchGenRef.current += 1;
    lightGenRef.current += 1;
    setSelectedKey(nextKey);
    setLongTermBusy(false);
    setHits(null);
    setPreviewBlock(null);
    setIndexInfo(null);
    setLongTermNotice(null);
    setScopeFilter('all');
    setWriteOpen(false);
    setWriteContent('');
    setWriteSubject('');
    setWriteTags('');
    setWriteScope('');
    setForgetIdInput('');
    setLightMemory(null);
    setLightError(null);
  };

  const loadLight = useCallback(async (key: string) => {
    const trimmed = key.trim();
    const gen = ++lightGenRef.current;
    setLightMemory(null);
    setLightError(null);
    if (!trimmed) {
      setLightLoading(false);
      return;
    }
    setLightLoading(true);
    try {
      const res = await fetchLightMemory(trimmed);
      if (!mountedRef.current || gen !== lightGenRef.current) return;
      setLightMemory(res);
    } catch (err) {
      if (!mountedRef.current || gen !== lightGenRef.current) return;
      setLightMemory(null);
      setLightError(formatErrorMessage(err, '加载对话轻记忆失败'));
    } finally {
      if (mountedRef.current && gen === lightGenRef.current) {
        setLightLoading(false);
      }
    }
  }, []);

  useEffect(() => {
    if (selectedKey) {
      void loadLight(selectedKey);
    }
  }, [selectedKey, loadLight]);

  const explainLongTermRestriction = (err: unknown, fallback: string) => {
    if (err instanceof ApiRequestError && err.status === 403) {
      const msg = err.message || '';
      if (msg.includes('agent token') || msg.includes('会话自己的令牌')) {
        setLongTermNotice(
          '长期记忆检索/写入/预览/索引/遗忘在后端限定由对应会话的 agent token 调用，控制台会话未携带该会话令牌；上方可直接执行按 ID 归档、在会话抽屉中编辑轻记忆及调整记忆参数。',
        );
      } else {
        setLongTermNotice(msg || '当前会话无权访问或删除该条记忆（不在可见范围）');
      }
      return;
    }
    setLongTermNotice(formatErrorMessage(err, fallback));
  };

  const handleSearchLongTerm = async (e?: React.FormEvent) => {
    if (e) e.preventDefault();
    if (!selectedKey) {
      pushToast('请先选择目标会话', 'warn');
      return;
    }
    const gen = ++searchGenRef.current;
    setLongTermBusy(true);
    setLongTermNotice(null);
    try {
      const res = await searchMemories(
        selectedKey,
        searchQuery.trim(),
        kindFilter.trim() || undefined,
        30,
      );
      if (!mountedRef.current || gen !== searchGenRef.current) return;
      setHits(res.memories ?? []);
    } catch (err) {
      if (!mountedRef.current || gen !== searchGenRef.current) return;
      explainLongTermRestriction(err, '检索长期记忆失败');
    } finally {
      if (mountedRef.current && gen === searchGenRef.current) {
        setLongTermBusy(false);
      }
    }
  };

  const handlePreviewLongTerm = async () => {
    if (!selectedKey) return;
    const gen = ++searchGenRef.current;
    setLongTermBusy(true);
    setLongTermNotice(null);
    try {
      const res = await previewMemory(selectedKey);
      if (!mountedRef.current || gen !== searchGenRef.current) return;
      setPreviewBlock({ chars: res.chars, block: res.block });
    } catch (err) {
      if (!mountedRef.current || gen !== searchGenRef.current) return;
      explainLongTermRestriction(err, '获取召回预览失败');
    } finally {
      if (mountedRef.current && gen === searchGenRef.current) {
        setLongTermBusy(false);
      }
    }
  };

  const handleIndexLongTerm = async () => {
    if (!selectedKey) return;
    const gen = ++searchGenRef.current;
    setLongTermBusy(true);
    setLongTermNotice(null);
    try {
      const res = await fetchMemoryIndex(selectedKey);
      if (!mountedRef.current || gen !== searchGenRef.current) return;
      setIndexInfo({ total: res.total, terms: res.terms });
    } catch (err) {
      if (!mountedRef.current || gen !== searchGenRef.current) return;
      explainLongTermRestriction(err, '读取记忆索引失败');
    } finally {
      if (mountedRef.current && gen === searchGenRef.current) {
        setLongTermBusy(false);
      }
    }
  };

  const handleWriteLongTerm = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!selectedKey || !writeContent.trim()) {
      pushToast('请填写记忆正文内容', 'warn');
      return;
    }
    const targetKey = selectedKey;
    const gen = ++searchGenRef.current;
    setLongTermBusy(true);
    setLongTermNotice(null);
    try {
      const res = await writeMemory({
        key: targetKey,
        content: writeContent.trim(),
        kind: writeKind.trim() || 'fact',
        scope: writeScope.trim() || undefined,
        subject: writeSubject.trim() || undefined,
        tags: writeTags.trim() || undefined,
      });
      if (!mountedRef.current || gen !== searchGenRef.current) return;
      pushToast(res.deduped ? `已去重复用记忆 #${res.id}` : `已写入记忆 #${res.id}`, 'success');
      setWriteContent('');
      setWriteOpen(false);
      await handleSearchLongTerm();
    } catch (err) {
      if (!mountedRef.current || gen !== searchGenRef.current) return;
      explainLongTermRestriction(err, '写入长期记忆失败');
    } finally {
      if (mountedRef.current && gen === searchGenRef.current) {
        setLongTermBusy(false);
      }
    }
  };

  const handleToggleArchiveHit = async (hit: LongTermMemoryHit) => {
    if (longTermBusy) return;
    const targetArchived = hit.archived === 0;
    const gen = searchGenRef.current;
    setLongTermBusy(true);
    try {
      const res = await archiveMemory(hit.id, targetArchived);
      if (!mountedRef.current || gen !== searchGenRef.current) return;
      if (res.changed) {
        pushToast(res.archived ? `记忆 #${res.id} 已归档` : `记忆 #${res.id} 已取消归档`, 'success');
        setHits((prev) =>
          prev
            ? prev.map((x) => (x.id === hit.id ? { ...x, archived: res.archived ? 1 : 0 } : x))
            : prev,
        );
      } else {
        pushToast(
          `记忆 #${res.id} 状态未变更（可能已被删除、并发修改或被新记忆替代），已从列表移除`,
          'info',
        );
        setHits((prev) => (prev ? prev.filter((x) => x.id !== hit.id) : prev));
      }
    } catch (err) {
      if (mountedRef.current && gen === searchGenRef.current) {
        pushToast(formatErrorMessage(err, '归档操作失败'), 'danger');
      }
    } finally {
      if (mountedRef.current && gen === searchGenRef.current) {
        setLongTermBusy(false);
      }
    }
  };

  const handleForgetById = (id: number) => {
    const targetKey = selectedKey;
    if (!targetKey || !Number.isInteger(id) || id <= 0) {
      pushToast('请先选择会话并输入有效的正整数记忆 ID', 'warn');
      return;
    }
    const gen = searchGenRef.current;
    openConfirm({
      title: `确认永久遗忘记忆 #${id}？`,
      description: `将在会话 ${targetKey} 作用域下从 SQLite 记忆库与 FTS 索引中物理删除该条目，不可恢复。`,
      confirmLabel: '永久遗忘',
      danger: true,
      onConfirm: async () => {
        setLongTermBusy(true);
        try {
          await forgetMemory(targetKey, id);
          if (!mountedRef.current || gen !== searchGenRef.current) return;
          pushToast(`已永久遗忘记忆 #${id}`, 'success');
          setForgetIdInput('');
          setHits((prev) => (prev ? prev.filter((item) => item.id !== id) : prev));
        } catch (err) {
          if (!mountedRef.current || gen !== searchGenRef.current) return;
          if (err instanceof ApiRequestError && err.status === 403) {
            const msg = err.message || '';
            if (msg.includes('agent token') || msg.includes('会话自己的令牌')) {
              pushToast('缺少该会话的 agent token：浏览器管理员会话无法直接调用遗忘接口', 'warn');
            } else {
              pushToast(msg || '该条记忆不在当前会话可见范围内，无权删除', 'danger');
            }
          } else {
            pushToast(formatErrorMessage(err, '永久遗忘失败'), 'danger');
          }
        } finally {
          if (mountedRef.current && gen === searchGenRef.current) {
            setLongTermBusy(false);
          }
        }
      },
    });
  };

  const handleArchiveById = async (archived: boolean) => {
    const num = Number(archiveIdInput.trim());
    if (!Number.isInteger(num) || num <= 0) {
      pushToast('请输入有效的正整数记忆 ID', 'warn');
      return;
    }
    const gen = searchGenRef.current;
    setLongTermBusy(true);
    try {
      const res = await archiveMemory(num, archived);
      if (!mountedRef.current) return;
      if (res.changed) {
        pushToast(`记忆 #${res.id} 已${res.archived ? '归档' : '取消归档'}`, 'success');
        if (gen === searchGenRef.current) {
          setHits((prev) =>
            prev
              ? prev.map((h) => (h.id === num ? { ...h, archived: res.archived ? 1 : 0 } : h))
              : prev,
          );
        }
      } else {
        pushToast(
          `记忆 #${res.id} 状态未变化（可能已被删除、新记忆替代或已处于目标状态）`,
          'info',
        );
        if (gen === searchGenRef.current) {
          setHits((prev) => (prev ? prev.filter((h) => h.id !== num) : prev));
        }
      }
    } catch (err) {
      if (mountedRef.current) {
        pushToast(formatErrorMessage(err, '归档操作失败'), 'danger');
      }
    } finally {
      if (mountedRef.current && gen === searchGenRef.current) {
        setLongTermBusy(false);
      }
    }
  };

  const scopeOptions = useMemo(() => {
    const set = new Set<string>(['group', 'private']);
    if (hits) {
      for (const h of hits) {
        if (h.scope) set.add(h.scope);
      }
    }
    return Array.from(set);
  }, [hits]);

  const filteredHits = useMemo(() => {
    if (!hits) return null;
    return hits.filter((h) => {
      if (provenanceFilter !== 'all' && h.provenance !== provenanceFilter) return false;
      if (scopeFilter !== 'all' && h.scope !== scopeFilter) return false;
      return true;
    });
  }, [hits, provenanceFilter, scopeFilter]);

  const lightMatched = lightMemory !== null && lightMemory.key === selectedKey;

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader
          title={
            <span className="flex items-center gap-2">
              <Database className="h-4 w-4 text-[var(--accent)]" />
              <span>长期记忆检索、召回预览与归档</span>
            </span>
          }
          subtitle="按事实/推断溯源与作用域筛选、预览唤醒注入块、查看索引或执行按 ID 归档。"
          actions={
            <select
              value={selectedKey}
              aria-label="选择记忆库目标会话"
              onChange={(e) => handleSelectKey(e.target.value)}
              className="h-7 rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs text-[var(--text-primary)]"
            >
              {conversations.length === 0 && <option value="">暂无会话</option>}
              {conversations.map((c) => (
                <option key={c.key} value={c.key}>
                  {c.name ? `${c.name} (${c.key})` : c.key}
                </option>
              ))}
            </select>
          }
        />

        <div className="space-y-3 p-4 text-xs">
          <form
            className="flex flex-wrap items-center gap-2"
            onSubmit={(e) => void handleSearchLongTerm(e)}
          >
            <div className="min-w-[200px] flex-1">
              <SearchInput
                value={searchQuery}
                onChange={setSearchQuery}
                placeholder="输入关键词检索（留空返回置顶与近期记忆）…"
              />
            </div>
            <select
              value={provenanceFilter}
              aria-label="按溯源过滤记忆"
              onChange={(e) =>
                setProvenanceFilter(e.target.value as 'all' | 'fact' | 'inference')
              }
              className="h-8 rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs"
            >
              <option value="all">全部溯源</option>
              <option value="fact">仅事实 (fact)</option>
              <option value="inference">仅推断 (inference)</option>
            </select>
            <select
              value={scopeFilter}
              aria-label="按作用域过滤记忆"
              onChange={(e) => setScopeFilter(e.target.value)}
              className="h-8 rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs"
            >
              <option value="all">全部范围</option>
              {scopeOptions.map((sc) => (
                <option key={sc} value={sc}>
                  范围: {sc}
                </option>
              ))}
            </select>
            <input
              type="text"
              value={kindFilter}
              onChange={(e) => setKindFilter(e.target.value)}
              placeholder="类型过滤 (fact,inference)"
              aria-label="记忆类型过滤"
              className="h-8 w-40 rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs"
            />
            <Button size="sm" variant="primary" type="submit" disabled={longTermBusy || !selectedKey}>
              <Search className="h-3.5 w-3.5" />
              搜索
            </Button>
            <Button
              size="sm"
              variant="secondary"
              disabled={longTermBusy || !selectedKey}
              onClick={() => void handlePreviewLongTerm()}
            >
              <Eye className="h-3.5 w-3.5" />
              召回预览
            </Button>
            <Button
              size="sm"
              variant="secondary"
              disabled={longTermBusy || !selectedKey}
              onClick={() => void handleIndexLongTerm()}
            >
              <BookOpen className="h-3.5 w-3.5" />
              索引词
            </Button>
            <Button size="sm" variant="secondary" onClick={() => setWriteOpen((v) => !v)}>
              <Plus className="h-3.5 w-3.5" />
              写入记忆
            </Button>
          </form>

          {writeOpen && (
            <Card className="bg-[var(--bg-subtle)]/40 p-3">
              <form className="space-y-2" onSubmit={(e) => void handleWriteLongTerm(e)}>
                <div className="grid grid-cols-1 gap-2 sm:grid-cols-4">
                  <input
                    type="text"
                    value={writeKind}
                    onChange={(e) => setWriteKind(e.target.value)}
                    placeholder="kind (如 fact / inference)"
                    className={INPUT_CLASS}
                  />
                  <input
                    type="text"
                    value={writeScope}
                    onChange={(e) => setWriteScope(e.target.value)}
                    placeholder="scope (默认按会话)"
                    className={INPUT_CLASS}
                  />
                  <input
                    type="text"
                    value={writeSubject}
                    onChange={(e) => setWriteSubject(e.target.value)}
                    placeholder="subject（可选）"
                    className={INPUT_CLASS}
                  />
                  <input
                    type="text"
                    value={writeTags}
                    onChange={(e) => setWriteTags(e.target.value)}
                    placeholder="tags（可选）"
                    className={INPUT_CLASS}
                  />
                </div>
                <textarea
                  rows={3}
                  value={writeContent}
                  onChange={(e) => setWriteContent(e.target.value)}
                  placeholder={`为 ${selectedKey || '所选会话'} 写入记忆正文 *`}
                  className={INPUT_CLASS}
                />
                <div className="flex justify-end gap-2">
                  <Button size="sm" variant="primary" type="submit" disabled={longTermBusy}>
                    提交写入
                  </Button>
                </div>
              </form>
            </Card>
          )}

          {longTermNotice && (
            <div className="rounded-md border border-amber-300 bg-[var(--warn-soft)] p-3 text-xs text-[var(--warn)] dark:border-amber-900">
              {longTermNotice}
            </div>
          )}

          {previewBlock && (
            <Card className="bg-[var(--bg-subtle)]/40 p-3">
              <div className="mb-1.5 flex items-center justify-between text-[11px] text-[var(--text-muted)]">
                <span>唤醒召回注入预览（{previewBlock.chars} 字符）</span>
                <button
                  type="button"
                  onClick={() => setPreviewBlock(null)}
                  className="hover:text-[var(--text-primary)]"
                >
                  关闭
                </button>
              </div>
              <pre className="max-h-56 overflow-auto whitespace-pre-wrap font-mono text-[11px] text-[var(--text-secondary)]">
                {previewBlock.block || '（当前无召回注入内容）'}
              </pre>
            </Card>
          )}

          {indexInfo && (
            <div className="flex flex-wrap items-center gap-1.5 rounded-md border border-[var(--border-hairline)] p-2.5">
              <span className="text-[11px] text-[var(--text-muted)]">
                可见记忆总数 {indexInfo.total} · 索引词：
              </span>
              {indexInfo.terms.length === 0 ? (
                <span className="text-[11px] text-[var(--text-muted)]">（空）</span>
              ) : (
                indexInfo.terms.map((t) => (
                  <Badge key={t} tone="neutral">
                    {t}
                  </Badge>
                ))
              )}
            </div>
          )}

          {filteredHits &&
            (filteredHits.length === 0 ? (
              <EmptyState title="未命中匹配记忆" description="尝试调整搜索词或切换溯源/作用域过滤条件。" />
            ) : (
              <div className="space-y-2">
                {filteredHits.map((hit) => (
                  <div
                    key={hit.id}
                    className="rounded-md border border-[var(--border-hairline)] p-3"
                  >
                    <div className="flex flex-wrap items-center justify-between gap-2">
                      <div className="flex flex-wrap items-center gap-1.5">
                        <span className="font-mono text-[11px] font-semibold">#{hit.id}</span>
                        <Badge tone={hit.provenance === 'fact' ? 'success' : 'accent'}>
                          {hit.provenance === 'fact' ? '事实' : '推断'}
                        </Badge>
                        <Badge tone="neutral">{hit.kind}</Badge>
                        <Badge tone="neutral">{hit.scope}</Badge>
                        {hit.archived === 1 && <Badge tone="warn">已归档</Badge>}
                        {hit.attributed_to && (
                          <span className="text-[11px] text-[var(--text-muted)]">
                            引用发言人 {hit.attributed_to}
                          </span>
                        )}
                      </div>
                      <div className="flex items-center gap-1">
                        <Button
                          size="sm"
                          variant="ghost"
                          disabled={longTermBusy}
                          onClick={() => void handleToggleArchiveHit(hit)}
                        >
                          <Archive className="h-3 w-3" />
                          {hit.archived === 0 ? '归档' : '取消归档'}
                        </Button>
                        <Button
                          size="sm"
                          variant="ghost"
                          disabled={longTermBusy}
                          onClick={() => setForgetIdInput(String(hit.id))}
                        >
                          <Trash2 className="h-3 w-3 text-[var(--danger)]" />
                          填入下方遗忘区
                        </Button>
                      </div>
                    </div>
                    <p className="mt-1.5 text-xs text-[var(--text-primary)]">{hit.content}</p>
                    <div className="mt-1 flex flex-wrap gap-3 text-[10.5px] text-[var(--text-muted)]">
                      <span>owner={hit.owner_id}</span>
                      {hit.subject && <span>subject={hit.subject}</span>}
                      <span>命中 {hit.hits}</span>
                      <span>创建于 {formatDateTimeMs(hit.created_at)}</span>
                    </div>
                  </div>
                ))}
              </div>
            ))}

          <div className="flex flex-wrap items-center gap-2 border-t border-[var(--border-hairline)] pt-3">
            <span className="text-[11px] font-medium text-[var(--text-secondary)]">
              按记录 ID 归档 / 恢复（管理员直通）：
            </span>
            <input
              type="number"
              min={1}
              value={archiveIdInput}
              onChange={(e) => setArchiveIdInput(e.target.value)}
              placeholder="输入记忆 ID"
              aria-label="按 ID 归档的记忆编号"
              className="h-7 w-36 rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs"
            />
            <Button
              size="sm"
              variant="secondary"
              disabled={longTermBusy}
              onClick={() => void handleArchiveById(true)}
            >
              <Archive className="h-3 w-3" />
              归档
            </Button>
            <Button
              size="sm"
              variant="ghost"
              disabled={longTermBusy}
              onClick={() => void handleArchiveById(false)}
            >
              取消归档
            </Button>
          </div>
        </div>
      </Card>

      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader
            title={
              <span className="flex items-center gap-2">
                <Brain className="h-4 w-4 text-[var(--accent)]" />
                <span>对话轻记忆总览 ({selectedKey || '未选'})</span>
              </span>
            }
            subtitle="单会话轻记忆的唯一编辑入口位于「会话」右侧抽屉；此处仅提供只读查看与直达入口。"
            actions={
              <div className="flex items-center gap-1.5">
                <Button
                  size="sm"
                  variant="ghost"
                  aria-label="刷新对话轻记忆"
                  disabled={lightLoading || !selectedKey}
                  onClick={() => void loadLight(selectedKey)}
                >
                  <RefreshCw className={`h-3.5 w-3.5 ${lightLoading ? 'animate-spin' : ''}`} />
                </Button>
                <Button
                  size="sm"
                  variant="secondary"
                  disabled={!selectedKey}
                  onClick={() =>
                    selectedKey && setInspector({ kind: 'conversation', id: selectedKey })
                  }
                >
                  <ArrowUpRight className="h-3.5 w-3.5" />
                  在会话抽屉中编辑轻记忆
                </Button>
              </div>
            }
          />
          <div className="space-y-3 p-4 text-xs">
            {lightError && (
              <div className="rounded border border-rose-300 bg-[var(--danger-soft)] p-2.5 text-[var(--danger)]">
                {lightError}
              </div>
            )}

            {lightLoading && !lightMatched && (
              <div className="text-[11px] text-[var(--text-muted)]">正在加载轻记忆快照…</div>
            )}

            {lightMatched && lightMemory && (
              <div className="space-y-2.5">
                <div>
                  <div className="mb-1 text-[11px] font-semibold text-[var(--text-secondary)]">
                    活跃话题 ({lightMemory.raw.activeTopics.length})
                  </div>
                  {lightMemory.raw.activeTopics.length === 0 ? (
                    <div className="text-[11px] text-[var(--text-muted)]">无</div>
                  ) : (
                    lightMemory.raw.activeTopics.map((t) => (
                      <div
                        key={t.text}
                        className="mb-1 rounded border border-[var(--border-hairline)] px-2.5 py-1.5"
                      >
                        <div className="text-[var(--text-primary)]">{t.text}</div>
                        {t.pendingQuestion && (
                          <div className="mt-0.5 text-[11px] text-[var(--text-muted)]">
                            待跟进：{t.pendingQuestion}
                          </div>
                        )}
                      </div>
                    ))
                  )}
                </div>

                <div>
                  <div className="mb-1 text-[11px] font-semibold text-[var(--text-secondary)]">
                    待处理想法 ({lightMemory.raw.pendingThoughts.length})
                  </div>
                  {lightMemory.raw.pendingThoughts.length === 0 ? (
                    <div className="text-[11px] text-[var(--text-muted)]">无</div>
                  ) : (
                    lightMemory.raw.pendingThoughts.map((t) => (
                      <div
                        key={t.text}
                        className="mb-1 rounded border border-[var(--border-hairline)] px-2.5 py-1.5"
                      >
                        <div className="text-[var(--text-primary)]">{t.text}</div>
                        {t.motivation && (
                          <div className="mt-0.5 text-[11px] text-[var(--text-muted)]">
                            动机：{t.motivation}
                          </div>
                        )}
                      </div>
                    ))
                  )}
                </div>

                <div>
                  <div className="mb-1 text-[11px] font-semibold text-[var(--text-secondary)]">
                    成员印象 ({Object.keys(lightMemory.raw.memberImpressions).length})
                  </div>
                  {Object.keys(lightMemory.raw.memberImpressions).length === 0 ? (
                    <div className="text-[11px] text-[var(--text-muted)]">无</div>
                  ) : (
                    Object.entries(lightMemory.raw.memberImpressions).map(([member, info]) => (
                      <div
                        key={member}
                        className="mb-1 flex items-center justify-between gap-2 rounded border border-[var(--border-hairline)] px-2.5 py-1.5"
                      >
                        <span className="font-mono font-medium">{member}</span>
                        <span className="truncate text-[var(--text-secondary)]">
                          {(info.traits ?? []).join(', ') || '（无特征）'}
                        </span>
                      </div>
                    ))
                  )}
                </div>
              </div>
            )}
          </div>
        </Card>

        <Card className="border-dashed">
          <CardHeader
            title={
              <span className="flex items-center gap-2">
                <Sparkles className="h-4 w-4 text-[var(--accent)]" />
                <span>印象卡 (#26 预留槽位)</span>
              </span>
            }
            subtitle="成员与群聊长期结构化印象卡浏览、溯源对照与编辑视图将在 #26 接入此处。"
          />
          <div className="p-4 text-xs text-[var(--text-muted)]">
            此位置为 #26 印象卡预留工作区；当前版本已支持上方长期记忆的「事实 / 推断」溯源筛选与按会话只读轻记忆快照预览。
          </div>
        </Card>
      </div>

      <Card>
        <CardHeader
          title="记忆召回、分层预算与整理参数 (/api/settings)"
          subtitle="所有参数均直连设置注册表；修改后通过底部保存栏提交。"
        />
        <div className="grid grid-cols-1 gap-4 p-4 md:grid-cols-2 xl:grid-cols-3">
          {memorySettings.map((setting) => {
            const currentDraft =
              setting.key in settingDrafts ? settingDrafts[setting.key] : setting.value;
            return (
              <div
                key={setting.key}
                className="rounded-md border border-[var(--border-hairline)] p-3"
              >
                <SettingField
                  setting={setting}
                  draftValue={currentDraft}
                  onDraftChange={(val) => setSettingDraft(setting.key, val)}
                  onReset={() => handleResetMemorySetting(setting.key)}
                />
              </div>
            );
          })}
        </div>
      </Card>

      <Card danger>
        <CardHeader
          title={
            <span className="flex items-center gap-2 text-[var(--danger)]">
              <AlertTriangle className="h-4 w-4" />
              <span>危险操作：永久遗忘长期记忆</span>
            </span>
          }
          subtitle="从 SQLite 与 FTS 索引中物理删除指定记忆（需当前选定会话权限，执行前需二次确认，不可恢复）。"
        />
        <div className="flex flex-wrap items-center gap-2 p-4 text-xs">
          <input
            type="number"
            min={1}
            value={forgetIdInput}
            onChange={(e) => setForgetIdInput(e.target.value)}
            placeholder="输入要遗忘的记忆 ID"
            aria-label="永久遗忘记忆 ID"
            className="h-8 w-44 rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs"
          />
          <Button
            size="sm"
            variant="danger"
            disabled={longTermBusy || !selectedKey || !forgetIdInput.trim()}
            onClick={() => handleForgetById(Number(forgetIdInput.trim()))}
          >
            <Trash2 className="h-3.5 w-3.5" />
            永久遗忘
          </Button>
        </div>
      </Card>
    </div>
  );
};
