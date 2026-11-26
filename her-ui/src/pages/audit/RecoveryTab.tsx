import React, { useMemo } from 'react';
import { Activity, AlertTriangle, ArrowUpRight, CheckCircle2, RotateCcw } from 'lucide-react';
import {
  InboundGapRow,
  InboundGapsResponse,
  RecoveryRecord,
  TurnsResponse,
  resolveGlobalTurn,
  retryGlobalInboundGaps,
} from '../../api/security';
import { EmptyState } from '../../components/ConsoleWidgets';
import { Badge, Button, Card, CardHeader } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import { formatDateTimeMs, formatErrorMessage } from '../../utils/format';

const RECOVERY_COMPONENTS = [
  { value: '', label: '全部组件' },
  { value: 'dsh', label: 'dsh' },
  { value: 'onebot', label: 'onebot' },
  { value: 'model-chain', label: 'model-chain' },
  { value: 'queue', label: 'queue' },
  { value: 'alert', label: 'alert' },
  { value: 'snowluma', label: 'snowluma' },
  { value: 'process', label: 'process' },
  { value: 'stt', label: 'stt' },
];

const RECOVERY_OUTCOMES = [
  { value: '', label: '全部结果' },
  { value: 'ok', label: 'ok' },
  { value: 'failed', label: 'failed' },
  { value: 'unknown', label: 'unknown' },
  { value: 'skipped', label: 'skipped' },
];

const SELECT_CLASS =
  'rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 py-1.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none';

function outcomeTone(outcome: string): 'success' | 'danger' | 'warn' | 'neutral' {
  if (outcome === 'ok') return 'success';
  if (outcome === 'failed') return 'danger';
  if (outcome === 'unknown') return 'warn';
  return 'neutral';
}

interface RecoveryTabProps {
  turnsData: TurnsResponse | null;
  turnsError: string | null;
  gapsData: InboundGapsResponse | null;
  gapsError: string | null;
  recoveryRows: RecoveryRecord[];
  recoveryError: string | null;
  nextBefore: number | null;
  componentFilter: string;
  setComponentFilter: (val: string) => void;
  outcomeFilter: string;
  setOutcomeFilter: (val: string) => void;
  loading: boolean;
  loadingMore: boolean;
  onRefresh: () => Promise<void>;
  onLoadMore: () => Promise<void>;
}

export const RecoveryTab: React.FC<RecoveryTabProps> = ({
  turnsData,
  turnsError,
  gapsData,
  gapsError,
  recoveryRows,
  recoveryError,
  nextBefore,
  componentFilter,
  setComponentFilter,
  outcomeFilter,
  setOutcomeFilter,
  loading,
  loadingMore,
  onRefresh,
  onLoadMore,
}) => {
  const { navigate, setInspector, openConfirm, pushToast, refreshLiveData } = useConsole();

  const mergedTurns = useMemo(() => {
    const pausedMap = new Map((turnsData?.paused ?? []).map((p) => [p.turnId, p]));
    const rows = (turnsData?.turns ?? []).map((t) => ({
      ...t,
      pausedSummary: pausedMap.get(t.turnId),
      isPaused: t.state === 'paused' || pausedMap.has(t.turnId),
    }));
    const knownIds = new Set(rows.map((r) => r.turnId));
    for (const p of turnsData?.paused ?? []) {
      if (!knownIds.has(p.turnId)) {
        rows.unshift({
          turnId: p.turnId,
          code: p.code,
          key: p.key,
          reason: p.reason,
          state: 'paused',
          pauseReason: p.reason,
          pauseDetail: p.detail,
          pausedSummary: p,
          isPaused: true,
        });
      }
    }
    return rows.sort((a, b) => Number(b.isPaused) - Number(a.isPaused));
  }, [turnsData]);

  const orderedGaps = useMemo(() => {
    const unresolved = gapsData?.unresolvedGaps ?? [];
    const seen = new Set(unresolved.map((g) => g.id));
    const others = (gapsData?.gaps ?? []).filter((g) => !seen.has(g.id));
    return [...unresolved, ...others];
  }, [gapsData]);

  const handleResolveTurn = (
    turnId: string,
    convKey: string,
    action: 'retry' | 'abandon',
    pauseReason?: string | null,
  ) => {
    const sendUnresolved = action === 'retry' && pauseReason === 'send-unresolved';
    openConfirm({
      title: sendUnresolved
        ? '存在重复发送风险：确认重试该回合？'
        : action === 'retry'
          ? '确认重试该暂停回合？'
          : '确认放弃该暂停回合？',
      description: sendUnresolved
        ? `会话 ${convKey}（回合 ${turnId}）存在发送结果未知的消息：原消息可能已成功发出，强制重试可能导致群/好友收到重复内容。确认可以接受后再提交！`
        : `会话 ${convKey}（回合 ${turnId}）将标记为 ${action === 'retry' ? '重试并交回队列' : '放弃并关闭事件'}。`,
      confirmLabel: sendUnresolved
        ? '已知晓风险，确认重试'
        : action === 'retry'
          ? '确认重试'
          : '确认放弃',
      danger: action === 'abandon' || sendUnresolved,
      onConfirm: async () => {
        try {
          await resolveGlobalTurn(turnId, action, action === 'abandon' || sendUnresolved);
          pushToast(`回合 ${turnId} 已提交 ${action}`, 'success');
          await Promise.all([onRefresh(), refreshLiveData()]);
        } catch (err) {
          pushToast(formatErrorMessage(err, '处理暂停回合失败'), 'danger');
        }
      },
    });
  };

  const handleRetryGap = (gap: InboundGapRow) => {
    openConfirm({
      title: '确认重置并重试入站缺口补收？',
      description: `将清零会话 ${gap.conversation} 可重试缺口的重试计数与退避，并在下次重连/轮询时尝试补收。`,
      confirmLabel: '确认重试缺口',
      onConfirm: async () => {
        try {
          const res = await retryGlobalInboundGaps(gap.conversation);
          pushToast(`已重置 ${res.reset} 条缺口重试状态`, 'success');
          await onRefresh();
        } catch (err) {
          pushToast(formatErrorMessage(err, '重试入站缺口失败'), 'danger');
        }
      },
    });
  };

  return (
    <div className="space-y-4">
      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader
            title={`全局回合账本与暂停处置 (${turnsData?.pausedCount ?? 0} 个暂停)`}
            subtitle="跨会话展示活跃或等待管理员决策的回合"
          />
          <div className="p-4">
            {turnsError ? (
              <EmptyState
                icon={<AlertTriangle className="h-6 w-6 text-[var(--danger)]" />}
                title="回合账本读取失败"
                description={turnsError}
                action={
                  <Button size="sm" onClick={() => void onRefresh()}>
                    重新加载
                  </Button>
                }
              />
            ) : turnsData === null ? (
              <EmptyState
                icon={<Activity className="h-6 w-6" />}
                title="正在读取回合账本…"
              />
            ) : mergedTurns.length === 0 ? (
              <EmptyState
                icon={<CheckCircle2 className="h-6 w-6 text-[var(--success)]" />}
                title="当前无暂停或待恢复回合"
                description="所有已记录回合均处于正常推进或完结状态。"
              />
            ) : (
              <div className="space-y-2.5">
                {mergedTurns.map((turn) => {
                  const reasonText = turn.pauseReason || turn.pausedSummary?.reason || turn.reason;
                  return (
                    <div
                      key={turn.turnId}
                      className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)] p-3 text-xs"
                    >
                      <div className="flex flex-wrap items-center justify-between gap-2">
                        <div className="flex items-center gap-1.5">
                          <Badge tone={turn.isPaused ? 'warn' : 'neutral'}>{turn.state}</Badge>
                          <span className="font-mono text-[11px] font-medium text-[var(--text-primary)]">
                            {turn.key}
                          </span>
                        </div>
                        <Button
                          size="sm"
                          variant="ghost"
                          onClick={() => {
                            navigate('conversations');
                            setInspector({ kind: 'conversation', id: turn.key });
                          }}
                        >
                          会话详情
                          <ArrowUpRight className="h-3 w-3" />
                        </Button>
                      </div>
                      <div className="mt-1 font-mono text-[11px] text-[var(--text-muted)]">
                        turnId: {turn.turnId}
                        {turn.backend ? ` · ${turn.backend}` : ''}
                      </div>
                      {(reasonText || turn.pauseDetail) && (
                        <p className="mt-1 text-[11px] text-[var(--text-secondary)]">
                          {reasonText}
                          {turn.pauseDetail ? ` — ${turn.pauseDetail}` : ''}
                        </p>
                      )}
                      {turn.isPaused && reasonText === 'send-unresolved' && (
                        <div className="mt-1.5 rounded border border-amber-300 bg-[var(--warn-soft)] px-2 py-1 text-[10.5px] text-[var(--warn)] dark:border-amber-900">
                          发送结果未知：重试可能引发重复消息投递，请在确认后谨慎执行。
                        </div>
                      )}
                      {turn.isPaused && (
                        <div className="mt-2.5 flex items-center justify-end gap-2">
                          <Button
                            size="sm"
                            onClick={() =>
                              handleResolveTurn(turn.turnId, turn.key, 'retry', reasonText)
                            }
                          >
                            <RotateCcw className="h-3 w-3" />
                            重试回合
                          </Button>
                          <Button
                            size="sm"
                            variant="danger"
                            onClick={() =>
                              handleResolveTurn(turn.turnId, turn.key, 'abandon', reasonText)
                            }
                          >
                            放弃回合
                          </Button>
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        </Card>

        <Card>
          <CardHeader
            title={`全局入站缺口 (${gapsData?.unresolvedGapCount ?? 0} 个未解决)`}
            subtitle={`重复抑制 ${gapsData?.duplicates ?? 0} · 中断事件数 ${gapsData?.stages?.interrupted ?? gapsData?.interrupted?.length ?? 0}`}
          />
          <div className="p-4">
            {gapsError ? (
              <EmptyState
                icon={<AlertTriangle className="h-6 w-6 text-[var(--danger)]" />}
                title="入站缺口读取失败"
                description={gapsError}
                action={
                  <Button size="sm" onClick={() => void onRefresh()}>
                    重新加载
                  </Button>
                }
              />
            ) : gapsData === null ? (
              <EmptyState
                icon={<Activity className="h-6 w-6" />}
                title="正在读取入站缺口…"
              />
            ) : orderedGaps.length === 0 ? (
              <EmptyState
                icon={<CheckCircle2 className="h-6 w-6 text-[var(--success)]" />}
                title="暂无未解决的入站消息缺口"
                description="网关断线重连后的消息补收均已完成或无缺口。"
              />
            ) : (
              <div className="space-y-2.5">
                {orderedGaps.map((gap) => {
                  const unresolved = gap.status !== 'resolved' && gap.status !== 'recovered';
                  const blockedPrivateHistory = gap.reason === 'private-history-unverified';
                  return (
                    <div
                      key={gap.id}
                      className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)] p-3 text-xs"
                    >
                      <div className="flex flex-wrap items-center justify-between gap-2">
                        <div className="flex items-center gap-1.5">
                          <Badge tone={unresolved ? 'warn' : 'success'}>{gap.status}</Badge>
                          <span className="font-mono text-[11px] font-medium">
                            {gap.conversation}
                          </span>
                        </div>
                        {unresolved &&
                          (blockedPrivateHistory ? (
                            <Badge tone="neutral">私聊历史未校验（不予补收）</Badge>
                          ) : (
                            <Button size="sm" onClick={() => handleRetryGap(gap)}>
                              <RotateCcw className="h-3 w-3" />
                              重试缺口
                            </Button>
                          ))}
                      </div>
                      <div className="mt-1 text-[11px] text-[var(--text-muted)]">
                        区间 {formatDateTimeMs(gap.fromMs)} ~ {formatDateTimeMs(gap.toMs)} · 尝试{' '}
                        {gap.attempts} 次 · 已恢复 {gap.recoveredCount} 条
                      </div>
                      {(gap.reason || gap.detail) && (
                        <p className="mt-1 text-[11px] text-[var(--text-secondary)]">
                          {gap.reason}
                          {gap.detail ? `：${gap.detail}` : ''}
                        </p>
                      )}
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        </Card>
      </div>

      <Card>
        <CardHeader
          title={`依赖与恢复运行记录 (${recoveryRows.length})`}
          subtitle="按组件与结果过滤并按 seq 游标翻页"
          actions={
            <div className="flex flex-wrap items-center gap-2">
              <select
                value={componentFilter}
                onChange={(e) => setComponentFilter(e.target.value)}
                className={SELECT_CLASS}
                aria-label="按组件筛选"
              >
                {RECOVERY_COMPONENTS.map((opt) => (
                  <option key={opt.value || 'all'} value={opt.value}>
                    {opt.label}
                  </option>
                ))}
              </select>
              <select
                value={outcomeFilter}
                onChange={(e) => setOutcomeFilter(e.target.value)}
                className={SELECT_CLASS}
                aria-label="按结果筛选"
              >
                {RECOVERY_OUTCOMES.map((opt) => (
                  <option key={opt.value || 'all'} value={opt.value}>
                    {opt.label}
                  </option>
                ))}
              </select>
            </div>
          }
        />
        <div className="p-4">
          {recoveryError ? (
            <EmptyState
              icon={<AlertTriangle className="h-6 w-6 text-[var(--danger)]" />}
              title="恢复记录读取失败"
              description={recoveryError}
              action={
                <Button size="sm" onClick={() => void onRefresh()}>
                  重新加载
                </Button>
              }
            />
          ) : recoveryRows.length === 0 ? (
            <EmptyState
              icon={<Activity className="h-6 w-6" />}
              title={loading ? '正在加载恢复记录…' : '暂无符合条件的恢复记录'}
              description="可调整上方组件或结果筛选条件重试。"
            />
          ) : (
            <div className="space-y-3">
              <div className="overflow-x-auto">
                <table className="w-full border-collapse text-left text-xs">
                  <thead>
                    <tr className="border-b border-[var(--border-hairline)] text-[11px] text-[var(--text-muted)]">
                      <th className="py-2 pr-3 font-medium">序号 / 时间</th>
                      <th className="py-2 pr-3 font-medium">组件</th>
                      <th className="py-2 pr-3 font-medium">事件 / 触发</th>
                      <th className="py-2 pr-3 font-medium">结果</th>
                      <th className="py-2 font-medium">原因说明</th>
                    </tr>
                  </thead>
                  <tbody className="divide-y divide-[var(--border-hairline)]">
                    {recoveryRows.map((rec) => (
                      <tr key={rec.seq}>
                        <td className="whitespace-nowrap py-2 pr-3 text-[11px] text-[var(--text-muted)]">
                          <span className="font-mono">#{rec.seq}</span> ·{' '}
                          {formatDateTimeMs(rec.ts)}
                        </td>
                        <td className="py-2 pr-3">
                          <Badge tone="neutral">{rec.component}</Badge>
                        </td>
                        <td className="py-2 pr-3 text-[11px]">
                          <div className="font-mono text-[var(--text-primary)]">{rec.event}</div>
                          <div className="text-[10.5px] text-[var(--text-muted)]">
                            trigger={rec.trigger}
                            {rec.repeat && rec.repeat > 1 ? ` · 重复 ×${rec.repeat}` : ''}
                          </div>
                        </td>
                        <td className="py-2 pr-3">
                          <Badge tone={outcomeTone(rec.outcome)}>{rec.outcome}</Badge>
                        </td>
                        <td className="py-2 text-[11px] text-[var(--text-secondary)]">
                          {rec.reason || '—'}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>

              {nextBefore !== null && (
                <div className="flex justify-center pt-1">
                  <Button
                    size="sm"
                    disabled={loading || loadingMore}
                    onClick={() => void onLoadMore()}
                  >
                    {loadingMore ? '加载中…' : `加载更早记录 (before=${nextBefore})`}
                  </Button>
                </div>
              )}
            </div>
          )}
        </div>
      </Card>
    </div>
  );
};
