import React, { useCallback, useEffect, useRef, useState } from 'react';
import {
  Activity,
  AlertTriangle,
  ArrowUpRight,
  Clock,
  Copy,
  PauseCircle,
  PlayCircle,
  RefreshCw,
} from 'lucide-react';
import {
  PendingItem,
  PresetOption,
  StatusDetail,
  TokenSeriesResponse,
  TokenSummaryResponse,
  fetchInboundGaps,
  fetchPendingDetailed,
  fetchPresetsOptional,
  fetchStatusDetail,
  fetchTokenSeries24h,
  fetchTokenSummary,
  fetchTurns,
  saveMode,
  saveRoleMode,
  saveSocialV2Activity,
} from '../api/run';
import { Badge, Button, Card, CardHeader, PageHeader, Switch } from '../components/Primitives';
import { useConsole } from '../stores/consoleStore';

const MODE_LABELS: Record<string, string> = {
  reserved2: '二代仿真 (reserved2)',
  chat: '普通聊天 (chat)',
  reserved: '一代仿真 (reserved)',
  'closed-agent': '管理员代理 (closed-agent)',
};

function formatCost(symbol: string | undefined, cost: number | undefined): string {
  if (typeof cost !== 'number' || Number.isNaN(cost)) return '—';
  return `${symbol || '¥'}${cost.toFixed(4)}`;
}

export const OverviewPage: React.FC = () => {
  const {
    navigate,
    setInspector,
    updateAvailable,
    conversations,
    pushToast,
    refreshLiveData,
  } = useConsole();

  const [loading, setLoading] = useState(true);
  const [lastUpdated, setLastUpdated] = useState<string>('—');
  const [partialError, setPartialError] = useState<string | null>(null);
  const [status, setStatus] = useState<StatusDetail | null>(null);
  const [pending, setPending] = useState<PendingItem[]>([]);
  const [presets, setPresets] = useState<PresetOption[]>([]);
  const [presetsAvailable, setPresetsAvailable] = useState(false);
  const [series24h, setSeries24h] = useState<TokenSeriesResponse | null>(null);
  const [tokenSummary, setTokenSummary] = useState<TokenSummaryResponse | null>(null);
  const [pausedTurnCount, setPausedTurnCount] = useState(0);
  const [unresolvedGapCount, setUnresolvedGapCount] = useState(0);

  const [modeDraft, setModeDraft] = useState('reserved2');
  const [presetDraft, setPresetDraft] = useState('');
  const [busyAction, setBusyAction] = useState(false);
  const [customReplies, setCustomReplies] = useState<Record<string, string>>({});

  const loadSeqRef = useRef(0);
  const mountedRef = useRef(true);
  const modeTouchedRef = useRef(false);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const loadAll = useCallback(async (quiet = false, resetModeDraft = false) => {
    const seq = ++loadSeqRef.current;
    if (!quiet) setLoading(true);

    const [statusRes, pendingRes, presetsRes, seriesRes, summaryRes, turnsRes, gapsRes] =
      await Promise.allSettled([
        fetchStatusDetail(),
        fetchPendingDetailed(),
        fetchPresetsOptional(),
        fetchTokenSeries24h(),
        fetchTokenSummary(),
        fetchTurns(),
        fetchInboundGaps(),
      ]);

    if (!mountedRef.current || seq !== loadSeqRef.current) return;

    const failedSources: string[] = [];

    if (statusRes.status === 'fulfilled') {
      const s = statusRes.value;
      setStatus(s);
      if (resetModeDraft || !modeTouchedRef.current) {
        setModeDraft(s.mode || 'reserved2');
        setPresetDraft(s.closedAgentPreset || '');
        modeTouchedRef.current = false;
      }
    } else {
      failedSources.push('运行状态');
    }

    if (pendingRes.status === 'fulfilled') {
      setPending(pendingRes.value.pending);
    } else {
      failedSources.push('待处理队列');
    }

    if (presetsRes.status === 'fulfilled') {
      setPresetsAvailable(presetsRes.value.available);
      setPresets(presetsRes.value.presets);
    } else {
      failedSources.push('代理预设');
    }

    if (seriesRes.status === 'fulfilled') {
      setSeries24h(seriesRes.value);
    } else {
      failedSources.push('24小时花费');
    }

    if (summaryRes.status === 'fulfilled') {
      setTokenSummary(summaryRes.value);
    } else {
      failedSources.push('累计花费');
    }

    if (turnsRes.status === 'fulfilled') {
      setPausedTurnCount(turnsRes.value.pausedCount ?? turnsRes.value.paused.length);
    } else {
      failedSources.push('轮次状态');
    }

    if (gapsRes.status === 'fulfilled') {
      setUnresolvedGapCount(gapsRes.value.unresolvedGapCount ?? gapsRes.value.unresolvedGaps.length);
    } else {
      failedSources.push('入站缺口');
    }

    if (failedSources.length > 0) {
      setPartialError(`部分监控数据读取失败：${failedSources.join('、')}`);
    } else {
      setPartialError(null);
    }

    if (statusRes.status === 'fulfilled') {
      const now = new Date();
      setLastUpdated(
        `${String(now.getHours()).padStart(2, '0')}:${String(now.getMinutes()).padStart(2, '0')}:${String(now.getSeconds()).padStart(2, '0')}`,
      );
    }

    setLoading(false);
  }, []);

  const refreshAll = useCallback(
    async (quiet = false, resetModeDraft = false) => {
      await Promise.all([loadAll(quiet, resetModeDraft), refreshLiveData()]);
    },
    [loadAll, refreshLiveData],
  );

  useEffect(() => {
    void loadAll(false, true);
    const timer = window.setInterval(() => {
      if (!document.hidden) {
        void refreshAll(true, false);
      }
    }, 15_000);
    const onVisibility = () => {
      if (!document.hidden) {
        void refreshAll(true, false);
      }
    };
    document.addEventListener('visibilitychange', onVisibility);
    return () => {
      window.clearInterval(timer);
      document.removeEventListener('visibilitychange', onVisibility);
    };
  }, [loadAll, refreshAll]);

  const handleSaveMode = async () => {
    if (busyAction) return;
    setBusyAction(true);
    try {
      await saveMode(modeDraft, modeDraft === 'closed-agent' ? presetDraft : undefined);
      modeTouchedRef.current = false;
      pushToast(`参与模式已切换为 ${MODE_LABELS[modeDraft] || modeDraft}`, 'success');
      await refreshAll(true, true);
    } catch (err) {
      pushToast(err instanceof Error ? err.message : '切换参与模式失败', 'danger');
    } finally {
      if (mountedRef.current) setBusyAction(false);
    }
  };

  const handleToggleSilent = async (nextSilent: boolean) => {
    if (busyAction) return;
    setBusyAction(true);
    try {
      const target = nextSilent ? 'silent' : 'active';
      await saveRoleMode(target);
      pushToast(nextSilent ? '已开启全局静默模式' : '已恢复全局活跃响应', 'success');
      await Promise.all([loadAll(true, false), refreshLiveData()]);
    } catch (err) {
      pushToast(err instanceof Error ? err.message : '切换静默状态失败', 'danger');
    } finally {
      if (mountedRef.current) setBusyAction(false);
    }
  };

  const handleTogglePaused = async (nextPaused: boolean) => {
    if (busyAction) return;
    setBusyAction(true);
    try {
      await saveSocialV2Activity(nextPaused);
      pushToast(nextPaused ? '已暂停二代仿真调度' : '已恢复二代仿真调度', 'success');
      await Promise.all([loadAll(true, false), refreshLiveData()]);
    } catch (err) {
      pushToast(err instanceof Error ? err.message : '切换暂停状态失败', 'danger');
    } finally {
      if (mountedRef.current) setBusyAction(false);
    }
  };

  const copyCommand = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      pushToast(`已复制指令：${text}`, 'success');
    } catch {
      pushToast(`请手动复制：${text}`, 'info');
    }
  };

  const inspectConversation = (key: string) => {
    navigate('conversations');
    setInspector({ kind: 'conversation', id: key });
  };

  const activityLines = (status?.activity || '')
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean)
    .reverse();

  const recoveryAlerts = pausedTurnCount + unresolvedGapCount;

  return (
    <div className="space-y-4">
      <PageHeader
        title="总览"
        description="实时运行态势、待处理审批与提问、快捷模式切换及近期活动日志。"
        badge={
          status?.socialV2Paused ? (
            <Badge tone="warn">已暂停</Badge>
          ) : (
            <Badge tone="success">运行中</Badge>
          )
        }
        actions={
          <div className="flex items-center gap-2 text-xs text-[var(--text-muted)]">
            <span className="inline-flex items-center gap-1 tabular-nums">
              <Clock className="h-3.5 w-3.5" />
              上次更新 {lastUpdated}
            </span>
              <Button
                size="sm"
                variant="secondary"
                disabled={loading}
                onClick={() => void refreshAll(false, false)}
              >
                <RefreshCw className="h-3.5 w-3.5" />
                立即刷新
              </Button>
          </div>
        }
      />

      {partialError && (
        <Card className="px-4 py-2.5 text-xs text-[var(--warn)]">{partialError}</Card>
      )}

      {updateAvailable && (
        <Card className="flex flex-wrap items-center justify-between gap-2 border-blue-200 bg-[var(--accent-soft)] px-4 py-2.5 text-xs dark:border-blue-900">
          <div className="font-medium text-[var(--accent)]">
            检测到可用新版本 {updateAvailable}，可前往运维中心审阅并批准更新。
          </div>
          <Button size="sm" variant="primary" onClick={() => navigate('system', 'updates')}>
            查看更新
          </Button>
        </Card>
      )}

      <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
        <Card className="p-3.5">
          <div className="text-[11px] font-medium text-[var(--text-muted)]">桥接与网关</div>
          <div className="mt-1.5 flex items-center gap-2">
            <Badge tone={status?.snowluma?.connected ? 'success' : 'warn'}>
              {status?.snowluma?.connected ? 'QQ 网关已连接' : 'QQ 网关未知/断开'}
            </Badge>
            <Badge tone={status?.dshReady ? 'success' : 'neutral'}>
              {status?.dshReady ? 'DSH 就绪' : 'DSH 待机'}
            </Badge>
          </div>
          <div className="mt-2 truncate text-[11px] text-[var(--text-secondary)]">
            {status?.snowluma?.hint ||
              (status?.role ? `当前人设：${status.role}` : '使用默认人设分配')}
          </div>
        </Card>

        <Card className="p-3.5">
          <div className="text-[11px] font-medium text-[var(--text-muted)]">待处理审批与提问</div>
          <div className="mt-1 text-xl font-semibold tabular-nums text-[var(--text-primary)]">
            {pending.length}
          </div>
          <div className="mt-1 text-[11px] text-[var(--text-secondary)]">
            {pending.length > 0 ? '需管理员在 QQ 私聊确认/答复' : '当前无阻塞的待办项'}
          </div>
        </Card>

        <Card className="p-3.5">
          <div className="text-[11px] font-medium text-[var(--text-muted)]">近 24 小时花费</div>
          <div className="mt-1 text-xl font-semibold tabular-nums text-[var(--text-primary)]">
            {formatCost(series24h?.symbol || tokenSummary?.symbol, series24h?.totals?.cost)}
          </div>
          <div className="mt-1 flex items-center justify-between text-[11px] text-[var(--text-secondary)]">
            <span>
              累计总花费：
              {formatCost(tokenSummary?.symbol, tokenSummary?.totals?.cost)}
            </span>
            <button
              type="button"
              onClick={() => navigate('models', 'spend')}
              className="text-[var(--accent)] hover:underline"
            >
              详情
            </button>
          </div>
        </Card>

        <Card className="p-3.5">
          <div className="text-[11px] font-medium text-[var(--text-muted)]">活跃会话与恢复</div>
          <div className="mt-1 flex items-baseline gap-2">
            <span className="text-xl font-semibold tabular-nums text-[var(--text-primary)]">
              {conversations.length}
            </span>
            <span className="text-xs text-[var(--text-muted)]">已记录会话</span>
          </div>
          <div className="mt-1 flex items-center justify-between text-[11px]">
            <span
              className={
                recoveryAlerts > 0 ? 'font-medium text-[var(--warn)]' : 'text-[var(--text-secondary)]'
              }
            >
              暂停轮次 {pausedTurnCount} · 入站缺口 {unresolvedGapCount}
            </span>
            <button
              type="button"
              onClick={() => navigate('conversations')}
              className="text-[var(--accent)] hover:underline"
            >
              会话台
            </button>
          </div>
        </Card>
      </div>

      <div className="grid gap-4 lg:grid-cols-3">
        <Card className="lg:col-span-2">
          <CardHeader
            title="待处理审批与提问"
            subtitle="只读展示 /api/pending 队列：当前后端通过管理员 QQ 私聊接收答复，可在此一键复制指令或答复文本"
            actions={<Badge tone={pending.length > 0 ? 'warn' : 'neutral'}>{pending.length} 项</Badge>}
          />
          <div className="divide-y divide-[var(--border-hairline)]">
            {pending.length === 0 ? (
              <div className="px-4 py-8 text-center text-xs text-[var(--text-muted)]">
                暂无待处理审批或提问
              </div>
            ) : (
              pending.map((item, idx) => {
                const itemKey = `${item.key}-${item.requestId || item.sessionId || idx}`;
                return (
                <div key={itemKey} className="space-y-2 p-4 text-xs">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <div className="flex flex-wrap items-center gap-1.5">
                      <Badge tone={item.kind === 'approval' ? 'warn' : 'accent'}>
                        {item.kind === 'approval' ? '操作审批' : '等待回答'}
                      </Badge>
                      <span className="font-mono font-medium text-[var(--text-primary)]">
                        {item.key}
                      </span>
                      {item.toolName && <Badge tone="neutral">{item.toolName}</Badge>}
                      {item.status === 'outcome-unknown' && (
                        <Badge tone="danger">
                          <AlertTriangle className="h-3 w-3" />
                          状态待核实
                        </Badge>
                      )}
                    </div>
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={() => inspectConversation(item.key)}
                    >
                      检视会话
                      <ArrowUpRight className="h-3.5 w-3.5" />
                    </Button>
                  </div>

                  {item.target && (
                    <div className="text-[var(--text-secondary)]">
                      目标会话：<code className="font-mono">{item.target}</code>
                    </div>
                  )}
                  {item.message && (
                    <div className="rounded border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/50 p-2.5 text-[var(--text-primary)] whitespace-pre-wrap">
                      {item.message}
                    </div>
                  )}

                  {item.status === 'outcome-unknown' ? (
                    <div className="rounded border border-rose-300 bg-[var(--danger-soft)]/50 p-2.5 text-[11px] text-[var(--danger)] dark:border-rose-900">
                      回执结果不明：请先在 DSH 核对实际执行结果，或点击上方「检视会话」进入恢复与队列重置会话；此时无法直接通过回复批准或拒绝来结算。
                    </div>
                  ) : (
                    <>
                      {item.questions && item.questions.length > 0 && (
                        <div className="space-y-2">
                          {item.questions.length > 1 && (
                            <div className="rounded border border-amber-300 bg-[var(--warn-soft)]/40 px-2.5 py-1.5 text-[11px] text-[var(--warn)] dark:border-amber-900">
                              多题说明：在 QQ 管理员私聊中回复的一条文本会统一应用于本次请求的全部问题。
                            </div>
                          )}
                          {item.questions.map((q, qIdx) => (
                            <div
                              key={q.id || qIdx}
                              className="space-y-1.5 rounded border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/50 p-2.5"
                            >
                              <div className="font-medium text-[var(--text-primary)]">{q.question}</div>
                              {q.options && q.options.length > 0 && (
                                <div className="flex flex-wrap gap-1.5">
                                  {q.options.map((opt) => (
                                    <Button
                                      key={opt.label}
                                      size="sm"
                                      variant="secondary"
                                      onClick={() => void copyCommand(opt.label)}
                                    >
                                      <Copy className="h-3 w-3" />
                                      复制答复「{opt.label}」
                                    </Button>
                                  ))}
                                </div>
                              )}
                            </div>
                          ))}
                          {(() => {
                            const replyStateKey = `${itemKey}-reply`;
                            const customReply = customReplies[replyStateKey] || '';
                            const isMulti = item.questions.length > 1;
                            return (
                              <div className="flex flex-wrap items-center gap-1.5">
                                <input
                                  type="text"
                                  value={customReply}
                                  onChange={(e) =>
                                    setCustomReplies((prev) => ({
                                      ...prev,
                                      [replyStateKey]: e.target.value,
                                    }))
                                  }
                                  placeholder={
                                    isMulti
                                      ? '输入用于 QQ 私聊回复的全部问题统一文本…'
                                      : '输入要回复给 QQ 的自定义文本…'
                                  }
                                  className="h-7 min-w-[200px] flex-1 rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
                                />
                                <Button
                                  size="sm"
                                  variant="secondary"
                                  disabled={!customReply.trim()}
                                  onClick={() => void copyCommand(customReply.trim())}
                                >
                                  <Copy className="h-3 w-3" />
                                  {isMulti ? '复制统一回复' : '复制回复'}
                                </Button>
                              </div>
                            );
                          })()}
                        </div>
                      )}

                      {item.kind === 'approval' && (
                        <div className="flex flex-wrap items-center gap-2 pt-1">
                          {item.requestId ? (
                            <>
                              <Button
                                size="sm"
                                variant="primary"
                                onClick={() => void copyCommand(`/approve ${item.requestId}`)}
                              >
                                <Copy className="h-3 w-3" />
                                复制批准指令
                              </Button>
                              <Button
                                size="sm"
                                variant="danger"
                                onClick={() => void copyCommand(`/deny ${item.requestId}`)}
                              >
                                <Copy className="h-3 w-3" />
                                复制拒绝指令
                              </Button>
                            </>
                          ) : (
                            <>
                              <Button size="sm" variant="primary" onClick={() => void copyCommand('通过')}>
                                <Copy className="h-3 w-3" />
                                复制「通过」
                              </Button>
                              <Button size="sm" variant="danger" onClick={() => void copyCommand('拒绝')}>
                                <Copy className="h-3 w-3" />
                                复制「拒绝」
                              </Button>
                            </>
                          )}
                        </div>
                      )}
                    </>
                  )}
                </div>
                );
              })
            )}
          </div>
        </Card>

        <Card>
          <CardHeader title="快捷运行控制" subtitle="即时生效的全局参与开关与模式切换" />
          <div className="space-y-4 p-4 text-xs">
            <div className="space-y-2">
              <label className="block space-y-1">
                <span className="text-[var(--text-secondary)]">全局参与模式</span>
                <select
                  value={modeDraft}
                  onChange={(e) => {
                    modeTouchedRef.current = true;
                    setModeDraft(e.target.value);
                  }}
                  className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
                >
                  {Object.entries(MODE_LABELS).map(([val, label]) => (
                    <option key={val} value={val}>
                      {label}
                    </option>
                  ))}
                </select>
              </label>

              {modeDraft === 'closed-agent' && (
                <label className="block space-y-1">
                  <span className="text-[var(--text-secondary)]">管理员代理预设</span>
                  {presetsAvailable && presets.length > 0 ? (
                    <select
                      value={presetDraft}
                      onChange={(e) => {
                        modeTouchedRef.current = true;
                        setPresetDraft(e.target.value);
                      }}
                      className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
                    >
                      <option value="">（默认预设）</option>
                      {presets.map((p) => (
                        <option key={p.id} value={p.id}>
                          {p.id} ({p.trust})
                        </option>
                      ))}
                    </select>
                  ) : (
                    <input
                      type="text"
                      value={presetDraft}
                      onChange={(e) => {
                        modeTouchedRef.current = true;
                        setPresetDraft(e.target.value);
                      }}
                      placeholder="可选预设 ID（DSH 未连时手动输入）"
                      className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
                    />
                  )}
                </label>
              )}

              <Button
                size="sm"
                variant="primary"
                disabled={busyAction}
                onClick={() => void handleSaveMode()}
                className="w-full"
              >
                切换参与模式
              </Button>
            </div>

            <div className="space-y-2 border-t border-[var(--border-hairline)] pt-3">
              <div className="flex items-center justify-between">
                <div>
                  <div className="font-medium text-[var(--text-primary)]">全局静默模式</div>
                  <div className="text-[11px] text-[var(--text-muted)]">关闭主动及被动发送（工具例外除外）</div>
                </div>
                <Switch
                  checked={status?.roleMode === 'silent'}
                  disabled={busyAction || !status}
                  onChange={(next) => void handleToggleSilent(next)}
                  label="全局静默模式"
                />
              </div>

              <div className="flex items-center justify-between">
                <div className="flex items-center gap-1.5">
                  {status?.socialV2Paused ? (
                    <PauseCircle className="h-3.5 w-3.5 text-[var(--warn)]" />
                  ) : (
                    <PlayCircle className="h-3.5 w-3.5 text-[var(--success)]" />
                  )}
                  <div>
                    <div className="font-medium text-[var(--text-primary)]">暂停二代仿真调度</div>
                    <div className="text-[11px] text-[var(--text-muted)]">仅冻结 reserved2 唤醒与代理派发</div>
                  </div>
                </div>
                <Switch
                  checked={status?.socialV2Paused === true}
                  disabled={busyAction || !status}
                  onChange={(next) => void handleTogglePaused(next)}
                  label="暂停二代仿真调度"
                />
              </div>
            </div>
          </div>
        </Card>
      </div>

      <Card>
        <CardHeader
          title={
            <span className="inline-flex items-center gap-1.5">
              <Activity className="h-4 w-4 text-[var(--accent)]" />
              近期活动日志（最新 100 条）
            </span>
          }
          actions={
            <span className="text-[11px] tabular-nums text-[var(--text-muted)]">
              {activityLines.length} 行
            </span>
          }
        />
        <div className="max-h-72 overflow-y-auto p-3 font-mono text-[11px] leading-relaxed text-[var(--text-secondary)]">
          {activityLines.length === 0 ? (
            <div className="py-4 text-center text-[var(--text-muted)]">暂无活动记录</div>
          ) : (
            activityLines.map((line, idx) => (
              <div
                key={`${idx}-${line.slice(0, 24)}`}
                className="border-b border-[var(--border-hairline)]/50 py-1 last:border-b-0 break-all"
              >
                {line}
              </div>
            ))
          )}
        </div>
      </Card>
    </div>
  );
};
