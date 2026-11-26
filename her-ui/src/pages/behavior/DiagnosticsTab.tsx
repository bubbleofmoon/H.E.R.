import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { RefreshCw, Trash2 } from 'lucide-react';
import {
  clearSocialV2Feedback,
  clearToolLog,
  fetchSendOutbox,
  fetchSocialV2Feedback,
  fetchToolLog,
  SendOutboxBatch,
  SocialV2FeedbackEntry,
  ToolLogEntry,
} from '../../api/config';
import { EmptyState, SearchInput, SegmentedControl } from '../../components/ConsoleWidgets';
import { Badge, Button, Card, CardHeader } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import { formatDateTimeMs, formatDateTimeValue, formatErrorMessage } from '../../utils/format';
import { INPUT_CLASS } from '../../utils/herHelpers';

export const DiagnosticsTab: React.FC = () => {
  const { conversations, setInspector, openConfirm, pushToast } = useConsole();

  const [feedback, setFeedback] = useState<SocialV2FeedbackEntry[]>([]);
  const [feedbackLevel, setFeedbackLevel] = useState<'all' | 'info' | 'warning' | 'error'>('all');
  const [loadingFeedback, setLoadingFeedback] = useState(true);

  const [toolLog, setToolLog] = useState<ToolLogEntry[]>([]);
  const [toolLimit, setToolLimit] = useState<50 | 200 | 500>(200);
  const [toolQuery, setToolQuery] = useState('');
  const [loadingTools, setLoadingTools] = useState(true);

  const [selectedOutboxKey, setSelectedOutboxKey] = useState<string>('');
  const [outboxBatches, setOutboxBatches] = useState<SendOutboxBatch[]>([]);
  const [outboxError, setOutboxError] = useState<string | null>(null);
  const [loadingOutbox, setLoadingOutbox] = useState(false);

  const mountedRef = useRef(true);
  const autoSelectedOutboxRef = useRef(false);
  const feedbackSeqRef = useRef(0);
  const toolSeqRef = useRef(0);
  const outboxSeqRef = useRef(0);

  const loadFeedback = useCallback(async () => {
    const seq = ++feedbackSeqRef.current;
    setLoadingFeedback(true);
    try {
      const res = await fetchSocialV2Feedback();
      if (!mountedRef.current || seq !== feedbackSeqRef.current) return;
      setFeedback(Array.isArray(res.entries) ? res.entries : []);
    } catch (err) {
      if (mountedRef.current && seq === feedbackSeqRef.current) {
        pushToast(formatErrorMessage(err, '加载 AI 反馈日志失败'), 'danger');
      }
    } finally {
      if (mountedRef.current && seq === feedbackSeqRef.current) {
        setLoadingFeedback(false);
      }
    }
  }, [pushToast]);

  const loadToolLog = useCallback(
    async (limit: number) => {
      const seq = ++toolSeqRef.current;
      setLoadingTools(true);
      try {
        const res = await fetchToolLog(limit);
        if (!mountedRef.current || seq !== toolSeqRef.current) return;
        setToolLog(Array.isArray(res.entries) ? res.entries : []);
      } catch (err) {
        if (mountedRef.current && seq === toolSeqRef.current) {
          pushToast(formatErrorMessage(err, '读取工具执行记录失败'), 'danger');
        }
      } finally {
        if (mountedRef.current && seq === toolSeqRef.current) {
          setLoadingTools(false);
        }
      }
    },
    [pushToast],
  );

  const loadOutbox = useCallback(
    async (convKey: string) => {
      const trimmed = convKey.trim();
      const seq = ++outboxSeqRef.current;
      setOutboxBatches([]);
      setOutboxError(null);
      if (!trimmed) {
        setLoadingOutbox(false);
        return;
      }
      setLoadingOutbox(true);
      try {
        const res = await fetchSendOutbox(trimmed);
        if (!mountedRef.current || seq !== outboxSeqRef.current) return;
        setOutboxBatches(Array.isArray(res.batches) ? res.batches : []);
      } catch (err) {
        if (mountedRef.current && seq === outboxSeqRef.current) {
          const msg = formatErrorMessage(err, '加载发送队列失败');
          setOutboxError(msg);
          pushToast(msg, 'danger');
        }
      } finally {
        if (mountedRef.current && seq === outboxSeqRef.current) {
          setLoadingOutbox(false);
        }
      }
    },
    [pushToast],
  );

  useEffect(() => {
    mountedRef.current = true;
    void loadFeedback();
    return () => {
      mountedRef.current = false;
    };
  }, [loadFeedback]);

  useEffect(() => {
    void loadToolLog(toolLimit);
  }, [loadToolLog, toolLimit]);

  useEffect(() => {
    if (autoSelectedOutboxRef.current || selectedOutboxKey) return;
    if (conversations.length > 0) {
      autoSelectedOutboxRef.current = true;
      const firstKey = conversations[0].key;
      setSelectedOutboxKey(firstKey);
      void loadOutbox(firstKey);
    }
  }, [conversations, selectedOutboxKey, loadOutbox]);

  const handleClearFeedback = () => {
    openConfirm({
      title: '确认清空 AI 运行反馈？',
      description: '将清除控制台内存中的运行反馈条目。',
      confirmLabel: '确认清空',
      danger: true,
      onConfirm: async () => {
        const clearEpoch = ++feedbackSeqRef.current;
        setLoadingFeedback(false);
        try {
          await clearSocialV2Feedback();
          if (!mountedRef.current || clearEpoch !== feedbackSeqRef.current) return;
          setFeedback([]);
          pushToast('已清空 AI 运行反馈', 'success');
        } catch (err) {
          if (mountedRef.current) {
            pushToast(formatErrorMessage(err, '清空反馈失败'), 'danger');
          }
        }
      },
    });
  };

  const handleClearToolLog = () => {
    openConfirm({
      title: '确认清空工具调用日志？',
      description: '将清除当前内存中的工具调用轨迹记录。',
      confirmLabel: '确认清空',
      danger: true,
      onConfirm: async () => {
        const clearEpoch = ++toolSeqRef.current;
        setLoadingTools(false);
        try {
          await clearToolLog();
          if (!mountedRef.current || clearEpoch !== toolSeqRef.current) return;
          setToolLog([]);
          pushToast('已清空工具调用日志', 'success');
        } catch (err) {
          if (mountedRef.current) {
            pushToast(formatErrorMessage(err, '清空工具日志失败'), 'danger');
          }
        }
      },
    });
  };

  const filteredFeedback = useMemo(
    () =>
      feedback.filter((item) =>
        feedbackLevel === 'all' ? true : item.level === feedbackLevel,
      ),
    [feedback, feedbackLevel],
  );

  const filteredToolLog = useMemo(() => {
    const q = toolQuery.trim().toLowerCase();
    if (!q) return toolLog;
    return toolLog.filter((entry) => JSON.stringify(entry).toLowerCase().includes(q));
  }, [toolLog, toolQuery]);

  const levelTone = (level: string): 'neutral' | 'warn' | 'danger' => {
    if (level === 'error') return 'danger';
    if (level === 'warning') return 'warn';
    return 'neutral';
  };

  const outboxItemTone = (state: string): 'success' | 'warn' | 'danger' | 'neutral' => {
    if (state === 'confirmed') return 'success';
    if (state === 'unknown' || state === 'not_committed') return 'warn';
    if (state === 'cancelled') return 'danger';
    return 'neutral';
  };

  return (
    <div className="space-y-4">
      <Card className="p-4">
        <CardHeader
          title="AI 运行反馈"
          subtitle="来自社交状态机与自动调度循环的提示、告警与错误事件"
          actions={
            <div className="flex flex-wrap items-center gap-2">
              <SegmentedControl
                value={feedbackLevel}
                onChange={setFeedbackLevel}
                options={[
                  { value: 'all', label: '全部' },
                  { value: 'info', label: 'info' },
                  { value: 'warning', label: 'warning' },
                  { value: 'error', label: 'error' },
                ]}
              />
              <Button
                size="sm"
                variant="secondary"
                disabled={loadingFeedback}
                onClick={() => void loadFeedback()}
              >
                <RefreshCw className="h-3.5 w-3.5" />
                刷新
              </Button>
              <Button
                size="sm"
                variant="ghost"
                disabled={feedback.length === 0}
                onClick={handleClearFeedback}
              >
                <Trash2 className="h-3.5 w-3.5" />
                清空
              </Button>
            </div>
          }
        />

        {filteredFeedback.length === 0 ? (
          <EmptyState
            title={loadingFeedback ? '加载反馈日志中…' : '暂无符合条件的运行反馈'}
          />
        ) : (
          <div className="divide-y divide-[var(--border-hairline)]">
            {filteredFeedback.map((entry) => (
              <div key={entry.id} className="flex flex-col gap-1 py-2.5 text-xs">
                <div className="flex flex-wrap items-center justify-between gap-2">
                  <div className="flex flex-wrap items-center gap-2">
                    <Badge tone={levelTone(entry.level)}>{entry.level}</Badge>
                    {entry.key && (
                      <button
                        type="button"
                        onClick={() => setInspector({ kind: 'conversation', id: entry.key! })}
                        className="font-mono text-[11px] text-[var(--accent)] hover:underline"
                      >
                        {entry.key}
                      </button>
                    )}
                  </div>
                  <span className="font-mono text-[11px] text-[var(--text-muted)]">
                    {formatDateTimeValue(entry.time)}
                  </span>
                </div>
                <div className="text-[var(--text-primary)]">{entry.message}</div>
              </div>
            ))}
          </div>
        )}
      </Card>

      <Card className="p-4">
        <CardHeader
          title="工具调用日志"
          subtitle="查看运行时工具调用参数、执行耗时与成功/失败结果"
          actions={
            <div className="flex flex-wrap items-center gap-2">
              <SegmentedControl
                value={String(toolLimit) as '50' | '200' | '500'}
                onChange={(v) => setToolLimit(Number(v) as 50 | 200 | 500)}
                options={[
                  { value: '50', label: '50 条' },
                  { value: '200', label: '200 条' },
                  { value: '500', label: '500 条' },
                ]}
              />
              <Button
                size="sm"
                variant="secondary"
                disabled={loadingTools}
                onClick={() => void loadToolLog(toolLimit)}
              >
                <RefreshCw className="h-3.5 w-3.5" />
                刷新
              </Button>
              <Button
                size="sm"
                variant="ghost"
                disabled={toolLog.length === 0}
                onClick={handleClearToolLog}
              >
                <Trash2 className="h-3.5 w-3.5" />
                清空
              </Button>
            </div>
          }
        />

        <div className="mb-3 max-w-sm">
          <SearchInput
            value={toolQuery}
            onChange={setToolQuery}
            placeholder="过滤工具名、会话键或详情…"
          />
        </div>

        {filteredToolLog.length === 0 ? (
          <EmptyState
            title={loadingTools ? '加载工具日志中…' : '暂无匹配的工具调用记录'}
          />
        ) : (
          <div className="divide-y divide-[var(--border-hairline)]">
            {filteredToolLog.map((entry, idx) => {
              const name = String(entry.tool || entry.name || `entry-${idx + 1}`);
              const convKey = entry.key || entry.conversation;
              const ts = typeof entry.time === 'number' ? entry.time : entry.at;
              const statusTone =
                entry.ok === false || entry.status === 'error'
                  ? 'danger'
                  : entry.ok === true
                    ? 'success'
                    : 'neutral';
              return (
                <div key={`${name}-${ts ?? idx}`} className="py-2.5 text-xs">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="font-mono font-semibold text-[var(--text-primary)]">
                        {name}
                      </span>
                      <Badge tone={statusTone}>
                        {entry.status || (entry.ok === false ? 'failed' : entry.ok === true ? 'ok' : 'logged')}
                      </Badge>
                      {convKey && (
                        <button
                          type="button"
                          onClick={() => setInspector({ kind: 'conversation', id: convKey })}
                          className="font-mono text-[11px] text-[var(--accent)] hover:underline"
                        >
                          {convKey}
                        </button>
                      )}
                    </div>
                    <span className="font-mono text-[11px] text-[var(--text-muted)]">
                      {formatDateTimeMs(ts)}
                    </span>
                  </div>
                  {entry.summary && (
                    <div className="mt-1 text-[var(--text-secondary)]">{entry.summary}</div>
                  )}
                  <details className="mt-1.5">
                    <summary className="cursor-pointer text-[11px] text-[var(--text-muted)] hover:text-[var(--text-primary)]">
                      查看原始详情 JSON
                    </summary>
                    <pre className="mt-1 overflow-x-auto rounded bg-[var(--bg-subtle)] p-2 font-mono text-[11px] text-[var(--text-secondary)]">
                      {JSON.stringify(entry, null, 2)}
                    </pre>
                  </details>
                </div>
              );
            })}
          </div>
        )}
      </Card>

      <Card className="p-4">
        <CardHeader
          title="发送发件箱检查（send-outbox）"
          subtitle="按会话键查看最近 20 个分段发送批次与回执确认情况"
          actions={
            <Button
              size="sm"
              variant="secondary"
              disabled={loadingOutbox || !selectedOutboxKey.trim()}
              onClick={() => void loadOutbox(selectedOutboxKey)}
            >
              <RefreshCw className="h-3.5 w-3.5" />
              查询批次
            </Button>
          }
        />

        <div className="mb-3 grid gap-2 sm:grid-cols-[1fr_240px]">
          {conversations.length > 0 ? (
            <select
              aria-label="选择会话查看发送队列"
              value={selectedOutboxKey}
              onChange={(e) => {
                const next = e.target.value;
                setSelectedOutboxKey(next);
                void loadOutbox(next);
              }}
              className={INPUT_CLASS}
            >
              <option value="">（请选择会话）</option>
              {conversations.map((c) => (
                <option key={c.key} value={c.key}>
                  {c.name ? `${c.name} (${c.key})` : c.key}
                </option>
              ))}
            </select>
          ) : (
            <input
              type="text"
              value={selectedOutboxKey}
              onChange={(e) => {
                outboxSeqRef.current += 1;
                setLoadingOutbox(false);
                setSelectedOutboxKey(e.target.value);
                setOutboxBatches([]);
                setOutboxError(null);
              }}
              placeholder="填入打算查询的对话标识，例如以 group: 开头的群聊或 private: 开头的单聊"
              className={INPUT_CLASS}
            />
          )}
          <input
            type="text"
            value={selectedOutboxKey}
            onChange={(e) => {
              outboxSeqRef.current += 1;
              setLoadingOutbox(false);
              setSelectedOutboxKey(e.target.value);
              setOutboxBatches([]);
              setOutboxError(null);
            }}
            onKeyDown={(e) => {
              if (e.key === 'Enter') {
                void loadOutbox(selectedOutboxKey);
              }
            }}
            placeholder="或手动输入会话键后回车查询"
            className={INPUT_CLASS}
          />
        </div>

        {outboxError && (
          <div className="mb-3 rounded-md border border-[var(--danger)]/40 bg-[var(--danger-soft)] px-3 py-2 text-xs text-[var(--text-primary)]">
            {outboxError}
          </div>
        )}

        {!selectedOutboxKey.trim() ? (
          <EmptyState title="请先选择或输入会话键" description="send-outbox 按会话维查询最近发送批次。" />
        ) : outboxBatches.length === 0 ? (
          <EmptyState
            title={
              loadingOutbox
                ? '查询发件箱中…'
                : outboxError
                  ? '查询发件箱失败'
                  : '该会话暂无发送批次记录'
            }
          />
        ) : (
          <div className="space-y-2.5">
            {outboxBatches.map((batch) => {
              const batchTone =
                batch.unknown.length > 0
                  ? 'warn'
                  : batch.sent === batch.total && batch.pending.length === 0
                    ? 'success'
                    : 'accent';
              return (
                <div
                  key={batch.batchId}
                  className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/35 p-3 text-xs"
                >
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="font-mono font-semibold text-[var(--text-primary)]">
                        {batch.batchId}
                      </span>
                      <Badge tone={batchTone}>
                        已发 {batch.sent}/{batch.total}
                      </Badge>
                      {batch.pending.length > 0 && (
                        <Badge tone="accent">待发 {batch.pending.length}</Badge>
                      )}
                      {batch.unknown.length > 0 && (
                        <Badge tone="warn">待确认 {batch.unknown.length}</Badge>
                      )}
                    </div>
                    <span className="font-mono text-[11px] text-[var(--text-muted)]">
                      {formatDateTimeMs(batch.createdAt)}
                    </span>
                  </div>
                  <div className="mt-2 space-y-1">
                    {batch.items.map((item) => (
                      <div
                        key={`${batch.batchId}-${item.idx}`}
                        className="flex flex-wrap items-center justify-between gap-2 rounded bg-[var(--bg-surface)] px-2 py-1"
                      >
                        <span className="min-w-0 flex-1 truncate text-[var(--text-primary)]">
                          #{item.idx + 1} · {item.text}
                        </span>
                        <div className="flex items-center gap-1.5">
                          <Badge tone={outboxItemTone(item.state)}>{item.state}</Badge>
                          {item.verified && <Badge tone="success">已核验</Badge>}
                          {item.messageId != null && (
                            <span className="font-mono text-[10px] text-[var(--text-muted)]">
                              msg:{item.messageId}
                            </span>
                          )}
                        </div>
                      </div>
                    ))}
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </Card>
    </div>
  );
};
