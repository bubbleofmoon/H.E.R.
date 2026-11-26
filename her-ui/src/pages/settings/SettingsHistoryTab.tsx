import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { RefreshCw, RotateCcw } from 'lucide-react';
import {
  fetchGlobalSettingsHistory,
  SettingsHistoryEntry,
  undoGlobalSetting,
} from '../../api/config';
import { EmptyState } from '../../components/ConsoleWidgets';
import { Badge, Button, Card, CardHeader } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import { formatDateTimeMs, formatErrorMessage } from '../../utils/format';
import { INPUT_CLASS } from '../../utils/herHelpers';

function formatHistoryValue(val: unknown): string {
  if (val === undefined) return '（未定义）';
  if (val === null) return 'null（默认）';
  if (typeof val === 'string') return val;
  return JSON.stringify(val);
}

export const SettingsHistoryTab: React.FC = () => {
  const {
    settings,
    conversations,
    jumpToSetting,
    setInspector,
    refreshLiveData,
    openConfirm,
    pushToast,
  } = useConsole();

  const [settingFilterInput, setSettingFilterInput] = useState('');
  const [debouncedSettingFilter, setDebouncedSettingFilter] = useState('');
  const [conversationFilter, setConversationFilter] = useState('');
  const [entries, setEntries] = useState<SettingsHistoryEntry[]>([]);
  const [nextCursor, setNextCursor] = useState<number | null>(null);
  const [historyError, setHistoryError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [undoingEntry, setUndoingEntry] = useState<number | null>(null);

  const mountedRef = useRef(true);
  const seqRef = useRef(0);
  const settingFilterRef = useRef(debouncedSettingFilter);
  const conversationFilterRef = useRef(conversationFilter);
  settingFilterRef.current = debouncedSettingFilter;
  conversationFilterRef.current = conversationFilter;

  const isFilterPending = settingFilterInput !== debouncedSettingFilter;

  const settingMap = useMemo(() => {
    const map = new Map<string, (typeof settings)[number]>();
    for (const s of settings) {
      map.set(s.key, s);
    }
    return map;
  }, [settings]);

  useEffect(() => {
    const timer = window.setTimeout(() => {
      setDebouncedSettingFilter(settingFilterInput);
    }, 300);
    return () => window.clearTimeout(timer);
  }, [settingFilterInput]);

  const loadHistory = useCallback(
    async (opts?: { append?: boolean; before?: number | null }) => {
      const seq = ++seqRef.current;
      const isAppend = Boolean(opts?.append);
      if (isAppend) {
        setLoadingMore(true);
      } else {
        setLoading(true);
        setLoadingMore(false);
        setHistoryError(null);
        setEntries([]);
        setNextCursor(null);
      }
      try {
        const res = await fetchGlobalSettingsHistory({
          setting: settingFilterRef.current,
          conversation: conversationFilterRef.current,
          before: opts?.before ?? null,
        });
        if (!mountedRef.current || seq !== seqRef.current) return;
        const incoming = Array.isArray(res.entries) ? res.entries : [];
        setEntries((prev) => (isAppend ? [...prev, ...incoming] : incoming));
        setNextCursor(typeof res.next === 'number' ? res.next : null);
      } catch (err) {
        if (mountedRef.current && seq === seqRef.current) {
          const msg = formatErrorMessage(err, '加载设置变更历史失败');
          setHistoryError(msg);
          if (!isAppend) {
            setEntries([]);
            setNextCursor(null);
          }
          pushToast(msg, 'danger');
        }
      } finally {
        if (mountedRef.current && seq === seqRef.current) {
          setLoading(false);
          setLoadingMore(false);
        }
      }
    },
    [pushToast],
  );

  useEffect(() => {
    mountedRef.current = true;
    void loadHistory({ append: false, before: null });
    return () => {
      mountedRef.current = false;
    };
  }, [loadHistory, debouncedSettingFilter, conversationFilter]);

  const canUndoEntry = (entry: SettingsHistoryEntry): { ok: boolean; reason?: string } => {
    const meta = settingMap.get(entry.setting);
    if (
      meta?.kind.type === 'password' ||
      entry.old === '(hidden)' ||
      entry.new === '(hidden)'
    ) {
      return { ok: false, reason: '凭据条目不支持从历史撤销' };
    }
    if (
      entry.source === 'migration' ||
      entry.source === 'access' ||
      entry.source === 'switch-rollback'
    ) {
      return { ok: false, reason: `来源 ${entry.source} 禁止撤销` };
    }
    return { ok: true };
  };

  const handleUndo = (entry: SettingsHistoryEntry) => {
    if (undoingEntry !== null) return;
    const meta = settingMap.get(entry.setting);
    const needsConfirm = Boolean(meta?.confirm);
    openConfirm({
      title: needsConfirm
        ? `确认撤销安全设置 #${entry.entry}（${entry.setting}）？`
        : `确认撤销变更 #${entry.entry}（${entry.setting}）？`,
      description: `将尝试把 ${entry.setting} 回滚至变更前的旧值：${formatHistoryValue(entry.old)}。\n仅在仍为该设置最新一条变更时允许撤销。`,
      confirmLabel: '确认撤销',
      danger: true,
      onConfirm: async () => {
        setUndoingEntry(entry.entry);
        try {
          const res = await undoGlobalSetting(entry.entry, needsConfirm);
          if (!mountedRef.current) return;
          const warnMsg = [res.mirrorError, res.wakeRefreshError].filter(Boolean).join('；');
          pushToast(
            warnMsg
              ? `已撤销 ${entry.setting}（附加提示：${warnMsg}）`
              : `已撤销变更 #${entry.entry}（${entry.setting}）`,
            warnMsg ? 'warn' : 'success',
          );
          await Promise.all([loadHistory({ append: false, before: null }), refreshLiveData()]);
        } catch (err) {
          if (mountedRef.current) {
            pushToast(formatErrorMessage(err, `撤销变更 #${entry.entry} 失败`), 'danger');
          }
        } finally {
          if (mountedRef.current) {
            setUndoingEntry(null);
          }
        }
      },
    });
  };

  return (
    <div className="space-y-4">
      <Card className="p-4">
        <CardHeader
          title="筛选历史记录"
          subtitle="每页返回 100 条设置审计轨迹；精确输入 setting key 后自动查询，或按会话作用域过滤"
          actions={
            <Button
              size="sm"
              variant="secondary"
              disabled={loading}
              onClick={() => {
                settingFilterRef.current = settingFilterInput;
                setDebouncedSettingFilter(settingFilterInput);
                void loadHistory({ append: false, before: null });
              }}
            >
              <RefreshCw className="h-3.5 w-3.5" />
              刷新历史
            </Button>
          }
        />

        <div className="grid gap-3 md:grid-cols-[1fr_1fr_auto]">
          <input
            type="text"
            value={settingFilterInput}
            onChange={(e) => setSettingFilterInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') {
                setDebouncedSettingFilter(settingFilterInput);
              }
            }}
            placeholder="输入完整 setting key 过滤（如 harness.model）"
            className={INPUT_CLASS}
          />
          <select
            aria-label="按会话过滤设置历史"
            value={conversationFilter}
            onChange={(e) => setConversationFilter(e.target.value)}
            className={INPUT_CLASS}
          >
            <option value="">全部作用域（含全局与会话）</option>
            {conversations.map((c) => (
              <option key={c.key} value={c.key}>
                {c.name ? `${c.name} (${c.key})` : c.key}
              </option>
            ))}
          </select>
          {(settingFilterInput || conversationFilter) && (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => {
                setSettingFilterInput('');
                setDebouncedSettingFilter('');
                setConversationFilter('');
              }}
            >
              重置筛选
            </Button>
          )}
        </div>
      </Card>

      <Card className="p-4">
        <CardHeader
          title="变更历史时间线"
          subtitle="撤销操作仅允许从当前最新历史记录逆序回退"
          actions={<Badge tone="neutral">已加载 {entries.length} 条</Badge>}
        />

        {historyError && (
          <div className="mb-3 rounded-md border border-[var(--danger)]/40 bg-[var(--danger-soft)] px-3 py-2 text-xs text-[var(--text-primary)]">
            {historyError}
          </div>
        )}

        {entries.length === 0 ? (
          <EmptyState
            title={
              loading
                ? '加载设置变更历史中…'
                : historyError
                  ? '查询设置历史失败（需完整且已注册的 setting key）'
                  : '暂无符合条件的设置变更记录'
            }
          />
        ) : (
          <div className="divide-y divide-[var(--border-hairline)]">
            {entries.map((entry) => {
              const undoPolicy = canUndoEntry(entry);
              const isConversationScope = entry.scope !== 'global' && entry.scope.length > 0;
              const isGlobalSetting = settingMap.has(entry.setting);
              const undoBusy = loading || loadingMore || isFilterPending || undoingEntry !== null;
              return (
                <div key={entry.entry} className="space-y-2 py-3 text-xs">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="font-mono font-semibold text-[var(--text-muted)]">
                        #{entry.entry}
                      </span>
                      <button
                        type="button"
                        onClick={() => {
                          if (!isConversationScope && isGlobalSetting) {
                            jumpToSetting(entry.setting);
                          } else if (isConversationScope) {
                            setInspector({ kind: 'conversation', id: entry.scope });
                          } else {
                            jumpToSetting(entry.setting, false);
                          }
                        }}
                        className="font-mono font-semibold text-[var(--accent)] hover:underline"
                      >
                        {entry.setting}
                      </button>
                      <Badge tone="neutral">{entry.source}</Badge>
                      {isConversationScope ? (
                        <button
                          type="button"
                          onClick={() => setInspector({ kind: 'conversation', id: entry.scope })}
                          className="font-mono text-[11px] text-[var(--accent)] hover:underline"
                        >
                          会话: {entry.scope}
                        </button>
                      ) : (
                        <Badge tone="accent">global</Badge>
                      )}
                      <span className="text-[11px] text-[var(--text-muted)]">
                        操作者: {entry.actor || '—'}
                      </span>
                    </div>

                    <div className="flex items-center gap-2">
                      <span className="font-mono text-[11px] text-[var(--text-muted)]">
                        {formatDateTimeMs(entry.atMs)}
                      </span>
                      <Button
                        size="sm"
                        variant="secondary"
                        disabled={!undoPolicy.ok || undoBusy}
                        title={undoPolicy.reason}
                        onClick={() => handleUndo(entry)}
                      >
                        <RotateCcw className="h-3 w-3" />
                        {undoingEntry === entry.entry ? '撤销中…' : '撤销'}
                      </Button>
                    </div>
                  </div>

                  <div className="grid gap-2 md:grid-cols-2">
                    <div className="rounded bg-[var(--bg-subtle)]/50 px-2.5 py-1.5">
                      <div className="text-[10.5px] text-[var(--text-muted)]">变更前 (old)</div>
                      <pre className="mt-0.5 overflow-x-auto whitespace-pre-wrap break-all font-mono text-[11px] text-[var(--text-secondary)]">
                        {formatHistoryValue(entry.old)}
                      </pre>
                    </div>
                    <div className="rounded bg-[var(--bg-subtle)]/50 px-2.5 py-1.5">
                      <div className="text-[10.5px] text-[var(--text-muted)]">变更后 (new)</div>
                      <pre className="mt-0.5 overflow-x-auto whitespace-pre-wrap break-all font-mono text-[11px] text-[var(--text-primary)]">
                        {formatHistoryValue(entry.new)}
                      </pre>
                    </div>
                  </div>
                </div>
              );
            })}
          </div>
        )}

        {nextCursor !== null && (
          <div className="mt-4 flex justify-center">
            <Button
              size="sm"
              variant="secondary"
              disabled={loading || loadingMore || isFilterPending}
              onClick={() => void loadHistory({ append: true, before: nextCursor })}
            >
              {loadingMore ? '加载中…' : '继续加载更早记录'}
            </Button>
          </div>
        )}
      </Card>
    </div>
  );
};
